//! Session-side tracker: the policy layer gluing scope resolution,
//! capture, and pre-edit attach for one session.
//!
//! Wiring rules:
//! - **Session-scoped binding**: whether a session tracks is decided at
//!   session start — a new session follows the host's own setting; a
//!   resumed one follows the persisted marker (snapshot-log existence).
//!   The state never changes mid-session.
//! - **Scope**: the union of three partitions (see `scope`) — what the
//!   project's index lists, what the agent has edited this session, and what
//!   changed most recently. Rooted at the session's own workspace roots,
//!   falling back to the turn cwd when none of them relate to it.
//! - Capture failures degrade to "no snapshot for this turn" — they are
//!   logged and never fail the turn.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use crate::error::Result;
use crate::scope::HiddenFiles;
use crate::scope::is_ignored;
use crate::scope::load_ignore;
use crate::scope::tracked_files;
use crate::store::PreEditImage;
use crate::store::STORE_DIR_NAME;
use crate::store::SnapshotStore;
use tracing::info;
use tracing::warn;

/// How this session's tracking state is decided. Fixed at session start;
/// it never changes while the session runs, so a session's snapshot chain
/// is always complete-from-the-first-turn or absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStart {
    /// A session starting fresh. The host's own setting alone decides
    /// whether it tracks, and nothing is written until the first capture.
    New { tracking_enabled: bool },
    /// A session being resumed. The persisted log decides, whatever the
    /// host's setting now says: one that captured nothing has nothing to
    /// resume tracking *from*, and one that did must not stop mid-chain.
    Resumed,
}

pub struct SnapshotTracker {
    store: SnapshotStore,
    session_id: String,
    hidden: HiddenFiles,
    state: Mutex<TrackState>,
}

#[derive(Default)]
struct TrackState {
    /// Agent-edited paths registered via pre-edit attach; unioned into
    /// every checkpoint scan so post-edit states keep being observed.
    /// In-memory for v1: lost on resume (recorded manifests stay valid).
    extras: BTreeSet<PathBuf>,
    /// Directory whose ignore rules scope this session's captures: the
    /// workspace root when one was found, else the invocation directory.
    /// Recorded by the turn-start checkpoint, which always precedes tool
    /// execution within a turn.
    ignore_root: Option<PathBuf>,
}

impl SnapshotTracker {
    /// Build a tracker if this session should track (see module doc), with
    /// the store living in `STORE_DIR_NAME` under `data_dir`.
    pub fn maybe_new(
        data_dir: &Path,
        session_id: String,
        start: SessionStart,
        hidden: HiddenFiles,
    ) -> Option<Arc<Self>> {
        let store = match SnapshotStore::open(data_dir.join(STORE_DIR_NAME)) {
            Ok(store) => store,
            Err(err) => {
                warn!("filesnap: failed to open store, tracking disabled: {err}");
                return None;
            }
        };
        let active = match start {
            SessionStart::New { tracking_enabled } => tracking_enabled,
            // A resumed session keeps tracking if it has snapshots. One that
            // never captured any has nothing to resume tracking *from*, so
            // whether it once had tracking on makes no observable
            // difference — see the lazy marker below.
            SessionStart::Resumed => store.session_exists(&session_id),
        };
        if !active {
            return None;
        }
        // The log is written by the first capture, not here. A host that
        // mints a session id when it launches and abandons it if the user
        // immediately quits would otherwise make this the one component
        // that litters: measured on a development machine, better than half
        // the logs were empty. Deferring costs nothing, because the marker's
        // only consumer asks whether snapshots exist.
        info!("filesnap: tracking enabled for session {session_id}");
        Some(Arc::new(Self {
            store,
            session_id,
            hidden,
            state: Mutex::new(TrackState::default()),
        }))
    }

    /// Capture the turn-start checkpoint.
    ///
    /// Stat-walks the tracked set and hashes what changed, so this can take
    /// hundreds of milliseconds on a large project — call it off an async
    /// runtime's reactor thread.
    pub fn checkpoint_turn_start(&self, turn_id: &str, cwd: &Path, workspace_roots: &[PathBuf]) {
        if let Err(err) = self.checkpoint_inner(turn_id, cwd, workspace_roots) {
            warn!("filesnap: turn-start checkpoint failed (turn {turn_id}): {err}");
        }
    }

    fn checkpoint_inner(
        &self,
        turn_id: &str,
        cwd: &Path,
        workspace_roots: &[PathBuf],
    ) -> Result<()> {
        // The session's own workspace roots come first: they are what the
        // user declared the workspace to be, and on a sandboxed host they are
        // also where the agent is permitted to write, so scoping to them
        // makes "whatever can be changed can be reverted" structural rather
        // than coincidental. The marker walk-up is a guess about intent and
        // only stands in when there is nothing to go on.
        //
        // Roots unrelated to the turn's cwd are dropped. A configured root can
        // describe a different environment, or simply be stale, and scanning
        // an unrelated tree is the over-capture failure this feature exists to
        // avoid — a root that neither contains nor sits under the directory
        // being worked in is not this session's workspace.
        let related: Vec<PathBuf> = workspace_roots
            .iter()
            .filter(|root| cwd.starts_with(root) || root.starts_with(cwd))
            .cloned()
            .collect();
        let roots: Vec<PathBuf> = if related.is_empty() {
            vec![cwd.to_path_buf()]
        } else {
            related
        };
        // Whichever directory scoped this capture also scopes the ignore rules
        // applied to edit-hook captures (see `attach_pre_edits`).
        let primary = roots.first().cloned().unwrap_or_else(|| cwd.to_path_buf());
        self.lock_state().ignore_root = Some(primary);

        // Three partitions, unioned (see `scope`), plus what the agent has
        // written this session — wherever it lives. Walking the subtree
        // instead was unbounded by construction: on a repository of any age
        // most of what is on disk is build output, which is both the bulk of
        // the cost and the least worth keeping.
        let extras: Vec<PathBuf> = self.lock_state().extras.iter().cloned().collect();
        let files = tracked_files(&roots, extras, self.hidden);
        let checkpoint = self.store.checkpoint(&self.session_id, turn_id, files)?;
        info!(
            "filesnap: turn {turn_id} checkpoint {} ({} reused, {} hashed, {} skipped)",
            checkpoint.id,
            checkpoint.stats.reused,
            checkpoint.stats.hashed,
            checkpoint.stats.skipped,
        );
        Ok(())
    }

    /// Record pre-edit images from an applied edit and register the paths
    /// for future checkpoints. `pre_images` pairs each absolute path with
    /// what it held before the edit.
    ///
    /// Hashes and writes blobs — call it off an async runtime's reactor
    /// thread.
    pub fn attach_pre_edits(&self, turn_id: &str, pre_images: Vec<(PathBuf, PreEditImage)>) {
        // Symmetric ignore: a path the user excluded from snapshots must not
        // enter the store through the edit hook either. Without this, editing
        // an ignored file would both store its pre-edit content and register
        // the path as an extra, so every later checkpoint would capture it as
        // well — bypassing the scan's own ignore filter. The rules are read
        // fresh so the current ignore file governs.
        let ignore = self.lock_state().ignore_root.as_deref().map(load_ignore);
        for (path, image) in pre_images {
            if ignore
                .as_ref()
                .is_some_and(|rules| is_ignored(rules, &path))
            {
                continue;
            }
            // The edit-touched partition: unbounded on purpose, since its size
            // follows what the agent did rather than what is on disk.
            self.lock_state().extras.insert(path.clone());
            let key = path.to_string_lossy().into_owned();
            if let Err(err) = self
                .store
                .attach_pre_edit(&self.session_id, turn_id, &key, &image)
            {
                warn!("filesnap: pre-edit attach failed for {key}: {err}");
            }
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, TrackState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn session_scoped_binding() {
        let home = tempfile::tempdir().unwrap();

        // New session, tracking off → inactive, no marker.
        assert!(
            SnapshotTracker::maybe_new(
                home.path(),
                "t1".into(),
                SessionStart::New {
                    tracking_enabled: false
                },
                HiddenFiles::Skip,
            )
            .is_none()
        );
        // New session, tracking on → active. The marker is not written yet:
        // a session that never captures anything must leave nothing behind.
        let controller = SnapshotTracker::maybe_new(
            home.path(),
            "t1".into(),
            SessionStart::New {
                tracking_enabled: true,
            },
            HiddenFiles::Skip,
        )
        .expect("tracking on for a new session");
        assert!(
            !home.path().join("file_snapshots/refs/t1.json").exists(),
            "no snapshots yet, so no marker"
        );

        // Capturing one writes it.
        let ws = home.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.txt"), "x").unwrap();
        controller.checkpoint_turn_start("turn-1", &ws, &[]);
        assert!(home.path().join("file_snapshots/refs/t1.json").exists());

        // Resume with tracking now OFF → marker wins, still tracking.
        assert!(
            SnapshotTracker::maybe_new(
                home.path(),
                "t1".into(),
                SessionStart::Resumed,
                HiddenFiles::Skip,
            )
            .is_some()
        );
        // Resume of a session that never tracked → stays off, whatever the
        // host's setting now says.
        assert!(
            SnapshotTracker::maybe_new(
                home.path(),
                "t2".into(),
                SessionStart::Resumed,
                HiddenFiles::Skip,
            )
            .is_none()
        );
    }

    #[test]
    fn hidden_entries_are_skipped_unless_edited() {
        let home = tempfile::tempdir().unwrap();
        let ctl = SnapshotTracker::maybe_new(
            home.path(),
            "t1".into(),
            SessionStart::New {
                tracking_enabled: true,
            },
            HiddenFiles::Skip,
        )
        .unwrap();

        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".git")).unwrap();
        std::fs::write(ws.path().join(".env"), "SECRET=1").unwrap();
        std::fs::create_dir_all(ws.path().join(".github/workflows")).unwrap();
        std::fs::write(ws.path().join(".github/workflows/ci.yml"), "on: push").unwrap();
        std::fs::write(ws.path().join("src.rs"), "code").unwrap();

        ctl.checkpoint_turn_start("turn-1", ws.path(), &[]);
        let scanned = ctl.store.tracked_paths("t1").unwrap();
        assert!(
            scanned.iter().all(|p| !p.contains("/.env")),
            "tool state and credentials stay out of snapshots: {scanned:?}"
        );
        assert!(scanned.iter().all(|p| !p.contains("/.git")));
        assert!(scanned.iter().any(|p| p.ends_with("src.rs")));

        // An edited hidden file is a different matter: it is work product,
        // so the edit hook tracks it and a rewind can restore it.
        let workflow = ws.path().join(".github/workflows/ci.yml");
        ctl.attach_pre_edits(
            "turn-1",
            vec![(
                workflow.clone(),
                PreEditImage::Existed(b"on: push".to_vec()),
            )],
        );
        assert!(
            ctl.store
                .tracked_paths("t1")
                .unwrap()
                .contains(&workflow.to_string_lossy().into_owned()),
            "explicitly edited hidden files must remain restorable"
        );
    }

    #[test]
    fn a_large_directory_cannot_flood_a_capture() {
        // The property a plain subtree walk lacked. Without a bound, a capture
        // costs whatever happens to be on disk — on a real repository that was
        // 57k files and 100 GB, nearly all of it build output.
        let home = tempfile::tempdir().unwrap();
        let ctl = SnapshotTracker::maybe_new(
            home.path(),
            "t1".into(),
            SessionStart::New {
                tracking_enabled: true,
            },
            HiddenFiles::Skip,
        )
        .unwrap();

        let loose = tempfile::tempdir().unwrap();
        for i in 0..(crate::scope::RECENT_LIMIT + 50) {
            std::fs::write(loose.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        ctl.checkpoint_turn_start("turn-1", loose.path(), &[]);

        let history = ctl.store.thread_history("t1").unwrap();
        assert_eq!(
            history[0].1.entries.len(),
            crate::scope::RECENT_LIMIT,
            "no repository here, so only the recency partition contributes"
        );
    }

    #[test]
    fn ignored_paths_are_not_captured_through_the_edit_hook() {
        let home = tempfile::tempdir().unwrap();
        let ctl = SnapshotTracker::maybe_new(
            home.path(),
            "t1".into(),
            SessionStart::New {
                tracking_enabled: true,
            },
            HiddenFiles::Skip,
        )
        .unwrap();

        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".git")).unwrap();
        std::fs::write(
            ws.path().join(crate::scope::SNAPSHOT_IGNORE_FILENAME),
            "secrets/**\n",
        )
        .unwrap();
        std::fs::create_dir_all(ws.path().join("secrets")).unwrap();
        std::fs::write(ws.path().join("secrets/key.pem"), "private").unwrap();
        std::fs::write(ws.path().join("src.rs"), "code").unwrap();

        // Turn-start scan establishes the ignore scope for the session.
        ctl.checkpoint_turn_start("turn-1", ws.path(), &[]);

        let secret = ws.path().join("secrets/key.pem");
        let tracked = ws.path().join("src.rs");
        ctl.attach_pre_edits(
            "turn-1",
            vec![
                (secret.clone(), PreEditImage::Existed(b"private".to_vec())),
                (tracked.clone(), PreEditImage::Existed(b"code".to_vec())),
            ],
        );
        ctl.checkpoint_turn_start("turn-2", ws.path(), &[]);

        let paths = ctl.store.tracked_paths("t1").unwrap();
        assert!(
            !paths.contains(&secret.to_string_lossy().into_owned()),
            "ignored path must never reach the store, not even via the edit hook: {paths:?}"
        );
        assert!(paths.contains(&tracked.to_string_lossy().into_owned()));
    }

    #[test]
    fn checkpoint_workspace_and_fallback_modes() {
        let home = tempfile::tempdir().unwrap();
        let ctl = SnapshotTracker::maybe_new(
            home.path(),
            "t1".into(),
            SessionStart::New {
                tracking_enabled: true,
            },
            HiddenFiles::Skip,
        )
        .unwrap();

        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.txt"), "alpha").unwrap();
        ctl.checkpoint_turn_start("turn-1", ws.path(), &[]);

        let history = ctl.store.thread_history("t1").unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].1.entries.len(), 1);

        // Paths registered via pre-edit attach are observed by later
        // checkpoints, wherever they live.
        let loose = tempfile::tempdir().unwrap();
        std::fs::write(loose.path().join("note.md"), "n1").unwrap();
        let ctl2 = SnapshotTracker::maybe_new(
            home.path(),
            "t2".into(),
            SessionStart::New {
                tracking_enabled: true,
            },
            HiddenFiles::Skip,
        )
        .unwrap();
        ctl2.checkpoint_turn_start("turn-1", loose.path(), &[]);
        let outside = home.path().join("elsewhere.cfg");
        ctl2.attach_pre_edits(
            "turn-1",
            vec![(outside.clone(), PreEditImage::Existed(b"pre".to_vec()))],
        );
        std::fs::write(&outside, "post").unwrap();
        ctl2.checkpoint_turn_start("turn-2", loose.path(), &[]);

        let history = ctl2.store.thread_history("t2").unwrap();
        // turn-1 scan + turn-1 supplemental attach + turn-2 scan.
        assert_eq!(history.len(), 3);
        let outside_key = outside.to_string_lossy().into_owned();
        assert!(
            !history[0].1.entries.contains_key(&outside_key),
            "a path nothing had pointed at yet is simply not observed"
        );
        let last = &history[2].1;
        assert!(
            last.entries
                .contains_key(&outside.to_string_lossy().into_owned()),
            "extras are unioned into later checkpoints"
        );
    }
}
