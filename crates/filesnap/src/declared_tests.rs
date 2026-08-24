//! The declared set: what survives a process, and what ages out.

#![allow(clippy::unwrap_used)]

use super::*;
use pretty_assertions::assert_eq;

const S: &str = "session-1";

fn store() -> (tempfile::TempDir, DeclaredStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = DeclaredStore::open(dir.path().join("declared")).unwrap();
    (dir, store)
}

fn p(name: &str) -> PathBuf {
    PathBuf::from(format!("/ws/{name}"))
}

/// The whole point: a new process picks up what the last one declared.
#[test]
fn declarations_outlive_the_process_that_made_them() {
    let (dir, store) = store();
    store.declare(S, "turn-1", &[p("a.rs"), p("b.rs")]).unwrap();

    // A second handle on the same directory is what a resumed session gets.
    let reopened = DeclaredStore::open(dir.path().join("declared")).unwrap();
    assert_eq!(
        reopened.active(S).unwrap(),
        BTreeSet::from([p("a.rs"), p("b.rs")])
    );
}

#[test]
fn a_session_that_never_declared_anything_yields_nothing() {
    let (_dir, store) = store();
    assert_eq!(store.active("never-used").unwrap(), BTreeSet::new());
    assert_eq!(store.all("never-used").unwrap(), BTreeSet::new());
}

/// Sessions do not see each other's declarations.
#[test]
fn declarations_are_per_session() {
    let (_dir, store) = store();
    store.declare(S, "turn-1", &[p("mine.rs")]).unwrap();
    store.declare("other", "turn-1", &[p("theirs.rs")]).unwrap();

    assert_eq!(store.active(S).unwrap(), BTreeSet::from([p("mine.rs")]));
    assert_eq!(
        store.active("other").unwrap(),
        BTreeSet::from([p("theirs.rs")])
    );
}

/// A path drops out of *observation* once the window passes it.
#[test]
fn a_path_ages_out_of_the_window() {
    let (_dir, store) = store();
    store.declare(S, "turn-0", &[p("old.rs")]).unwrap();
    for i in 1..=DECLARED_WINDOW_TURNS {
        store
            .declare(S, &format!("turn-{i}"), &[p("recent.rs")])
            .unwrap();
    }

    let active = store.active(S).unwrap();
    assert!(active.contains(&p("recent.rs")));
    assert!(!active.contains(&p("old.rs")), "past the window");

    // But it is not forgotten. The window governs what is *watched*, never
    // what can be restored — and a restore's safety scope must still look at
    // a path this session once observed, or it can never remove it again.
    assert!(store.all(S).unwrap().contains(&p("old.rs")));
}

/// Declaring a path again moves it forward, so a file the agent keeps editing
/// never ages out from under it.
#[test]
fn redeclaring_a_path_renews_it() {
    let (_dir, store) = store();
    store.declare(S, "turn-0", &[p("hot.rs")]).unwrap();
    for i in 1..=DECLARED_WINDOW_TURNS {
        let turn = format!("turn-{i}");
        store.declare(S, &turn, &[p("hot.rs")]).unwrap();
    }

    assert!(store.active(S).unwrap().contains(&p("hot.rs")));
}

/// Several declarations within one turn count as one turn. A session's log
/// holds several entries per turn, so counting entries rather than turns
/// would shrink the window unpredictably (D6's caveat).
#[test]
fn the_window_counts_turns_not_declarations() {
    let (_dir, store) = store();
    for i in 0..(DECLARED_WINDOW_TURNS * 3) {
        store
            .declare(S, "turn-1", &[PathBuf::from(format!("/ws/f{i}.rs"))])
            .unwrap();
    }
    store.declare(S, "turn-2", &[p("later.rs")]).unwrap();

    assert_eq!(
        store.active(S).unwrap().len(),
        (DECLARED_WINDOW_TURNS * 3 + 1) as usize,
        "two turns of declarations, none of them past the window"
    );
}

#[test]
fn deleting_a_session_drops_its_declarations() {
    let (_dir, store) = store();
    store.declare(S, "turn-1", &[p("a.rs")]).unwrap();
    store.remove(S).unwrap();

    assert_eq!(store.active(S).unwrap(), BTreeSet::new());
    // Idempotent: delete must not depend on anything being there first (D9).
    store.remove(S).unwrap();
}

/// A record from a build we do not understand is refused, not read as an
/// empty set — which would read as "this session declared nothing".
#[test]
fn a_record_from_an_unknown_build_is_refused() {
    let (dir, store) = store();
    store.declare(S, "turn-1", &[p("a.rs")]).unwrap();

    let path = dir.path().join("declared").join(format!("{S}.json"));
    let raw = fs::read_to_string(&path).unwrap();
    fs::write(&path, raw.replace("\"version\": 1", "\"version\": 99")).unwrap();

    assert!(matches!(
        store.active(S),
        Err(SnapshotError::UnknownRecordVersion { .. })
    ));
}
