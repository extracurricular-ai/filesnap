//! Test fixtures: a realistic store in one call.
//!
//! Available to this crate's own tests and, behind the `test-support`
//! feature, to its integration tests — **one shared place**, because
//! coverage is thin wherever setup is expensive, and that is a property of
//! the fixtures rather than of anyone's discipline. Before this existed,
//! building a workspace with a session and a few turns cost thirty lines
//! every time, and the two largest modules had almost no tests.
//!
//! Nothing here reaches around the public API. A fixture that knew the
//! layout would keep passing when the layout broke.

// This is test code that is not `#[cfg(test)]` — integration tests reach it
// through the `test-support` feature — so clippy's `allow-expect-in-tests`
// does not apply to it. Panicking is right here for the same reason it is
// right in a test body: a fixture that cannot build its own preconditions
// has nothing useful to hand back.
#![allow(clippy::expect_used)]

use std::path::Path;
use std::path::PathBuf;

use tempfile::TempDir;

use crate::PreEditImage;
use crate::WorkspaceStore;
use crate::scope::Gitignore;
use crate::scope::HiddenFiles;
use crate::scope::is_ignored;
use crate::scope::load_ignore;

/// A host data directory and a workspace, both real on disk.
pub struct Fixture {
    data: TempDir,
    workspace: TempDir,
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            data: TempDir::new().expect("temp data dir"),
            workspace: TempDir::new().expect("temp workspace"),
        }
    }

    pub fn data_dir(&self) -> &Path {
        self.data.path()
    }

    pub fn workspace(&self) -> &Path {
        self.workspace.path()
    }

    /// A store open on this fixture's workspace.
    ///
    /// Returns a fresh handle each time on purpose: two handles on one
    /// partition is the ordinary case for a CLI, and a test that assumes
    /// otherwise is testing something the product does not promise.
    pub fn store(&self) -> WorkspaceStore {
        WorkspaceStore::open(self.data_dir(), self.workspace()).expect("open store")
    }

    /// Write `content` at `rel` inside the workspace, creating parents.
    pub fn write(&self, rel: &str, content: impl AsRef<[u8]>) -> PathBuf {
        let path = self.workspace().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, content).expect("write file");
        path
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.workspace().join(rel)).expect("read file")
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.workspace().join(rel).exists()
    }

    pub fn remove(&self, rel: &str) {
        std::fs::remove_file(self.workspace().join(rel)).expect("remove file");
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.workspace().join(rel)
    }

    /// Every regular file in the workspace that the ignore rules admit,
    /// skipping dot-entries.
    ///
    /// Spelled out here rather than exposed by the library: the engine
    /// deliberately offers no subtree walk, because bounding tracking by the
    /// project rather than by the tree is the whole point of the partitions.
    /// A test that wants one says so.
    pub fn all_files(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, ignore: &ignore::gitignore::Gitignore, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let path = entry.path();
                if is_ignored(ignore, &path) {
                    continue;
                }
                if path.is_dir() {
                    walk(&path, ignore, out);
                } else if path.is_file() {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(self.workspace(), &load_ignore(self.workspace()), &mut out);
        out.sort();
        out
    }

    /// What a restore should be asked to look at: the workspace as it stands,
    /// plus every path this session has ever observed.
    ///
    /// The second half is not optional. A file the agent deleted is on no
    /// walk of the directory, so without it the safety capture cannot record
    /// that it was gone, and a redo could never take it away again.
    pub fn restore_scope(&self, session: &str) -> Vec<PathBuf> {
        let store = self.store();
        let mut files = self.all_files();
        files.extend(
            store
                .tracked_paths(session)
                .expect("tracked paths")
                .into_iter()
                .map(PathBuf::from),
        );
        files
    }

    /// This workspace's ignore rules, read fresh so the *current* file
    /// governs — newly ignoring a path protects it retroactively (II.3).
    pub fn protection(&self) -> Gitignore {
        load_ignore(self.workspace())
    }

    /// Capture a turn over everything currently in the workspace.
    pub fn capture(&self, session: &str, turn: &str) -> crate::Checkpoint {
        self.store()
            .checkpoint(session, turn, self.all_files())
            .expect("checkpoint")
    }

    /// Record that `rel` is about to be edited, reading its pre-image the way
    /// the real edit hook does.
    pub fn declare(&self, session: &str, turn: &str, rel: &str) {
        let path = self.path(rel);
        let image = match std::fs::read(&path) {
            Ok(bytes) => PreEditImage::Existed(bytes),
            Err(_) => PreEditImage::DidNotExist,
        };
        self.store()
            .attach_pre_edit(session, turn, &path.to_string_lossy(), &image)
            .expect("attach pre-edit");
    }

    /// Backdate this fixture's store past the collection grace window.
    pub fn age_store(&self) {
        age_store(self.data_dir());
    }

    /// The tracked set the engine would compute for this workspace.
    pub fn tracked_set(&self, already_known: Vec<PathBuf>) -> Vec<PathBuf> {
        crate::scope::tracked_files(
            &[self.workspace().to_path_buf()],
            already_known,
            HiddenFiles::Skip,
            crate::ScanLimits::default(),
        )
        .into_iter()
        .collect()
    }
}

/// Backdate everything under `data_dir` past the collection grace window.
///
/// Collection spares anything written recently, because a capture publishes
/// its content before the manifest naming it, and a sweep must not take away
/// a snapshot its writer believes it holds. In a test everything was written
/// seconds ago — so proving that collection *reclaims* means ageing the store
/// rather than waiting five minutes for it.
pub fn age_store(data_dir: &Path) {
    fn walk(dir: &Path, when: std::time::SystemTime) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, when);
            } else if let Ok(file) = std::fs::File::options().write(true).open(&path) {
                let _ = file.set_times(std::fs::FileTimes::new().set_modified(when));
            }
        }
    }
    walk(
        data_dir,
        std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
    );
}

/// How many content objects the store holds, across every workspace.
///
/// Here rather than in a test because it needs to know where content lives,
/// and a test that knew the layout would keep passing when the layout broke.
pub fn blob_count(data_dir: &Path) -> usize {
    fn walk(dir: &Path, seen: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, seen);
            } else {
                *seen += 1;
            }
        }
    }
    let Ok(root) = crate::workspace::store_root(data_dir) else {
        return 0;
    };
    let mut seen = 0;
    walk(&crate::workspace::blobs_dir(&root), &mut seen);
    seen
}

/// Rules that protect nothing — the ordinary case, spelled once.
pub fn no_rules() -> Gitignore {
    Gitignore::empty()
}

/// Rules built **in memory** from one pattern, the way a caller adds a
/// temporary protection without touching the user's `.filesnapignore` (D12).
pub fn rules_for(root: &Path, pattern: &str) -> Gitignore {
    let mut builder = crate::scope::GitignoreBuilder::new(root);
    builder
        .add_line(None, pattern)
        .expect("valid ignore pattern");
    builder.build().expect("build ignore rules")
}
