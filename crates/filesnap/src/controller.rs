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
use crate::scope::ScanLimits;
use crate::scope::is_ignored;
use crate::scope::load_ignore;
use crate::scope::tracked_files;
use crate::store::PreEditImage;
use crate::store::WorkspaceStore;
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
    store: WorkspaceStore,
    session_id: String,
    hidden: HiddenFiles,
    /// Carried rather than read at each scan so one session's bound cannot
    /// change under it mid-conversation (D14).
    limits: ScanLimits,
    /// The directory this session is bound to (D4), and the fallback the
    /// ignore filter uses before any capture has run — see `ignore_root`.
    workspace: PathBuf,
    state: Mutex<TrackState>,
}

#[derive(Default)]
struct TrackState {
    /// Agent-edited paths registered via pre-edit attach, unioned into every
    /// checkpoint scan so post-edit states keep being observed.
    ///
    /// A cache over the persisted set, not the truth: the store holds it, so
    /// a session resuming in a new process picks up what the last one
    /// declared (D25).
    extras: BTreeSet<PathBuf>,
    /// Directory whose ignore rules scope this session's captures: the
    /// workspace root when one was found, else the invocation directory.
    /// Recorded by the turn-start checkpoint.
    ///
    /// `None` until then, which used to mean the edit hook's filter matched
    /// nothing at all — `None.is_some_and(..)` is `false`. The ordering that
    /// saved it ("the turn-start checkpoint always precedes tool execution")
    /// was a promise about a caller in another crate, with nothing here to
    /// enforce it, and the cost of it not holding is an ignored file — a
    /// `.env`, a key — entering the blob store and then being kept by every
    /// later capture, since the path is registered as an extra. II.3 requires
    /// the rule to hold from the first operation, so the session's bound
    /// directory is the fallback (C6).
    ignore_root: Option<PathBuf>,
}

impl SnapshotTracker {
    /// Build a tracker if this session should track (see module doc).
    ///
    /// `workspace` is the directory this session is bound to, and the binding
    /// is permanent: its records live in that workspace's partition, so the
    /// same id used against a different directory addresses a *different*
    /// session rather than reaching this one's data. That is structural here
    /// rather than a rule the caller has to keep.
    pub fn maybe_new(
        data_dir: &Path,
        workspace: &Path,
        session_id: String,
        start: SessionStart,
        hidden: HiddenFiles,
        limits: ScanLimits,
    ) -> Option<Arc<Self>> {
        let store = match WorkspaceStore::open(data_dir, workspace) {
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
            limits,
            workspace: workspace.to_path_buf(),
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
        // The persisted set is authoritative and applies the turn window; the
        // in-memory one is this process's own additions since it last wrote.
        let mut extras: BTreeSet<PathBuf> = self.lock_state().extras.iter().cloned().collect();
        match self.store.declared_paths(&self.session_id) {
            Ok(declared) => extras.extend(declared),
            Err(err) => warn!("filesnap: declared set unreadable, using this process's: {err}"),
        }
        let extras: Vec<PathBuf> = extras.into_iter().collect();
        let scan = tracked_files(&roots, extras, self.hidden, self.limits);
        // What the *scan* passed over is a drop too, and the capture cannot
        // see it: an over-size file never reaches the manifest at all.
        let scan_dropped = scan.dropped;
        let mut checkpoint = self
            .store
            .checkpoint(&self.session_id, turn_id, scan.files)?;
        for drop in scan_dropped {
            checkpoint.stats.dropped += 1;
            if checkpoint.stats.sample.len() < crate::checkpoint::DROP_SAMPLE_LIMIT {
                checkpoint.stats.sample.push(drop);
            }
        }
        info!(
            "filesnap: turn {turn_id} checkpoint {} ({} reused, {} hashed, {} dropped)",
            checkpoint.id,
            checkpoint.stats.reused,
            checkpoint.stats.hashed,
            checkpoint.stats.dropped,
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
        // Never `None`: an unfiltered edit hook is how an ignored file gets
        // into the store permanently (C6).
        let root = self
            .lock_state()
            .ignore_root
            .clone()
            .unwrap_or_else(|| self.workspace.clone());
        let ignore = load_ignore(&root);
        let mut declared = Vec::new();
        for (path, image) in pre_images {
            if is_ignored(&ignore, &path) {
                continue;
            }
            // The edit-touched partition. Bounded by a rolling window of
            // turns rather than by count, so its size follows what the agent
            // is working on rather than everything it ever touched (D25).
            self.lock_state().extras.insert(path.clone());
            declared.push(path.clone());
            let key = path.to_string_lossy().into_owned();
            if let Err(err) = self
                .store
                .attach_pre_edit(&self.session_id, turn_id, &key, &image)
            {
                warn!("filesnap: pre-edit attach failed for {key}: {err}");
            }
        }
        // Persisted last, and separately: a declaration that fails to land
        // costs this session future *observation* of those paths, which is
        // recoverable by editing them again. Letting it fail the attach above
        // would cost the pre-edit images themselves, which is not.
        if let Err(err) = self
            .store
            .declare_paths(&self.session_id, turn_id, &declared)
        {
            warn!("filesnap: could not persist the declared set: {err}");
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

    /// A data directory and a workspace, both real on disk.
    ///
    /// The workspace has to exist before a tracker can be built now: a
    /// session's records live in its workspace's partition, so there is no
    /// tracker without a workspace to key it on.
    fn dirs() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    fn tracker(
        home: &tempfile::TempDir,
        ws: &tempfile::TempDir,
        id: &str,
        start: SessionStart,
    ) -> Option<Arc<SnapshotTracker>> {
        SnapshotTracker::maybe_new(
            home.path(),
            ws.path(),
            id.into(),
            start,
            HiddenFiles::Skip,
            ScanLimits::default(),
        )
    }

    fn tracking(
        home: &tempfile::TempDir,
        ws: &tempfile::TempDir,
        id: &str,
    ) -> Arc<SnapshotTracker> {
        tracker(
            home,
            ws,
            id,
            SessionStart::New {
                tracking_enabled: true,
            },
        )
        .expect("tracking on for a new session")
    }

    #[test]
    fn session_scoped_binding() {
        let (home, ws) = dirs();
        // The marker is the log's existence, so ask the store rather than a
        // path: where records live is the layout's business, not this test's.
        let marker = |id: &str| {
            WorkspaceStore::open(home.path(), ws.path())
                .unwrap()
                .session_exists(id)
        };

        // New session, tracking off → inactive, no marker.
        assert!(
            tracker(
                &home,
                &ws,
                "t1",
                SessionStart::New {
                    tracking_enabled: false
                }
            )
            .is_none()
        );
        // New session, tracking on → active. The marker is not written yet:
        // a session that never captures anything must leave nothing behind.
        let controller = tracking(&home, &ws, "t1");
        assert!(!marker("t1"), "no snapshots yet, so no marker");

        // Capturing one writes it.
        std::fs::write(ws.path().join("a.txt"), "x").unwrap();
        controller.checkpoint_turn_start("turn-1", ws.path(), &[]);
        assert!(marker("t1"));

        // Resume with tracking now OFF → marker wins, still tracking.
        assert!(tracker(&home, &ws, "t1", SessionStart::Resumed).is_some());
        // Resume of a session that never tracked → stays off, whatever the
        // host's setting now says.
        assert!(tracker(&home, &ws, "t2", SessionStart::Resumed).is_none());
    }

    /// A session id is scoped to the workspace it was bound to. Used against
    /// a different directory it addresses a *different* session rather than
    /// reaching the first one's data — which is D4 made structural: manifest
    /// keys are absolute paths, so a session resolved across workspaces would
    /// silently operate on the original directory while the user stood in
    /// another.
    #[test]
    fn a_session_id_does_not_reach_across_workspaces() {
        let (home, ws) = dirs();
        let other = tempfile::tempdir().unwrap();

        std::fs::write(ws.path().join("a.txt"), "x").unwrap();
        tracking(&home, &ws, "shared-id").checkpoint_turn_start("turn-1", ws.path(), &[]);

        assert!(
            WorkspaceStore::open(home.path(), ws.path())
                .unwrap()
                .session_exists("shared-id")
        );
        assert!(
            !WorkspaceStore::open(home.path(), other.path())
                .unwrap()
                .session_exists("shared-id"),
            "the same id in another workspace is another session, not this one"
        );
        assert!(
            tracker(&home, &other, "shared-id", SessionStart::Resumed).is_none(),
            "and resuming it there finds nothing to resume"
        );
    }

    #[test]
    fn hidden_entries_are_skipped_unless_edited() {
        let (home, ws) = dirs();
        let ctl = tracking(&home, &ws, "t1");

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
        let (home, loose) = dirs();
        let ctl = tracking(&home, &loose, "t1");

        for i in 0..(crate::ScanLimits::default().max_files + 50) {
            std::fs::write(loose.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        ctl.checkpoint_turn_start("turn-1", loose.path(), &[]);

        let history = ctl.store.thread_history("t1").unwrap();
        assert_eq!(
            history[0].1.entries.len(),
            crate::ScanLimits::default().max_files,
            "no repository here, so only the recency partition contributes"
        );
    }

    #[test]
    fn ignored_paths_are_not_captured_through_the_edit_hook() {
        let (home, ws) = dirs();
        let ctl = tracking(&home, &ws, "t1");

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
        let (home, ws) = dirs();
        let ctl = tracking(&home, &ws, "t1");

        std::fs::write(ws.path().join("a.txt"), "alpha").unwrap();
        ctl.checkpoint_turn_start("turn-1", ws.path(), &[]);

        let history = ctl.store.thread_history("t1").unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].1.entries.len(), 1);

        // Paths registered via pre-edit attach are observed by later
        // checkpoints, wherever they live.
        let loose = tempfile::tempdir().unwrap();
        std::fs::write(loose.path().join("note.md"), "n1").unwrap();
        let ctl2 = tracking(&home, &loose, "t2");
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

    /// An edit that lands before any capture is still filtered.
    ///
    /// `ignore_root` is `None` until the turn-start checkpoint records it,
    /// and the filter was `None.is_some_and(..)` — which is `false`, so
    /// nothing was ignored. The ordering that saved it was a promise about a
    /// caller in another crate. The cost of it not holding is an ignored file
    /// entering the blob store and then being kept by every later capture,
    /// because the path is registered as an extra (C6, II.3).
    #[test]
    fn the_edit_hook_filters_before_the_first_capture_has_run() {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join(crate::SNAPSHOT_IGNORE_FILENAME), ".env\n").unwrap();
        let secret = ws.path().join(".env");
        std::fs::write(&secret, "TOKEN=hunter2").unwrap();

        let tracker = SnapshotTracker::maybe_new(
            home.path(),
            ws.path(),
            "s1".into(),
            SessionStart::New {
                tracking_enabled: true,
            },
            HiddenFiles::Skip,
            ScanLimits::default(),
        )
        .expect("tracking enabled");

        // No checkpoint yet, so nothing has recorded an ignore root.
        tracker.attach_pre_edits(
            "turn-1",
            vec![(
                secret.clone(),
                PreEditImage::Existed(b"TOKEN=hunter2".to_vec()),
            )],
        );

        let store = crate::WorkspaceStore::open(home.path(), ws.path()).unwrap();
        assert!(
            !store
                .tracked_paths("s1")
                .unwrap()
                .contains(&secret.to_string_lossy().into_owned()),
            "an ignored path entered the store through the edit hook"
        );
    }
}
