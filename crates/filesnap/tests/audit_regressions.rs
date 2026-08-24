//! Defects found by a drift audit run *after* the code was believed finished.
//!
//! Five of the six were introduced by the round of fixing that closed the
//! previous audit's findings. That is the pattern worth pinning, not just the
//! individual bugs: a fix moves a boundary, and something standing on the old
//! one falls over quietly.

#![allow(clippy::unwrap_used)]

use filesnap::RestoreKind;
use filesnap::fixture::Fixture;
use filesnap::fixture::no_rules;
use pretty_assertions::assert_eq;

/// **Collection must not eat an undo a live session could still spend.**
///
/// A rewind files its undo record under the session it hands the workspace
/// to, and that session may have no log of its own yet — a fork's destination
/// is created by the rewind. Collection treated "no log" as the definition of
/// an orphaned undo record, so after the grace window it removed a pending
/// one, and with it the two manifests it was the last root for.
#[test]
fn collection_spares_an_undo_filed_under_a_session_with_no_captures() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture("s", "turn-1");
    fx.write("a.txt", "two");

    let store = fx.store();
    let target = store.target_for_turn("turn-1").unwrap().unwrap();
    store
        .restore_to(
            "s",
            &target,
            RestoreKind::Rewind {
                undo_for: Some("branch"),
            },
            fx.restore_scope("s"),
            &no_rules(),
        )
        .unwrap();

    fx.age_store();
    filesnap::collect_garbage(fx.data_dir()).unwrap();

    let undo = store.last_restore_target("branch").unwrap();
    assert!(undo.is_some(), "collection removed a pending undo record");
    // And the state it returns to is still there to return to.
    assert!(store.manifest(undo.unwrap().manifest_id()).is_ok());
}

/// Delete reads the **whole** undo stack, not just its top.
///
/// Up to twenty records live there. Reading one left the manifests named by
/// the other nineteen out of the doomed set — reclaimed eventually by
/// collection, but never by the delete that was supposed to own them.
#[test]
fn delete_reclaims_the_manifests_of_every_undo_record_not_just_the_last() {
    let fx = Fixture::new();
    fx.write("a.txt", "v0");
    let store = fx.store();
    fx.capture("s", "turn-0");
    let origin = store.target_for_turn("turn-0").unwrap().unwrap();

    for i in 1..=3 {
        fx.write("a.txt", format!("v{i}"));
        store
            .restore_to(
                "s",
                &origin,
                RestoreKind::Rewind {
                    undo_for: Some("s"),
                },
                fx.restore_scope("s"),
                &no_rules(),
            )
            .unwrap();
    }

    fx.age_store();
    let outcome = store.delete_sessions(&["s".to_string()]);
    assert!(outcome.refused.is_empty() && outcome.incomplete.is_empty());
    assert_eq!(
        filesnap::collect_garbage(fx.data_dir())
            .unwrap()
            .manifests_removed,
        0,
        "delete left manifests for collection to find — it owned all of them"
    );
}

/// The window counts turns, not turns that declared something.
///
/// Assigning an ordinal only on declaration made it "the last 100 turns that
/// declared anything", so a session that declares once and then runs hundreds
/// of edit-free turns ages nothing out — which is the growth the bound exists
/// to stop.
#[test]
fn edit_free_turns_still_advance_the_declared_window() {
    let fx = Fixture::new();
    let store = fx.store();
    let early = fx.path("edited-once.txt");
    store
        .declare_paths("s", "turn-0", std::slice::from_ref(&early))
        .unwrap();
    assert!(store.declared_paths("s").unwrap().contains(&early));

    for i in 1..=filesnap::DECLARED_WINDOW_TURNS {
        store.note_turn("s", &format!("turn-{i}")).unwrap();
    }

    assert!(
        !store.declared_paths("s").unwrap().contains(&early),
        "edit-free turns did not advance the window"
    );
}

/// Content residue is swept. It is nested under the two-character fan-out,
/// which the sweep did not descend into — so a capture killed mid-write left
/// whole-file bytes that nothing ever reclaimed.
#[test]
fn half_written_content_is_reclaimed() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture("s", "turn-1");

    // What a process killed between the temp write and the rename leaves.
    let blobs = fx.data_dir().join("filesnap/v1/blobs/ab");
    std::fs::create_dir_all(&blobs).unwrap();
    let stray = blobs.join("cdef.tmp");
    std::fs::write(&stray, vec![0u8; 4096]).unwrap();

    filesnap::collect_garbage(fx.data_dir()).unwrap();
    assert!(stray.exists(), "fresh residue may still be in use");

    fx.age_store();
    filesnap::collect_garbage(fx.data_dir()).unwrap();
    assert!(!stray.exists(), "settled content residue is nobody's");
}
