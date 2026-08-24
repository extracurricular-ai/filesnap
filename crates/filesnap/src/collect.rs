//! Whole-store collection: orphaned records and unreferenced content.
//!
//! This is the only operation that spans every workspace, and it is a free
//! function for exactly that reason — it belongs to no partition, so it
//! cannot be reached by mistake from one. Its counterpart,
//! [`crate::WorkspaceStore::delete_sessions`], is a method for the mirror
//! reason: what it removes is answerable inside a single partition.
//!
//! **The contract is that running it changes nothing anyone can observe.**
//! Collect, or do not collect, any number of times: every turn still resolves
//! to the same manifest and every session can still rewind exactly as far.
//! Only bytes nothing can reach go away. A test can hold that down, and one
//! does.
//!
//! Delete does not wait on this and this does not wait on delete. Delete
//! reclaims records; collection reclaims content and whatever records a
//! crashed operation left behind.

use std::collections::BTreeSet;
use std::path::Path;

use crate::blob::BlobStore;
use crate::error::Result;
use crate::manifest::ManifestStore;
use crate::refs::GcStats;
use crate::refs::RefStore;
use crate::refs::TurnIndex;
use crate::sweep::collect_partition;
use crate::workspace;

/// How many bytes the shared content store holds.
///
/// A free function for the same reason [`collect_garbage`] is one: content
/// belongs to no workspace. A dashboard reports it *beside* a partition's
/// record usage rather than added to it, because a blob is named by however
/// many manifests happen to name it and attributing it to one workspace would
/// report the same bytes once per reference (D19, D34).
pub fn content_disk_usage(data_dir: &Path) -> Result<u64> {
    fn walk(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|entry| match entry.metadata() {
                Ok(meta) if meta.is_dir() => walk(&entry.path()),
                Ok(meta) => meta.len(),
                Err(_) => 0,
            })
            .sum()
    }
    Ok(walk(&workspace::blobs_dir(&workspace::store_root(
        data_dir,
    )?)))
}

/// Reclaim what nothing references, across every workspace in the store.
///
/// Content liveness is a whole-store question — a blob written for one
/// workspace may be named by a manifest in another, because content is
/// deduplicated and lineage has nothing to do with it. So the mark phase
/// walks every partition's manifests before a single blob is considered.
///
/// An absent store is not an error: there is simply nothing to collect.
pub fn collect_garbage(data_dir: &Path) -> Result<GcStats> {
    let root = workspace::store_root(data_dir)?;
    let blobs = BlobStore::open(workspace::blobs_dir(&root))?;
    let mut stats = GcStats::default();

    // Records first, per partition. An orphaned manifest is not merely wasted
    // space: content liveness is computed from the manifests that survive, so
    // one nothing references would pin its blobs indefinitely. Sweeping
    // records before content is what stops that, and it is why collection
    // owns orphaned records rather than only unreferenced bytes (D8).
    let mut live_blobs = BTreeSet::new();
    // Whether every manifest in the store could be read. Content is removed
    // only on a complete answer.
    let mut complete = true;
    for key in workspace::all_partitions(&root)? {
        let partition = workspace::partition_dir(&root, &key);
        let refs = RefStore::open(partition.join("refs"))?;
        let turns = TurnIndex::open(&partition)?;
        let manifests = ManifestStore::open(partition.join("manifests"))?;

        stats = stats.plus(collect_partition(&refs, &turns, &manifests)?);

        // Residue from a write killed between its temp file and its rename.
        // Nothing else removes one — every enumeration merely *skips* `.tmp`,
        // which made a stray permanent and, where a record's name could
        // collide with it, an uncollectable root (C4). D10 assigns it here.
        for dir in ["refs", "manifests", "turns", "restores", "declared"] {
            crate::sweep::sweep_residue(&partition.join(dir));
        }

        // The third record filed under a session's name. Delete removes it
        // with the other two; this is the one a crash left behind.
        let declared = crate::declared::DeclaredStore::open(crate::declared::dir_in(&partition))?;
        crate::sweep::orphan_declared(&declared, &refs)?;

        // Mark: every hash named by a manifest that survived that sweep.
        for id in manifests.ids()? {
            // A manifest that cannot be read is not evidence that its content
            // is dead — it is evidence of nothing at all, so no blob may be
            // removed on the strength of an answer that is missing it.
            //
            // Skipping it does **not** keep what it named alive, which is
            // what this said and what makes the bug worth naming: its hashes
            // are simply never marked, so every blob only it named falls out
            // of `live_blobs` and, once settled, is removed. The manifest
            // itself survives — the record sweep keeps it, because a live log
            // names it — so the result is an intact, still-referenced record
            // pointing at content that has been destroyed. A transient EIO or
            // EMFILE reaches that outcome as readily as real corruption.
            //
            // This is the guard the record sweep already has as
            // `Coverage::Partial`, on the phase that lacked it.
            let Ok(manifest) = manifests.load(&id) else {
                complete = false;
                continue;
            };
            for entry in manifest.entries.values() {
                live_blobs.insert(entry.hash.clone());
            }
        }
    }

    // Content residue is nested under the two-character fan-out, and is the
    // most expensive kind: a capture killed mid-write leaves whole-file bytes.
    crate::sweep::sweep_residue(&workspace::blobs_dir(&root));

    // Then content — but only if the mark above saw everything. An
    // under-approximated live set is not a smaller sweep, it is a wrong one:
    // every blob the unread manifests named looks unreferenced.
    if !complete {
        stats.blobs_kept += blobs.hashes()?.len();
        return Ok(stats);
    }

    // Sparing anything too young to judge. A capture publishes its blobs
    // before its manifest, so a blob written moments ago may belong to a
    // manifest that has not landed yet.
    for hash in blobs.hashes()? {
        // Unprovable age keeps it, and so does an id we cannot even build a
        // path for: a sweep removes only what it can show is both unreachable
        // and settled.
        if live_blobs.contains(&hash)
            || !blobs
                .path_for(&hash)
                .is_ok_and(|p| crate::sweep::settled(&p))
        {
            stats.blobs_kept += 1;
        } else {
            blobs.remove(&hash)?;
            stats.blobs_removed += 1;
        }
    }
    Ok(stats)
}

#[cfg(test)]
#[path = "collect_tests.rs"]
mod tests;
