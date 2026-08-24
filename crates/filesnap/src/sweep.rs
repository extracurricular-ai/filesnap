//! Reclaiming records: what delete prunes, and what collection sweeps.
//!
//! Both operations answer the same question — which manifests is anything
//! still pointing at — and D10 requires them to answer it through **one
//! read-only query**. Sharing a primitive is not a dependency; reaching
//! through the other operation's entry point is. Before this module, delete
//! ran gc's whole mutating partition sweep, so delete's result depended on
//! gc's marking logic, and gc's marking logic pruned records delete owned.
//!
//! **Nothing here touches content.** Whether a blob is still referenced is a
//! question about every workspace at once, because content is deduplicated
//! and lineage has nothing to do with it (D10). Only
//! [`crate::collect_garbage`] can answer it, so only it may remove a blob.
//! Everything in this module is scoped to one partition, which is exactly
//! why none of it is allowed near the shared blob store: a partition-scoped
//! answer applied to a global space deletes other workspaces' content.
//!
//! The two entry points differ in what they are allowed to reach:
//!
//! | | reaches | removes |
//! |---|---|---|
//! | [`prune_sessions`] | the turn ids and manifests the deleted logs named | only those |
//! | [`collect_partition`] | every record in the partition | anything unreachable |

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use crate::error::Result;
use crate::manifest::ManifestStore;
use crate::refs::GcStats;
use crate::refs::RefStore;
use crate::refs::TurnIndex;

/// How new a file has to be for a sweep to leave it alone regardless.
///
/// A capture publishes in three steps — blobs, then the manifest, then the
/// log entry — and none of it is atomic across processes. A sweep that read
/// the logs before that last step but listed the files after it would delete
/// a snapshot a live session believes it holds, which is worse than any
/// amount of retained garbage. Nothing coordinates the two: a workspace is
/// explicitly multi-session, and collection runs from whichever process asked
/// for it.
///
/// Git answers the same race the same way rather than by locking
/// (`gc.pruneExpire`): fresh objects are simply never pruned, and whatever
/// garbage is among them waits for the next sweep. Reclamation is delayed;
/// nothing is lost.
pub(crate) const GC_GRACE: Duration = Duration::from_secs(300);

/// Whether `path` is old enough that its absence from the live set can be
/// trusted.
///
/// Unreadable or undatable files count as **young**: a sweep declines to
/// delete anything it cannot age, because the cost of waiting is retained
/// bytes and the cost of guessing is a snapshot a live session believes it
/// holds.
pub(crate) fn settled(path: &Path) -> bool {
    let cutoff = SystemTime::now()
        .checked_sub(GC_GRACE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|written| written <= cutoff)
}

/// A temporary name for `path` that no other writer can be using.
///
/// **Not `path.with_extension("tmp")`.** Two writers producing the same final
/// path — which is the ordinary case for content-addressed records, and for a
/// turn entry two forks share — would then write the *same* temporary and race
/// on it: one renames it into place and the other's rename fails with ENOENT,
/// so a capture that had done all its work reports an I/O error on a file it
/// successfully wrote. Nothing wider than a session is locked (D18), so this
/// has to be safe without a lock rather than because of one.
///
/// The process id distinguishes writers across processes and the counter
/// distinguishes them within one. Both are only needed until the rename; after
/// it the name is gone.
pub(crate) fn tmp_name(path: &Path) -> PathBuf {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Mark `path` as referenced now, so the grace window measures last use
/// rather than creation.
///
/// Both stores dedup: a write whose content is already present writes
/// nothing. That makes an object's mtime the time it was *created*, and the
/// window is asking a different question — whether anyone might still be in
/// the middle of publishing something that names it. Freshening makes the
/// timestamp answer the question actually being asked.
///
/// Failure is ignored. A blob that cannot be freshened is one the sweep will
/// judge by an older timestamp; the cost is a race that was already there,
/// and failing a capture over it would be worse.
pub(crate) fn freshen(path: &Path) {
    if let Ok(file) = fs::File::options().write(true).open(path) {
        let _ = file.set_times(fs::FileTimes::new().set_modified(SystemTime::now()));
    }
}

/// Whether the live set below could be built from every root there is.
///
/// A record that cannot be read is not evidence that anything is dead — it is
/// evidence of nothing at all. So an unreadable root does not make a sweep
/// fail; it makes the sweep's answer **incomplete**, and an incomplete answer
/// may not be used to remove anything. The cost is retained bytes until the
/// damaged record is dealt with. The alternative is deleting the snapshots of
/// a session nobody touched, because the file naming them happened to be
/// corrupt.
///
/// This is also what keeps delete free of preconditions (D9): a corrupt log
/// belonging to some *other* session cannot make deleting this one fail. It
/// only defers the reclamation, which was never part of delete's success
/// criterion (VIII.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Coverage {
    /// Every root was read. Anything absent from the live set is unreachable.
    Complete,
    /// At least one root could not be read. Nothing may be removed.
    Partial,
}

/// What the surviving **logs** name: their manifests, and their turn ids in
/// on-disk form.
///
/// Logs are the primary root. The turn index and the undo records are roots
/// too, but they are read *after* the stale entries among them have been
/// pruned — otherwise a turn entry about to be removed vouches for the very
/// manifest it was the last thing pointing at, and the sweep converges one
/// pass later than it should.
///
/// **Read-only.** An earlier version pruned the turn index as a side effect,
/// which is how delete's own record cleanup ended up inside gc's marking
/// helper — one hole with two symptoms (D10, C14). What each operation prunes
/// is now explicit at its call site.
fn roots_from_logs(refs: &RefStore) -> Result<(BTreeSet<String>, BTreeSet<String>, Coverage)> {
    let mut manifests = BTreeSet::new();
    let mut turn_files = BTreeSet::new();

    let logs = refs.thread_logs()?;
    let coverage = if logs.incomplete {
        Coverage::Partial
    } else {
        Coverage::Complete
    };
    for log in logs.logs {
        for entry in log.entries {
            turn_files.insert(crate::refs::turn_file_name(&entry.turn_id));
            manifests.insert(entry.manifest_id);
        }
    }
    Ok((manifests, turn_files, coverage))
}

/// Remove the records the just-deleted sessions owned, and nothing else.
///
/// `doomed_turns` and `doomed_manifests` are what those sessions' logs named,
/// gathered **before** the logs were unlinked — after the unlink there is no
/// way to learn it, which is why delete reads first and removes second.
///
/// Scoped on purpose. The previous implementation reconciled the whole turn
/// index by global elimination, so deleting one conversation could unlink a
/// turn entry belonging to a live session whose log entry had landed but
/// whose turn file had not yet been written — a rewind lost permanently, in a
/// session nobody deleted (C12). Nothing here enumerates a directory: every
/// candidate was named by a log that is now gone.
pub(crate) fn prune_sessions(
    refs: &RefStore,
    turns: &TurnIndex,
    manifests: &ManifestStore,
    doomed_turns: &BTreeSet<String>,
    doomed_manifests: &BTreeSet<String>,
) -> Result<GcStats> {
    let mut stats = GcStats::default();
    let (mut live_manifests, live_turns, coverage) = roots_from_logs(refs)?;

    if coverage == Coverage::Partial {
        // Something is unreadable, so "nothing points at this any more" is
        // not a claim we can make. The sessions are already unreachable —
        // that part is done and is what delete promised.
        stats.manifests_kept = doomed_manifests.len();
        return Ok(stats);
    }

    // Turn entries first. A doomed session's turn entry names a doomed
    // manifest, so leaving it in place until after the liveness question
    // would have it answer that question in its own favour.
    // Turn entries go immediately, with no age gate — unlike the manifests
    // below. A turn entry is the *name* by which a state is reachable, so
    // sparing a fresh one leaves a deleted session's snapshots restorable by
    // id, which is precisely the unreachability delete promises and promises
    // now (VIII.3). A manifest has no such name: nothing outside a log or the
    // turn index can reach it, so delaying its removal costs only disk.
    //
    // The cost accepted here is a narrow race with `inherit_log`: a fork that
    // copies these turn ids into its own log after this read loses them. A
    // grace window does not fix that — an inherited turn's file was written
    // at the original capture and is long settled — so the window would buy
    // nothing while breaking the promise above.
    for turn_file in doomed_turns {
        if !live_turns.contains(turn_file) {
            turns.remove_turn_file(turn_file)?;
        }
    }

    let held = turns.all_manifest_ids()?;
    if held.incomplete {
        stats.manifests_kept = doomed_manifests.len();
        return Ok(stats);
    }
    live_manifests.extend(held.ids);

    for id in doomed_manifests {
        // Unreachable *and* old enough to trust as unreachable — the same
        // gate `collect_partition` applies, and for a reason delete cannot
        // opt out of. `ManifestStore::save` dedups, so a live session that
        // re-derives an existing manifest writes nothing and appends its log
        // entry afterwards. A delete whose liveness read fell in that gap
        // would unlink a manifest that session is about to name, and it would
        // then stop capturing entirely. Reclaiming late costs disk;
        // reclaiming early costs a live session.
        if live_manifests.contains(id) || !manifests.path_for(id).is_ok_and(|p| settled(&p)) {
            stats.manifests_kept += 1;
            continue;
        }
        manifests.remove(id)?;
        stats.manifests_removed += 1;
    }
    Ok(stats)
}

/// Sweep every record in this partition that nothing points at.
///
/// Unlike [`prune_sessions`] this enumerates, so it is the one that finds
/// what a crashed operation left behind — the orphans D8 makes collection's
/// job. It removes **records only**; see the module header.
pub(crate) fn collect_partition(
    refs: &RefStore,
    turns: &TurnIndex,
    manifests: &ManifestStore,
) -> Result<GcStats> {
    let mut stats = GcStats::default();
    let (mut live_manifests, live_turns, coverage) = roots_from_logs(refs)?;
    if coverage == Coverage::Partial {
        return Ok(stats);
    }

    // An undo record for a session that has no log is not a root, it is
    // residue: nothing can reach it to spend it, and left in place it pins
    // its two manifests for good. Dropped before liveness is computed, so
    // what it was holding becomes collectable in this same pass.
    for thread_id in turns.orphan_restore_logs(refs)? {
        turns.remove_restores(&thread_id)?;
    }

    // Turn entries next: pure index, rebuilt by nothing, and a stale one
    // keeps a manifest alive. Grace-gated — a capture writes its log entry
    // before its turn file, so a turn younger than the window may belong to
    // a log that was read moments ago.
    turns.retain_turns(&live_turns)?;

    // Only now are the remaining index and undo records asked what they hold.
    let held = turns.all_manifest_ids()?;
    if held.incomplete {
        return Ok(stats);
    }
    live_manifests.extend(held.ids);

    for id in manifests.ids()? {
        // A manifest too young to sweep is also too young to trust as dead:
        // the capture that is about to name it may not have landed yet.
        if live_manifests.contains(&id) || !manifests.path_for(&id).is_ok_and(|p| settled(&p)) {
            stats.manifests_kept += 1;
        } else {
            manifests.remove(&id)?;
            stats.manifests_removed += 1;
        }
    }
    Ok(stats)
}

/// Unlink `*.tmp` residue under `dir` that is past the grace window.
///
/// Every atomic write in the store is write-to-`.tmp`-then-rename, so a
/// process killed between the two leaves one behind. D10 assigns this to
/// collection, and until now nothing removed one: all three enumerations
/// merely *skipped* `.tmp`, which made a stray file permanent — and, where a
/// record's name could collide with it, an uncollectable GC root (C4).
///
/// Errors on individual entries are ignored. Residue is a tidiness matter,
/// and failing a collection over a file nobody can read would trade a small
/// leak for a large one.
pub(crate) fn sweep_residue(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        // One level down, for the content store's two-character fan-out. A
        // half-written blob is whole-file content bytes, so it is the most
        // expensive residue there is and was the one nothing swept.
        if path.is_dir() {
            removed += sweep_residue(&path);
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "tmp")
            && settled(&path)
            && fs::remove_file(&path).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Declared sets belonging to sessions that have no log any more.
///
/// The third record under a session's name, and the one nothing enumerated.
/// Delete removes it with the other two; this finds the ones a crash left.
pub(crate) fn orphan_declared(
    declared: &crate::declared::DeclaredStore,
    refs: &RefStore,
) -> Result<usize> {
    let mut removed = 0;
    for session_id in declared.session_ids()? {
        if !refs.exists(&session_id) && declared.settled(&session_id) {
            declared.remove(&session_id)?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
#[path = "sweep_tests.rs"]
mod tests;
