//! Per-session snapshot logs and garbage collection.
//!
//! Each thread has an append-only log of `(turn_id, manifest_id)` refs.
//! Snapshot lifetime is tied to thread lifetime: removing a thread drops
//! its refs, and a mark-and-sweep pass then deletes unreferenced
//! manifests and blobs. No timers, no retention windows.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Result;
use crate::error::SnapshotError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRef {
    pub turn_id: String,
    pub manifest_id: String,
    /// When this entry was appended, in unix seconds.
    ///
    /// For display — a listing that shows only a sequence number and an
    /// external conversation id gives a person nothing to recognise. It takes
    /// part in no decision, so VIII.1's ban on age-based *behaviour* is
    /// untouched: that rule is about reclamation, not about showing a clock.
    pub at: u64,
    /// Hash of the preceding entry, or `None` for the first.
    ///
    /// **Written and never read.** No threat model on the table asks for
    /// tamper-evidence, so what a reader should do with a broken chain is not
    /// yet decided — and deciding it wrongly is worse than deferring, since
    /// refusing a log whose chain is broken can make a recoverable situation
    /// unrecoverable.
    ///
    /// It is written now because a chain only means anything if it starts at
    /// the first entry. Added later, every existing log stays permanently
    /// unchained before the cut, and "this is broken" becomes impossible to
    /// tell from "this predates the chain". Starting at entry zero keeps that
    /// distinction available for whenever it is wanted.
    ///
    /// This is not the defect VII.4 names. That rule is about a property
    /// *claimed* and not enforced; nothing here claims tamper-evidence, and
    /// unused data is not a false promise.
    pub prev_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadLog {
    /// Format version of this record — see [`crate::manifest::Manifest`].
    pub version: u32,
    pub entries: Vec<SnapshotRef>,
}

/// Every log a partition holds, and whether the enumeration was whole.
///
/// The flag exists so a sweep can tell "nothing points at this" from "I could
/// not read everything that might". Only the first licenses a removal.
#[derive(Debug, Default)]
pub struct ThreadLogs {
    pub logs: Vec<ThreadLog>,
    /// True when at least one log could not be read or parsed.
    pub incomplete: bool,
}

impl Default for ThreadLog {
    fn default() -> Self {
        Self {
            version: crate::workspace::FORMAT_VERSION,
            entries: Vec::new(),
        }
    }
}

impl SnapshotRef {
    /// Build an entry that chains onto `previous`.
    pub(crate) fn chained(turn_id: String, manifest_id: String, previous: Option<&Self>) -> Self {
        Self {
            turn_id,
            manifest_id,
            at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            prev_hash: previous.map(Self::digest),
        }
    }

    /// This entry's identity for the chain: the hash of its canonical JSON,
    /// which already includes its own `prev_hash` and so covers everything
    /// before it.
    fn digest(entry: &Self) -> String {
        serde_json::to_vec(entry).map_or_else(
            |_| String::new(),
            |bytes| crate::blob::BlobStore::hash_bytes(&bytes),
        )
    }
}

/// Refuse a record this build cannot read.
///
/// The same guard `Manifest::load` applies, shared by the four readers that
/// needed it. Refusing is the point: a record whose version we do not know
/// may mean anything, and the one thing it must not do is look like a record
/// that means something else. `RestoreLog`'s `entries` and `Manifest`'s
/// `absent` both deserialize an absent key as empty — which reads as "this
/// session never rewound" and "this capture looked for nothing", and the
/// second silently voids every deletion the record had licensed.
fn check_version(kind: &'static str, id: &str, found: u32) -> Result<()> {
    if found == crate::workspace::FORMAT_VERSION {
        return Ok(());
    }
    Err(SnapshotError::UnknownRecordVersion {
        kind,
        id: id.to_string(),
        found,
        supported: crate::workspace::FORMAT_VERSION,
    })
}

/// What a turn's index entry is suffixed with, so the directory can be read
/// by whitelist and `with_extension("tmp")` has an extension to replace
/// rather than eating the id's own last dot (D5, D9).
pub(crate) const TURN_SUFFIX: &str = ".turn";

pub struct RefStore {
    root: PathBuf,
}

impl RefStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| SnapshotError::io(&root, e))?;
        Ok(Self { root })
    }

    /// Append a `(turn_id, manifest_id)` pair, chained onto whatever is
    /// already there.
    ///
    /// Read-modify-write, which is why a session takes a lock against itself:
    /// two concurrent appends for one session would otherwise lose an entry,
    /// silently. A long-lived library could not hit that; a CLI invoked twice
    /// can.
    pub fn append(&self, thread_id: &str, turn_id: String, manifest_id: String) -> Result<()> {
        let mut log = self.load(thread_id)?;
        let entry = SnapshotRef::chained(turn_id, manifest_id, log.entries.last());
        log.entries.push(entry);
        let path = self.log_path(thread_id)?;
        let tmp = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(&log)?;
        fs::write(&tmp, bytes).map_err(|e| SnapshotError::io(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| SnapshotError::io(&path, e))?;
        Ok(())
    }

    /// Whether a log file exists for `thread_id`. With session-scoped
    /// binding, log existence *is* the persisted "tracking enabled" state.
    pub fn exists(&self, thread_id: &str) -> bool {
        self.log_path(thread_id).is_ok_and(|p| p.exists())
    }

    /// Create an empty log for `thread_id` if none exists — the durable
    /// marker that this session is tracking.
    pub fn ensure(&self, thread_id: &str) -> Result<()> {
        let path = self.log_path(thread_id)?;
        if path.exists() {
            return Ok(());
        }
        let tmp = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(&ThreadLog::default())?;
        fs::write(&tmp, bytes).map_err(|e| SnapshotError::io(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| SnapshotError::io(&path, e))?;
        Ok(())
    }

    /// Load a thread's log; a thread with no snapshots yields an empty log.
    pub fn load(&self, thread_id: &str) -> Result<ThreadLog> {
        let path = self.log_path(thread_id)?;
        match fs::read(&path) {
            Ok(bytes) => {
                let log: ThreadLog = serde_json::from_slice(&bytes)?;
                check_version("thread log", thread_id, log.version)?;
                Ok(log)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ThreadLog::default()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    /// Drop a thread's refs (its snapshots become garbage for the next GC).
    pub fn remove(&self, thread_id: &str) -> Result<()> {
        let path = self.log_path(thread_id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    /// Every thread log in this partition, and whether any could not be read.
    ///
    /// One damaged log used to abort the whole enumeration with `?`, which
    /// made every sweep — and therefore every delete's reclamation — hostage
    /// to a single corrupt file. It now reports what it could read and says
    /// that it is incomplete, so a caller can reclaim nothing rather than
    /// fail outright. Which it must: an unreadable log is not evidence that
    /// its manifests are dead.
    pub fn thread_logs(&self) -> Result<ThreadLogs> {
        let mut out = ThreadLogs::default();
        let entries = fs::read_dir(&self.root).map_err(|e| SnapshotError::io(&self.root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.root, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") {
                continue;
            }
            match fs::read(entry.path()).map(|b| serde_json::from_slice::<ThreadLog>(&b)) {
                // A version this build cannot read counts as unreadable, for
                // the same reason: it is not evidence that anything is dead.
                Ok(Ok(log)) if log.version == crate::workspace::FORMAT_VERSION => {
                    out.logs.push(log);
                }
                _ => out.incomplete = true,
            }
        }
        Ok(out)
    }

    /// Where `thread_id`'s log lives, once the id is proven able to be one.
    ///
    /// The id becomes the filename unchanged. It used to be character-mapped
    /// here — and mapped a second time, differently, when the turn index was
    /// built — so two spellings could land on one file and a sweep could
    /// disagree with a writer about which record was which (D7).
    fn log_path(&self, thread_id: &str) -> Result<PathBuf> {
        crate::id::validate_stored("session id", thread_id)?;
        Ok(self.root.join(format!("{thread_id}.json")))
    }
}

/// How many restores a thread remembers. Undo only needs the most recent
/// one; the rest is history that would otherwise keep manifests alive.
pub(crate) const MAX_RESTORE_HISTORY: usize = 20;

/// A restore that has been applied, recorded so it can be undone.
///
/// Filed under the thread the rewind switched **to** — the one the user is
/// sitting in when they ask to undo. Filing it under the thread that
/// *performed* the rewind would lose it immediately, since a rewind leaves
/// that thread behind; filing it per workspace makes concurrent sessions in
/// one directory share a stack, where one session's undo pops another's
/// record. The destination is the only key that is both reachable and
/// private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRecord {
    /// The state the rewind put the workspace into.
    pub target_manifest_id: String,
    /// What it looked like just before — where an undo returns to.
    ///
    /// Only the files are recorded here. The conversation an undo returns to
    /// is the thread the rewind superseded, which is archived rather than
    /// copied: archiving moves a rollout out of the sessions directory, and
    /// thread lookup only searches that directory, so an archived thread
    /// cannot be resumed or continued behind our back. Keeping the identity
    /// rather than a copy also preserves the turn ids the snapshot store is
    /// keyed on — a rebuilt conversation gets fresh ones, which would sever
    /// every later rewind on that line from its snapshots.
    pub safety_manifest_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreLog {
    /// Format version of this record — see [`crate::manifest::Manifest`].
    pub version: u32,
    pub entries: Vec<RestoreRecord>,
}

/// Hand-written rather than derived: `#[derive(Default)]` would write
/// `version: 0`, which is a version number no build has ever used and is
/// worse than having no field at all — a reader would refuse a record this
/// build itself wrote.
impl Default for RestoreLog {
    fn default() -> Self {
        Self {
            version: crate::workspace::FORMAT_VERSION,
            entries: Vec::new(),
        }
    }
}

/// Undo records across a partition, and whether any could not be read.
#[derive(Debug, Default)]
pub struct RestoreLogs {
    pub logs: Vec<RestoreLog>,
    pub incomplete: bool,
}

/// Manifests the turn index and undo records still name.
#[derive(Debug, Default)]
pub struct HeldManifests {
    pub ids: BTreeSet<String>,
    pub incomplete: bool,
}

/// Maps turn ids to the state captured at their start, and threads to
/// their restore history. Both are deliberately independent of any thread:
/// a fork inherits its parent's turn ids, so a turn resolves to the same
/// snapshot from every branch, and an undo works wherever the user happens
/// to be.
pub struct TurnIndex {
    turns_root: PathBuf,
    restores_root: PathBuf,
}

impl TurnIndex {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let turns_root = root.join("turns");
        let restores_root = root.join("restores");
        fs::create_dir_all(&turns_root).map_err(|e| SnapshotError::io(&turns_root, e))?;
        fs::create_dir_all(&restores_root).map_err(|e| SnapshotError::io(&restores_root, e))?;
        Ok(Self {
            turns_root,
            restores_root,
        })
    }

    /// Record the snapshot captured at `turn_id`'s start. Later writes win:
    /// a supplemental pre-edit attach extends that turn's capture.
    pub fn set_turn(&self, turn_id: &str, manifest_id: &str) -> Result<()> {
        let path = self.turn_path(turn_id)?;
        write_atomic(&path, manifest_id.as_bytes())
    }

    pub fn manifest_for_turn(&self, turn_id: &str) -> Result<Option<String>> {
        let path = self.turn_path(turn_id)?;
        match fs::read_to_string(&path) {
            Ok(id) => Ok(Some(id.trim().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    /// Manifests the turn index and the undo records still name, and whether
    /// either could be read whole. See [`crate::refs::ThreadLogs`].
    pub fn all_manifest_ids(&self) -> Result<HeldManifests> {
        let mut out = HeldManifests::default();
        let entries =
            fs::read_dir(&self.turns_root).map_err(|e| SnapshotError::io(&self.turns_root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.turns_root, e))?;
            // Whitelist, not a `.tmp` blacklist: a blacklist admits anything
            // nobody thought to exclude, which is how a half-written file
            // became readable as a record (D9, C4).
            if !entry.file_name().to_string_lossy().ends_with(TURN_SUFFIX) {
                continue;
            }
            match fs::read_to_string(entry.path()) {
                Ok(id) => {
                    out.ids.insert(id.trim().to_string());
                }
                Err(_) => out.incomplete = true,
            }
        }
        let logs = self.all_restore_logs()?;
        out.incomplete |= logs.incomplete;
        for log in logs.logs {
            for record in log.entries {
                out.ids.insert(record.target_manifest_id);
                out.ids.insert(record.safety_manifest_id);
            }
        }
        Ok(out)
    }

    /// Undo records filed under a session that has no log any more.
    ///
    /// Nothing can reach such a record to spend it, so it is not a root — but
    /// `all_manifest_ids` reads it as one, which pins its two manifests for
    /// good. Delete removes the pair together (D11); this finds the ones a
    /// crash left behind.
    pub fn orphan_restore_logs(&self, refs: &RefStore) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let entries = fs::read_dir(&self.restores_root)
            .map_err(|e| SnapshotError::io(&self.restores_root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.restores_root, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(thread_id) = name.strip_suffix(".json") else {
                continue;
            };
            // Grace-gated like every other reclamation: a rewind writes the
            // undo record under a session whose own log may be moments away.
            if !refs.exists(thread_id) && crate::sweep::settled(&entry.path()) {
                out.push(thread_id.to_string());
            }
        }
        Ok(out)
    }

    /// Remove one turn entry by its on-disk name, for the sessions a delete
    /// just removed. Missing is fine — delete is idempotent.
    ///
    /// Validated like every other path builder, and for the same reason as
    /// `blob_path`: the name is derived from a `turn_id` read back out of a
    /// log this build deserialized but never vetted, so a forged or corrupted
    /// entry could otherwise aim `remove_file` outside the partition (D5).
    pub fn remove_turn_file(&self, turn_file: &str) -> Result<()> {
        let Some(turn_id) = turn_file.strip_suffix(TURN_SUFFIX) else {
            return Err(SnapshotError::InvalidId {
                kind: "turn record",
                id: turn_file.to_string(),
                reason: "a turn record's name must end in `.turn`",
            });
        };
        crate::id::validate_stored("turn id", turn_id)?;
        let path = self.turns_root.join(turn_file);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    pub fn push_restore(&self, thread_id: &str, record: RestoreRecord) -> Result<()> {
        let mut log = self.restore_log(thread_id)?;
        log.entries.push(record);
        // Undo reaches back one step, so a long tail only pins storage.
        if log.entries.len() > MAX_RESTORE_HISTORY {
            let excess = log.entries.len() - MAX_RESTORE_HISTORY;
            log.entries.drain(..excess);
        }
        let path = self.restore_path(thread_id)?;
        write_atomic(&path, &serde_json::to_vec_pretty(&log)?)
    }

    /// Every undo record this thread holds, oldest first.
    ///
    /// Delete needs all of them: reading only the top left the manifests
    /// named by the other nineteen out of the doomed set, so the delete that
    /// owned them never reclaimed them.
    pub fn restore_records(&self, thread_id: &str) -> Result<Vec<RestoreRecord>> {
        Ok(self.restore_log(thread_id)?.entries)
    }

    /// The most recent restore this thread has to undo, if any.
    pub fn last_restore(&self, thread_id: &str) -> Result<Option<RestoreRecord>> {
        Ok(self.restore_log(thread_id)?.entries.pop())
    }

    /// Discard the most recent restore record, because it has just been
    /// reversed. The log is a stack: rewinds push, undos pop, so repeated
    /// undos walk back through repeated rewinds instead of oscillating
    /// between the last two states.
    pub fn pop_restore(&self, thread_id: &str) -> Result<Option<RestoreRecord>> {
        let mut log = self.restore_log(thread_id)?;
        let popped = log.entries.pop();
        if popped.is_some() {
            let path = self.restore_path(thread_id)?;
            write_atomic(&path, &serde_json::to_vec_pretty(&log)?)?;
        }
        Ok(popped)
    }

    /// Drop a thread's undo records outright, for when the conversation they
    /// belong to is deleted.
    ///
    /// Separate from `RefStore::remove` because the two files answer to
    /// different lifetimes: a thread's log is what it captured, its restore
    /// log is what was handed *to* it, and a rewind writes the second under a
    /// thread that may have no log of its own yet. Only deleting the
    /// conversation ends both — and leaving this behind would strand a GC
    /// root, pinning the manifests it names for good.
    pub fn remove_restores(&self, thread_id: &str) -> Result<()> {
        let path = self.restore_path(thread_id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    fn restore_log(&self, thread_id: &str) -> Result<RestoreLog> {
        let path = self.restore_path(thread_id)?;
        match fs::read(&path) {
            Ok(bytes) => {
                let log: RestoreLog = serde_json::from_slice(&bytes)?;
                check_version("restore log", thread_id, log.version)?;
                Ok(log)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RestoreLog::default()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    fn all_restore_logs(&self) -> Result<RestoreLogs> {
        let mut out = RestoreLogs::default();
        let entries = fs::read_dir(&self.restores_root)
            .map_err(|e| SnapshotError::io(&self.restores_root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.restores_root, e))?;
            if !entry.file_name().to_string_lossy().ends_with(".json") {
                continue;
            }
            // An undo record that will not parse used to be skipped in
            // silence, which reads to a sweep as "this holds nothing" — and
            // its two manifests then look dead. Say so instead.
            match fs::read(entry.path()).map(|b| serde_json::from_slice::<RestoreLog>(&b)) {
                Ok(Ok(log)) if log.version == crate::workspace::FORMAT_VERSION => {
                    out.logs.push(log);
                }
                _ => out.incomplete = true,
            }
        }
        Ok(out)
    }

    /// Drop index entries for turns no thread holds any more.
    ///
    /// **Grace-gated, like every other reclamation.** A capture writes its
    /// log entry before its turn file, so between those two writes a live
    /// session's turn exists on disk while no log names it yet. Without the
    /// window this unlinked it, and nothing ever rebuilds a turn entry —
    /// `set_turn` runs only at capture time — so the rewind was lost
    /// permanently, in a session nobody deleted (C12).
    ///
    /// `.tmp` residue is skipped here and reclaimed by
    /// [`crate::sweep::sweep_residue`]; unlinking a half-written record
    /// mid-rename is not this pass's job.
    pub fn retain_turns(&self, live_turn_ids: &BTreeSet<String>) -> Result<()> {
        let entries =
            fs::read_dir(&self.turns_root).map_err(|e| SnapshotError::io(&self.turns_root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.turns_root, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(TURN_SUFFIX)
                || live_turn_ids.contains(&name)
                || !crate::sweep::settled(&entry.path())
            {
                continue;
            }
            fs::remove_file(entry.path()).map_err(|e| SnapshotError::io(entry.path(), e))?;
        }
        Ok(())
    }

    /// Where `turn_id`'s index entry lives.
    ///
    /// The `.turn` suffix is not decoration. Without an extension,
    /// `write_atomic`'s `with_extension("tmp")` truncated at the last dot, so
    /// `v1.2` and `v1.9` shared the temporary path `v1.tmp` — and dots in an
    /// external conversation id are ordinary. Two concurrent writes could
    /// leave one turn pointing at another turn's manifest. The suffix also
    /// makes the whitelist D9 asks for possible here at all.
    fn turn_path(&self, turn_id: &str) -> Result<PathBuf> {
        crate::id::validate_stored("turn id", turn_id)?;
        Ok(self.turns_root.join(format!("{turn_id}{TURN_SUFFIX}")))
    }

    fn restore_path(&self, thread_id: &str) -> Result<PathBuf> {
        crate::id::validate_stored("session id", thread_id)?;
        Ok(self.restores_root.join(format!("{thread_id}.json")))
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|e| SnapshotError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| SnapshotError::io(path, e))
}

/// Thread and turn ids are UUID-like; anything else is mapped to a
/// filename-safe character set.
/// What a turn's index entry is named, given an id already proven storable.
pub(crate) fn turn_file_name(turn_id: &str) -> String {
    format!("{turn_id}{TURN_SUFFIX}")
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcStats {
    pub manifests_kept: usize,
    pub manifests_removed: usize,
    pub blobs_kept: usize,
    pub blobs_removed: usize,
}

impl GcStats {
    /// Combine two sweeps, for a collection that walks several partitions.
    pub(crate) fn plus(self, other: Self) -> Self {
        Self {
            manifests_kept: self.manifests_kept + other.manifests_kept,
            manifests_removed: self.manifests_removed + other.manifests_removed,
            blobs_kept: self.blobs_kept + other.blobs_kept,
            blobs_removed: self.blobs_removed + other.blobs_removed,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn append_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let refs = RefStore::open(dir.path().join("refs")).unwrap();

        assert_eq!(refs.load("t1").unwrap(), ThreadLog::default());
        refs.append("t1", "turn-1".into(), "m1".into()).unwrap();
        refs.append("t1", "turn-2".into(), "m2".into()).unwrap();

        let log = refs.load("t1").unwrap();
        assert_eq!(log.entries.len(), 2);
        assert_eq!(log.entries[1].manifest_id, "m2");
    }

    /// A log that will not parse is reported as such rather than aborting the
    /// enumeration, so one damaged file cannot hold every sweep hostage.
    #[test]
    fn an_unreadable_log_makes_the_enumeration_incomplete_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("refs");
        let refs = RefStore::open(&root).unwrap();
        refs.append("good", "turn-1".into(), "m1".into()).unwrap();
        fs::write(root.join("bad.json"), b"{ truncated").unwrap();

        let logs = refs.thread_logs().unwrap();
        assert_eq!(logs.logs.len(), 1, "the readable one still comes back");
        assert!(logs.incomplete);
    }

    /// The chain runs unbroken from the first entry.
    ///
    /// That is the whole reason it is written now rather than when something
    /// needs it: a chain begun partway leaves everything before the cut
    /// permanently unlinked, and "this is broken" stops being distinguishable
    /// from "this predates the chain". Nothing reads it yet, so what is under
    /// test is that the data will be there and complete when something does.
    #[test]
    fn the_chain_starts_at_the_first_entry_and_never_breaks() {
        let dir = tempfile::tempdir().unwrap();
        let refs = RefStore::open(dir.path()).unwrap();

        for i in 0..4 {
            refs.append("t1", format!("turn-{i}"), format!("manifest-{i}"))
                .unwrap();
        }

        let log = refs.load("t1").unwrap();
        assert_eq!(log.entries.len(), 4);
        assert_eq!(
            log.entries[0].prev_hash, None,
            "the first entry has nothing behind it"
        );
        for pair in log.entries.windows(2) {
            assert_eq!(
                pair[1].prev_hash.as_deref(),
                Some(SnapshotRef::digest(&pair[0]).as_str()),
                "each entry names the one before it"
            );
        }
    }

    /// A timestamp for display, and nothing decides anything by it.
    #[test]
    fn entries_carry_the_time_they_were_appended() {
        let dir = tempfile::tempdir().unwrap();
        let refs = RefStore::open(dir.path()).unwrap();
        let before = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        refs.append("t1", "turn-1".into(), "m1".into()).unwrap();

        let at = refs.load("t1").unwrap().entries[0].at;
        assert!(at >= before, "recorded at least when we started");
    }

    /// An inherited log is re-chained rather than copied, because the entries
    /// are new entries in a new log: a chain carrying its parent's links
    /// would describe a history this log does not have.
    #[test]
    fn inherited_entries_are_rechained() {
        let dir = tempfile::tempdir().unwrap();
        let refs = RefStore::open(dir.path()).unwrap();
        refs.append("src", "turn-1".into(), "m1".into()).unwrap();
        refs.append("src", "turn-2".into(), "m2".into()).unwrap();

        let source = refs.load("src").unwrap();
        for entry in &source.entries {
            refs.append("fork", entry.turn_id.clone(), entry.manifest_id.clone())
                .unwrap();
        }

        let fork = refs.load("fork").unwrap();
        assert_eq!(fork.entries[0].prev_hash, None);
        assert_eq!(
            fork.entries[1].prev_hash.as_deref(),
            Some(SnapshotRef::digest(&fork.entries[0]).as_str())
        );
    }

    /// `last_restore` peeks and `pop_restore` pops.
    ///
    /// If the peek mutated, asking what an undo would do would consume the
    /// answer — the conflict check reads the same record before the undo
    /// runs, so undoing would then reverse the rewind *before* the one the
    /// user was shown.
    #[test]
    fn reading_the_top_of_the_undo_stack_does_not_consume_it() {
        let dir = tempfile::tempdir().unwrap();
        let turns = TurnIndex::open(dir.path()).unwrap();

        let record = |n: &str| RestoreRecord {
            target_manifest_id: format!("target-{n}"),
            safety_manifest_id: format!("safety-{n}"),
        };
        turns.push_restore("t1", record("a")).unwrap();
        turns.push_restore("t1", record("b")).unwrap();

        assert_eq!(turns.last_restore("t1").unwrap(), Some(record("b")));
        assert_eq!(
            turns.last_restore("t1").unwrap(),
            Some(record("b")),
            "reading twice reads the same thing"
        );

        assert_eq!(turns.pop_restore("t1").unwrap(), Some(record("b")));
        assert_eq!(
            turns.last_restore("t1").unwrap(),
            Some(record("a")),
            "a second undo walks back another rewind rather than oscillating"
        );

        assert_eq!(turns.pop_restore("t1").unwrap(), Some(record("a")));
        assert_eq!(turns.pop_restore("t1").unwrap(), None);
    }

    /// A record from a build we do not understand is refused, not read
    /// optimistically.
    ///
    /// The hazard is specific: both `ThreadLog::entries` and
    /// `RestoreLog::entries` deserialize a missing key as empty, so a record
    /// written by a build that spelled them differently would come back as a
    /// session that captured nothing and never rewound — losing history
    /// silently rather than loudly.
    #[test]
    fn a_record_from_an_unknown_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("refs");
        let refs = RefStore::open(&root).unwrap();
        refs.append("t1", "turn-1".into(), "m1".into()).unwrap();

        let raw = fs::read_to_string(root.join("t1.json")).unwrap();
        fs::write(
            root.join("t1.json"),
            raw.replace("\"version\": 1", "\"version\": 99"),
        )
        .unwrap();

        let err = refs.load("t1").unwrap_err();
        assert!(
            matches!(
                &err,
                SnapshotError::UnknownRecordVersion { kind, found: 99, .. } if *kind == "thread log"
            ),
            "{err:?}"
        );

        // And a sweep treats it as unreadable rather than as an empty log,
        // so nothing it names is mistaken for garbage.
        assert!(refs.thread_logs().unwrap().incomplete);
    }

    /// The undo record carries a version too, and refuses the same way.
    #[test]
    fn an_undo_record_from_an_unknown_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let turns = TurnIndex::open(dir.path()).unwrap();
        turns
            .push_restore(
                "t1",
                RestoreRecord {
                    target_manifest_id: "target".into(),
                    safety_manifest_id: "safety".into(),
                },
            )
            .unwrap();

        let path = dir.path().join("restores").join("t1.json");
        let raw = fs::read_to_string(&path).unwrap();
        fs::write(&path, raw.replace("\"version\": 1", "\"version\": 99")).unwrap();

        let err = turns.last_restore("t1").unwrap_err();
        assert!(
            matches!(
                &err,
                SnapshotError::UnknownRecordVersion { kind, found: 99, .. } if *kind == "restore log"
            ),
            "{err:?}"
        );
    }

    /// A turn id read back out of a log is not a proven id.
    ///
    /// Every other path builder validates; this one did not, and its input is
    /// derived from a `ThreadLog` this build deserialized but never vetted.
    /// A forged or corrupted entry could therefore aim `remove_file` outside
    /// the partition — the same threat `blob_path` already guarded against
    /// (D5).
    #[test]
    fn a_forged_turn_record_name_cannot_escape_the_turns_directory() {
        let dir = tempfile::tempdir().unwrap();
        let turns = TurnIndex::open(dir.path()).unwrap();
        let outside = dir.path().join("witness.txt");
        fs::write(&outside, b"not ours to remove").unwrap();

        for forged in [
            "../witness.txt",
            "../../etc/passwd.turn",
            "..",
            "",
            "no-suffix",
        ] {
            let err = turns.remove_turn_file(forged).unwrap_err();
            assert!(
                matches!(err, SnapshotError::InvalidId { .. }),
                "{forged:?} was not refused: {err:?}"
            );
        }
        assert!(
            outside.exists(),
            "a forged name reached outside the partition"
        );

        // A real one still works, or the guard would be useless.
        turns.set_turn("turn-1", "m1").unwrap();
        turns.remove_turn_file(&turn_file_name("turn-1")).unwrap();
        assert_eq!(turns.manifest_for_turn("turn-1").unwrap(), None);
    }
}
