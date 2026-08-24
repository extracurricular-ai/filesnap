//! Which ids are admitted, and — the point of the module — which are refused
//! rather than quietly turned into something else.

#![allow(clippy::unwrap_used)]

use super::*;

fn rejected(id: &str) -> String {
    match validate_stored("turn id", id) {
        Err(SnapshotError::InvalidId { reason, .. }) => reason.to_string(),
        other => panic!("expected {id:?} to be refused, got {other:?}"),
    }
}

#[test]
fn an_ordinary_conversation_id_is_admitted() {
    for id in [
        "0193f2a1-4c7e-7b3a-9f1d-2e5c8a6b4d90",
        "turn.12",
        "a",
        "A-B_c.9",
    ] {
        validate_external("turn id", id).unwrap();
    }
}

/// The two that survive any character-level filter and still name a directory
/// rather than a record in one.
#[test]
fn the_relative_directory_names_are_refused() {
    for id in [".", ".."] {
        assert!(rejected(id).contains("`.` or `..`"));
    }
}

#[test]
fn an_empty_id_is_refused() {
    assert!(rejected("").contains("may not be empty"));
}

/// The heart of D7: these three used to become one filename, so three
/// conversations shared one log and one undo stack.
#[test]
fn ids_that_used_to_collide_are_now_each_refused() {
    for id in ["my session", "my/session", "my:session"] {
        assert!(rejected(id).contains("letters, digits"), "{id}");
    }
    // And the id they all used to become is itself perfectly valid, which is
    // what made the collision silent.
    validate_external("session id", "my_session").unwrap();
}

#[test]
fn a_traversal_shaped_id_is_refused_rather_than_mapped() {
    for id in ["../../etc/passwd", "..\\..\\windows", "/etc/passwd"] {
        assert!(rejected(id).contains("letters, digits"), "{id}");
    }
}

/// The engine's namespace is closed to callers, so a host cannot hand us an
/// id that shadows the safety checkpoint taken before every restore.
#[test]
fn a_host_may_not_claim_the_reserved_prefix() {
    validate_stored("turn id", "_safety-abc").unwrap();
    let err = validate_external("turn id", "_safety-abc").unwrap_err();
    assert!(
        matches!(err, SnapshotError::InvalidId { reason, .. } if reason.contains("reserved")),
        "{err:?}"
    );
}

#[test]
fn an_id_too_long_to_store_is_refused() {
    assert!(rejected(&"a".repeat(MAX_ID_BYTES + 1)).contains("longer than"));
    validate_external("turn id", &"a".repeat(MAX_ID_BYTES)).unwrap();
}

/// Object ids are ours, and are read back out of records and handed to
/// `remove` — so the shape is checked before the path is built.
#[test]
fn only_a_real_object_name_is_admitted_as_one() {
    let real = "a".repeat(64);
    assert!(is_object_name(&real));
    validate_object("blob", &real).unwrap();

    for forged in [
        "../../etc/passwd",
        "/etc/passwd",
        "",
        &"a".repeat(63),
        &"A".repeat(64),
        &"g".repeat(64),
    ] {
        assert!(!is_object_name(forged), "{forged:?}");
        assert!(validate_object("blob", forged).is_err(), "{forged:?}");
    }
}
