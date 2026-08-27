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
//! # Why the window is a parameter and not a rule
//!
//! Both answers are defensible, and which one is right is a fact about the
//! host's product rather than about this engine (D25).
//!
//! Keeping a path for the whole session is what the codex fork this engine
//! came from did, and it buys the asymmetry above for as long as the
//! conversation lasts: a shell command that touches a file the agent edited
//! two hundred turns ago is still caught. Dropping it quickly is the better
//! answer for paths *outside* the workspace, because the window is precisely
//! what decides whether a rewind performed much later writes to them — a path
//! in no manifest is one a restore leaves alone. One turn is the smallest
//! window that is not wrong; [`DeclaredWindow::Turns`] says why.
//!
//! Neither setting bounds anything by the size of the tree, which is what
//! IV.1 asks of the partitions, so [`DeclaredWindow::Unlimited`] is not the
//! widening IV.4 declines to expose: this partition's size follows what the
//! agent did under either.
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
use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Result;
use crate::error::SnapshotError;

/// The default window, and a default rather than a rule — see
/// [`DeclaredWindow`].
///
/// **99, so that passing no window behaves exactly as 0.3.1 did.** Claude
/// Code's cap is 100 and this engine's constant said 100, but its arithmetic
/// reached 99 of them (see [`DeclaredStore::active`]). Fixing that off-by-one
/// while leaving the constant at 100 would have moved the default by a turn
/// for every existing caller, to no one's benefit: the number was never the
/// point, and a release that quietly changes what it captures is.
pub const DECLARED_WINDOW_TURNS: NonZeroU64 = NonZeroU64::new(99).expect("99 is not zero");

/// How long a declared path keeps being watched after the turn that last
/// declared it.
///
/// A parameter with a correct default, not a user setting (D14): the host
/// picks it once for a session, and the CLI exposes it because a host that
/// drives the binary has nowhere else to say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredWindow {
    /// Watch a path through this many *further* turns after the one that
    /// declared it. `Turns(1)` reaches the next turn's capture and no more.
    ///
    /// **One is the smallest value that is not wrong**, which is why zero is
    /// unrepresentable here. The capture at the head of the next turn is what
    /// records what the edit produced; a window shorter than that would leave
    /// the file out of the very manifest a user rewinding to just after their
    /// own edit lands on.
    Turns(NonZeroU64),
    /// Watch every path the session ever declared.
    ///
    /// What the codex fork this engine came from did, and what a host wants
    /// when an edit outside the workspace has to stay reversible for as long
    /// as the conversation lasts.
    Unlimited,
}

impl Default for DeclaredWindow {
    fn default() -> Self {
        Self::Turns(DECLARED_WINDOW_TURNS)
    }
}

/// The spelling a CLI over this library reads back, so the two cannot drift
/// apart: whatever `Display` writes, the parser accepts.
impl std::fmt::Display for DeclaredWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Turns(turns) => write!(f, "{turns}"),
            Self::Unlimited => f.write_str("unlimited"),
        }
    }
}

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
    /// Whose declarations these are; see [`crate::refs::ThreadLog::session`].
    #[serde(default)]
    session: String,
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
            session: String::new(),
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

    /// The paths still inside `window`, for the next capture's scan.
    ///
    /// A session with no file yields nothing, which is the ordinary case for
    /// one that has never used the edit API.
    pub fn active(&self, session_id: &str, window: DeclaredWindow) -> Result<BTreeSet<PathBuf>> {
        let file = self.load(session_id)?;
        let cutoff = match window {
            // Ordinal zero is the first turn, so a cutoff of zero admits
            // every entry — the same filter serves both arms rather than one
            // of them growing a second path nothing exercises.
            DeclaredWindow::Unlimited => 0,
            // `+ 1` because the turn being captured is already in `turns`:
            // `note_turn` runs at the head of the capture that then reads
            // this. Without it a window of one would reach no capture at all,
            // and the default would reach 99 turns while saying 100 —
            // the doc-versus-code gap VII.4 calls a defect.
            DeclaredWindow::Turns(turns) => {
                (file.turns.len() as u64).saturating_sub(turns.get().saturating_add(1))
            }
        };
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
            if !entry.file_name().to_string_lossy().ends_with(".json") {
                continue;
            }
            // From inside the record: the filename is a digest.
            if let Ok(bytes) = fs::read(entry.path())
                && let Ok(file) = serde_json::from_slice::<DeclaredFile>(&bytes)
                && !file.session.is_empty()
            {
                out.push(file.session);
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
        let mut file = file.clone();
        file.session = session_id.to_string();
        let file = &file;
        let path = self.path_for(session_id)?;
        let tmp = crate::sweep::tmp_name(&path);
        fs::write(&tmp, serde_json::to_vec_pretty(file)?)
            .map_err(|e| SnapshotError::io(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| SnapshotError::io(&path, e))
    }

    fn path_for(&self, session_id: &str) -> Result<PathBuf> {
        crate::id::validate_stored("session id", session_id)?;
        Ok(self
            .root
            .join(format!("{}.json", crate::id::record_name(session_id))))
    }
}

/// Where a partition keeps its declared sets.
pub(crate) fn dir_in(partition: &Path) -> PathBuf {
    partition.join("declared")
}

#[cfg(test)]
#[path = "declared_tests.rs"]
mod tests;
