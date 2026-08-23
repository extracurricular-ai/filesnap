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
use crate::refs::collect_partition;
use crate::workspace;

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
    for key in workspace::all_partitions(&root)? {
        let partition = workspace::partition_dir(&root, &key);
        let refs = RefStore::open(partition.join("refs"))?;
        let turns = TurnIndex::open(&partition)?;
        let manifests = ManifestStore::open(partition.join("manifests"))?;

        stats = stats.plus(collect_partition(&refs, &turns, &manifests, &blobs)?);

        // Mark: every hash named by a manifest that survived that sweep.
        for id in manifests.ids()? {
            // A manifest that cannot be read is not evidence that its content
            // is dead — it is evidence of nothing at all. Skipping it keeps
            // whatever it named alive, which is the safe direction: the cost
            // is retained bytes, and the alternative is deleting content a
            // readable manifest may still name.
            let Ok(manifest) = manifests.load(&id) else {
                continue;
            };
            for entry in manifest.entries.values() {
                live_blobs.insert(entry.hash.clone());
            }
        }
    }

    // Then content, sparing anything too young to judge. A capture publishes
    // its blobs before its manifest, so a blob written moments ago may belong
    // to a manifest that has not landed yet.
    for hash in blobs.hashes()? {
        if live_blobs.contains(&hash) || !crate::refs::settled(&blobs.path_for(&hash)) {
            stats.blobs_kept += 1;
        } else {
            blobs.remove(&hash)?;
            stats.blobs_removed += 1;
        }
    }
    Ok(stats)
}
