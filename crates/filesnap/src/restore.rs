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

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::blob::BlobStore;
use crate::error::Result;
use crate::error::SnapshotError;
use crate::manifest::Manifest;

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
/// `is_protected` is the symmetric-ignore predicate over manifest path keys.
/// Both directions honour it, so an ignored path is neither written nor
/// removed.
pub fn plan_restore(
    target: &Manifest,
    current: &Manifest,
    is_protected: &dyn Fn(&str) -> bool,
) -> RestorePlan {
    let mut plan = RestorePlan::default();

    for (path, entry) in &target.entries {
        if is_protected(path) {
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
        if !is_protected(path) && target.absent.contains(path) {
            plan.deletes.push(path.clone());
        }
    }
    plan
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyStats {
    pub written: usize,
    pub deleted: usize,
}

/// Apply a plan: write blob contents (atomically, restoring permissions)
/// and remove paths the target recorded as absent. Missing targets are fine.
pub fn apply_plan(blobs: &BlobStore, plan: &RestorePlan) -> Result<ApplyStats> {
    let mut stats = ApplyStats::default();
    for write in &plan.writes {
        let path = PathBuf::from(&write.path);
        let content = blobs.load(&write.hash)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| SnapshotError::io(parent, e))?;
        }
        let tmp = tmp_path(&path);
        fs::write(&tmp, &content).map_err(|e| SnapshotError::io(&tmp, e))?;
        // A write is a replace, so "leave the permissions alone" has to mean
        // carrying the existing ones onto the replacement. Doing nothing
        // would hand the file whatever the temporary got from the umask,
        // which is a change made by omission — the same executable bit lost
        // by a different route.
        set_mode(&tmp, write.mode.or_else(|| current_mode(&path)))?;
        fs::rename(&tmp, &path).map_err(|e| SnapshotError::io(&path, e))?;
        stats.written += 1;
    }
    for del in &plan.deletes {
        let path = PathBuf::from(del);
        match fs::remove_file(&path) {
            Ok(()) => stats.deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(SnapshotError::io(&path, e)),
        }
    }
    Ok(stats)
}

/// Sibling name a restore writes to before renaming into place. It lands in
/// the user's own directory, so it is named after this tool rather than
/// after whatever host embeds it.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".filesnap-restore-tmp");
    path.with_file_name(name)
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

        let plan = plan_restore(&target, &current, &|_| false);
        let paths: Vec<&str> = plan.writes.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, vec!["/a", "/b"]);
    }

    #[test]
    fn identical_states_need_no_work() {
        let m = manifest(&[("/a", "h")]);
        assert_eq!(plan_restore(&m, &m, &|_| false), RestorePlan::default());
    }

    #[test]
    fn deletion_needs_the_target_to_have_looked() {
        // The single rule. Earlier designs inferred absence from a scan having
        // been exhaustive, which made every deletion depend on a premise that
        // cost a full tree walk — and still did not hold for paths the edit
        // hook contributed from outside any scan.
        let target = manifest(&[("/kept", "h-k")]);
        let current = manifest(&[("/kept", "h-k"), ("/added", "h-a")]);

        let plan = plan_restore(&target, &current, &|_| false);
        assert!(
            plan.deletes.is_empty(),
            "missing from the target says nothing on its own"
        );

        // Recorded as looked-for-and-absent, it says everything.
        let mut target = target;
        target.absent.insert("/added".to_string());
        let plan = plan_restore(&target, &current, &|_| false);
        assert_eq!(plan.deletes, vec!["/added"]);
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn protected_paths_are_untouched_in_both_directions() {
        let target = manifest(&[("/secret/a", "h-1"), ("/ok", "h-ok")]);
        let current = manifest(&[("/secret/b", "h-2")]);

        let protect = |p: &str| p.starts_with("/secret/");
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

        let first = plan_restore(&target, &current, &|_| false);
        let after_undo = plan_restore(&target, &current, &|_| false);
        assert_eq!(first, after_undo);
    }
}
