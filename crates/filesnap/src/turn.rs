//! One turn's work, as functions rather than as a stateful object.
//!
//! Capture and declare used to be methods on a tracker holding a mutex over an
//! `extras` cache and an `ignore_root`, because it was written for a host that
//! stays running. A CLI is stateless per invocation (D3): `declare` and
//! `capture` are two separate processes, and anything held between them is
//! lost with the first one. The tracker would have to be rebuilt for every
//! call and would live exactly as long as that call.
//!
//! **D25 had already removed the reason for the state.** Once the declared set
//! is persisted, nothing has to be in memory: `ignore_root` follows from the
//! scope each call already carries, and the declared set is on disk.
//!
//! `extras` is gone rather than kept as a cache. It was the source of the
//! defect the second audit found — the capture path unioned it with the
//! windowed persisted set, so inside one process a path that had aged out was
//! re-stat'd for the rest of the session and the bound did nothing. A cache
//! whose only job is to disagree with the truth is not a cache (D38).

use std::path::PathBuf;

use tracing::info;
use tracing::warn;

use crate::checkpoint::Checkpoint;
use crate::error::Result;
use crate::scope::HiddenFiles;
use crate::scope::ScanLimits;
use crate::scope::canonical_key;
use crate::scope::is_ignored;
use crate::scope::load_ignore;
use crate::scope::tracked_files;
use crate::store::PreEditImage;
use crate::store::WorkspaceStore;

/// Where a turn is happening, and how far a scan may reach.
///
/// The whole of what a capture needs to know beyond its ids — which is what
/// makes the operation a function. Build one per call; it holds nothing that
/// outlives the turn.
#[derive(Debug, Clone)]
pub struct TurnScope {
    /// The directory this turn runs in.
    pub cwd: PathBuf,
    /// The session's configured workspace roots, if it has any.
    pub roots: Vec<PathBuf>,
    pub hidden: HiddenFiles,
    pub limits: ScanLimits,
}

impl TurnScope {
    /// A scope rooted at one directory, with the defaults.
    pub fn at(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            roots: Vec::new(),
            hidden: HiddenFiles::Skip,
            limits: ScanLimits::default(),
        }
    }

    /// The roots this turn actually scans.
    ///
    /// The session's own workspace roots come first: they are what the user
    /// declared the workspace to be, and on a sandboxed host they are also
    /// where the agent is permitted to write, so scoping to them makes
    /// "whatever can be changed can be reverted" structural rather than
    /// coincidental.
    ///
    /// Roots unrelated to the turn's cwd are dropped. A configured root can
    /// describe a different environment, or simply be stale, and scanning an
    /// unrelated tree is the over-capture this feature exists to avoid — a
    /// root that neither contains nor sits under the directory being worked in
    /// is not this session's workspace.
    pub fn scan_roots(&self) -> Vec<PathBuf> {
        let related: Vec<PathBuf> = self
            .roots
            .iter()
            .filter(|root| self.cwd.starts_with(root) || root.starts_with(&self.cwd))
            .cloned()
            .collect();
        let roots = if related.is_empty() {
            vec![self.cwd.clone()]
        } else {
            related
        };
        // Canonical, so the keys a walk produces do not depend on how the
        // caller spelled the root — see [`crate::scope::canonical_key`].
        roots.iter().map(|r| canonical_key(r)).collect()
    }

    /// The directory whose ignore rules govern this turn.
    ///
    /// Derived rather than remembered. It used to be recorded by the
    /// turn-start capture and read back by the edit hook, which meant the
    /// filter matched nothing at all until the first capture had run — an
    /// ignored `.env` could enter the store and be kept by every capture after
    /// (C6). A value computed from the arguments cannot be absent.
    pub fn ignore_root(&self) -> PathBuf {
        self.scan_roots()
            .first()
            .cloned()
            .unwrap_or_else(|| self.cwd.clone())
    }
}

/// Capture the state of `scope` at the start of `turn_id`.
///
/// Reads the whole tracked set and hashes what the stat cache does not cover:
/// hundreds of milliseconds on a large project. Call it off an async runtime's
/// reactor thread.
pub fn capture_turn(
    store: &WorkspaceStore,
    session_id: &str,
    turn_id: &str,
    scope: &TurnScope,
) -> Result<Checkpoint> {
    // Note the turn even when it declares nothing, so the declared set's
    // window counts turns rather than "turns that declared something" —
    // otherwise a session that declares once and then runs 500 edit-free turns
    // ages nothing out at all.
    if let Err(err) = store.note_turn(session_id, turn_id) {
        warn!("filesnap: could not record turn order for the declared set: {err}");
    }

    // Three partitions, unioned (see `scope`), plus what the edit API has
    // declared — wherever it lives. Walking the subtree instead was unbounded
    // by construction: on a repository of any age most of what is on disk is
    // build output, which is both the bulk of the cost and the least worth
    // keeping.
    //
    // The persisted set is the only source, and it is the only thing that
    // applies the window.
    let declared: Vec<PathBuf> = store.declared_paths(session_id)?.into_iter().collect();
    let scan = tracked_files(&scope.scan_roots(), declared, scope.hidden, scope.limits);

    // What the *scan* passed over is a drop too, and the capture cannot see
    // it: an over-size file never reaches the manifest at all.
    let scan_dropped = scan.dropped;
    let mut checkpoint = store.checkpoint(session_id, turn_id, scan.files)?;
    for drop in scan_dropped {
        checkpoint.stats.dropped += 1;
        if checkpoint.stats.sample.len() < crate::checkpoint::DROP_SAMPLE_LIMIT {
            checkpoint.stats.sample.push(drop);
        }
    }
    info!(
        "filesnap: turn {turn_id} checkpoint {} ({} reused, {} hashed, {} dropped)",
        checkpoint.id, checkpoint.stats.reused, checkpoint.stats.hashed, checkpoint.stats.dropped,
    );
    Ok(checkpoint)
}

/// What a restore must be asked to look at.
///
/// The workspace as it stands **plus every path this session has observed**.
/// The second half is not optional and is easy to leave out: a file the agent
/// deleted is on no walk of the directory, so without it the safety capture
/// cannot record that it was gone, and a later undo could never take it away
/// again.
///
/// `restore_to` additionally folds in the target's own paths, which makes the
/// safety capture sufficient by construction rather than by the caller having
/// got this right. This is still the supported way to build the argument —
/// the alternative is every caller rediscovering the deleted-file case.
pub fn restore_scope(
    store: &WorkspaceStore,
    session_id: &str,
    scope: &TurnScope,
) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = tracked_files(
        &scope.scan_roots(),
        store.declared_paths(session_id)?,
        scope.hidden,
        scope.limits,
    )
    .files
    .into_iter()
    .collect();
    files.extend(
        store
            .tracked_paths(session_id)?
            .into_iter()
            .map(PathBuf::from),
    );
    files.sort();
    files.dedup();
    Ok(files)
}

/// What a declare did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeclareOutcome {
    /// Paths whose pre-edit image was stored and which are now watched.
    pub recorded: Vec<PathBuf>,
    /// Paths skipped because the ignore rules exclude them.
    pub ignored: Vec<PathBuf>,
}

/// Record what `pre_images` held before an edit changes them, and register the
/// paths so later captures keep watching them.
///
/// Hashes and writes blobs — call it off an async runtime's reactor thread.
pub fn declare_edits(
    store: &WorkspaceStore,
    session_id: &str,
    turn_id: &str,
    scope: &TurnScope,
    pre_images: Vec<(PathBuf, PreEditImage)>,
) -> Result<DeclareOutcome> {
    // Symmetric ignore: a path the user excluded from snapshots must not enter
    // the store through the edit API either. Without this, editing an ignored
    // file would both store its pre-edit content and register the path, so
    // every later capture would capture it as well — bypassing the scan's own
    // ignore filter. The rules are read fresh so the current file governs.
    let rules = load_ignore(&scope.ignore_root());
    let mut outcome = DeclareOutcome::default();

    for (path, image) in pre_images {
        // Same spelling rule as the scan partitions produce, so an edit and a
        // scan of one file agree on its key.
        let path = canonical_key(&path);
        if is_ignored(&rules, &path) {
            outcome.ignored.push(path);
            continue;
        }
        let key = path.to_string_lossy().into_owned();
        if let Err(err) = store.attach_pre_edit(session_id, turn_id, &key, &image) {
            warn!("filesnap: pre-edit attach failed for {key}: {err}");
            continue;
        }
        outcome.recorded.push(path);
    }

    // Persisted last, and separately: a declaration that fails to land costs
    // this session future *observation* of those paths, which is recoverable
    // by editing them again. Letting it fail the attaches above would cost the
    // pre-edit images themselves, which is not.
    if let Err(err) = store.declare_paths(session_id, turn_id, &outcome.recorded) {
        warn!("filesnap: could not persist the declared set: {err}");
    }
    Ok(outcome)
}

#[cfg(test)]
#[path = "turn_tests.rs"]
mod tests;
