//! The whole-store collector: the only thing allowed to remove content.
//!
//! The blob half of these tests used to live beside the partition sweep,
//! which is exactly the confusion that produced the cross-workspace defect —
//! a partition-scoped answer applied to a shared space. They live here now
//! because this is the only caller that has looked at every partition first.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use super::*;
use crate::WorkspaceStore;
use crate::fixture::age_store;
use crate::fixture::blob_count;
use crate::fixture::no_rules;
use pretty_assertions::assert_eq;

/// A workspace with one captured file, sharing `data` with any other.
fn workspace(data: &Path, name: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
    let ws = tempfile::tempdir().unwrap();
    let store = WorkspaceStore::open(data, ws.path()).unwrap();
    let paths: Vec<_> = files
        .iter()
        .map(|(rel, content)| {
            let p = ws.path().join(rel);
            std::fs::write(&p, content).unwrap();
            p
        })
        .collect();
    store
        .checkpoint(name, &format!("turn-{name}"), paths)
        .unwrap();
    ws
}

/// **Collecting must not change what any session can restore.**
///
/// The contract this module's header states, tested across two workspaces —
/// the only configuration where it could fail. Content is deduplicated and
/// shared store-wide, so a sweep that marks from one partition and deletes
/// from the shared blob store destroys the other's snapshots while both
/// sessions are alive and nothing has been deleted.
#[test]
fn collecting_leaves_every_workspace_able_to_restore() {
    let data = tempfile::tempdir().unwrap();
    let a = workspace(
        data.path(),
        "a",
        &[("shared.txt", "same"), ("only-a.txt", "a")],
    );
    let b = workspace(
        data.path(),
        "b",
        &[("shared.txt", "same"), ("only-b.txt", "b")],
    );

    let before = blob_count(data.path());
    assert_eq!(before, 3, "the identical file is one blob, not two");

    age_store(data.path());
    let stats = collect_garbage(data.path()).unwrap();

    assert_eq!(stats.blobs_removed, 0, "nothing here is unreachable");
    assert_eq!(blob_count(data.path()), before);

    // The proof that matters: both can still put their files back.
    for (ws, session, rel, content) in [(&a, "a", "only-a.txt", "a"), (&b, "b", "only-b.txt", "b")]
    {
        let store = WorkspaceStore::open(data.path(), ws.path()).unwrap();
        std::fs::write(ws.path().join(rel), "clobbered").unwrap();
        let target = store
            .target_for_turn(&format!("turn-{session}"))
            .unwrap()
            .unwrap();
        store
            .restore_to(
                session,
                &target,
                crate::RestoreKind::Rewind { undo_for: None },
                vec![ws.path().join(rel)],
                &no_rules(),
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.path().join(rel)).unwrap(),
            content
        );
    }
}

/// Content nothing names is reclaimed — and only once it is old enough to
/// judge, because a capture publishes its blobs before the manifest naming
/// them.
#[test]
fn unreachable_content_is_reclaimed_after_the_grace_window() {
    let data = tempfile::tempdir().unwrap();
    let ws = workspace(data.path(), "s", &[("a.txt", "kept")]);
    let store = WorkspaceStore::open(data.path(), ws.path()).unwrap();

    // A second capture whose content nothing will reference once its session
    // is deleted.
    std::fs::write(ws.path().join("a.txt"), "doomed").unwrap();
    store
        .checkpoint("doomed", "turn-doomed", vec![ws.path().join("a.txt")])
        .unwrap();
    assert_eq!(blob_count(data.path()), 2);

    store.delete_sessions(&["doomed".to_string()]);
    assert_eq!(
        blob_count(data.path()),
        2,
        "delete reclaims records, never content (D19)"
    );

    // Too young to sweep.
    assert_eq!(collect_garbage(data.path()).unwrap().blobs_removed, 0);

    age_store(data.path());
    assert_eq!(collect_garbage(data.path()).unwrap().blobs_removed, 1);
    assert_eq!(blob_count(data.path()), 1);
}

/// Collecting twice reclaims nothing the second time: it is a function of
/// what is on disk, so it converges and repairs itself after an interruption.
#[test]
fn collecting_is_idempotent() {
    let data = tempfile::tempdir().unwrap();
    let ws = workspace(data.path(), "s", &[("a.txt", "one")]);
    let store = WorkspaceStore::open(data.path(), ws.path()).unwrap();
    store.delete_sessions(&["s".to_string()]);
    age_store(data.path());

    let first = collect_garbage(data.path()).unwrap();
    let second = collect_garbage(data.path()).unwrap();

    assert!(first.blobs_removed > 0);
    assert_eq!(second.blobs_removed, 0);
    assert_eq!(second.manifests_removed, 0);
}

/// An empty store is nothing to collect, not an error.
#[test]
fn an_absent_store_is_not_an_error() {
    let data = tempfile::tempdir().unwrap();
    assert_eq!(collect_garbage(data.path()).unwrap(), GcStats::default());
}

/// An old blob adopted by a new capture is not collected before the manifest
/// naming it has landed.
///
/// The subtle half of the grace window. `store_bytes` writes nothing when the
/// hash is already present, so without freshening a blob's mtime records when
/// it was *created* — and a three-day-old blob adopted one second ago is
/// already settled, so the window never sees it. The race is real because a
/// capture publishes its blobs before the manifest that names them: collection
/// running in that gap finds a settled blob nothing references.
///
/// Written at the store level because that gap is exactly what the public API
/// does not let a caller stand in.
#[test]
fn content_reused_by_a_new_capture_survives_the_gap_before_its_manifest() {
    let data = tempfile::tempdir().unwrap();
    let root = crate::workspace::store_root(data.path()).unwrap();
    let blobs = crate::BlobStore::open(crate::workspace::blobs_dir(&root)).unwrap();

    blobs.store_bytes(b"shared content").unwrap();
    age_store(data.path());
    assert_eq!(
        collect_garbage(data.path()).unwrap().blobs_removed,
        1,
        "unreferenced and settled: this one really is garbage"
    );

    // Again, and this time a second capture adopts it while it is old.
    let hash = blobs.store_bytes(b"shared content").unwrap();
    age_store(data.path());
    let adopted = blobs.store_bytes(b"shared content").unwrap();
    assert_eq!(adopted, hash, "same content, same object — nothing written");

    // Collection runs in the gap before that capture's manifest lands.
    assert_eq!(
        collect_garbage(data.path()).unwrap().blobs_removed,
        0,
        "freshening is what stops the sweep judging it by its creation time"
    );
    assert!(blobs.contains(&hash));
}

/// **An unreadable manifest must not cost another one its content.**
///
/// The mark phase skipped a manifest it could not load, with a comment saying
/// that kept what it named alive. It does the opposite: the hashes are never
/// marked, so every blob only that manifest named looks unreferenced and is
/// removed once settled — while the manifest itself survives, because a live
/// log names it. The result is an intact, still-referenced record pointing at
/// content that is gone, reachable from a transient EIO as readily as from
/// real corruption.
#[test]
fn an_unreadable_manifest_does_not_cost_the_others_their_content() {
    let data = tempfile::tempdir().unwrap();
    let ws = workspace(data.path(), "s", &[("a.txt", "kept")]);
    let store = WorkspaceStore::open(data.path(), ws.path()).unwrap();

    // A second session with content of its own.
    std::fs::write(ws.path().join("b.txt"), "also kept").unwrap();
    store
        .checkpoint("other", "turn-other", vec![ws.path().join("b.txt")])
        .unwrap();
    let before = blob_count(data.path());

    // Corrupt one manifest. Its log still names it, so the record sweep keeps
    // it — only the *mark* fails.
    let root = crate::workspace::store_root(data.path()).unwrap();
    let key = crate::WorkspaceKey::of(ws.path()).unwrap();
    let manifests = crate::workspace::partition_dir(&root, &key).join("manifests");
    let victim = std::fs::read_dir(&manifests)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .next()
        .unwrap();
    std::fs::write(&victim, b"{ truncated").unwrap();

    age_store(data.path());
    let stats = collect_garbage(data.path()).unwrap();

    assert_eq!(
        stats.blobs_removed, 0,
        "an incomplete answer removed content anyway"
    );
    assert_eq!(blob_count(data.path()), before);
}
