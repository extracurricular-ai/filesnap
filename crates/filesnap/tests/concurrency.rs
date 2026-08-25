//! The race D18 exists for, and the deadlock a careless fix would introduce.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::Barrier;

use filesnap::RestoreKind;
use filesnap::SnapshotError;
use filesnap::WorkspaceStore;
use filesnap::fixture::Fixture;
use filesnap::fixture::no_rules;
use pretty_assertions::assert_eq;

/// **Two concurrent invocations of one session do not lose a log entry.**
///
/// A log append is read-modify-write: load, push, rewrite. Without the lock
/// the second writer reads before the first has written and then overwrites
/// it, so one of the two turns is simply gone — and nothing anywhere reports
/// that it happened.
///
/// Threads rather than processes because `flock` is per open-file-description:
/// two `File` handles in one process contend exactly as two processes do.
#[test]
fn concurrent_captures_for_one_session_keep_every_turn() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    let data = fx.data_dir().to_path_buf();
    let ws = fx.workspace().to_path_buf();

    const WRITERS: usize = 8;
    let gate = Arc::new(Barrier::new(WRITERS));
    let mut handles = Vec::new();
    for i in 0..WRITERS {
        let (data, ws, gate) = (data.clone(), ws.clone(), Arc::clone(&gate));
        // Opened *before* the thread, so a failure here fails the test.
        // Inside the closure it would panic one thread while the other seven
        // blocked on a `Barrier` that can never be reached — and a `Barrier`
        // has no timeout and no poisoning, so the test would hang rather than
        // fail. With no `timeout-minutes` that used to cost the CI default of
        // 360 minutes per platform instead of a red X.
        let store = WorkspaceStore::open(&data, &ws).unwrap();
        handles.push(std::thread::spawn(move || {
            let files = vec![ws.join("a.txt")];
            gate.wait();
            store.checkpoint("s1", &format!("turn-{i}"), files)
        }));
    }

    let mut landed = 0;
    let mut busy = 0;
    for handle in handles {
        match handle.join().unwrap() {
            Ok(_) => landed += 1,
            // A legitimate outcome: the caller is told to retry rather than
            // having its write silently dropped.
            Err(SnapshotError::SessionBusy { .. }) => busy += 1,
            Err(err) => panic!("unexpected: {err:?}"),
        }
    }

    let history = WorkspaceStore::open(&data, &ws)
        .unwrap()
        .thread_history("s1")
        .unwrap();
    assert_eq!(
        history.len(),
        landed,
        "{landed} captures reported success, {busy} were told the session was busy, \
         but the log holds {} entries",
        history.len()
    );
    assert!(landed > 0);
}

/// Two *different* sessions are not serialized against each other. Nothing
/// wider than one session is locked, deliberately (D18).
#[test]
fn different_sessions_do_not_wait_on_each_other() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    let data = fx.data_dir().to_path_buf();
    let ws = fx.workspace().to_path_buf();

    const SESSIONS: usize = 8;
    let gate = Arc::new(Barrier::new(SESSIONS));
    let mut handles = Vec::new();
    for i in 0..SESSIONS {
        let (data, ws, gate) = (data.clone(), ws.clone(), Arc::clone(&gate));
        // See above: nothing fallible inside the closure before the barrier.
        let store = WorkspaceStore::open(&data, &ws).unwrap();
        handles.push(std::thread::spawn(move || {
            let files = vec![ws.join("a.txt")];
            gate.wait();
            store.checkpoint(&format!("s{i}"), "turn-1", files)
        }));
    }

    for handle in handles {
        handle
            .join()
            .unwrap()
            .expect("an unrelated session was blocked");
    }
}

/// A restore takes its session's lock and then captures a safety point under
/// the same session. A second acquire there would block against the first —
/// `flock` is per open-file-description, so a process contends with itself —
/// and the restore would fail after burning the whole budget.
#[test]
fn a_restore_does_not_deadlock_against_its_own_safety_capture() {
    let fx = Fixture::new();
    fx.write("a.txt", "before");
    fx.capture("s1", "turn-1");
    fx.write("a.txt", "after");

    let store = fx.store();
    let target = store.target_for_turn("turn-1").unwrap().unwrap();

    let started = std::time::Instant::now();
    store
        .restore_to(
            "s1",
            &target,
            RestoreKind::Rewind {
                undo_for: Some("s1"),
            },
            fx.restore_scope("s1"),
            &no_rules(),
        )
        .expect("the restore deadlocked against its own lock");

    assert_eq!(fx.read("a.txt"), "before");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the restore waited on a lock it already held"
    );
}

/// Deleting takes each session's lock in turn, so the sessions in one batch
/// are handled independently.
///
/// The refusal itself — a session another invocation is holding — needs a
/// lock held *across* the call, which no public API can do: every operation
/// that takes a session lock also releases it before returning. An earlier
/// version of this test spawned a thread and synchronised on barriers to
/// suggest otherwise, but the holder's capture had already finished by the
/// time the barrier opened, and a lock failure lands in `refused` rather
/// than `incomplete`, which was the only field asserted. It passed with
/// session locking removed entirely.
///
/// The refusal is covered in
/// `store_tests::deleting_refuses_a_session_another_invocation_is_using`,
/// which reaches the lock directly.
#[test]
fn deleting_handles_each_session_in_a_batch_independently() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture("one", "turn-1");
    fx.capture("two", "turn-2");

    let outcome = fx
        .store()
        .delete_sessions(&["one".to_string(), "two".to_string()]);

    assert!(outcome.refused.is_empty(), "{:?}", outcome.refused);
    assert!(outcome.incomplete.is_empty(), "{:?}", outcome.incomplete);
    assert!(!fx.store().session_exists("one"));
    assert!(!fx.store().session_exists("two"));
}
