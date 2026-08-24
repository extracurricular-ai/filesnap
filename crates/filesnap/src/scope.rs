//! Tracking scope: which files a checkpoint observes.
//!
//! Three partitions, unioned. Each answers a different question, and each is
//! bounded by something other than the size of the directory tree — which is
//! the property a plain subtree walk lacks, and the reason it was abandoned:
//! on this repository it enumerated 57k files and 100 GB, almost all of it
//! build output.
//!
//! 1. **Git-tracked** — what the project itself considers its files, read
//!    from the index. Bounded by the project rather than by what has been
//!    built into it: the same tree that scanned to 100 GB is 6k files and
//!    56 MB here, because build output is precisely what is not committed.
//!    Empty when there is no repository, which costs nothing — the other two
//!    partitions carry that case.
//! 2. **Edit-touched** — paths the agent's own tools have written, carried
//!    forward for the session. Unbounded on purpose: its size is set by what
//!    the agent did, not by what is on disk, and every entry is a file
//!    someone deliberately changed.
//! 3. **Recently modified** — the residue. Catches shell-made changes to
//!    files outside the other two, where "changed recently" is exactly the
//!    signal that matters. Capped, since this is the one partition a large
//!    tree can flood.
//!
//! The ignore file uses gitignore syntax but is deliberately separate from
//! `.gitignore`: session history may track files git ignores, and ignore
//! semantics here are symmetric — an ignored path is never snapshotted,
//! never restored, and never deleted by a restore.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use ignore::WalkBuilder;
pub use ignore::gitignore::Gitignore;
pub use ignore::gitignore::GitignoreBuilder;

/// Name of the dedicated snapshot ignore file, in gitignore syntax.
///
/// This is a name the user types, so it carries no vendor prefix.
pub const SNAPSHOT_IGNORE_FILENAME: &str = ".filesnapignore";

/// Whether a scan descends into dot-files and dot-directories.
///
/// `.git` is excluded either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenFiles {
    /// Hidden entries are invisible to scans — the default, and what keeps
    /// `.env` and credential files out of the store. Work product that
    /// happens to be hidden still enters through the edit hook.
    Skip,
    /// Hidden entries are scanned like any other file.
    Track,
}

/// Git-style marker walk-up: return the nearest ancestor of `start`
/// (inclusive) containing one of `markers`.
pub fn find_workspace_root(start: &Path, markers: &[String]) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        for marker in markers {
            if d.join(marker).exists() {
                return Some(d.to_path_buf());
            }
        }
        dir = d.parent();
    }
    None
}

/// Compile the ignore matcher for `root` from its snapshot ignore file.
/// A missing ignore file yields an empty matcher (nothing ignored).
pub fn load_ignore(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    builder.add(root.join(SNAPSHOT_IGNORE_FILENAME));
    // An unparsable ignore file degrades to "nothing ignored" rather than
    // failing the checkpoint; restores stay conservative either way.
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Symmetric protection check: is `path` invisible to snapshot operations?
pub fn is_ignored(ignore: &Gitignore, path: &Path) -> bool {
    ignore.matched_path_or_any_parents(path, false).is_ignore()
}

/// Why a file the scan saw is not in the snapshot.
///
/// Three reasons, because they call for three different responses and were
/// previously one bare `continue` and one undifferentiated counter. Over the
/// size limit is a bound working as designed; unreadable is usually
/// permissions and usually fixable; not-a-regular-file is out of scope for v1
/// and always will be for some paths (D23, C8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DropReason {
    /// Larger than [`ScanLimits::max_file_bytes`].
    OverSizeLimit,
    /// Present, but could not be read — permissions, most often.
    Unreadable,
    /// A symlink, socket, device: out of scope for v1.
    NotARegularFile,
}

/// A file left out of the snapshot, and why.
pub type Drop = (PathBuf, DropReason);

/// What a scan picked up, and what it passed over.
#[derive(Debug, Default)]
pub struct Scan {
    pub files: BTreeSet<PathBuf>,
    /// Complete for this scan. A capture bounds it before reporting; see
    /// [`crate::CheckpointStats`].
    pub dropped: Vec<Drop>,
}

/// What bounds the recency partition.
///
/// A parameter with a correct default, not a setting. IV.4 forbids a bound
/// being a knob a user has to find, and requires the engine to be affordable
/// where nothing is configured — both still hold. What is parameterised is
/// the *mechanism*, for whoever embeds it; the CLI does not expose this, and
/// overriding it is usually the wrong move.
///
/// It replaces two `pub const`s in a private module, which were unreachable
/// from outside the crate — surface that looked like API and was not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    /// How many recently-modified files the recency partition carries.
    pub max_files: usize,
    /// Largest file that partition will pick up.
    ///
    /// Its job is to exclude pathological objects — media, model weights,
    /// database files — not to draw a line through text. Generated code, lock
    /// files, notebooks and fixtures routinely reach a few MB and are exactly
    /// what a user wants back, so the limit sits well above them. Git-tracked
    /// files are not subject to it: whatever the project commits is the
    /// project's own content, however large.
    pub max_file_bytes: u64,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_files: 100,
            max_file_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Directory names the recency partition never descends into.
///
/// Decision 6 rejected exclusion lists as too inflexible, and preferred an
/// activity model. That preference stands for *what to keep* — this is a
/// different question. These directories churn constantly, so recency
/// selects them ahead of everything else and the partition fills with build
/// output before reaching a single source file. Excluding them is what makes
/// the activity signal usable, not a replacement for it.
///
/// **This list only affects the recency partition.** Anything git tracks
/// arrives through the index regardless of what is named here, so adding a
/// name costs exactly one thing: *untracked* files underneath it stop being
/// tracked. That is the whole test for whether a name belongs. Under
/// `node_modules/` or `Pods/` everything untracked is machine-generated and
/// excluding it is pure gain; under `packages/` — the standard source layout
/// for pnpm and yarn workspaces, present in four checkouts on this machine
/// alone — an untracked file is usually something a person just wrote. So
/// `packages` must never be added, and neither may `bin`, `obj`, `Library`,
/// `Temp` or `env`: each is a real directory name in some ecosystem and a
/// perfectly ordinary source directory in others, and the cost of guessing
/// wrong is silent.
///
/// Most modern tooling hides its state behind a dot — `.next`, `.gradle`,
/// `.tox`, `.mypy_cache`, `.terraform`, `.dart_tool`, `.stack-work`,
/// `.bundle`, `.parcel-cache` — and hidden entries are already excluded by
/// default, which is why this list stays short. What is left is the
/// ecosystems that, like node, vendor their dependencies into the project in
/// plain sight.
const RECENT_SKIP_DIRS: &[&str] = &[
    // Dependencies installed in-tree.
    "node_modules",     // npm / yarn / pnpm
    "bower_components", // bower
    "vendor",           // go, php composer, ruby bundler
    "Pods",             // cocoapods — a full source copy of every pod
    "Carthage",         // carthage
    "deps",             // elixir mix
    ".venv",
    "venv", // python — `.venv` is redundant under the hidden rule, `venv` is not
    // Build output, which churns hardest of all.
    "target",        // rust, maven
    "build",         // gradle, cmake, many others
    "_build",        // elixir mix, ocaml dune
    "dist",          // python, javascript
    "dist-newstyle", // haskell cabal
    "out",
    "DerivedData", // xcode
    "__pycache__",
];

/// Everything a capture should look at: the three partitions over `roots`,
/// plus `already_known` — paths this thread has observed before, whatever
/// partition first brought them in.
///
/// Both the turn-start checkpoint and the safety checkpoint a restore takes
/// go through here, but **they do not agree on a single argument** — not the
/// roots, not `already_known`, not `include_hidden`. Sharing this function
/// therefore guarantees nothing on its own, and an earlier version of this
/// comment claimed otherwise, which would have sent anyone checking the
/// property off to compare the wrong things.
///
/// What actually makes the safety checkpoint sufficient is a coverage
/// property: a restore can only write `target.entries` and only delete
/// `target.absent`, and the safety scan is handed the thread's entire
/// observed history — which is the union of exactly those two sets over
/// every manifest in its log. So it is always asked about a superset of what
/// the restore can touch, whatever the other arguments say.
///
/// The consequence worth guarding: narrowing that history breaks
/// undoability without touching a line of restore code. Passing only recent
/// turns would look like a harmless optimization — loading every manifest is
/// not free — and would leave files restorable but not undoable.
///
/// The order is load-bearing. Recency is the *residue* partition, so it runs
/// last and is told what the other two already hold: its budget is small and
/// fixed, and spending it on files the git index already covers is spending it
/// on nothing. Measured on this repository before the exclusion existed, 97 of
/// its 100 slots went to files the git partition had already contributed —
/// which is invisible until enough tracked files are touched at once (a
/// `cargo fmt`, a branch switch, a codegen run) to push the genuinely
/// untracked ones off the end of the list entirely.
pub fn tracked_files(
    roots: &[PathBuf],
    already_known: impl IntoIterator<Item = PathBuf>,
    hidden: HiddenFiles,
    limits: ScanLimits,
) -> Scan {
    let ignores: Vec<Gitignore> = roots.iter().map(|root| load_ignore(root)).collect();

    let mut scan = Scan {
        files: already_known.into_iter().collect(),
        dropped: Vec::new(),
    };
    for (root, ignore) in roots.iter().zip(&ignores) {
        scan.files.extend(git_tracked_files(root, ignore));
    }
    for (root, ignore) in roots.iter().zip(&ignores) {
        let picked = recent_files(root, ignore, hidden, &scan.files, limits);
        scan.files.extend(picked.files);
        scan.dropped.extend(picked.dropped);
    }
    scan
}

/// Files the project's own index lists that lie under `root`.
///
/// Reads the index directly rather than shelling out: the index is a file,
/// `git ls-files` is a process, and this runs at the head of every turn.
/// Returns empty for anything with no repository above it, which is not an
/// error — it just means this partition contributes nothing.
///
/// Discovery walks *up*, because a session is far more often opened in a
/// subdirectory of a repository than at its root, and `gix::open` succeeds
/// only at the root itself. That is not a licence to widen the scope: the
/// repository supplies file *names*, and `root` still decides which of them
/// count. Entries are clipped to `root` by a binary search over the index
/// (entries are sorted by path, so this is O(log n) rather than a scan of a
/// monorepo's worth of paths), and re-checked afterwards because the prefix
/// is an optimization and must not be the thing correctness rests on.
///
/// Nothing here stats the filesystem. An entry whose file is missing from the
/// worktree is deliberately returned: the capture will look for it, fail to
/// find it, and record a tombstone — which is the only evidence a later
/// restore has for deleting it again. Filtering those out here would silently
/// discard exactly that evidence.
pub fn git_tracked_files(root: &Path, ignore: &Gitignore) -> Vec<PathBuf> {
    let Ok(repo) = gix::discover(root) else {
        return Vec::new();
    };
    let Ok(index) = repo.index_or_empty() else {
        return Vec::new();
    };
    // A bare repository has nothing checked out, so it has nothing to snapshot.
    let Some(workdir) = repo.workdir() else {
        return Vec::new();
    };

    // Compared through `canonicalize` because gix may hand back a workdir in a
    // different spelling than the caller's root (`/var` vs `/private/var`, a
    // symlinked checkout), and a prefix test on the raw forms would then fail.
    // Results are still anchored on `root` rather than on the workdir: manifest
    // keys are absolute path strings, so two spellings of one file would be two
    // separate entries, captured twice and restored inconsistently.
    let root_real = root.canonicalize();
    let workdir_real = workdir.canonicalize();
    let relative = match (&root_real, &workdir_real) {
        (Ok(root_real), Ok(workdir_real)) => root_real.strip_prefix(workdir_real).ok(),
        _ => root.strip_prefix(workdir).ok(),
    };
    // Discovery found the repository by walking up from `root`, so the workdir
    // is always an ancestor. If it somehow is not, this session's scope is not
    // inside this repository and the partition has nothing to say.
    let Some(relative) = relative else {
        return Vec::new();
    };

    // The trailing separator is what stops `app` from also matching
    // `app-extra/…`. An empty prefix means `root` *is* the repository root.
    let mut prefix = relative.to_string_lossy().into_owned();
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }

    let entries = if prefix.is_empty() {
        index.entries()
    } else {
        match index.prefixed_entries(prefix.as_bytes().into()) {
            Some(entries) => entries,
            // The subdirectory holds nothing git tracks.
            None => return Vec::new(),
        }
    };

    entries
        .iter()
        .filter_map(|entry| {
            // Submodules are gitlinks: a commit id in the index and a
            // directory on disk. Recognising them from the index costs
            // nothing, where noticing on disk would cost a stat each.
            if entry.mode.is_submodule() {
                return None;
            }
            let rel = std::str::from_utf8(entry.path(&index)).ok()?;
            let path = root.join(rel.strip_prefix(prefix.as_str())?);
            (!is_ignored(ignore, &path)).then_some(path)
        })
        .collect()
}

/// The most recently modified files under `dir` that `covered` does not
/// already account for — the residue partition.
///
/// Bounded three ways — count, per-file size, and the churn directories
/// above — because this is the partition a large tree would otherwise flood.
///
/// `covered` is what the other partitions already hold. Excluding it *before*
/// ranking rather than after is the whole point: the limit is a budget, and a
/// budget spent on duplicates buys nothing. It also saves a stat per excluded
/// candidate, since the check happens before the metadata call.
pub fn recent_files(
    dir: &Path,
    ignore: &Gitignore,
    hidden: HiddenFiles,
    covered: &BTreeSet<PathBuf>,
    limits: ScanLimits,
) -> Recent {
    let walker = WalkBuilder::new(dir)
        .standard_filters(false)
        .hidden(hidden == HiddenFiles::Skip)
        .follow_links(false)
        .filter_entry(|entry| {
            let name = entry.file_name();
            name != ".git" && !RECENT_SKIP_DIRS.iter().any(|skip| name == *skip)
        })
        .build();

    let mut dropped: Vec<Drop> = Vec::new();
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            // A directory is not a candidate and never was. A symlink,
            // socket or device *is* something in the project the scan
            // declines to store, so it is named rather than passed over in
            // silence — the same bare `continue` D23 was written against,
            // one class along.
            if entry.file_type().is_some_and(|t| !t.is_dir()) {
                let path = entry.into_path();
                if !is_ignored(ignore, &path) {
                    dropped.push((path, DropReason::NotARegularFile));
                }
            }
            continue;
        }
        let path = entry.into_path();
        if covered.contains(&path) || is_ignored(ignore, &path) {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            dropped.push((path, DropReason::Unreadable));
            continue;
        };
        if meta.len() > limits.max_file_bytes {
            // The one IV.3 most wants named: a bound silently dropping a file
            // the user would expect back reads as data loss (C8).
            dropped.push((path, DropReason::OverSizeLimit));
            continue;
        }
        let Ok(modified) = meta.modified() else {
            dropped.push((path, DropReason::Unreadable));
            continue;
        };
        candidates.push((modified, path));
    }
    candidates.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    // Past the budget is not a drop worth naming: those files are ordinary
    // and simply lost a ranking. What IV.3 is about is a file excluded for a
    // reason the user cannot see.
    candidates.truncate(limits.max_files);
    Recent {
        files: candidates.into_iter().map(|(_, path)| path).collect(),
        dropped,
    }
}

/// What one root's recency pass picked up, and what it passed over.
#[derive(Debug, Default)]
pub struct Recent {
    pub files: Vec<PathBuf>,
    pub dropped: Vec<Drop>,
}

/// Everything in `roots` the scan would leave out of a snapshot, with reasons.
///
/// The diagnostic half of D23 — *what in my project is not protected?* — where
/// [`crate::CheckpointStats`] is the per-turn half. Two readers, two reports:
/// one is printed every turn and must be short, this one is asked for and must
/// be complete.
///
/// **Re-scans rather than reading anything a capture stored.** Nothing is
/// persisted, and the answer is about the project as it stands now rather than
/// as it stood at some past turn — which is the question someone asking it
/// actually has.
pub fn scan_report(roots: &[PathBuf], hidden: HiddenFiles, limits: ScanLimits) -> Vec<Drop> {
    let scan = tracked_files(roots, [], hidden, limits);
    let mut dropped = scan.dropped;

    // A readability probe the per-turn path deliberately does not pay for.
    // `recent_files` only ever *stats* a candidate, and the commonest way to
    // be unreadable — the file exists, `stat` succeeds, `read` is denied —
    // passes a stat cleanly. Without this the diagnostic could never name a
    // `.pem` or a root-owned file, which is most of what someone asking "what
    // is not protected" wants to hear about. This also covers the git-index
    // partition, which contributes no drops of its own.
    //
    // One `open` per tracked file is affordable here precisely because this
    // is asked for rather than run every turn.
    for path in &scan.files {
        if fs::File::open(path).is_err() {
            dropped.push((path.clone(), DropReason::Unreadable));
        }
    }

    dropped.sort();
    dropped.dedup();
    dropped
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pretty_assertions::assert_eq;

    fn touch(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
    }

    fn set_mtime_ago(path: &Path, secs: u64) {
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    /// A throwaway repository holding `paths`, all added to the index.
    ///
    /// Returns `None` when git is not on the machine, so the tests that need
    /// a real index skip instead of failing — the index format is what is
    /// under test here, and hand-rolling one would test our own fiction.
    fn git_fixture(paths: &[&str]) -> Option<(PathBuf, tempfile::TempDir)> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // Keep the developer's own git config out of the fixture by pointing
        // at a file that does not exist. `/dev/null` would do it on Unix and
        // fail on Windows, and these tests are expected to run everywhere.
        let absent_config = root.join("no-such-gitconfig");
        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", &absent_config)
                .env("GIT_CONFIG_SYSTEM", &absent_config)
                .output()
                .is_ok_and(|out| out.status.success())
        };
        if !git(&["init", "--quiet"]) {
            return None;
        }
        for rel in paths {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            touch(&path, "content");
        }
        git(&["add", "-A"]).then_some((root, dir))
    }

    #[test]
    fn walk_up_finds_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        let deep = root.join("a/b/c");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(root.join(".marker")).unwrap();

        let markers = vec![".marker".to_string()];
        assert_eq!(
            find_workspace_root(&deep, &markers),
            Some(root),
            "nearest ancestor with marker wins"
        );
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(find_workspace_root(elsewhere.path(), &markers), None);
    }

    #[test]
    fn the_recency_walk_respects_the_ignore_file_and_skips_git() {
        // These rules used to live on a subtree walk that production no
        // longer performs. They still matter, because the recency partition
        // walks the filesystem too and inherits every one of them.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("keep.txt"), "k");
        touch(&root.join("skip.log"), "s");
        fs::create_dir_all(root.join("sub")).unwrap();
        touch(&root.join("sub/also.log"), "s");
        touch(&root.join("sub/keep2.txt"), "k");
        fs::create_dir_all(root.join(".git")).unwrap();
        touch(&root.join(".git/HEAD"), "ref");
        touch(&root.join(SNAPSHOT_IGNORE_FILENAME), "*.log\n");

        let ignore = load_ignore(root);
        let names = |hidden: HiddenFiles| -> Vec<String> {
            let mut out: Vec<String> = recent_files(
                root,
                &ignore,
                hidden,
                &BTreeSet::new(),
                ScanLimits::default(),
            )
            .files
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
            out.sort();
            out
        };

        assert_eq!(
            names(HiddenFiles::Skip),
            vec!["keep.txt".to_string(), "sub/keep2.txt".to_string()],
            "logs ignored; .git and other dot-entries skipped — including the \
             ignore file itself, which follows the same rule as any other \
             hidden file and is tracked only if the agent edits it"
        );

        // Opting in reaches hidden entries, but never `.git`.
        let with_hidden = names(HiddenFiles::Track);
        assert!(with_hidden.contains(&SNAPSHOT_IGNORE_FILENAME.to_string()));
        assert!(
            with_hidden.iter().all(|name| !name.starts_with(".git/")),
            "repository internals are never scanned: {with_hidden:?}"
        );
    }

    #[test]
    fn recency_is_bounded_by_count_size_and_churn() {
        // The one partition a large tree can flood, so all three bounds are
        // load-bearing. Without the directory exclusions especially, build
        // output wins every recency contest and the partition never reaches a
        // source file.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..(ScanLimits::default().max_files + 20) {
            touch(&root.join(format!("src{i}.rs")), "fn main() {}");
        }
        std::fs::create_dir_all(root.join("target")).unwrap();
        touch(&root.join("target/artifact.o"), "built");
        touch(&root.join("huge.bin"), "x");
        std::fs::write(
            root.join("huge.bin"),
            vec![0u8; (ScanLimits::default().max_file_bytes + 1) as usize],
        )
        .unwrap();

        let ignore = load_ignore(root);
        let picked = recent_files(
            root,
            &ignore,
            HiddenFiles::Skip,
            &BTreeSet::new(),
            ScanLimits::default(),
        );

        assert_eq!(
            picked.files.len(),
            ScanLimits::default().max_files,
            "count is capped"
        );
        assert!(
            !picked.files.iter().any(|p| p.ends_with("artifact.o")),
            "churn directories are never descended into"
        );
        assert!(
            !picked.files.iter().any(|p| p.ends_with("huge.bin")),
            "oversized files are left out however recent"
        );

        // And the one left out for a reason is *named*. A bound that silently
        // drops a file the user expects back reads as data loss; a bound that
        // says which file is a bound (IV.3, D23).
        assert!(
            picked
                .dropped
                .iter()
                .any(|(p, why)| p.ends_with("huge.bin") && *why == DropReason::OverSizeLimit),
            "{:?}",
            picked.dropped
        );
        assert!(
            !picked
                .dropped
                .iter()
                .any(|(p, _)| p.ends_with("artifact.o")),
            "an ignored path is not a drop — nothing was left out against the user's wishes"
        );
    }

    /// The diagnostic half of D23: complete, asked for rather than printed,
    /// and about the project as it stands now.
    #[test]
    fn the_report_names_every_file_the_scan_would_leave_out() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("ordinary.rs"), "kept");
        std::fs::write(
            root.join("huge.bin"),
            vec![0u8; (ScanLimits::default().max_file_bytes + 1) as usize],
        )
        .unwrap();
        std::fs::write(
            root.join("also-huge.bin"),
            vec![0u8; (ScanLimits::default().max_file_bytes + 1) as usize],
        )
        .unwrap();

        let report = scan_report(
            &[root.to_path_buf()],
            HiddenFiles::Skip,
            ScanLimits::default(),
        );

        // Complete — both, not a sample. That is the whole difference from
        // what a capture reports per turn.
        let names: Vec<String> = report
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["also-huge.bin", "huge.bin"]);
        assert!(
            report
                .iter()
                .all(|(_, why)| *why == DropReason::OverSizeLimit)
        );
        assert!(
            !names.iter().any(|n| n == "ordinary.rs"),
            "a file that made it in is not a drop"
        );
    }

    /// All three reasons are reachable from the report.
    ///
    /// Two of them used not to be. `recent_files` only ever *stats* a
    /// candidate, so a mode-000 file passed cleanly, and a symlink was a bare
    /// `continue` — the same shape D23 was written against, one class along.
    /// The query promised "everything the scan would leave out" and could say
    /// only "too big", which is the least of the three that someone asking
    /// "what in my project is not protected" wants to hear about.
    #[cfg(unix)]
    #[test]
    fn the_report_can_name_all_three_reasons() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("fine.rs"), "kept");
        std::fs::write(
            root.join("huge.bin"),
            vec![0u8; (ScanLimits::default().max_file_bytes + 1) as usize],
        )
        .unwrap();
        touch(&root.join("secret.pem"), "key material");
        fs::set_permissions(root.join("secret.pem"), fs::Permissions::from_mode(0o000)).unwrap();
        std::os::unix::fs::symlink(root.join("fine.rs"), root.join("link.rs")).unwrap();

        let report = scan_report(
            &[root.to_path_buf()],
            HiddenFiles::Skip,
            ScanLimits::default(),
        );
        // Reopen before asserting, so a panic cannot leave the temp directory
        // undeletable.
        fs::set_permissions(root.join("secret.pem"), fs::Permissions::from_mode(0o644)).unwrap();

        let says = |name: &str, why: DropReason| {
            report.iter().any(|(p, w)| p.ends_with(name) && *w == why)
        };
        assert!(says("huge.bin", DropReason::OverSizeLimit), "{report:?}");
        assert!(says("secret.pem", DropReason::Unreadable), "{report:?}");
        assert!(says("link.rs", DropReason::NotARegularFile), "{report:?}");
        assert!(
            !report.iter().any(|(p, _)| p.ends_with("fine.rs")),
            "a file that made it in is not a drop"
        );
    }

    #[test]
    fn recency_spends_its_budget_only_on_what_is_not_covered() {
        // The budget is small and fixed, so a slot spent on a file the git
        // partition already contributed buys nothing. Before this exclusion,
        // 97 of the 100 slots on this repository went to already-covered
        // files — harmless until a bulk touch of tracked files pushes the
        // genuinely untracked ones off the end of the list.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..(ScanLimits::default().max_files + 5) {
            touch(&root.join(format!("known{i}.rs")), "covered");
        }
        // Older than everything above, so a plain recency ranking would never
        // reach it.
        touch(&root.join("stray.txt"), "the residue");
        set_mtime_ago(&root.join("stray.txt"), 3600);

        let ignore = load_ignore(root);
        let covered: BTreeSet<PathBuf> = (0..(ScanLimits::default().max_files + 5))
            .map(|i| root.join(format!("known{i}.rs")))
            .collect();
        let picked = recent_files(
            root,
            &ignore,
            HiddenFiles::Skip,
            &covered,
            ScanLimits::default(),
        );

        assert_eq!(
            picked.files,
            vec![root.join("stray.txt")],
            "the one uncovered file wins, however old"
        );
    }

    #[test]
    fn a_directory_without_a_repository_contributes_no_git_partition() {
        // Not an error, just an empty partition — the other two carry it.
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.txt"), "alpha");
        let ignore = load_ignore(dir.path());
        assert!(git_tracked_files(dir.path(), &ignore).is_empty());
    }

    #[test]
    fn the_git_partition_works_from_a_subdirectory_and_stays_inside_it() {
        // A session is opened in a subdirectory far more often than at the
        // repository root. `gix::open` succeeds only at the root, so this
        // partition used to return nothing at all for the common case —
        // silently, with the recency partition left to carry a whole
        // repository on a 100-file budget.
        let Some((repo, _guard)) = git_fixture(&[
            "app/main.rs",
            "app/deep/util.rs",
            "app-extra/other.rs",
            "top.rs",
        ]) else {
            return;
        };
        let sub = repo.join("app");
        let ignore = load_ignore(&sub);
        let mut found = git_tracked_files(&sub, &ignore);
        found.sort();

        assert_eq!(
            found,
            vec![repo.join("app/deep/util.rs"), repo.join("app/main.rs")],
            "everything under the session's root, and nothing above or beside \
             it — note `app-extra/` shares a prefix with `app` and must not be \
             swept in"
        );

        // From the repository root the same call sees the whole index.
        let ignore = load_ignore(&repo);
        assert_eq!(git_tracked_files(&repo, &ignore).len(), 4);
    }

    #[test]
    fn an_indexed_file_missing_from_disk_is_still_reported() {
        // So the capture can look, fail to find it, and write a tombstone —
        // the only evidence a later restore has for deleting it again.
        // Filtering it out here (which a `path.is_file()` check did) threw
        // that evidence away and left a rewind unable to re-delete a
        // tracked file that had been removed and then recreated.
        let Some((repo, _guard)) = git_fixture(&["kept.rs", "vanished.rs"]) else {
            return;
        };
        fs::remove_file(repo.join("vanished.rs")).unwrap();

        let ignore = load_ignore(&repo);
        let found = git_tracked_files(&repo, &ignore);
        assert!(
            found.contains(&repo.join("vanished.rs")),
            "a path git still tracks must reach the capture even when the \
             worktree no longer has it: {found:?}"
        );
    }

    #[test]
    fn ignored_paths_are_protected_symmetrically() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join(SNAPSHOT_IGNORE_FILENAME), "secret/**\n");
        let ignore = load_ignore(dir.path());
        assert!(is_ignored(&ignore, &dir.path().join("secret/key.pem")));
        assert!(!is_ignored(&ignore, &dir.path().join("src/main.rs")));
    }
}
