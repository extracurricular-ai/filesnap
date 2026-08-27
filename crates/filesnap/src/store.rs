//! [`WorkspaceStore`]: the facade tying blobs, manifests, session logs,
//! checkpoints and restore together for one workspace.
//!
//! The store lives inside the host's own data directory and never inside the
//! user's workspace; [`crate::workspace`] owns the layout underneath it.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::blob::BlobStore;
use crate::checkpoint::Checkpoint;
use crate::checkpoint::capture;
use crate::error::Result;
use crate::error::SnapshotError;
use crate::manifest::Manifest;
use crate::manifest::ManifestStore;
use crate::refs::GcStats;
use crate::refs::RefStore;
use crate::refs::RestoreRecord;
use crate::refs::SnapshotRef;
use crate::refs::TurnIndex;
use crate::restore::ApplyStats;
use crate::restore::RestorePlan;
use crate::restore::apply_plan;
use crate::restore::is_protected;
use crate::restore::plan_restore;
use crate::workspace;
use crate::workspace::WorkspaceKey;
use ignore::gitignore::Gitignore;

/// Turn-id prefix for the safety checkpoint recorded before every restore.
///
/// Begins with the reserved character, so it lies in a namespace no caller can
/// reach: [`WorkspaceStore::checkpoint`] and its siblings refuse an id
/// starting with `_`. It used to be `safety-restore:`, which the old sanitizer
/// mapped to `safety-restore_` — a name a user's own turn could hold exactly,
/// putting a rewind and a safety capture in one record (D7).
pub const SAFETY_TURN_PREFIX: &str = "_safety-restore-";

/// The state a restore moves the workspace to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreTarget {
    manifest_id: String,
}

/// What a path held immediately before an edit changed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreEditImage {
    /// The file existed and held these bytes.
    Existed(Vec<u8>),
    /// The edit created the file; it did not exist before. This witnessed
    /// birth is the only thing that ever licenses a restore to delete the
    /// file again, so it is recorded rather than skipped.
    DidNotExist,
}

/// Which direction a restore moves the workspace's undo history, and whose
/// undo stack it touches.
///
/// The destination belongs to the kind because the two are not independent:
/// a rewind files a record under the session it hands the workspace to, and
/// an undo spends the record filed under the session asking for it. Keeping
/// them as separate arguments let a meaningless fourth combination be
/// spelled, and made `None` at a call site say nothing about which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreKind<'a> {
    /// Going back in history, leaving an undo behind, filed under the
    /// session this hands the workspace to.
    ///
    /// `undo_for: None` is a rewind with no destination session to be
    /// reachable from: nothing is recorded and the restore is not undoable.
    /// The caller is expected to have said so before running it.
    ///
    /// **The lock does not reach a destination that is not the performer.**
    /// A session's lock serializes it against itself (D18), and the undo
    /// record is written under `undo_for` — so naming a *different* session
    /// writes to a file this lock does not cover. It is safe in the case that
    /// motivates it, where a forking host creates that session immediately
    /// before the call and nothing else is using it yet. A host that hands
    /// `undo_for` a session already in use is outside what the lock protects,
    /// and the two concurrent `push_restore` read-modify-writes can lose an
    /// entry (D26).
    Rewind { undo_for: Option<&'a str> },
    /// Reversing `spending`'s most recent rewind and consuming that record,
    /// so undoing twice walks back through two rewinds rather than
    /// oscillating between the last two states.
    Undo { spending: &'a str },
}

impl RestoreTarget {
    pub fn manifest_id(&self) -> &str {
        &self.manifest_id
    }
}

/// One workspace's records, plus the content store they share with every
/// other workspace.
///
/// Named for its scope rather than its contents, because the question a
/// reader has here is whether an operation reaches the whole store or one
/// slice of it. Everything on this type is one slice; what spans the store
/// is a free function ([`crate::collect_garbage`]) and therefore cannot be
/// reached by mistake from an instance.
pub struct WorkspaceStore {
    blobs: BlobStore,
    manifests: ManifestStore,
    refs: RefStore,
    turns: TurnIndex,
    declared: crate::declared::DeclaredStore,
    key: WorkspaceKey,
    partition: PathBuf,
}

#[derive(Debug)]
pub struct RestoreOutcome {
    /// The pre-restore state; restoring to it undoes this restore (redo).
    pub safety: RestoreTarget,
    pub plan: RestorePlan,
    pub stats: ApplyStats,
}

/// The session id [`WorkspaceStore::locking_is_enforced`] probes under.
///
/// Internal on purpose: it begins with `_`, so [`crate::id::validate_external`]
/// refuses it from a host, and a probe can never contend with a real session
/// or be mistaken for one.
pub(crate) const LOCK_PROBE_ID: &str = "_lock-probe";

impl WorkspaceStore {
    /// Open the partition for `workspace`, creating it if absent.
    ///
    /// The caller passes its own data directory, not a store root: the layout
    /// and the format version underneath are the engine's, because a reader
    /// that must refuse a version it does not understand cannot refuse what
    /// it was handed.
    ///
    /// `workspace` is canonicalized and must exist. For a workspace that has
    /// been removed but whose snapshots have not, see [`Self::open_at`].
    pub fn open(data_dir: &Path, workspace: &Path) -> Result<Self> {
        Self::open_at(data_dir, &WorkspaceKey::of(workspace)?)
    }

    /// Open a partition by the key [`Self::open`] would have derived.
    ///
    /// The escape hatch for operations that must still work when the
    /// directory is gone — deleting a finished project's snapshots, or
    /// listing what is left of one.
    pub fn open_at(data_dir: &Path, key: &WorkspaceKey) -> Result<Self> {
        let root = workspace::store_root(data_dir)?;
        let partition = workspace::partition_dir(&root, key);
        Ok(Self {
            // Content is shared across every workspace, so this one root is
            // outside the partition.
            blobs: BlobStore::open(workspace::blobs_dir(&root))?,
            manifests: ManifestStore::open(partition.join("manifests"))?,
            refs: RefStore::open(partition.join("refs"))?,
            turns: TurnIndex::open(&partition)?,
            declared: crate::declared::DeclaredStore::open(crate::declared::dir_in(&partition))?,
            key: key.clone(),
            partition,
        })
    }

    /// Run `f` holding this session's lock, so a second invocation of the
    /// same session waits rather than interleaving with it.
    ///
    /// **Never nest this.** `flock` is per open-file-description, so a second
    /// acquire inside the first would block against itself until the budget
    /// expired and then report the session busy. Public methods take the
    /// lock; the private `*_locked` helpers they call do not.
    fn locked<T>(&self, session_id: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _guard = self.lock_session(session_id)?;
        f()
    }

    /// Take this session's lock and hand back the guard, for a caller that
    /// needs to hold it across more than one expression. Same rule: never
    /// nest it.
    fn lock_session(&self, session_id: &str) -> Result<crate::lock::SessionGuard> {
        crate::lock::acquire(&self.partition, session_id, crate::lock::LOCK_BUDGET)?.ok_or_else(
            || SnapshotError::SessionBusy {
                session: session_id.to_string(),
            },
        )
    }

    /// Whether this store's filesystem actually enforces the session lock.
    ///
    /// A store on a filesystem without locking still works — D18 takes cargo's
    /// line and proceeds unlocked rather than refusing a user who has no other
    /// machine. But that is a fact about their setup, and the only way to
    /// learn it otherwise is a race they cannot reproduce on demand, so
    /// `doctor` reports it.
    ///
    /// Probed under a fixed internal id: one lock file per partition rather
    /// than one per call, and it cannot collide with a session, because an
    /// external id may not begin with `_`.
    pub fn locking_is_enforced(&self) -> Result<bool> {
        match crate::lock::acquire(&self.partition, LOCK_PROBE_ID, crate::lock::LOCK_BUDGET)? {
            Some(guard) => Ok(guard.is_enforced()),
            // Contended: another invocation is holding it, and holding it is
            // the one thing a filesystem without locks cannot do.
            None => Ok(true),
        }
    }

    /// Which workspace's records this reaches.
    pub fn key(&self) -> &WorkspaceKey {
        &self.key
    }

    /// Capture a checkpoint of `files` for `thread_id` and append it to
    /// the thread's snapshot log. The previous checkpoint (if any) serves
    /// as the stat cache. Every path in `files` is either recorded or noted
    /// as absent, and absence is the only evidence a later restore has for
    /// deleting anything — so what is passed here bounds what can be undone.
    pub fn checkpoint(
        &self,
        thread_id: &str,
        turn_id: &str,
        files: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Checkpoint> {
        crate::id::validate_external("session id", thread_id)?;
        crate::id::validate_external("turn id", turn_id)?;
        self.locked(thread_id, || {
            self.checkpoint_internal(thread_id, turn_id, files)
        })
    }

    /// The capture itself, reachable with an id from the reserved namespace.
    ///
    /// Only one caller needs that: the safety checkpoint `restore_to` takes
    /// before it writes, whose turn id is minted rather than supplied. Every
    /// other route is the public one above, which refuses a reserved id.
    fn checkpoint_internal(
        &self,
        thread_id: &str,
        turn_id: &str,
        files: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Checkpoint> {
        let prev = self.latest_manifest(thread_id)?;
        let cp = capture(&self.blobs, &self.manifests, files, prev.as_ref())?;
        self.refs
            .append(thread_id, turn_id.to_string(), cp.id.clone())?;
        // Turn ids survive forking, so this index is what lets a rewind
        // resolve the same state from any branch.
        self.turns.set_turn(turn_id, &cp.id)?;
        Ok(cp)
    }

    /// Record that `paths` are being edited during `turn_id`, so later
    /// captures keep watching them.
    ///
    /// Persisted, so a session resuming in a new process picks up what the
    /// last one declared. A path stays watched for
    /// [`crate::DECLARED_WINDOW_TURNS`] turns after its last declaration
    /// (D25); redeclaring renews it.
    pub fn declare_paths(&self, session_id: &str, turn_id: &str, paths: &[PathBuf]) -> Result<()> {
        crate::id::validate_external("session id", session_id)?;
        crate::id::validate_external("turn id", turn_id)?;
        self.locked(session_id, || {
            self.declared.declare(session_id, turn_id, paths)
        })
    }

    /// Record that a turn happened, so the declared set's window counts
    /// turns rather than only the turns that declared something.
    pub fn note_turn(&self, session_id: &str, turn_id: &str) -> Result<()> {
        self.locked(session_id, || self.declared.note_turn(session_id, turn_id))
    }

    /// Paths this session declared that are still inside the window — what a
    /// capture unions into its scan.
    pub fn declared_paths(&self, session_id: &str) -> Result<std::collections::BTreeSet<PathBuf>> {
        self.declared.active(session_id)
    }

    /// Whether `session_id` has a snapshot log. With session-scoped binding,
    /// log existence *is* the persisted "tracking enabled" state.
    pub fn session_exists(&self, session_id: &str) -> bool {
        self.refs.exists(session_id)
    }

    /// Create the (empty) snapshot log for `session_id` if missing, marking
    /// the session as tracking for its whole lifetime.
    pub fn ensure_session(&self, session_id: &str) -> Result<()> {
        crate::id::validate_external("session id", session_id)?;
        self.locked(session_id, || self.refs.ensure(session_id))
    }

    /// Retroactively record what `path_key` held before an edit in `turn_id`.
    ///
    /// Returns `Ok(None)` when nothing was appended — the latest manifest
    /// already covers the path, because the turn-start scan saw it, or the
    /// tombstone for a created file is already recorded. Otherwise a
    /// supplemental manifest is appended under the same turn id: the latest
    /// plus the pre-edit entry, or plus the tombstone for
    /// [`PreEditImage::DidNotExist`]. Restoring to this turn then recovers
    /// the pre-edit content, or removes a file the edit created.
    ///
    /// Resolution for a turn with several entries picks the last, which is
    /// the most complete.
    pub fn attach_pre_edit(
        &self,
        session_id: &str,
        turn_id: &str,
        path_key: &str,
        image: &PreEditImage,
    ) -> Result<Option<String>> {
        crate::id::validate_external("session id", session_id)?;
        crate::id::validate_external("turn id", turn_id)?;
        self.locked(session_id, || {
            self.attach_pre_edit_locked(session_id, turn_id, path_key, image)
        })
    }

    /// The body of [`Self::attach_pre_edit`], with the session lock already
    /// held. Never call it without one.
    fn attach_pre_edit_locked(
        &self,
        session_id: &str,
        turn_id: &str,
        path_key: &str,
        image: &PreEditImage,
    ) -> Result<Option<String>> {
        let thread_id = session_id;
        // **The second place a manifest key is minted**, and it has to agree
        // with the first. `capture` records canonical keys; storing the
        // caller's spelling here would give one file two keys again — and a
        // tombstone under the wrong spelling is a tombstone `plan_restore`
        // can never match, so the file it licensed removing is never removed.
        let path_key = &crate::scope::canonical_key(std::path::Path::new(path_key))
            .to_string_lossy()
            .into_owned();
        let latest = self.latest_manifest(thread_id)?.unwrap_or_default();
        if latest.entries.contains_key(path_key) {
            return Ok(None);
        }
        let PreEditImage::Existed(content) = image else {
            // The edit created this file. Record that it did not exist rather
            // than recording nothing: absence has to be *stated* to be usable
            // as evidence outside a complete scan, which is precisely the case
            // here — the path may well be outside the scanned scope.
            if latest.absent.contains(path_key) {
                return Ok(None);
            }
            let mut manifest = latest;
            manifest.absent.insert(path_key.to_string());
            let id = self.manifests.save(&manifest)?;
            self.refs
                .append(thread_id, turn_id.to_string(), id.clone())?;
            self.turns.set_turn(turn_id, &id)?;
            return Ok(Some(id));
        };

        let hash = self.blobs.store_bytes(content)?;
        let mut manifest = latest;
        manifest.entries.insert(
            path_key.to_string(),
            crate::manifest::FileEntry {
                // Pre-edit images come from the edit's own content, not the
                // filesystem: there is no stat behind them, so neither the
                // fingerprint nor the mode was ever observed. Both say so.
                // The zero fingerprint disables the stat-cache fast path;
                // `None` keeps a restore from applying permissions nobody
                // ever saw.
                mode: None,
                size: content.len() as u64,
                mtime_secs: 0,
                mtime_nanos: 0,
                hash,
            },
        );
        let id = self.manifests.save(&manifest)?;
        self.refs
            .append(thread_id, turn_id.to_string(), id.clone())?;
        // A supplemental attach extends this turn's capture, so it becomes
        // the state the turn resolves to.
        self.turns.set_turn(turn_id, &id)?;
        Ok(Some(id))
    }

    /// The state captured at `turn_id`'s start (or extended by a later
    /// pre-edit attach). Resolved through the turn index rather than any
    /// thread's log, so every branch holding that turn gets the same answer.
    pub fn target_for_turn(&self, turn_id: &str) -> Result<Option<RestoreTarget>> {
        Ok(self
            .turns
            .manifest_for_turn(turn_id)?
            .map(|manifest_id| RestoreTarget { manifest_id }))
    }

    /// Where an undo returns to: the state captured just before the restore
    /// that produced `thread_id`. Filed under the thread a rewind switched
    /// **to**, which is where the user is when they ask to undo — and which
    /// no other session can reach, so concurrent sessions in one directory
    /// cannot consume each other's undos.
    pub fn last_restore_target(&self, thread_id: &str) -> Result<Option<RestoreTarget>> {
        Ok(self
            .turns
            .last_restore(thread_id)?
            .map(|record| RestoreTarget {
                manifest_id: record.safety_manifest_id,
            }))
    }

    /// Whether `path` on disk is still what `entry` recorded.
    ///
    /// Content **and** mode, because `plan_restore` treats a mode difference
    /// as a write — so comparing content alone let another session's
    /// `chmod +x` be reverted by an undo and never reported (C7, III.5). The
    /// crate re-reads mode even on a stat-cache hit for the same reason:
    /// chmod does not move mtime.
    ///
    /// Content is hashed rather than compared by fingerprint. The fast path
    /// can miss a same-length rewrite inside one timestamp tick, and here a
    /// false "unchanged" means overwriting someone's work.
    fn still_matches(path: &str, entry: &crate::manifest::FileEntry) -> bool {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return false;
        };
        if let (Some(recorded), Some(now)) = (entry.mode, crate::manifest::mode_of(&meta))
            && recorded != now
        {
            return false;
        }
        fs::read(path)
            .map(|bytes| BlobStore::hash_bytes(&bytes) == entry.hash)
            .unwrap_or(false)
    }

    /// Paths that have moved since the restore `thread_id` would undo.
    ///
    /// A rewind records the state it left the workspace in. If a path no
    /// longer matches that state, something else changed it after the fact —
    /// another session sharing the directory, or the user's own editor — and
    /// undoing would overwrite that change without ever mentioning it. The
    /// files are shared even though the undo records are not, so this is the
    /// only thing standing between a concurrent edit and silent loss.
    ///
    /// Contents are hashed rather than compared by stat fingerprint. The fast
    /// path can miss a same-length rewrite inside one timestamp tick, and here
    /// a false "unchanged" means overwriting someone's work — the opposite of
    /// what this is for. The set being checked is small: only what the restore
    /// would touch.
    ///
    /// Empty when there is nothing to undo, or when nothing has moved.
    pub fn undo_conflicts(&self, thread_id: &str, rules: &Gitignore) -> Result<Vec<String>> {
        let Some(record) = self.turns.last_restore(thread_id)? else {
            return Ok(Vec::new());
        };
        let expected = self.manifests.load(&record.target_manifest_id)?;

        let mut moved = Vec::new();
        for (path, entry) in &expected.entries {
            if is_protected(rules, path) {
                continue;
            }
            if !Self::still_matches(path, entry) {
                moved.push(path.clone());
            }
        }
        // A path the rewind removed, which is back: someone recreated it.
        for path in &expected.absent {
            if !is_protected(rules, path) && Path::new(path).exists() {
                moved.push(path.clone());
            }
        }

        // The target only knows the paths the rewind had an opinion about.
        // An undo restores the whole safety manifest, which is wider — it was
        // captured from the performing thread's scope, so it holds paths the
        // rewind left untouched. For those the target cannot say what the
        // rewind left, and comparing against it silently checks nothing.
        //
        // The safety manifest can, and for exactly these paths it is the
        // right thing to compare against: the rewind did not touch them, so
        // what the capture found is *also* what the rewind left — and it is
        // simultaneously what the undo is about to write over.
        let restoring = self.manifests.load(&record.safety_manifest_id)?;
        for (path, entry) in &restoring.entries {
            if is_protected(rules, path)
                || expected.entries.contains_key(path)
                || expected.absent.contains(path)
            {
                continue;
            }
            if !Self::still_matches(path, entry) {
                moved.push(path.clone());
            }
        }

        // And what the undo will *delete*, which the target likewise cannot
        // tell us: only the safety manifest knows a file existing now was
        // absent then and is about to be removed.
        //
        // Same guard as above, and it is load-bearing. Without it, a path the
        // rewind *recreated* — absent at the safety capture, present now
        // because the rewind put it back — was reported as a conflict on
        // every ordinary round trip. That is the undo doing exactly what it
        // is for, and calling it a conflict trains the reader to ignore the
        // warning, which is worse than not warning at all. When the target
        // has an opinion about the path, the loops above have already
        // compared against it.
        for path in &restoring.absent {
            if is_protected(rules, path)
                || expected.entries.contains_key(path)
                || expected.absent.contains(path)
            {
                continue;
            }
            if Path::new(path).exists() {
                moved.push(path.clone());
            }
        }

        moved.sort();
        moved.dedup();
        Ok(moved)
    }

    /// Union of every path key observed across the thread's manifests.
    /// Used to build the safety-checkpoint scope for a restore: it covers
    /// outside-workspace paths recorded via pre-edit attach that a plain
    /// workspace scan would miss.
    pub fn tracked_paths(&self, thread_id: &str) -> Result<std::collections::BTreeSet<String>> {
        let mut out = std::collections::BTreeSet::new();
        for (_, manifest) in self.thread_history(thread_id)? {
            out.extend(manifest.entries.into_keys());
            // Tombstoned paths count as observed too. They are precisely the
            // ones a restore may need to remove, and a path the safety scope
            // misses is a path the plan can never delete.
            out.extend(manifest.absent);
        }
        // Every path this session ever declared, window or not. The window
        // governs what future captures *watch*; the safety scope has to look
        // at everything ever observed, or a plan can never remove it again.
        out.extend(
            self.declared
                .all(thread_id)?
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned()),
        );
        Ok(out)
    }

    /// Fork inheritance: copy the source session's log entries up
    /// to and including the **last** entry for `through_turn_id` into
    /// `new_thread_id`'s log. Creates the new log if missing (which also
    /// marks the forked thread as tracking). Manifests are shared, not
    /// copied — GC marks from every log. Returns the number of entries
    /// inherited; 0 if the source has no entry for that turn.
    pub fn inherit_log(
        &self,
        source_thread_id: &str,
        new_thread_id: &str,
        through_turn_id: &str,
    ) -> Result<usize> {
        crate::id::validate_external("session id", source_thread_id)?;
        crate::id::validate_external("session id", new_thread_id)?;
        crate::id::validate_external("turn id", through_turn_id)?;
        // The *destination* is what this writes; the source is only read, and
        // locking both would be a wider lock than D18 allows — and a deadlock
        // the moment two forks cross.
        let _guard = self.lock_session(new_thread_id)?;
        let log = self.refs.load(source_thread_id)?;
        let Some(cut) = log
            .entries
            .iter()
            .rposition(|entry| entry.turn_id == through_turn_id)
        else {
            self.refs.ensure(new_thread_id)?;
            return Ok(0);
        };
        self.refs.ensure(new_thread_id)?;
        // Re-chained rather than copied verbatim: the inherited entries are
        // new entries in a new log, and a chain that carried its parent's
        // links would describe a history this log does not have.
        for entry in &log.entries[..=cut] {
            self.refs.append(
                new_thread_id,
                entry.turn_id.clone(),
                entry.manifest_id.clone(),
            )?;
        }
        Ok(cut + 1)
    }

    /// The thread's snapshot log with each manifest loaded, in capture order.
    pub fn thread_history(&self, thread_id: &str) -> Result<Vec<(SnapshotRef, Manifest)>> {
        let log = self.refs.load(thread_id)?;
        let mut out = Vec::with_capacity(log.entries.len());
        for entry in log.entries {
            let manifest = self.manifests.load(&entry.manifest_id)?;
            out.push((entry, manifest));
        }
        Ok(out)
    }

    /// Load a manifest by id.
    pub fn manifest(&self, manifest_id: &str) -> Result<Manifest> {
        self.manifests.load(manifest_id)
    }

    pub fn latest_manifest(&self, thread_id: &str) -> Result<Option<Manifest>> {
        let log = self.refs.load(thread_id)?;
        match log.entries.last() {
            Some(entry) => Ok(Some(self.manifests.load(&entry.manifest_id)?)),
            None => Ok(None),
        }
    }

    /// Restore `session_id`'s tracked state to `target`.
    ///
    /// `current_files` is the present tracked set — it is re-captured as the
    /// safety checkpoint first, so the restore is reversible. `rules`
    /// is the symmetric-ignore predicate over manifest path keys, evaluated
    /// against the *current* ignore rules, so newly ignoring a path protects
    /// it retroactively.
    pub fn restore_to(
        &self,
        session_id: &str,
        target: &RestoreTarget,
        kind: RestoreKind<'_>,
        current_files: impl IntoIterator<Item = PathBuf>,
        rules: &Gitignore,
    ) -> Result<RestoreOutcome> {
        let thread_id = session_id;
        // 1. Capture what is about to be replaced, so this restore can be
        // undone. The checkpoint is appended to the session doing the work;
        // where the *undo record* is filed is a separate question, answered
        // by `kind` below.
        //
        // Whatever the caller scanned, the target's own paths are added to
        // it. This makes the safety capture sufficient *by construction*
        // rather than by argument: a plan can only write `target.entries`
        // and only delete `target.absent`, so observing both means every
        // path the plan could touch has been looked at.
        //
        // Without it the sufficiency rested on the caller passing the
        // thread's whole history, which silently fails for an undo — the
        // undo record is filed under the thread the rewind switched *to*,
        // and `inherit_log` copies only up to the fork turn, so that
        // thread's history contains neither the safety manifest nor
        // anything the source learned afterwards. A path recorded absent by
        // the safety capture and put back on disk by something other than
        // the rewind (a shell command, an editor, another session) was then
        // in `target.absent` but missing from `current.entries`, and
        // `plan_restore` needs both to delete. It survived the undo in
        // silence.
        // Held for the whole restore: the safety capture appends to this
        // session's log, and the undo record and its `ensure` write below.
        //
        // It does **not** reach a destination that is not the performer.
        // D26 records that and accepts it: a forking host creates that
        // session immediately before the call and nothing else is using it
        // yet. Locking both would need a canonical order to avoid two
        // crossing restores deadlocking, for a case that does not arise.
        let _guard = self.lock_session(thread_id)?;
        let target_manifest = self.manifests.load(&target.manifest_id)?;
        let observed = current_files
            .into_iter()
            .chain(target_manifest.entries.keys().map(PathBuf::from))
            .chain(target_manifest.absent.iter().map(PathBuf::from));
        let safety = self.checkpoint_internal(
            thread_id,
            &format!("{SAFETY_TURN_PREFIX}{}", target.manifest_id),
            observed,
        )?;

        // 2. Compare the two states directly. Nothing here consults a
        // session's history, so the outcome depends only on where the
        // workspace is and where it is going.
        let current = self.manifests.load(&safety.id)?;
        let plan = plan_restore(&target_manifest, &current, rules);
        let stats = apply_plan(&self.blobs, &plan);

        // The undo record is filed under the session this hands the workspace
        // to. A rewind names the branch it creates; an undo names itself,
        // since that is where the record it spends was filed. A rewind with
        // no destination — restarting rather than branching — records
        // nothing and is therefore not undoable.
        match kind {
            RestoreKind::Rewind {
                undo_for: Some(owner),
            } => {
                // The destination is tracking from this moment: it holds the
                // workspace and it has an undo to spend. Without a log,
                // "no log" and "orphaned undo record" are the same state on
                // disk, and collection removed a record a live session could
                // still have spent — with its two manifests, if it was their
                // last root.
                self.refs.ensure(owner)?;
                self.turns.push_restore(
                    owner,
                    RestoreRecord {
                        target_manifest_id: target.manifest_id.clone(),
                        safety_manifest_id: safety.id.clone(),
                    },
                )?;
            }
            RestoreKind::Rewind { undo_for: None } => {}
            RestoreKind::Undo { spending } => {
                self.turns.pop_restore(spending)?;
            }
        }

        Ok(RestoreOutcome {
            safety: RestoreTarget {
                manifest_id: safety.id,
            },
            plan,
            stats,
        })
    }

    /// Drop a thread's snapshot log (its data becomes garbage for `gc`).
    fn remove_thread(&self, thread_id: &str) -> Result<()> {
        self.refs.remove(thread_id)
    }

    /// Paths this thread has observed that `target` has no opinion about and
    /// that lie outside `roots` — so a restore to it will leave them exactly
    /// as they are, whatever the user expected.
    ///
    /// Tracking is *discovered*, not retroactive. Files under a root are
    /// enumerated at every checkpoint, so every turn knows about them and
    /// rewinding further back restores strictly more. Files outside one
    /// arrive only when the edit hook first touches them, which means an
    /// earlier turn genuinely has no record of a file a later turn does —
    /// and rewinding *further back* then restores *less*, which is the
    /// opposite of what anyone expects.
    ///
    /// Leaving them alone is the right call: this turn has no content to
    /// write, and inventing some from a later turn's pre-image would put
    /// bytes of unknown provenance on disk. But it is invisible, so callers
    /// use this to say so before the user commits to it.
    pub fn unrestorable_outside(
        &self,
        thread_id: &str,
        target: &RestoreTarget,
        roots: &[PathBuf],
    ) -> Result<Vec<String>> {
        let manifest = self.manifests.load(&target.manifest_id)?;
        let mut out: Vec<String> = self
            .tracked_paths(thread_id)?
            .into_iter()
            .filter(|path| !manifest.entries.contains_key(path) && !manifest.absent.contains(path))
            .filter(|path| {
                let path = Path::new(path);
                !roots.iter().any(|root| path.starts_with(root))
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// Also delete the undo records filed under `thread_id`, which
    /// `remove_thread` deliberately leaves alone — a thread's log and the
    /// restores handed *to* it are separate lifetimes, and only deleting the
    /// conversation ends both.
    fn remove_restores(&self, thread_id: &str) -> Result<()> {
        self.turns.remove_restores(thread_id)
    }

    /// Bytes this workspace's **records** occupy.
    ///
    /// Not the content they name: blobs are shared with every other
    /// workspace, so attributing them to one would double-count the same
    /// bytes as many times as they are referenced. A dashboard reports the
    /// two separately for the same reason (D34).
    /// Every session this workspace holds records for.
    pub fn sessions(&self) -> Result<Vec<String>> {
        self.refs.thread_ids()
    }

    pub fn records_disk_usage(&self) -> Result<u64> {
        dir_size(&self.partition).map_err(|e| SnapshotError::io(&self.partition, e))
    }
}

fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

/// A session's records, gathered before any of them is unlinked.
#[derive(Debug, Default)]
struct SessionRecords {
    turns: std::collections::BTreeSet<String>,
    manifests: std::collections::BTreeSet<String>,
}

/// What a delete did, and what it declined to touch.
#[derive(Debug, Default)]
pub struct DeleteOutcome {
    /// Records reclaimed. Content is not counted here because delete does
    /// not reclaim content — see [`WorkspaceStore::delete_sessions`].
    pub reclaimed: GcStats,
    /// Sessions left **exactly as they were**, with why. Their records are
    /// intact and the call can be retried once the cause is addressed.
    ///
    /// Every pair here really is a session. The reclamation pass used to
    /// report its own failure as a `"<sweep>"` entry, which made the list a
    /// mix of two different things and gave that pseudo-session a name a real
    /// one could collide with.
    pub refused: Vec<(String, SnapshotError)>,
    /// Sessions whose removal **began and did not finish**, with why.
    ///
    /// Distinct from `refused`, which promises the session is untouched. The
    /// two used to be one list, so a failure partway through the unlinks was
    /// reported as "left exactly as it was" — the one thing it was not.
    /// Retrying is still the right move; delete is idempotent, and the
    /// ordering means the intermediate state is one the system reaches on its
    /// own (D11).
    pub incomplete: Vec<(String, SnapshotError)>,
    /// Why reclamation did not run, if it did not.
    ///
    /// Independent of `refused`: the sessions are unreachable either way —
    /// that is what delete promised and it is already done — and the bytes
    /// simply wait for the next collection (VIII.3).
    pub sweep_error: Option<SnapshotError>,
}

impl WorkspaceStore {
    /// Delete these sessions from this workspace, and reclaim the records
    /// nothing references any more.
    ///
    /// **What this promises:** afterwards the session is unreachable and its
    /// records are gone. **What it does not:** free the bytes its captures
    /// held. Content is deduplicated across every workspace, so "is anyone
    /// else still using this blob" is a question only a whole-store sweep can
    /// answer — [`crate::collect_garbage`] reclaims content, this reclaims
    /// records, and neither waits on the other.
    ///
    /// **A session whose log cannot be read is refused, not half-deleted.**
    /// Swallowing that read and deleting the log anyway destroys the only
    /// record of what the session held, so nothing can be reclaimed and
    /// nothing can be retried — a delete that reports success having done
    /// neither. One unreadable session does not block the rest.
    ///
    /// Takes the whole set at once because a session's manifests are
    /// routinely shared with the siblings being deleted alongside it: swept
    /// one at a time, each would still be pinned by the next.
    /// Everything a session's records name, read before any of them is
    /// removed. Afterwards there is no way to learn it.
    fn records_of(&self, session_id: &str) -> Result<SessionRecords> {
        let log = self.refs.load(session_id)?;
        let mut records = SessionRecords::default();
        // The **whole** undo stack, not just its top. Up to 20 records live
        // there, and reading one left the other 19's manifests out of the
        // doomed set entirely — reclaimed eventually by collection, but never
        // by the delete that was supposed to own them.
        for record in self.turns.restore_records(session_id)? {
            records.manifests.insert(record.target_manifest_id);
            records.manifests.insert(record.safety_manifest_id);
        }
        for entry in log.entries {
            records
                .turns
                .insert(crate::refs::turn_file_name(&entry.turn_id));
            records.manifests.insert(entry.manifest_id);
        }
        Ok(records)
    }

    fn remove_declared(&self, session_id: &str) -> Result<()> {
        self.declared.remove(session_id)
    }

    pub fn delete_sessions(&self, session_ids: &[String]) -> DeleteOutcome {
        let mut outcome = DeleteOutcome::default();
        if session_ids.is_empty() {
            return outcome;
        }

        // What the doomed sessions named, gathered before their logs are
        // unlinked — afterwards there is no way to learn it. This is also the
        // whole of what the prune below is permitted to remove: delete
        // enumerates nothing, so it can never reach a record belonging to a
        // session it was not asked about (D10, C12).
        let mut doomed_turns = std::collections::BTreeSet::new();
        let mut doomed_manifests = std::collections::BTreeSet::new();

        // Read everything first, and refuse before touching anything. A
        // session that fails partway through its unlinks is neither deleted
        // nor "left exactly as it was", which is what `refused` promises —
        // and the only honest way to keep that promise is to have done no
        // work by the time the promise is made.
        let mut doomed = Vec::new();
        for session_id in session_ids {
            match self.records_of(session_id) {
                Ok(records) => doomed.push((session_id, records)),
                Err(err) => outcome.refused.push((session_id.clone(), err)),
            }
        }

        for (session_id, records) in doomed {
            // Per session, inside the loop. Locking the whole batch first
            // would make one busy session block the rest, and would be a
            // wider lock than D18 allows the moment the batch is two.
            let guard = match self.lock_session(session_id) {
                Ok(guard) => guard,
                Err(err) => {
                    outcome.refused.push((session_id.clone(), err));
                    continue;
                }
            };
            doomed_turns.extend(records.turns);
            doomed_manifests.extend(records.manifests);

            // Undo records first, then the log. Interrupted between the two,
            // this leaves a session that simply never rewound — a state the
            // system reaches on its own and cannot tell apart from the
            // ordinary case. The other order leaves an undo record for a
            // session with no log, which pins its manifests as a root.
            //
            // A failure here is reported separately: the session is *not*
            // intact, so calling it refused would be a lie.
            for step in [
                Self::remove_declared as fn(&Self, &str) -> Result<()>,
                Self::remove_restores,
                Self::remove_thread,
            ] {
                if let Err(err) = step(self, session_id) {
                    outcome.incomplete.push((session_id.clone(), err));
                    break;
                }
            }
            drop(guard);
        }

        match crate::sweep::prune_sessions(
            &self.refs,
            &self.turns,
            &self.manifests,
            &doomed_turns,
            &doomed_manifests,
        ) {
            Ok(stats) => outcome.reclaimed = stats,
            Err(err) => outcome.sweep_error = Some(err),
        }
        outcome
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
