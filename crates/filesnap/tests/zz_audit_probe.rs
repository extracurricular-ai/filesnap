//! TEMPORARY audit probe - delete after running.
#![allow(clippy::unwrap_used)]

use filesnap::RestoreKind;
use filesnap::WorkspaceStore;
use filesnap::fixture::age_store;
use filesnap::fixture::no_rules;

#[test]
fn probe_gc_and_undo_of_a_logless_session() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();
    std::fs::write(ws.join("a.txt"), "one").unwrap();
    store
        .checkpoint("s", "turn-1", vec![ws.join("a.txt")])
        .unwrap();
    std::fs::write(ws.join("a.txt"), "two").unwrap();
    let target = store.target_for_turn("turn-1").unwrap().unwrap();
    store
        .restore_to(
            "s",
            &target,
            RestoreKind::Rewind {
                undo_for: Some("branch"),
            },
            vec![ws.join("a.txt")],
            &no_rules(),
        )
        .unwrap();
    println!("branch has a log? {}", store.session_exists("branch"));
    println!(
        "undo before gc: {:?}",
        store.last_restore_target("branch").unwrap().is_some()
    );
    age_store(dir.path());
    let stats = filesnap::collect_garbage(dir.path()).unwrap();
    println!("gc stats: {stats:?}");
    println!(
        "undo after gc: {:?}",
        store.last_restore_target("branch").unwrap().is_some()
    );
}
