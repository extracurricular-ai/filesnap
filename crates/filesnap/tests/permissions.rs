//! What a restore does to permissions, and what it deliberately does not.
//!
//! Both halves matter and they used to be one: every entry carried a `u32`,
//! so an entry whose permissions were never observed carried an invented
//! `0o644` that the planner compared and the applier applied.

#![allow(clippy::unwrap_used)]
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use filesnap::PreEditImage;
use filesnap::RestoreKind;
use filesnap::fixture::Fixture;
use filesnap::fixture::no_rules;
use pretty_assertions::assert_eq;

const SESSION: &str = "s1";

fn mode_of(fx: &Fixture, rel: &str) -> u32 {
    std::fs::metadata(fx.path(rel))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

fn chmod(fx: &Fixture, rel: &str, mode: u32) {
    std::fs::set_permissions(fx.path(rel), std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Restoring to a state recorded by the edit hook must not touch permissions.
///
/// The pre-image arrives as content, with no stat behind it, so nothing about
/// its permissions was ever observed. Recording a plausible `0o644` there was
/// not inert: a rewind wrote the right bytes back and then stripped the
/// script's executable bit — a file the user could run before the rewind and
/// could not run after.
#[test]
fn a_rewind_does_not_strip_an_executable_bit() {
    let fx = Fixture::new();
    fx.write("script.sh", "#!/bin/sh\necho old\n");
    chmod(&fx, "script.sh", 0o755);

    // The edit hook records what the file held, before the scan has ever
    // seen this path — which is exactly when the mode is unknown.
    let store = fx.store();
    store
        .attach_pre_edit(
            SESSION,
            "turn-1",
            &fx.path("script.sh").to_string_lossy(),
            &PreEditImage::Existed(b"#!/bin/sh\necho old\n".to_vec()),
        )
        .unwrap()
        .expect("a path no scan has covered attaches");

    fx.write("script.sh", "#!/bin/sh\necho new\n");
    chmod(&fx, "script.sh", 0o755);

    let target = store.target_for_turn("turn-1").unwrap().unwrap();
    store
        .restore_to(
            SESSION,
            &target,
            RestoreKind::Rewind { undo_for: None },
            fx.restore_scope(SESSION),
            &no_rules(),
        )
        .unwrap();

    assert_eq!(fx.read("script.sh"), "#!/bin/sh\necho old\n");
    assert_eq!(
        mode_of(&fx, "script.sh"),
        0o755,
        "permissions nobody observed are permissions a restore leaves alone"
    );
}

/// The other half: permissions that *were* observed are restored, including
/// when they are the only thing that changed.
///
/// A chmod does not move a file's mtime, which is why the capture re-reads
/// mode even on a stat-cache hit. Without this, making a script executable
/// and then rewinding would silently keep it executable.
#[test]
fn a_mode_only_change_is_restored() {
    let fx = Fixture::new();
    fx.write("script.sh", "#!/bin/sh\n");
    chmod(&fx, "script.sh", 0o644);
    fx.capture(SESSION, "turn-1");

    chmod(&fx, "script.sh", 0o755);
    fx.capture(SESSION, "turn-2");
    assert_eq!(mode_of(&fx, "script.sh"), 0o755);

    let store = fx.store();
    let target = store.target_for_turn("turn-1").unwrap().unwrap();
    let outcome = store
        .restore_to(
            SESSION,
            &target,
            RestoreKind::Rewind { undo_for: None },
            fx.restore_scope(SESSION),
            &no_rules(),
        )
        .unwrap();

    assert_eq!(
        outcome.stats.written, 1,
        "identical content, different permissions, still a write"
    );
    assert_eq!(mode_of(&fx, "script.sh"), 0o644);
}

/// A file whose content and permissions both already match the target is not
/// rewritten. It reads as an optimisation and it is really a correctness
/// property: an invented mode never equals a real one, so every restore used
/// to rewrite files it had no reason to touch.
#[test]
fn nothing_is_rewritten_when_the_state_already_matches() {
    let fx = Fixture::new();
    fx.write("a.txt", "same");
    chmod(&fx, "a.txt", 0o600);
    fx.capture(SESSION, "turn-1");

    let store = fx.store();
    let target = store.target_for_turn("turn-1").unwrap().unwrap();
    let outcome = store
        .restore_to(
            SESSION,
            &target,
            RestoreKind::Rewind { undo_for: None },
            fx.restore_scope(SESSION),
            &no_rules(),
        )
        .unwrap();

    assert_eq!((outcome.stats.written, outcome.stats.deleted), (0, 0));
    assert!(outcome.stats.failed.is_empty());
    assert_eq!(mode_of(&fx, "a.txt"), 0o600);
}

/// One unwritable file does not strand the rest, and the safety point is
/// still handed back.
///
/// `apply_plan` used to propagate the first error, so one bad file abandoned
/// the other 499 — and `RestoreOutcome` was built only on success, so the
/// caller got a bare `Io` with no record of how far it got and no way to
/// reach the state it could have returned to. That is III.1's reversibility
/// existing and being out of reach exactly when it matters (C20, D28).
#[test]
fn a_file_that_cannot_be_written_does_not_strand_the_others() {
    let fx = Fixture::new();
    fx.write("ok-one.txt", "before");
    fx.write("locked/inside.txt", "before");
    fx.write("ok-two.txt", "before");
    fx.capture(SESSION, "turn-1");

    for rel in ["ok-one.txt", "locked/inside.txt", "ok-two.txt"] {
        fx.write(rel, "after");
    }
    // Deny writes in the directory, so only that one file's rename fails.
    chmod(&fx, "locked", 0o500);

    let store = fx.store();
    let target = store.target_for_turn("turn-1").unwrap().unwrap();
    let outcome = store
        .restore_to(
            SESSION,
            &target,
            RestoreKind::Rewind { undo_for: None },
            fx.restore_scope(SESSION),
            &no_rules(),
        )
        .expect("a per-file failure is not a failed restore");

    chmod(&fx, "locked", 0o700);

    assert_eq!(outcome.stats.written, 2, "the other two still landed");
    assert_eq!(outcome.stats.failed.len(), 1);
    assert!(outcome.stats.failed[0].0.ends_with("inside.txt"));
    assert_eq!(fx.read("ok-one.txt"), "before");
    assert_eq!(fx.read("ok-two.txt"), "before");
    assert_eq!(fx.read("locked/inside.txt"), "after", "this one did not");

    // The point of returning an outcome at all: the caller can still get back.
    assert!(
        store.manifest(outcome.safety.manifest_id()).is_ok(),
        "the safety point is reachable, which is what makes this recoverable"
    );
}
