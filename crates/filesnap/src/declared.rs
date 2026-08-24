//! Paths the edit API has declared, kept across process boundaries.
//!
//! A path reaches the tracked set two ways: a scan finds it, or an edit
//! declares it. The second used to live only in memory, so a session that
//! resumed in a new process stopped watching everything it had edited — and
//! silently, since the manifests already written stayed perfectly valid. What
//! was lost was *future* observation.
//!
//! # Why accumulate at all
//!
//! Claude Code accumulates and never removes: `trackedFiles` is a set whose
//! only incremental mutation in its entire tree is `.add()`, and each per-turn
//! snapshot re-stats every file ever edited. That produces an asymmetry worth
//! copying — **a shell command that modifies an already-tracked file is
//! caught; one that touches a file the edit tools never saw is invisible.**
//! Accumulation is what makes the first half true (D25).
//!
//! # Why bound it
//!
//! Claude Code pays for accumulation with a snapshot cost that grows with
//! session length, and it carries no second and third partition on top of it.
//! A path drops out after [`DECLARED_WINDOW_TURNS`] turns without being
//! declared again, which keeps the same coverage across the span anyone
//! actually rewinds while stopping the set growing without limit.
//!
//! **The window governs observation, never restorability.** Every manifest
//! still restores exactly what it recorded; a path that has aged out is still
//! restored by every manifest already holding it. What ends is the engine
//! watching it in *new* captures. So VIII.1's ban on age-based behaviour is
//! untouched: that rule is about reclamation, and nothing here reclaims.
//!
//! # Why a file rather than a derivation
//!
//! Claude Code rebuilds its set from the union of its snapshots' keys, which
//! works because its snapshots hold *only* edit-tracked files. A filesnap
//! manifest mixes all three partitions, and VII.3 forbids recording which
//! partition supplied a path in a content-addressed record. There is no
//! derivation available, so there is a file.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Result;
use crate::error::SnapshotError;

/// How many turns a declared path keeps being watched without being declared
/// again. Matches Claude Code's own cap.
pub const DECLARED_WINDOW_TURNS: u64 = 100;

/// One declaration: which turn saw it, and what was declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Declaration {
    /// Position in this session's turn sequence, not a clock. D6 puts
    /// ordering in a sequence rather than a timestamp, and the window counts
    /// distinct turns — a session's log holds several entries per turn (one
    /// scan plus any pre-edit attaches), so counting entries would shrink the
    /// window unpredictably.
    turn: u64,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DeclaredFile {
    /// Format version of this record — see [`crate::manifest::Manifest`].
    version: u32,
    /// Turn ids in the order this session first saw them, so an ordinal can
    /// be assigned without the host supplying one (D6: we never mint ids, but
    /// the *order* is ours).
    turns: Vec<String>,
    entries: Vec<Declaration>,
}

impl Default for DeclaredFile {
    fn default() -> Self {
        Self {
            version: crate::workspace::FORMAT_VERSION,
            turns: Vec::new(),
            entries: Vec::new(),
        }
    }
}

/// One session's declared paths, on disk beside its log.
pub struct DeclaredStore {
    root: PathBuf,
}

impl DeclaredStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| SnapshotError::io(&root, e))?;
        Ok(Self { root })
    }

    /// Record that `paths` were declared during `turn_id`.
    ///
    /// A path declared again moves forward: only its latest turn counts, so
    /// a file the agent keeps editing never ages out.
    pub fn declare(&self, session_id: &str, turn_id: &str, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut file = self.load(session_id)?;
        let turn = match file.turns.iter().position(|t| t == turn_id) {
            Some(index) => index as u64,
            None => {
                file.turns.push(turn_id.to_string());
                (file.turns.len() - 1) as u64
            }
        };
        for path in paths {
            file.entries.retain(|entry| &entry.path != path);
            file.entries.push(Declaration {
                turn,
                path: path.clone(),
            });
        }
        self.save(session_id, &file)
    }

    /// Record that `turn_id` happened, whether or not it declared anything.
    ///
    /// The window counts turns. Assigning an ordinal only when something is
    /// declared makes it "the last N turns that declared something" instead,
    /// so a session that declares once and then runs hundreds of edit-free
    /// turns ages nothing out.
    pub fn note_turn(&self, session_id: &str, turn_id: &str) -> Result<()> {
        let mut file = self.load(session_id)?;
        if file.turns.iter().any(|t| t == turn_id) {
            return Ok(());
        }
        // Nothing has ever been declared, so there is no window to advance
        // and no file worth creating.
        if file.entries.is_empty() {
            return Ok(());
        }
        file.turns.push(turn_id.to_string());
        self.save(session_id, &file)
    }

    /// The paths still inside the window, for the next capture's scan.
    ///
    /// A session with no file yields nothing, which is the ordinary case for
    /// one that has never used the edit API.
    pub fn active(&self, session_id: &str) -> Result<BTreeSet<PathBuf>> {
        let file = self.load(session_id)?;
        let latest = file.turns.len() as u64;
        let cutoff = latest.saturating_sub(DECLARED_WINDOW_TURNS);
        Ok(file
            .entries
            .into_iter()
            .filter(|entry| entry.turn + 1 > cutoff)
            .map(|entry| entry.path)
            .collect())
    }

    /// Every path this session ever declared, window or not.
    ///
    /// A GC root: a manifest is kept alive by the log, but the *paths* here
    /// are what a restore's safety scope must still look at, so nothing may
    /// treat an aged-out path as never having been observed.
    pub fn all(&self, session_id: &str) -> Result<BTreeSet<PathBuf>> {
        Ok(self
            .load(session_id)?
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect())
    }

    /// Drop a session's declarations, for when its conversation is deleted.
    pub fn remove(&self, session_id: &str) -> Result<()> {
        let path = self.path_for(session_id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    /// Every session with a declared set here, for the collector.
    pub fn session_ids(&self) -> Result<Vec<String>> {
        let entries = fs::read_dir(&self.root).map_err(|e| SnapshotError::io(&self.root, e))?;
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".json") {
                out.push(id.to_string());
            }
        }
        Ok(out)
    }

    /// Whether this session's record is old enough to judge as unreferenced.
    pub fn settled(&self, session_id: &str) -> bool {
        self.path_for(session_id)
            .is_ok_and(|path| crate::sweep::settled(&path))
    }

    fn load(&self, session_id: &str) -> Result<DeclaredFile> {
        let path = self.path_for(session_id)?;
        match fs::read(&path) {
            Ok(bytes) => {
                let file: DeclaredFile = serde_json::from_slice(&bytes)?;
                if file.version != crate::workspace::FORMAT_VERSION {
                    return Err(SnapshotError::UnknownRecordVersion {
                        kind: "declared set",
                        id: session_id.to_string(),
                        found: file.version,
                        supported: crate::workspace::FORMAT_VERSION,
                    });
                }
                Ok(file)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DeclaredFile::default()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    fn save(&self, session_id: &str, file: &DeclaredFile) -> Result<()> {
        let path = self.path_for(session_id)?;
        let tmp = crate::sweep::tmp_name(&path);
        fs::write(&tmp, serde_json::to_vec_pretty(file)?)
            .map_err(|e| SnapshotError::io(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| SnapshotError::io(&path, e))
    }

    fn path_for(&self, session_id: &str) -> Result<PathBuf> {
        crate::id::validate_stored("session id", session_id)?;
        Ok(self.root.join(format!("{session_id}.json")))
    }
}

/// Where a partition keeps its declared sets.
pub(crate) fn dir_in(partition: &Path) -> PathBuf {
    partition.join("declared")
}

#[cfg(test)]
#[path = "declared_tests.rs"]
mod tests;
