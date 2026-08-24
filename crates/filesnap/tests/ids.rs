//! Ids that cannot be stored are refused at the door, not rewritten.
//!
//! The unit tests in `id.rs` cover the predicate. These cover the thing that
//! actually matters: that the predicate is reached from the public API, on
//! every route an id can take to becoming a filename.

#![allow(clippy::unwrap_used)]

use filesnap::PreEditImage;
use filesnap::SnapshotError;
use filesnap::fixture::Fixture;
use filesnap::fixture::no_rules;
use pretty_assertions::assert_eq;

/// Ids that used to be mapped onto one filename, merging three conversations
/// into one log and one undo stack. Each is now refused on its own terms.
const COLLIDING: [&str; 3] = ["my session", "my/session", "my:session"];

fn is_invalid(err: SnapshotError) -> bool {
    matches!(err, SnapshotError::InvalidId { .. })
}

#[test]
fn a_session_id_that_cannot_be_a_filename_is_refused() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");

    for id in COLLIDING.into_iter().chain(["", ".", ".."]) {
        let err = fx
            .store()
            .checkpoint(id, "turn-1", fx.all_files())
            .unwrap_err();
        assert!(is_invalid(err), "{id:?}");
        assert!(!fx.store().session_exists(id), "{id:?}");
    }
}

#[test]
fn a_turn_id_that_cannot_be_a_filename_is_refused() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");

    for id in COLLIDING.into_iter().chain(["", ".", ".."]) {
        let err = fx.store().checkpoint("s1", id, fx.all_files()).unwrap_err();
        assert!(is_invalid(err), "{id:?}");
        // Looking one up refuses too, because the check lives in the path
        // builder rather than in each entry point — there is no route by
        // which an unprovable id becomes a filename.
        assert!(
            is_invalid(fx.store().target_for_turn(id).unwrap_err()),
            "{id:?}"
        );
    }
}

/// The engine mints `_safety-restore-…` before every restore, so a caller
/// that could supply an id in that namespace could shadow one.
#[test]
fn a_caller_cannot_claim_the_reserved_namespace() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    let store = fx.store();

    assert!(is_invalid(
        store
            .checkpoint("s1", "_safety-restore-abc", fx.all_files())
            .unwrap_err()
    ));
    assert!(is_invalid(store.ensure_session("_s").unwrap_err()));
    assert!(is_invalid(
        store
            .attach_pre_edit("_s", "turn-1", "/tmp/x", &PreEditImage::DidNotExist)
            .unwrap_err()
    ));
    assert!(is_invalid(
        store.inherit_log("s1", "_fork", "turn-1").unwrap_err()
    ));
}

/// A dot in a turn id is ordinary — these are external conversation ids — and
/// two that differ only after one must stay two records.
///
/// Turn files had no extension, so `with_extension("tmp")` truncated at the
/// id's own last dot and `v1.2` and `v1.9` shared one temporary path. Two
/// concurrent writes could leave one turn resolving to the other's manifest.
#[test]
fn turn_ids_differing_after_a_dot_stay_distinct() {
    let fx = Fixture::new();
    let store = fx.store();

    fx.write("a.txt", "first");
    store.checkpoint("s1", "v1.2", fx.all_files()).unwrap();
    fx.write("a.txt", "second");
    store.checkpoint("s1", "v1.9", fx.all_files()).unwrap();

    let two = store.target_for_turn("v1.2").unwrap().unwrap();
    let nine = store.target_for_turn("v1.9").unwrap().unwrap();
    assert_ne!(two, nine, "one turn resolved to the other's manifest");

    // And each really restores its own state.
    for (turn, content) in [("v1.2", "first"), ("v1.9", "second")] {
        let target = store.target_for_turn(turn).unwrap().unwrap();
        store
            .restore_to(
                "s1",
                &target,
                filesnap::RestoreKind::Rewind { undo_for: None },
                fx.restore_scope("s1"),
                &no_rules(),
            )
            .unwrap();
        assert_eq!(fx.read("a.txt"), content, "{turn}");
    }
}

/// A turn id ending in `.tmp` is a record, not residue — the suffix makes the
/// two tellable apart, where before they were the same filename.
#[test]
fn a_turn_id_ending_in_tmp_is_still_a_record() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    let store = fx.store();
    store
        .checkpoint("s1", "release.tmp", fx.all_files())
        .unwrap();

    assert!(store.target_for_turn("release.tmp").unwrap().is_some());
    filesnap::collect_garbage(fx.data_dir()).unwrap();
    fx.age_store();
    filesnap::collect_garbage(fx.data_dir()).unwrap();
    assert!(
        store.target_for_turn("release.tmp").unwrap().is_some(),
        "collection mistook a record for residue"
    );
}
