//! The declared set: what survives a process, and what ages out.

#![allow(clippy::unwrap_used)]

use super::*;
use pretty_assertions::assert_eq;

const S: &str = "session-1";

/// The default, spelled once. Every test that is not *about* the window uses
/// it, so a change to the default cannot quietly change what they prove.
const DEFAULT: DeclaredWindow = DeclaredWindow::Turns(DECLARED_WINDOW_TURNS);

fn store() -> (tempfile::TempDir, DeclaredStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = DeclaredStore::open(dir.path().join("declared")).unwrap();
    (dir, store)
}

fn p(name: &str) -> PathBuf {
    PathBuf::from(format!("/ws/{name}"))
}

fn turns(n: u64) -> DeclaredWindow {
    DeclaredWindow::Turns(NonZeroU64::new(n).unwrap())
}

/// The whole point: a new process picks up what the last one declared.
#[test]
fn declarations_outlive_the_process_that_made_them() {
    let (dir, store) = store();
    store.declare(S, "turn-1", &[p("a.rs"), p("b.rs")]).unwrap();

    // A second handle on the same directory is what a resumed session gets.
    let reopened = DeclaredStore::open(dir.path().join("declared")).unwrap();
    assert_eq!(
        reopened.active(S, DEFAULT).unwrap(),
        BTreeSet::from([p("a.rs"), p("b.rs")])
    );
}

#[test]
fn a_session_that_never_declared_anything_yields_nothing() {
    let (_dir, store) = store();
    assert_eq!(
        store.active("never-used", DEFAULT).unwrap(),
        BTreeSet::new()
    );
    assert_eq!(store.all("never-used").unwrap(), BTreeSet::new());
}

/// Sessions do not see each other's declarations.
#[test]
fn declarations_are_per_session() {
    let (_dir, store) = store();
    store.declare(S, "turn-1", &[p("mine.rs")]).unwrap();
    store.declare("other", "turn-1", &[p("theirs.rs")]).unwrap();

    assert_eq!(
        store.active(S, DEFAULT).unwrap(),
        BTreeSet::from([p("mine.rs")])
    );
    assert_eq!(
        store.active("other", DEFAULT).unwrap(),
        BTreeSet::from([p("theirs.rs")])
    );
}

/// A path drops out of *observation* once the window passes it.
#[test]
fn a_path_ages_out_of_the_window() {
    let (_dir, store) = store();
    store.declare(S, "turn-0", &[p("old.rs")]).unwrap();
    for i in 1..=DECLARED_WINDOW_TURNS.get() + 1 {
        store
            .declare(S, &format!("turn-{i}"), &[p("recent.rs")])
            .unwrap();
    }

    let active = store.active(S, DEFAULT).unwrap();
    assert!(active.contains(&p("recent.rs")));
    assert!(!active.contains(&p("old.rs")), "past the window");

    // But it is not forgotten. The window governs what is *watched*, never
    // what can be restored — and a restore's safety scope must still look at
    // a path this session once observed, or it can never remove it again.
    assert!(store.all(S).unwrap().contains(&p("old.rs")));
}

/// The same turns, read through [`DeclaredWindow::Unlimited`]: nothing ages
/// out. This is the codex ancestor's behaviour, kept available as a setting
/// because how long an edit stays reversible is the host's product decision
/// and not this engine's (D25).
#[test]
fn an_unlimited_window_ages_nothing_out() {
    let (_dir, store) = store();
    store.declare(S, "turn-0", &[p("old.rs")]).unwrap();
    for i in 1..=DECLARED_WINDOW_TURNS.get() * 2 {
        store
            .declare(S, &format!("turn-{i}"), &[p("recent.rs")])
            .unwrap();
    }

    assert_eq!(
        store.active(S, DeclaredWindow::Unlimited).unwrap(),
        store.all(S).unwrap(),
        "unlimited saw less than the session has ever declared"
    );
}

/// `Turns(n)` counts *further* turns: a path declared during one turn is
/// watched through the next `n` and no more. The value that matters is one —
/// a window that reached no capture at all would be a setting whose only
/// effect is to look like it has one.
#[test]
fn a_window_of_n_turns_reaches_exactly_n_further_turns() {
    for n in [1, 2, 3] {
        let (_dir, store) = store();
        store.declare(S, "turn-0", &[p("edited.rs")]).unwrap();

        for i in 1..=n {
            store.note_turn(S, &format!("turn-{i}")).unwrap();
            assert!(
                store.active(S, turns(n)).unwrap().contains(&p("edited.rs")),
                "a window of {n} stopped short at turn {i}"
            );
        }

        store.note_turn(S, &format!("turn-{}", n + 1)).unwrap();
        assert!(
            !store.active(S, turns(n)).unwrap().contains(&p("edited.rs")),
            "a window of {n} reached turn {}",
            n + 1
        );
    }
}

/// **Passing no window behaves exactly as 0.3.1 did**, which is the whole
/// reason the default is 99 rather than the 100 its constant used to say: the
/// window became a parameter and its arithmetic gained the `+ 1` that makes
/// `Turns(n)` mean n further turns, and the default absorbed that so no
/// existing caller's captures moved.
///
/// The two numbers below are written out rather than derived from
/// [`DECLARED_WINDOW_TURNS`]. They are the boundary `cutoff = turns.len() -
/// 100` produced before any of this existed, and a test that recomputed them
/// from the constant would agree with a change to it — which is the one thing
/// it exists to catch.
#[test]
fn the_default_expires_on_the_turn_0_3_1_expired_on() {
    let (_dir, store) = store();
    store.declare(S, "turn-0", &[p("edited.rs")]).unwrap();

    for i in 1..=99 {
        store.note_turn(S, &format!("turn-{i}")).unwrap();
    }
    assert!(
        store.active(S, DEFAULT).unwrap().contains(&p("edited.rs")),
        "0.3.1 was still watching it at the 99th turn after the declaration"
    );

    store.note_turn(S, "turn-100").unwrap();
    assert!(
        !store.active(S, DEFAULT).unwrap().contains(&p("edited.rs")),
        "0.3.1 had stopped watching it by the 100th"
    );
}

/// Declaring a path again moves it forward, so a file the agent keeps editing
/// never ages out from under it.
#[test]
fn redeclaring_a_path_renews_it() {
    let (_dir, store) = store();
    store.declare(S, "turn-0", &[p("hot.rs")]).unwrap();
    for i in 1..=DECLARED_WINDOW_TURNS.get() {
        let turn = format!("turn-{i}");
        store.declare(S, &turn, &[p("hot.rs")]).unwrap();
    }

    assert!(store.active(S, DEFAULT).unwrap().contains(&p("hot.rs")));
}

/// Several declarations within one turn count as one turn. A session's log
/// holds several entries per turn, so counting entries rather than turns
/// would shrink the window unpredictably (D6's caveat).
#[test]
fn the_window_counts_turns_not_declarations() {
    let (_dir, store) = store();
    for i in 0..(DECLARED_WINDOW_TURNS.get() * 3) {
        store
            .declare(S, "turn-1", &[PathBuf::from(format!("/ws/f{i}.rs"))])
            .unwrap();
    }
    store.declare(S, "turn-2", &[p("later.rs")]).unwrap();

    assert_eq!(
        store.active(S, DEFAULT).unwrap().len(),
        (DECLARED_WINDOW_TURNS.get() * 3 + 1) as usize,
        "two turns of declarations, none of them past the window"
    );
}

#[test]
fn deleting_a_session_drops_its_declarations() {
    let (_dir, store) = store();
    store.declare(S, "turn-1", &[p("a.rs")]).unwrap();
    store.remove(S).unwrap();

    assert_eq!(store.active(S, DEFAULT).unwrap(), BTreeSet::new());
    // Idempotent: delete must not depend on anything being there first (D9).
    store.remove(S).unwrap();
}

/// A record from a build we do not understand is refused, not read as an
/// empty set — which would read as "this session declared nothing".
#[test]
fn a_record_from_an_unknown_build_is_refused() {
    let (dir, store) = store();
    store.declare(S, "turn-1", &[p("a.rs")]).unwrap();

    let path = dir
        .path()
        .join("declared")
        .join(format!("{}.json", crate::id::record_name(S)));
    let raw = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        raw.replace(
            &format!("\"version\": {}", crate::workspace::FORMAT_VERSION),
            "\"version\": 99",
        ),
    )
    .unwrap();

    assert!(matches!(
        store.active(S, DEFAULT),
        Err(SnapshotError::UnknownRecordVersion { .. })
    ));
}
