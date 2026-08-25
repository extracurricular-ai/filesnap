//! Restore planning and application.
//!
//! Restores are planned as a diff between the target manifest and the
//! **safety checkpoint** — a capture of the current state taken
//! immediately before any restore, which makes every restore reversible
//! (redo = restore the safety manifest).
//!
//! Deletion requires positive evidence of absence, and the only thing that
//! counts is the target having looked: a path it was asked about and did not
//! find. Nothing is inferred from a path merely being missing. Paths matched
//! by the protection predicate (the symmetric ignore rule) are untouched in
//! both directions.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use crate::blob::BlobStore;
use crate::error::Result;
use crate::error::SnapshotError;
use crate::manifest::Manifest;
use ignore::gitignore::Gitignore;

/// The symmetric-ignore test over a manifest's path keys.
pub(crate) fn is_protected(rules: &Gitignore, path: &str) -> bool {
    crate::scope::is_ignored(rules, Path::new(path))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteAction {
    pub path: String,
    pub hash: String,
    /// Permissions to set, or `None` to leave whatever is there.
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestorePlan {
    pub writes: Vec<WriteAction>,
    pub deletes: Vec<String>,
}

/// Plan the move from `current` to `target`.
///
/// Deletion needs positive evidence that a file did not exist at the target,
/// which only `target.absent` supplies: the capture looked for that path and
/// found nothing. Leaving an extra file behind is recoverable; deleting one
/// the system never observed is not.
///
/// `rules` are the ignore rules themselves, applied symmetrically: an ignored
/// path is neither written nor removed. They are data rather than a closure
/// on purpose (D12) — the protection can then be logged and shown ("these
/// four paths were protected and not written"), and it is the same concept as
/// the ignore file rather than a second one. A caller wanting a temporary
/// rule appends it in memory with `GitignoreBuilder::add_line`; the user's
/// `.filesnapignore` is theirs and is never written to.
pub fn plan_restore(target: &Manifest, current: &Manifest, rules: &Gitignore) -> RestorePlan {
    let mut plan = RestorePlan::default();

    for (path, entry) in &target.entries {
        if is_protected(rules, path) {
            continue;
        }
        // Mode counts only when both sides observed one. An entry whose
        // permissions were never seen has no opinion about them, and treating
        // its `None` as a difference would rewrite files whose content
        // already matches and then set them to permissions nobody recorded.
        let differs = current.entries.get(path).is_none_or(|cur| {
            cur.hash != entry.hash || matches!((cur.mode, entry.mode), (Some(a), Some(b)) if a != b)
        });
        if differs {
            plan.writes.push(WriteAction {
                path: path.clone(),
                hash: entry.hash.clone(),
                mode: entry.mode,
            });
        }
    }

    // One ground for deleting: the target looked for this path and did not
    // find it. Nothing is inferred from a path simply being missing from the
    // target — a capture only ever sees what it was asked about, so absence
    // from `entries` alone says nothing about whether the file existed.
    for path in current.entries.keys() {
        if !is_protected(rules, path) && target.absent.contains(path) {
            plan.deletes.push(path.clone());
        }
    }
    plan
}

/// What an apply managed, and what it could not.
///
/// Not `Clone`/`PartialEq`: `SnapshotError` carries a `std::io::Error`, which
/// is neither. Compare the counts and `failed.is_empty()` instead.
#[derive(Debug, Default)]
pub struct ApplyStats {
    pub written: usize,
    pub deleted: usize,
    /// Each file fails on its own without stopping the rest. Empty on a
    /// clean apply.
    ///
    /// **Collected is not shrugged off.** A restore with a non-empty `failed`
    /// must not read as success anywhere — not in an exit code, not in
    /// output. The peer review of competing implementations flags exactly
    /// this failure: per-file errors gathered into a struct nobody prints.
    pub failed: Vec<(PathBuf, SnapshotError)>,
}

/// Apply a plan: write blob contents (atomically, restoring permissions)
/// and remove paths the target recorded as absent. Missing targets are fine.
///
/// **Each file succeeds or fails on its own.** Propagating the first error
/// stranded the other 499 and handed the caller a bare `Io` with no record of
/// how far it got — and, because `RestoreOutcome` was built only on success,
/// no way to reach the safety point. That is III.1's reversibility existing
/// and being out of reach exactly when it is needed (C20).
///
/// **Nothing is rolled back automatically.** A rollback can itself fail,
/// producing a third state the caller cannot observe. III.1 promises
/// *reversible*, not *reversed for you*: the caller holds the safety target
/// and decides.
pub fn apply_plan(blobs: &BlobStore, plan: &RestorePlan) -> ApplyStats {
    let mut stats = ApplyStats::default();
    sweep_residue(plan);
    for write in &plan.writes {
        let path = PathBuf::from(&write.path);
        match write_one(blobs, write, &path) {
            Ok(()) => stats.written += 1,
            Err(err) => stats.failed.push((path, err)),
        }
    }
    for del in &plan.deletes {
        let path = PathBuf::from(del);
        match fs::remove_file(&path) {
            Ok(()) => stats.deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => stats
                .failed
                .push((path.clone(), SnapshotError::io(&path, e))),
        }
    }
    stats
}

/// One file, all of it or none of it.
fn write_one(blobs: &BlobStore, write: &WriteAction, path: &Path) -> Result<()> {
    let content = blobs.load(&write.hash)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| SnapshotError::io(parent, e))?;
    }
    let tmp = tmp_path(path);
    fs::write(&tmp, &content).map_err(|e| SnapshotError::io(&tmp, e))?;
    // A write is a replace, so "leave the permissions alone" has to mean
    // carrying the existing ones onto the replacement. Doing nothing would
    // hand the file whatever the temporary got from the umask, which is a
    // change made by omission — the same executable bit lost by a different
    // route.
    set_mode(&tmp, write.mode.or_else(|| current_mode(path)))?;
    fs::rename(&tmp, path).map_err(|e| SnapshotError::io(path, e))
}

/// Suffix of the sibling a restore writes before renaming into place.
///
/// It lands in the user's own directory, so it is named after this tool
/// rather than after whatever host embeds it — someone who finds one in their
/// project should be able to tell where it came from.
pub const RESTORE_TMP_SUFFIX: &str = ".filesnap-restore-tmp";

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(RESTORE_TMP_SUFFIX);
    path.with_file_name(name)
}

/// How long a stray temp file is left alone before a restore clears it.
///
/// Required, not caution: another restore may be holding one right now, and
/// unlinking it mid-write would make that restore fail for no reason.
const RESIDUE_GRACE: Duration = Duration::from_secs(300);

/// Clear this restore's own leavings from the directories it is about to
/// touch, before it touches them.
///
/// A write is temp-file-then-rename, so a process killed between the two
/// leaves a stray in the *user's project* — somewhere store collection can
/// never reach, because it knows the store and not the workspace. Sweeping
/// here is the self-healing half of D21; `doctor` (via [`residue_in`]) is the
/// half that reaches a workspace nothing restores into again.
///
/// Failures are ignored throughout. Residue is a tidiness matter and must
/// never be the reason a restore does not happen.
fn sweep_residue(plan: &RestorePlan) {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for write in &plan.writes {
        let Some(parent) = Path::new(&write.path).parent() else {
            continue;
        };
        if !seen.insert(parent.to_path_buf()) {
            continue;
        }
        for stray in residue_in(parent) {
            let _ = fs::remove_file(stray);
        }
    }
}

/// Stray restore temporaries in `dir` that are old enough to be nobody's.
///
/// The inspectable half of D21: a caller walks a workspace with this and
/// reports, or removes, what it finds.
pub fn residue_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_settled_residue(path))
        .collect()
}

/// What `path` is set to right now, if it exists and the platform says.
fn current_mode(path: &Path) -> Option<u32> {
    crate::manifest::mode_of(&fs::metadata(path).ok()?)
}

/// Apply permissions. `None` means there are none to apply — neither the
/// record nor the file on disk offered any — so whatever the newly written
/// file has is what it keeps.
fn set_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    let Some(mode) = mode else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|e| SnapshotError::io(path, e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Every stray restore temporary under `root`, wherever a restore reached.
///
/// The half of D21 that self-healing cannot cover. A restore clears the
/// directories it is about to write; a workspace nothing restores into again
/// keeps its stray forever, and the person who finds a
/// `.filesnap-restore-tmp` in their project is more likely to delete it or
/// file a bug than to know what it is.
///
/// Skips the same directories a scan does, and `.git`. Residue only appears
/// where a restore wrote, and a restore only writes what a capture recorded,
/// so a build directory cannot hold any — walking it would be cost for
/// nothing.
pub fn residue_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .hidden(false)
        .follow_links(false)
        .filter_entry(|entry| {
            let name = entry.file_name();
            name != ".git"
                && !crate::scope::RECENT_SKIP_DIRS
                    .iter()
                    .any(|skip| name == *skip)
        })
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if is_settled_residue(&path) {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Whether one path is a stray restore temporary old enough to be nobody's.
fn is_settled_residue(path: &Path) -> bool {
    let cutoff = SystemTime::now()
        .checked_sub(RESIDUE_GRACE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(RESTORE_TMP_SUFFIX))
        && fs::metadata(path)
            .and_then(|meta| meta.modified())
            .is_ok_and(|written| written <= cutoff)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::manifest::FileEntry;
    use pretty_assertions::assert_eq;

    fn manifest(entries: &[(&str, &str)]) -> Manifest {
        let mut m = Manifest::default();
        for (path, hash) in entries {
            m.entries.insert(
                (*path).to_string(),
                FileEntry {
                    mode: Some(0o644),
                    size: hash.len() as u64,
                    mtime_secs: 0,
                    mtime_nanos: 0,
                    hash: (*hash).to_string(),
                },
            );
        }
        m
    }

    #[test]
    fn writes_files_that_differ_or_are_missing() {
        let target = manifest(&[("/a", "h-old"), ("/b", "h-b")]);
        let current = manifest(&[("/a", "h-new")]);

        let plan = plan_restore(&target, &current, &Gitignore::empty());
        let paths: Vec<&str> = plan.writes.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, vec!["/a", "/b"]);
    }

    #[test]
    fn identical_states_need_no_work() {
        let m = manifest(&[("/a", "h")]);
        assert_eq!(
            plan_restore(&m, &m, &Gitignore::empty()),
            RestorePlan::default()
        );
    }

    #[test]
    fn deletion_needs_the_target_to_have_looked() {
        // The single rule. Earlier designs inferred absence from a scan having
        // been exhaustive, which made every deletion depend on a premise that
        // cost a full tree walk — and still did not hold for paths the edit
        // hook contributed from outside any scan.
        let target = manifest(&[("/kept", "h-k")]);
        let current = manifest(&[("/kept", "h-k"), ("/added", "h-a")]);

        let plan = plan_restore(&target, &current, &Gitignore::empty());
        assert!(
            plan.deletes.is_empty(),
            "missing from the target says nothing on its own"
        );

        // Recorded as looked-for-and-absent, it says everything.
        let mut target = target;
        target.absent.insert("/added".to_string());
        let plan = plan_restore(&target, &current, &Gitignore::empty());
        assert_eq!(plan.deletes, vec!["/added"]);
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn protected_paths_are_untouched_in_both_directions() {
        let target = manifest(&[("/secret/a", "h-1"), ("/ok", "h-ok")]);
        let current = manifest(&[("/secret/b", "h-2")]);

        // Built in memory, which is the point of D12: a temporary rule never
        // touches the user's own `.filesnapignore`.
        let mut builder = crate::scope::GitignoreBuilder::new("/");
        builder.add_line(None, "/secret/**").unwrap();
        let protect = builder.build().unwrap();
        let plan = plan_restore(&target, &current, &protect);
        let write_paths: Vec<&str> = plan.writes.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(
            write_paths,
            vec!["/ok"],
            "protected target entry not restored"
        );
        assert!(
            plan.deletes.is_empty(),
            "protected current entry not deleted"
        );
    }

    #[test]
    fn restoring_is_idempotent_for_a_given_target() {
        // The same target must plan the same way no matter what happened in
        // between — the property that broke when targets were located by
        // content and a redo made an older state recur.
        let target = manifest(&[("/a", "h-old")]);
        let current = manifest(&[("/a", "h-new")]);

        let first = plan_restore(&target, &current, &Gitignore::empty());
        let after_undo = plan_restore(&target, &current, &Gitignore::empty());
        assert_eq!(first, after_undo);
    }
}
