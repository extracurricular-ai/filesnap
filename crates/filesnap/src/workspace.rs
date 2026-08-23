//! Store layout: where a workspace's records live, and which format version
//! wrote them.
//!
//! Two liveness questions have two different scopes, and the layout follows
//! them rather than the other way round. Whether a *manifest* is still
//! referenced is answerable inside one workspace — nothing outside it can name
//! a manifest in its partition. Whether a *blob* is still referenced is global,
//! because content is deduplicated across every workspace. So records
//! partition and content does not:
//!
//! ```text
//! <data_dir>/filesnap/v1/
//!   ├── blobs/                    global; deduplicated across everything
//!   └── workspaces/<key>/
//!         ├── manifests/
//!         ├── refs/
//!         ├── turns/
//!         └── restores/
//! ```
//!
//! The version lives in the path because a reader has to be able to refuse a
//! format it does not understand, and it cannot refuse what it was handed: a
//! caller who composes the path itself can spell any version at all. So the
//! caller passes a data directory and the layout is computed here.

use std::path::Path;
use std::path::PathBuf;

use sha2::Digest;
use sha2::Sha256;

use crate::error::Result;
use crate::error::SnapshotError;

/// Directory the store occupies inside a host's data directory.
const STORE_DIR: &str = "filesnap";

/// The format version this build reads and writes.
///
/// A store is a directory named for its version, so a build that meets a
/// version it does not know simply does not open it — there is nothing to
/// misread. Bumping this is a deliberate, migrating act.
pub const FORMAT_VERSION: u32 = 1;

/// Identifies one workspace's partition.
///
/// Derived from the **canonical** absolute path, so that two spellings of one
/// directory are one partition and not two. On macOS `/var` is a symlink to
/// `/private/var`, which makes this the ordinary case rather than an exotic
/// one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceKey(String);

impl WorkspaceKey {
    /// Derive the key for `workspace`, which must exist — canonicalizing
    /// requires it.
    ///
    /// There is deliberately no fallback to the literal path when
    /// canonicalization fails. A fallback would hand two spellings of one
    /// directory two partitions, each with its own history, and neither
    /// would be wrong enough to notice.
    pub fn of(workspace: &Path) -> Result<Self> {
        let canonical = workspace
            .canonicalize()
            .map_err(|e| SnapshotError::io(workspace, e))?;
        Ok(Self::of_canonical(&canonical))
    }

    fn of_canonical(canonical: &Path) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        Self(format!("{:x}", hasher.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The versioned store root under `data_dir`, created if absent.
///
/// Refuses to proceed when the store directory holds only versions this build
/// does not know: that is a store written by a newer filesnap, and opening it
/// would mean guessing at a format (VII.1). A directory holding both this
/// version and a newer one is fine — this build uses its own.
pub fn store_root(data_dir: &Path) -> Result<PathBuf> {
    let store = data_dir.join(STORE_DIR);
    let root = store.join(format!("v{FORMAT_VERSION}"));
    if root.is_dir() {
        return Ok(root);
    }
    if let Some(newer) = newer_version_present(&store)? {
        return Err(SnapshotError::UnknownStoreVersion {
            path: store,
            found: newer,
            supported: FORMAT_VERSION,
        });
    }
    std::fs::create_dir_all(&root).map_err(|e| SnapshotError::io(&root, e))?;
    Ok(root)
}

/// The highest `v<n>` directory greater than what this build supports, if the
/// store holds one. `None` when the store is absent or empty, which is the
/// ordinary "nothing written yet" case rather than an error.
fn newer_version_present(store: &Path) -> Result<Option<u32>> {
    let entries = match std::fs::read_dir(store) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SnapshotError::io(store, e)),
    };
    let mut newest = None;
    for entry in entries {
        let entry = entry.map_err(|e| SnapshotError::io(store, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(version) = name.strip_prefix('v').and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if version > FORMAT_VERSION {
            newest = Some(newest.map_or(version, |current: u32| current.max(version)));
        }
    }
    Ok(newest)
}

/// Where one workspace's records live under a versioned store root.
pub fn partition_dir(root: &Path, key: &WorkspaceKey) -> PathBuf {
    root.join("workspaces").join(key.as_str())
}

/// Where content lives — one directory for every workspace, because blobs are
/// content-addressed and shared.
pub fn blobs_dir(root: &Path) -> PathBuf {
    root.join("blobs")
}

/// Every workspace partition that exists, for operations that span the store.
pub fn all_partitions(root: &Path) -> Result<Vec<WorkspaceKey>> {
    let workspaces = root.join("workspaces");
    let entries = match std::fs::read_dir(&workspaces) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(SnapshotError::io(&workspaces, e)),
    };
    let mut keys = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| SnapshotError::io(&workspaces, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // A partition key is a hex digest and nothing else. Anything that is
        // not one is residue, and residue is never a record (D9).
        if name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit()) {
            keys.push(WorkspaceKey(name));
        }
    }
    keys.sort();
    Ok(keys)
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
