//! Restore residue: cleared by the next restore, and findable in a workspace
//! that never gets one.
//!
//! A write is a temp file plus a rename, so a process killed between the two
//! leaves a stray in the **user's own project** — somewhere store collection
//! can never reach, because it knows the store and not the workspace (D21).

#![allow(clippy::unwrap_used)]

use std::time::Duration;
use std::time::SystemTime;

use filesnap::RESTORE_TMP_SUFFIX;
use filesnap::RestoreKind;
use filesnap::fixture::Fixture;
use filesnap::fixture::no_rules;
use filesnap::residue_in;
use pretty_assertions::assert_eq;

const SESSION: &str = "s1";

/// Backdate past the residue grace window.
fn age(path: &std::path::Path) {
    let when = SystemTime::now() - Duration::from_secs(3600);
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(when))
        .unwrap();
}

fn strand(fx: &Fixture, rel: &str) -> std::path::PathBuf {
    let path = fx.path(&format!("{rel}{RESTORE_TMP_SUFFIX}"));
    std::fs::write(&path, b"half a restore").unwrap();
    path
}

/// A fresh stray is left alone: another restore may be holding it right now,
/// and unlinking it mid-write would fail that restore for no reason.
#[test]
fn residue_still_in_use_is_not_reported() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    strand(&fx, "a.txt");

    assert!(residue_in(fx.workspace()).is_empty());
}

#[test]
fn settled_residue_is_reported_for_inspection() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    let stray = strand(&fx, "a.txt");
    age(&stray);

    assert_eq!(residue_in(fx.workspace()), vec![stray]);
}

/// The self-healing half: the next restore into that directory clears it,
/// without the user having to know the command exists.
#[test]
fn a_restore_clears_the_residue_in_the_directories_it_writes() {
    let fx = Fixture::new();
    fx.write("a.txt", "before");
    fx.capture(SESSION, "turn-1");
    fx.write("a.txt", "after");

    let stray = strand(&fx, "a.txt");
    age(&stray);
    let untouched = strand(&fx, "elsewhere.txt");
    age(&untouched);

    let store = fx.store();
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

    assert!(!stray.exists(), "the restore swept its own leavings");
    assert_eq!(fx.read("a.txt"), "before", "and still did the restore");
    // Same directory, so this one goes too — the sweep is per-directory,
    // which is the only unit it can be: residue names no owner.
    assert!(!untouched.exists());
}

/// A real file that merely ends in the suffix is not residue if it is not
/// old, and the sweep never touches anything else at all.
///
/// The age half is the one that matters here. `residue_in`'s grace window is
/// covered by `residue_still_in_use_is_not_reported`, but that is the
/// *reporting* path; this is the restore's own per-directory sweep, and a
/// concurrent restore's half-written temp file sits in exactly the directory
/// this one is about to clear.
#[test]
fn nothing_but_residue_is_removed() {
    let fx = Fixture::new();
    fx.write("a.txt", "before");
    fx.write("keep.txt", "mine");
    fx.capture(SESSION, "turn-1");
    fx.write("a.txt", "after");

    // Suffixed, and deliberately not aged: another restore may be writing it
    // at this instant.
    let fresh = strand(&fx, "b.txt");

    let store = fx.store();
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

    assert!(fx.exists("keep.txt"));
    assert_eq!(fx.read("keep.txt"), "mine");
    assert!(
        fresh.exists(),
        "a stray younger than the grace window was swept out from under \
         whatever is writing it"
    );
}
