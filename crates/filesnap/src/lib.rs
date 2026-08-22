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
//! The rules this crate is built to keep, and the places it does not yet
//! keep them, are in `.specify/memory/constitution.md` and
//! `.specify/memory/compliance.md`.

mod blob;
mod checkpoint;
mod controller;
mod error;
mod manifest;
mod refs;
mod restore;
mod scope;
mod store;

pub use blob::BlobStore;
pub use checkpoint::Checkpoint;
pub use checkpoint::CheckpointStats;
pub use checkpoint::capture;
pub use controller::SessionStart;
pub use controller::SnapshotTracker;
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
pub use restore::RestorePlan;
pub use restore::WriteAction;
pub use restore::apply_plan;
pub use restore::plan_restore;
pub use scope::HiddenFiles;
pub use scope::SNAPSHOT_IGNORE_FILENAME;
pub use scope::find_workspace_root;
pub use scope::git_tracked_files;
pub use scope::is_ignored;
pub use scope::load_ignore;
pub use scope::recent_files;
pub use scope::tracked_files;
pub use store::PreEditImage;
pub use store::RestoreKind;
pub use store::RestoreOutcome;
pub use store::RestoreTarget;
pub use store::SAFETY_TURN_PREFIX;
pub use store::STORE_DIR_NAME;
pub use store::SnapshotStore;
pub use store::forget_sessions;
