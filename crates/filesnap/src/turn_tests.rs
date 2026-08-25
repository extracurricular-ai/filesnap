//! The turn operations, which are now functions and so can be tested without
//! standing up a session-shaped object first.

#![allow(clippy::unwrap_used)]

use super::*;
use crate::fixture::Fixture;
use pretty_assertions::assert_eq;

const S: &str = "s1";

/// A root that neither contains nor sits under the turn's cwd is not this
/// session's workspace, and scanning it is the over-capture the feature exists
/// to avoid.
#[test]
fn unrelated_roots_are_dropped_and_cwd_stands_in() {
    // Real directories, because `scan_roots` canonicalizes what it returns.
    // Invented absolute paths cannot be resolved, and `/work` turned out to
    // exist on the Windows runner — so the test was asserting against
    // whatever that machine happened to have.
    let base = tempfile::tempdir().unwrap();
    let base = crate::scope::canonical_key(base.path());
    let work = base.join("work");
    let project = work.join("project");
    let elsewhere = base.join("elsewhere");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();

    let scope = TurnScope {
        cwd: project.clone(),
        roots: vec![elsewhere.clone(), work.clone()],
        hidden: HiddenFiles::Skip,
        limits: ScanLimits::default(),
    };
    assert_eq!(scope.scan_roots(), vec![work]);

    let orphan = TurnScope {
        roots: vec![elsewhere],
        ..scope
    };
    assert_eq!(
        orphan.scan_roots(),
        vec![project],
        "nothing to go on, so the turn's own directory scopes it"
    );
}

/// The ignore root is derived, so it is never absent.
///
/// It used to be recorded by the turn-start capture and read back by the edit
/// hook, which meant the filter matched nothing at all until the first capture
/// had run (C6).
#[test]
fn the_ignore_root_is_available_before_anything_has_been_captured() {
    let dir = tempfile::tempdir().unwrap();
    let root = crate::scope::canonical_key(dir.path());
    let scope = TurnScope::at(&root);
    assert_eq!(scope.ignore_root(), root);
}

/// A capture and a declare share no state, which is the whole of D38: two
/// separate processes reach the same result as one.
#[test]
fn a_declare_and_a_capture_share_only_the_store() {
    let fx = Fixture::new();
    fx.write("tracked.txt", "one");
    let outside = fx.data_dir().join("outside.cfg");
    std::fs::write(&outside, "before").unwrap();
    let scope = TurnScope::at(fx.workspace());

    // "Process" one: declare.
    let store = fx.store();
    let outcome = declare_edits(
        &store,
        S,
        "turn-1",
        &scope,
        vec![(outside.clone(), PreEditImage::Existed(b"before".to_vec()))],
    )
    .unwrap();
    assert_eq!(outcome.recorded, vec![outside.clone()]);
    drop(store);

    // "Process" two: capture, with nothing carried over but the store.
    let checkpoint = capture_turn(&fx.store(), S, "turn-1", &scope).unwrap();

    assert!(
        checkpoint
            .manifest
            .entries
            .contains_key(&outside.to_string_lossy().into_owned()),
        "the declared path was not picked up by a later process"
    );
}

/// An ignored path does not enter the store through the edit API either, and
/// the caller is told which ones were skipped rather than left to guess.
#[test]
fn a_declare_reports_what_the_ignore_rules_excluded() {
    let fx = Fixture::new();
    fx.write(crate::SNAPSHOT_IGNORE_FILENAME, "*.key\n");
    let secret = fx.write("private.key", "material");
    let ordinary = fx.write("main.rs", "fn main() {}");

    let outcome = declare_edits(
        &fx.store(),
        S,
        "turn-1",
        &TurnScope::at(fx.workspace()),
        vec![
            (secret.clone(), PreEditImage::Existed(b"material".to_vec())),
            (
                ordinary.clone(),
                PreEditImage::Existed(b"fn main() {}".to_vec()),
            ),
        ],
    )
    .unwrap();

    assert_eq!(outcome.ignored, vec![secret.clone()]);
    assert_eq!(outcome.recorded, vec![ordinary]);
    assert!(
        !fx.store()
            .tracked_paths(S)
            .unwrap()
            .contains(&secret.to_string_lossy().into_owned()),
        "an ignored path reached the store through the edit API"
    );
}

/// Capturing notes the turn even when nothing was declared, so the declared
/// set's window counts turns rather than only the turns that declared.
#[test]
fn a_capture_advances_the_declared_window() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    let scope = TurnScope::at(fx.workspace());
    let early = fx.path("edited-once.txt");
    std::fs::write(&early, "before").unwrap();

    declare_edits(
        &fx.store(),
        S,
        "turn-0",
        &scope,
        vec![(early.clone(), PreEditImage::Existed(b"before".to_vec()))],
    )
    .unwrap();

    for i in 1..=crate::declared::DECLARED_WINDOW_TURNS {
        capture_turn(&fx.store(), S, &format!("turn-{i}"), &scope).unwrap();
    }

    assert!(
        !fx.store().declared_paths(S).unwrap().contains(&early),
        "captures did not advance the window"
    );
}
