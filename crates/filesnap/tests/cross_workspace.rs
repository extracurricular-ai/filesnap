//! Content is shared between workspaces, so no single workspace may sweep it.

#![allow(clippy::unwrap_used)]

use filesnap::WorkspaceStore;
use filesnap::fixture::blob_count;
use pretty_assertions::assert_eq;

/// Deleting every session in one workspace must not touch content another
/// workspace still references.
///
/// The two share a blob whenever their files happen to match, which is the
/// point of content addressing — `store_bytes` writes nothing the second
/// time. So "is this blob still referenced" is a question no partition can
/// answer alone.
#[test]
fn deleting_a_workspace_leaves_another_workspaces_content_alone() {
    let data = tempfile::tempdir().unwrap();
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();

    // Same bytes in both workspaces: one blob, two manifests, two partitions.
    std::fs::write(ws_a.path().join("shared.txt"), b"identical").unwrap();
    std::fs::write(ws_b.path().join("shared.txt"), b"identical").unwrap();

    let a = WorkspaceStore::open(data.path(), ws_a.path()).unwrap();
    let b = WorkspaceStore::open(data.path(), ws_b.path()).unwrap();
    a.checkpoint("s-a", "turn-a", vec![ws_a.path().join("shared.txt")])
        .unwrap();
    b.checkpoint("s-b", "turn-b", vec![ws_b.path().join("shared.txt")])
        .unwrap();

    // B also has content of its own, which A has never heard of.
    std::fs::write(ws_b.path().join("only-b.txt"), b"b alone").unwrap();
    b.checkpoint("s-b", "turn-b2", vec![ws_b.path().join("only-b.txt")])
        .unwrap();

    let before = blob_count(data.path());
    filesnap::fixture::age_store(data.path());

    a.delete_sessions(&["s-a".to_string()]);

    assert_eq!(
        blob_count(data.path()),
        before,
        "deleting in workspace A removed content workspace B still references"
    );
    assert_eq!(
        b.latest_manifest("s-b").unwrap().unwrap().entries.len(),
        1,
        "B's own manifest is intact"
    );
    // The real proof: B can still restore, which needs the bytes.
    std::fs::write(ws_b.path().join("shared.txt"), b"changed").unwrap();
    let target = b.target_for_turn("turn-b").unwrap().unwrap();
    b.restore_to(
        "s-b",
        &target,
        filesnap::RestoreKind::Rewind { undo_for: None },
        vec![ws_b.path().join("shared.txt")],
        &|_| false,
    )
    .expect("B can still restore its own snapshot");
    assert_eq!(
        std::fs::read(ws_b.path().join("shared.txt")).unwrap(),
        b"identical"
    );
}
