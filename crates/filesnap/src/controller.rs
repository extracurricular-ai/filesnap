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

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use crate::declared::DeclaredWindow;
use crate::scope::HiddenFiles;
use crate::scope::ScanLimits;
use crate::store::PreEditImage;
use crate::store::WorkspaceStore;
use crate::turn::TurnScope;
use crate::turn::capture_turn;
use crate::turn::declare_edits;
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

/// A session's tracking decision plus the scope it captures at.
///
/// **Holds no mutable state.** It used to carry a mutex over an `extras` cache
/// and an `ignore_root`; D38 moved the work to [`crate::turn`] and both are
/// gone — the declared set is persisted (D25) and the ignore root is derived
/// from the scope each call carries. What remains is a convenience for an
/// embedder that wants the tracking decision made once: the CLI and any other
/// caller reach the same functions directly.
pub struct SnapshotTracker {
    store: WorkspaceStore,
    session_id: String,
    /// Carried rather than read at each scan so one session's bound cannot
    /// change under it mid-conversation (D14).
    hidden: HiddenFiles,
    limits: ScanLimits,
    declared: DeclaredWindow,
    /// The directory this session is bound to (D4).
    workspace: PathBuf,
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
        declared: DeclaredWindow,
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
            declared,
            workspace: workspace.to_path_buf(),
        }))
    }

    /// The scope a turn in `cwd` captures at, with this session's settings.
    fn scope(&self, cwd: &Path, workspace_roots: &[PathBuf]) -> TurnScope {
        TurnScope {
            cwd: cwd.to_path_buf(),
            roots: if workspace_roots.is_empty() {
                vec![self.workspace.clone()]
            } else {
                workspace_roots.to_vec()
            },
            hidden: self.hidden,
            limits: self.limits,
            declared: self.declared,
        }
    }

    /// Capture the turn-start checkpoint.
    ///
    /// Stat-walks the tracked set and hashes what changed, so this can take
    /// hundreds of milliseconds on a large project — call it off an async
    /// runtime's reactor thread.
    ///
    /// A failure degrades to "no snapshot for this turn": it is logged and
    /// never fails the turn, because a host that cannot snapshot should still
    /// be able to work.
    pub fn checkpoint_turn_start(&self, turn_id: &str, cwd: &Path, workspace_roots: &[PathBuf]) {
        let scope = self.scope(cwd, workspace_roots);
        if let Err(err) = capture_turn(&self.store, &self.session_id, turn_id, &scope) {
            warn!("filesnap: turn-start checkpoint failed (turn {turn_id}): {err}");
        }
    }

    /// Record pre-edit images from an applied edit and register the paths for
    /// future checkpoints.
    ///
    /// Takes the scope rather than remembering one: `cwd` is what makes the
    /// ignore rules resolvable, and a value derived per call cannot be absent
    /// the way a remembered one was before the first capture (C6).
    ///
    /// Hashes and writes blobs — call it off an async runtime's reactor
    /// thread.
    pub fn attach_pre_edits(
        &self,
        turn_id: &str,
        cwd: &Path,
        pre_images: Vec<(PathBuf, PreEditImage)>,
    ) {
        let scope = self.scope(cwd, &[]);
        if let Err(err) = declare_edits(&self.store, &self.session_id, turn_id, &scope, pre_images)
        {
            warn!("filesnap: declare failed (turn {turn_id}): {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    /// A temp directory that reports its **canonical** path.
    ///
    /// A capture records canonical keys, so a test comparing against the raw
    /// temp spelling compares two names for one file — `/var` against
    /// `/private/var` on macOS, `RUNNER~1` against `runneradmin` on Windows.
    /// Both are the ordinary case on their platform and neither exists on
    /// Linux, which is why these tests passed here and failed there.
    struct CanonicalDir {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    impl CanonicalDir {
        fn of(dir: tempfile::TempDir) -> Self {
            let path = crate::scope::canonical_key(dir.path());
            Self { _dir: dir, path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    use super::*;

    /// A data directory and a workspace, both real on disk.
    ///
    /// The workspace has to exist before a tracker can be built now: a
    /// session's records live in its workspace's partition, so there is no
    /// tracker without a workspace to key it on.
    fn dirs() -> (CanonicalDir, CanonicalDir) {
        // Both canonical: several tests put an "outside the workspace" file
        // under the data directory, and comparing that raw path against a
        // recorded key is the same two-names-for-one-file mistake.
        (
            CanonicalDir::of(tempfile::tempdir().unwrap()),
            CanonicalDir::of(tempfile::tempdir().unwrap()),
        )
    }

    fn tracker(
        home: &CanonicalDir,
        ws: &CanonicalDir,
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
            DeclaredWindow::default(),
        )
    }

    fn tracking(home: &CanonicalDir, ws: &CanonicalDir, id: &str) -> Arc<SnapshotTracker> {
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
        let other = CanonicalDir::of(tempfile::tempdir().unwrap());

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
        std::fs::create_dir_all(ws.path().join(".github").join("workflows")).unwrap();
        std::fs::write(
            ws.path().join(".github").join("workflows").join("ci.yml"),
            "on: push",
        )
        .unwrap();
        std::fs::write(ws.path().join("src.rs"), "code").unwrap();

        ctl.checkpoint_turn_start("turn-1", ws.path(), &[]);
        let scanned = ctl.store.tracked_paths("t1").unwrap();
        // By file name, not by a substring containing a separator. `"/.env"`
        // can never match on Windows, so the assertion would pass there
        // without checking anything — and this is the one asserting that a
        // credentials file stays out of the store.
        let named = |want: &str| {
            scanned
                .iter()
                .any(|p| Path::new(p).file_name().is_some_and(|n| n == want))
        };
        let under = |dir: &str| {
            scanned
                .iter()
                .any(|p| Path::new(p).components().any(|c| c.as_os_str() == dir))
        };
        assert!(
            !named(".env"),
            "tool state and credentials stay out of snapshots: {scanned:?}"
        );
        assert!(!under(".git"), "{scanned:?}");
        assert!(named("src.rs"));

        // An edited hidden file is a different matter: it is work product,
        // so the edit hook tracks it and a rewind can restore it.
        // Chained joins, not a literal `/` inside one. `join` appends its
        // argument verbatim, so `".github/workflows/ci.yml"` gives a
        // mixed-separator path on Windows — equal to the real one as a
        // `Path`, and different from it as the *string* a manifest key is.
        let workflow = ws.path().join(".github").join("workflows").join("ci.yml");
        ctl.attach_pre_edits(
            "turn-1",
            ws.path(),
            vec![(
                workflow.clone(),
                PreEditImage::Existed(b"on: push".to_vec()),
            )],
        );
        assert!(
            ctl.store
                .tracked_paths("t1")
                .unwrap()
                .contains(&crate::fixture::key_of(&workflow)),
            "explicitly edited hidden files must remain restorable"
        );
    }

    #[test]
    fn a_large_directory_cannot_flood_a_capture() {
        // The property a plain subtree walk lacked. Without a bound, a capture
        // costs whatever happens to be on disk — on a real repository that was
        // 70,609 files and 116 GB, nearly all of it build output.
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
        std::fs::write(ws.path().join("secrets").join("key.pem"), "private").unwrap();
        std::fs::write(ws.path().join("src.rs"), "code").unwrap();

        // Turn-start scan establishes the ignore scope for the session.
        ctl.checkpoint_turn_start("turn-1", ws.path(), &[]);

        let secret = ws.path().join("secrets").join("key.pem");
        let tracked = ws.path().join("src.rs");
        ctl.attach_pre_edits(
            "turn-1",
            ws.path(),
            vec![
                (secret.clone(), PreEditImage::Existed(b"private".to_vec())),
                (tracked.clone(), PreEditImage::Existed(b"code".to_vec())),
            ],
        );
        ctl.checkpoint_turn_start("turn-2", ws.path(), &[]);

        let paths = ctl.store.tracked_paths("t1").unwrap();
        assert!(
            !paths.contains(&crate::fixture::key_of(&secret)),
            "ignored path must never reach the store, not even via the edit hook: {paths:?}"
        );
        assert!(paths.contains(&crate::fixture::key_of(&tracked)));
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
        let loose = CanonicalDir::of(tempfile::tempdir().unwrap());
        std::fs::write(loose.path().join("note.md"), "n1").unwrap();
        let ctl2 = tracking(&home, &loose, "t2");
        ctl2.checkpoint_turn_start("turn-1", loose.path(), &[]);
        let outside = home.path().join("elsewhere.cfg");
        ctl2.attach_pre_edits(
            "turn-1",
            ws.path(),
            vec![(outside.clone(), PreEditImage::Existed(b"pre".to_vec()))],
        );
        std::fs::write(&outside, "post").unwrap();
        ctl2.checkpoint_turn_start("turn-2", loose.path(), &[]);

        let history = ctl2.store.thread_history("t2").unwrap();
        // turn-1 scan + turn-1 supplemental attach + turn-2 scan.
        assert_eq!(history.len(), 3);
        let outside_key = crate::fixture::key_of(&outside);
        assert!(
            !history[0].1.entries.contains_key(&outside_key),
            "a path nothing had pointed at yet is simply not observed"
        );
        let last = &history[2].1;
        assert!(
            last.entries.contains_key(&crate::fixture::key_of(&outside)),
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
        let home = CanonicalDir::of(tempfile::tempdir().unwrap());
        let ws = CanonicalDir::of(tempfile::tempdir().unwrap());
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
            DeclaredWindow::default(),
        )
        .expect("tracking enabled");

        // No checkpoint yet, so nothing has recorded an ignore root.
        tracker.attach_pre_edits(
            "turn-1",
            ws.path(),
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
                .contains(&crate::fixture::key_of(&secret)),
            "an ignored path entered the store through the edit hook"
        );
    }

    /// A capture in a **long-lived process** honours the window.
    ///
    /// The tracker kept its own copy of the declared set that nothing ever
    /// pruned, and unioned it with the windowed one. So inside a single
    /// process the bound did nothing: a path that had aged out was re-stat'd
    /// and re-captured for the rest of the session, and the tracked set grew
    /// without limit — which is exactly the growth D25 exists to stop. It
    /// took effect only after a restart, the opposite of the point.
    #[test]
    fn a_capture_in_one_process_stops_watching_a_path_past_the_window() {
        let home = CanonicalDir::of(tempfile::tempdir().unwrap());
        let ws = CanonicalDir::of(tempfile::tempdir().unwrap());
        let tracker = SnapshotTracker::maybe_new(
            home.path(),
            ws.path(),
            "s1".into(),
            SessionStart::New {
                tracking_enabled: true,
            },
            HiddenFiles::Skip,
            ScanLimits::default(),
            DeclaredWindow::default(),
        )
        .expect("tracking enabled");

        // A path outside the workspace, so only the declared set can carry it
        // — the scan partitions never will.
        let outside = home.path().join("edited-once.cfg");
        std::fs::write(&outside, b"before").unwrap();
        tracker.checkpoint_turn_start("turn-0", ws.path(), &[ws.path().to_path_buf()]);
        tracker.attach_pre_edits(
            "turn-0",
            ws.path(),
            vec![(outside.clone(), PreEditImage::Existed(b"before".to_vec()))],
        );

        let store = crate::WorkspaceStore::open(home.path(), ws.path()).unwrap();
        let key = crate::fixture::key_of(&outside);
        let captured = |store: &crate::WorkspaceStore| {
            store
                .latest_manifest("s1")
                .unwrap()
                .is_some_and(|m| m.entries.contains_key(&key))
        };

        tracker.checkpoint_turn_start("turn-1", ws.path(), &[ws.path().to_path_buf()]);
        assert!(captured(&store), "still inside the window");

        // The same process runs out the window without touching that file.
        for i in 2..=(crate::declared::DECLARED_WINDOW_TURNS.get() + 3) {
            tracker.checkpoint_turn_start(
                &format!("turn-{i}"),
                ws.path(),
                &[ws.path().to_path_buf()],
            );
        }

        assert!(
            !captured(&store),
            "the process's own cache kept an aged-out path alive"
        );
    }
}
