//! Git-free file snapshots and rewind.
//!
//! Content-addressed blobs, stat-cached manifests, per-session snapshot logs
//! with mark-and-sweep collection, and a restore planner that captures a
//! rescue point before it writes and deletes only against a tombstone.
//!
//! The engine knows nothing about its host: it takes opaque string ids and
//! absolute paths, and never reads or writes the user's git state. Git
//! appears only as one source of file *names*, and a directory that has
//! never seen `git init` is a first-class workspace.
//!
//! # What is covered, and what is not
//!
//! A file enters the tracked set three ways: the project's git index lists
//! it, a recency walk finds it, or an edit declares it. Only the third is
//! unbounded, and the first two have shapes worth knowing:
//!
//! - the index cannot see an untracked file;
//! - the recency walk skips hidden directories and a fixed list of build
//!   directories, drops files over [`ScanLimits::max_file_bytes`], and keeps
//!   at most [`ScanLimits::max_files`] paths per root.
//!
//! So a file created by a shell command inside `target/`, inside a dotted
//! directory, over the size limit, or beyond the recency budget on a busy
//! turn is covered **only** if it also flowed through the host's edit API.
//! This is a real gap and it is stated rather than implied: total coverage is
//! not promised, and [`scan_report`] answers "what in my project is not
//! protected" for any particular workspace.
//!
//! The rules this crate is built to keep, and the places it does not yet
//! keep them, are in `.specify/memory/constitution.md` and
//! `.specify/memory/compliance.md`.

mod blob;
mod checkpoint;
mod collect;
mod controller;
mod declared;
mod error;
#[cfg(any(test, feature = "test-support"))]
pub mod fixture;
mod id;
mod lock;
mod manifest;
mod refs;
mod restore;
mod scope;
mod store;
mod sweep;
mod turn;
mod workspace;

pub use blob::BlobStore;
pub use checkpoint::Checkpoint;
pub use checkpoint::CheckpointStats;
pub use checkpoint::DROP_SAMPLE_LIMIT;
pub use checkpoint::capture;
pub use collect::collect_garbage;
pub use collect::content_disk_usage;
pub use controller::SessionStart;
pub use controller::SnapshotTracker;
pub use declared::DECLARED_WINDOW_TURNS;
pub use error::Result;
pub use error::SnapshotError;
pub use manifest::FileEntry;
pub use manifest::Manifest;
pub use manifest::ManifestStore;
pub use manifest::mode_of;
pub use manifest::mtime_parts;
pub use refs::GcStats;
pub use refs::RefStore;
pub use refs::SnapshotRef;
pub use refs::ThreadLog;
pub use restore::ApplyStats;
pub use restore::RESTORE_TMP_SUFFIX;
pub use restore::RestorePlan;
pub use restore::WriteAction;
pub use restore::apply_plan;
pub use restore::plan_restore;
pub use restore::residue_in;
pub use scope::Drop;
pub use scope::DropReason;
pub use scope::HiddenFiles;
pub use scope::Recent;
pub use scope::SNAPSHOT_IGNORE_FILENAME;
pub use scope::Scan;
pub use scope::ScanLimits;
pub use scope::find_workspace_root;
pub use scope::git_tracked_files;
pub use scope::is_ignored;
pub use scope::scan_report;
pub use turn::DeclareOutcome;
pub use turn::TurnScope;
pub use turn::capture_turn;
pub use turn::declare_edits;
// D13: consumers name `Gitignore` in our signatures, so they get the type
// from us rather than adding their own dependency on `ignore`.
pub use ignore::gitignore::Gitignore;
pub use ignore::gitignore::GitignoreBuilder;
pub use scope::load_ignore;
pub use scope::recent_files;
pub use scope::tracked_files;
pub use store::DeleteOutcome;
pub use store::PreEditImage;
pub use store::RestoreKind;
pub use store::RestoreOutcome;
pub use store::RestoreTarget;
pub use store::SAFETY_TURN_PREFIX;
pub use store::WorkspaceStore;
pub use workspace::FORMAT_VERSION;
pub use workspace::WorkspaceKey;
