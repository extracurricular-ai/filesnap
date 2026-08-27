//! Unit tests for the facade, concentrating on branches an end-to-end
//! narrative never reaches — because a scenario that reaches a failure branch
//! is a scenario that failed, and nobody writes those as a story.
//!
//! This module exists because `store.rs` had 645 lines and no tests at all,
//! and the defect audit found five of its problems here. It is a child module
//! of `store`, so it can reach the partition path directly to corrupt a
//! record. That is deliberate and is the reason these are not integration
//! tests: `Fixture` refuses to know the layout, and proving what happens to a
//! damaged record means writing one.

#![allow(clippy::unwrap_used)]

use super::*;
use crate::fixture::Fixture;
use crate::fixture::no_rules;
use crate::fixture::rules_for;
use pretty_assertions::assert_eq;

const S: &str = "session-1";

/// Where a session's log lives. Only a test that must damage one needs this.
fn log_path(store: &WorkspaceStore, session: &str) -> PathBuf {
    // The digest, not the id: records are named for the hash of their id so
    // that two ids differing only in case cannot share one file.
    store
        .partition
        .join("refs")
        .join(format!("{}.json", crate::id::record_name(session)))
}

/// A session whose log cannot be read is left **exactly as it was**, and one
/// such session does not stop the others from being deleted.
///
/// The tempting alternative is to swallow the read error and remove the log
/// anyway. Nothing then enters the doomed set, so nothing is reclaimed — and
/// the only record of what the session held is gone, so the call cannot even
/// be retried. A delete that reports success having done neither thing is
/// worse than one that says it could not.
///
/// Reclamation is the part that does stop. See the test below.
#[test]
fn a_session_whose_log_cannot_be_read_is_refused_not_half_deleted() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture(S, "turn-1");
    fx.capture("healthy", "turn-2");

    let store = fx.store();
    let log = log_path(&store, S);
    // Truncated mid-write is what this looks like in the wild.
    std::fs::write(&log, b"{\"version\":1,\"entr").unwrap();

    let outcome = store.delete_sessions(&[S.to_string(), "healthy".to_string()]);

    assert!(
        outcome.refused.iter().any(|(id, _)| id == S),
        "{:?}",
        outcome.refused
    );
    assert!(log.exists(), "the refused session keeps its records");
    assert!(
        !store.session_exists("healthy"),
        "one unreadable session does not block deleting the others"
    );
}

/// An unreadable log defers **reclamation** without failing the deletion, and
/// without guessing that anything is dead.
///
/// Liveness is computed by reading every log there is. Skipping one that will
/// not parse would leave its manifests looking unreferenced, and the prune
/// would remove them — converting a damaged log into snapshots that are
/// actually gone, for a session nobody asked to delete. So an incomplete
/// answer licenses no removal at all.
///
/// It is not an error either. Unreachability is what delete promised and it
/// is already done; reclamation was never part of its success criterion
/// (VIII.3), so the bytes simply wait for the next collection.
#[test]
fn an_unreadable_log_defers_reclamation_without_failing_the_delete() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture(S, "turn-1");
    // Different content, or both sessions share one content-addressed
    // manifest and the survivor's turn entry vouches for it — which keeps it
    // alive on a *complete* pass too, so nothing here would be about
    // deferral.
    fx.write("a.txt", "two");
    fx.capture("healthy", "turn-2");
    // Past the grace window, or nothing on this store is removable at all and
    // the count below is guaranteed by age rather than by coverage.
    fx.age_store();

    let store = fx.store();
    std::fs::write(log_path(&store, S), b"not json at all").unwrap();

    let outcome = store.delete_sessions(&["healthy".to_string()]);

    assert!(!store.session_exists("healthy"), "the promise it can keep");
    assert_eq!(
        outcome.reclaimed.manifests_removed, 0,
        "the doomed manifest is unreferenced and settled: only the incomplete \
         answer keeps it"
    );
    assert!(
        outcome.sweep_error.is_none(),
        "deferring is not failing — delete has no preconditions (D9): {:?}",
        outcome.sweep_error
    );
    assert!(outcome.refused.is_empty());
}

/// Deleting takes the undo record as well as the log.
///
/// They are two files with two lifetimes: a session's log is what it
/// captured, its restore log is what was handed *to* it. Leaving the second
/// behind strands a GC root, because the sweep reads every file under
/// `restores/` without asking whether the session named still exists.
#[test]
fn deleting_takes_the_undo_record_with_the_log() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture(S, "turn-1");

    let store = fx.store();
    let target = store.target_for_turn("turn-1").unwrap().unwrap();
    store
        .restore_to(
            S,
            &target,
            RestoreKind::Rewind { undo_for: Some(S) },
            fx.restore_scope(S),
            &no_rules(),
        )
        .unwrap();
    assert!(store.last_restore_target(S).unwrap().is_some());

    store.delete_sessions(&[S.to_string()]);

    assert!(!store.session_exists(S));
    assert_eq!(
        store.last_restore_target(S).unwrap(),
        None,
        "the undo record goes too, or it pins its manifests as a root for good"
    );
}

#[test]
fn an_empty_delete_touches_nothing() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture(S, "turn-1");

    let outcome = fx.store().delete_sessions(&[]);

    assert_eq!(outcome.reclaimed, GcStats::default());
    assert!(outcome.refused.is_empty());
    assert!(fx.store().session_exists(S));
}

/// The undo stack drops its **oldest** records when full.
///
/// Draining the wrong end is invisible in every ordinary scenario: fewer
/// rewinds than the cap and the two behave identically. Past the cap the
/// wrong end makes the next undo reverse the *first* rewind rather than the
/// most recent, discarding every state in between while reporting success.
#[test]
fn a_full_undo_stack_forgets_the_oldest_rewind_not_the_newest() {
    let fx = Fixture::new();
    fx.write("a.txt", "v0");
    let store = fx.store();
    fx.capture(S, "turn-0");
    let origin = store.target_for_turn("turn-0").unwrap().unwrap();

    // Each pass leaves a distinct state and rewinds it away, so the undo
    // record pushed on pass i is the only route back to "v{i}".
    let passes = crate::refs::MAX_RESTORE_HISTORY + 3;
    for i in 1..=passes {
        fx.write("a.txt", format!("v{i}"));
        store
            .restore_to(
                S,
                &origin,
                RestoreKind::Rewind { undo_for: Some(S) },
                fx.restore_scope(S),
                &no_rules(),
            )
            .unwrap();
    }
    assert_eq!(fx.read("a.txt"), "v0");

    // Undoing returns to the state the newest rewind replaced.
    let undo = store.last_restore_target(S).unwrap().unwrap();
    store
        .restore_to(
            S,
            &undo,
            RestoreKind::Undo { spending: S },
            fx.restore_scope(S),
            &no_rules(),
        )
        .unwrap();

    assert_eq!(
        fx.read("a.txt"),
        format!("v{passes}"),
        "the record on top is the newest rewind's, not one from beyond the cut"
    );
}

/// A turn resolves to the *last* thing written for it, so a supplemental
/// pre-edit attach becomes the state that turn restores to.
#[test]
fn a_turn_resolves_to_its_most_complete_capture() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    let first = fx.capture(S, "turn-1");

    // A path the scan never covered, because it did not exist when the scan
    // ran. The edit hook extends the same turn with it.
    let late = fx.write("late.txt", "pre");
    let supplemental = fx
        .store()
        .attach_pre_edit(
            S,
            "turn-1",
            &late.to_string_lossy(),
            &PreEditImage::Existed(b"pre".to_vec()),
        )
        .unwrap()
        .expect("a path outside the scan attaches");

    assert_ne!(supplemental, first.id);
    assert_eq!(
        fx.store()
            .target_for_turn("turn-1")
            .unwrap()
            .unwrap()
            .manifest_id(),
        supplemental,
        "the turn resolves to the extended capture, not the original"
    );
}

#[test]
fn an_unknown_turn_resolves_to_nothing() {
    let fx = Fixture::new();
    assert_eq!(fx.store().target_for_turn("never-happened").unwrap(), None);
}

/// `attach_pre_edit` returning `Ok(None)` is an ordinary outcome, not a
/// failure: the scan already covered this path, so there is nothing to add.
#[test]
fn attaching_a_path_the_scan_already_covered_adds_nothing() {
    let fx = Fixture::new();
    let a = fx.write("a.txt", "one");
    fx.capture(S, "turn-1");

    assert_eq!(
        fx.store()
            .attach_pre_edit(
                S,
                "turn-1",
                &a.to_string_lossy(),
                &PreEditImage::Existed(b"one".to_vec()),
            )
            .unwrap(),
        None
    );
}

/// A path the turn created is tombstoned once, and the tombstone is what
/// licenses a rewind to remove it again.
#[test]
fn a_created_path_is_tombstoned_once() {
    let fx = Fixture::new();
    let store = fx.store();
    let born = fx.path("born.txt");
    let key = born.to_string_lossy().into_owned();

    let id = store
        .attach_pre_edit(S, "turn-1", &key, &PreEditImage::DidNotExist)
        .unwrap()
        .expect("a created path records that it did not exist");
    assert!(store.manifest(&id).unwrap().absent.contains(&key));

    assert_eq!(
        store
            .attach_pre_edit(S, "turn-1", &key, &PreEditImage::DidNotExist)
            .unwrap(),
        None,
        "the second attach says nothing the first did not"
    );
}

/// `tracked_paths` is the union of everything observed, tombstones included.
///
/// It is half of what builds a restore's safety scope, and a path missing
/// from it is a path no plan can ever delete: the safety capture never looks
/// there, so `current.entries` lacks it, and `plan_restore` needs both sides.
#[test]
fn tracked_paths_includes_what_was_looked_for_and_not_found() {
    let fx = Fixture::new();
    fx.write("present.txt", "here");
    fx.capture(S, "turn-1");

    let gone = fx.path("gone.txt").to_string_lossy().into_owned();
    fx.store()
        .attach_pre_edit(S, "turn-1", &gone, &PreEditImage::DidNotExist)
        .unwrap();

    let paths = fx.store().tracked_paths(S).unwrap();
    assert!(paths.contains(&fx.key("present.txt")));
    assert!(
        paths.contains(&gone),
        "a tombstone is an observation, and the safety scope needs it"
    );
}

/// Disk usage reports this workspace's records, not the content they name.
///
/// Content is shared with every other workspace, so charging it to one would
/// report the same bytes once per reference — a dashboard that adds up to
/// several times the true size.
#[test]
fn disk_usage_measures_records_rather_than_content() {
    let fx = Fixture::new();
    fx.write("big.txt", "x".repeat(200_000));
    fx.capture(S, "turn-1");

    let records = fx.store().records_disk_usage().unwrap();
    assert!(records > 0, "the manifest and log are real files");
    assert!(
        records < 200_000,
        "the 200 kB of content is not charged to the partition: {records}"
    );
}

/// Inheriting a log copies entries through the named turn and nothing after.
#[test]
fn inheriting_a_log_stops_at_the_named_turn() {
    let fx = Fixture::new();
    let store = fx.store();
    for i in 0..4 {
        fx.write("a.txt", format!("v{i}"));
        fx.capture(S, &format!("turn-{i}"));
    }

    assert_eq!(store.inherit_log(S, "fork", "turn-1").unwrap(), 2);
    let inherited: Vec<String> = store
        .thread_history("fork")
        .unwrap()
        .into_iter()
        .map(|(entry, _)| entry.turn_id)
        .collect();
    assert_eq!(inherited, vec!["turn-0".to_string(), "turn-1".to_string()]);
}

/// A fork from a turn the source never had inherits nothing — but still
/// exists.
///
/// The distinction matters to `session_exists`, which is how a caller tells a
/// session that has captured nothing yet from one that was never started.
#[test]
fn a_fork_from_an_unknown_turn_is_empty_but_real() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture(S, "turn-1");

    let store = fx.store();
    assert_eq!(store.inherit_log(S, "fork", "never-happened").unwrap(), 0);
    assert!(store.session_exists("fork"));
    assert!(store.thread_history("fork").unwrap().is_empty());
}

/// Nothing has moved right after a rewind, so there is no conflict to report.
/// An undo with nothing to undo is likewise quiet rather than an error.
#[test]
fn undo_conflicts_are_empty_when_nothing_moved() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture(S, "turn-1");
    let store = fx.store();

    assert!(
        store.undo_conflicts(S, &no_rules()).unwrap().is_empty(),
        "no rewind, nothing to conflict with"
    );

    fx.write("a.txt", "two");
    let target = store.target_for_turn("turn-1").unwrap().unwrap();
    store
        .restore_to(
            S,
            &target,
            RestoreKind::Rewind { undo_for: Some(S) },
            fx.restore_scope(S),
            &no_rules(),
        )
        .unwrap();

    assert!(store.undo_conflicts(S, &no_rules()).unwrap().is_empty());
}

/// A file changed after the rewind is reported, because undoing would
/// overwrite that change without mentioning it.
///
/// The undo records are per-session but the files are not, so this is the
/// only thing standing between a concurrent edit and silent loss.
#[test]
fn a_change_made_after_a_rewind_is_reported_as_a_conflict() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture(S, "turn-1");
    let store = fx.store();

    fx.write("a.txt", "two");
    let target = store.target_for_turn("turn-1").unwrap().unwrap();
    store
        .restore_to(
            S,
            &target,
            RestoreKind::Rewind { undo_for: Some(S) },
            fx.restore_scope(S),
            &no_rules(),
        )
        .unwrap();

    // Somebody else edits the file the rewind just wrote.
    fx.write("a.txt", "three");

    assert_eq!(
        store.undo_conflicts(S, &no_rules()).unwrap(),
        vec![fx.key("a.txt")]
    );
    assert!(
        store
            .undo_conflicts(S, &rules_for(fx.workspace(), "a.txt"))
            .unwrap()
            .is_empty(),
        "a protected path is not a conflict, because an undo would not touch it"
    );
}

/// A declared path is still watched by a **new** tracker on the same session.
///
/// This is D25's whole point. The set used to live only in memory, so a
/// session resuming in another process silently stopped watching everything
/// it had edited — silently, because the manifests already written stayed
/// perfectly valid. What was lost was future observation.
#[test]
fn a_declared_path_survives_the_process_that_declared_it() {
    let fx = Fixture::new();
    let outside = fx.path("declared-by-edit.txt");
    std::fs::write(&outside, "one").unwrap();

    fx.store()
        .declare_paths(S, "turn-1", std::slice::from_ref(&outside))
        .unwrap();

    // A fresh handle is what a resumed session gets.
    assert!(fx.store().declared_paths(S).unwrap().contains(&outside));
}

/// It is still in the safety scope after ageing out of the window.
///
/// The window governs what future captures *watch*, never what a restore may
/// touch. A path missing from `tracked_paths` is one no plan can ever remove,
/// so ageing out must not quietly make a file unremovable.
#[test]
fn a_path_past_the_window_is_still_in_the_safety_scope() {
    let fx = Fixture::new();
    let old = fx.path("edited-long-ago.txt");
    let store = fx.store();
    store
        .declare_paths(S, "turn-0", std::slice::from_ref(&old))
        .unwrap();
    for i in 1..=crate::declared::DECLARED_WINDOW_TURNS {
        store
            .declare_paths(S, &format!("turn-{i}"), &[fx.path("recent.txt")])
            .unwrap();
    }

    assert!(
        !store.declared_paths(S).unwrap().contains(&old),
        "no longer watched"
    );
    assert!(
        store
            .tracked_paths(S)
            .unwrap()
            .contains(&old.to_string_lossy().into_owned()),
        "but still observed, so a restore can still act on it"
    );
}

/// Deleting a session takes its declared set with it — a third file under a
/// third lifetime, and one left behind keeps naming paths nothing owns.
#[test]
fn deleting_a_session_drops_its_declared_set() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture(S, "turn-1");
    let store = fx.store();
    store
        .declare_paths(S, "turn-1", &[fx.path("edited.txt")])
        .unwrap();

    store.delete_sessions(&[S.to_string()]);
    assert_eq!(store.declared_paths(S).unwrap(), Default::default());
}

/// **An ordinary rewind-then-undo reports no conflicts.**
///
/// A file the rewind *recreated* is absent in the safety capture and present
/// now, which the conflict check read as "someone put this back" — on every
/// round trip, for the file the undo was about to remove on purpose. Crying
/// wolf trains the reader to ignore the warning, and then the one real
/// conflict is ignored too, which is worse than not warning at all.
#[test]
fn a_clean_round_trip_reports_nothing_moved() {
    let fx = Fixture::new();
    fx.write("kept.txt", "v1");
    fx.write("deleted-later.txt", "here");
    fx.capture(S, "turn-1");

    // The agent's turn: change one file, remove another, add a third.
    fx.write("kept.txt", "v2");
    fx.remove("deleted-later.txt");
    fx.write("added.txt", "new");

    let store = fx.store();
    let target = store.target_for_turn("turn-1").unwrap().unwrap();
    store
        .restore_to(
            S,
            &target,
            RestoreKind::Rewind { undo_for: Some(S) },
            fx.restore_scope(S),
            &no_rules(),
        )
        .unwrap();
    assert_eq!(
        fx.read("deleted-later.txt"),
        "here",
        "the rewind put it back"
    );

    assert_eq!(
        store.undo_conflicts(S, &no_rules()).unwrap(),
        Vec::<String>::new(),
        "nothing moved; the rewind's own work was reported as a conflict"
    );
}

/// And the check still fires on a real one: a file the rewind recreated, which
/// somebody then edited.
#[test]
fn editing_what_the_rewind_recreated_is_a_real_conflict() {
    let fx = Fixture::new();
    fx.write("a.txt", "original");
    fx.capture(S, "turn-1");
    fx.remove("a.txt");

    let store = fx.store();
    let target = store.target_for_turn("turn-1").unwrap().unwrap();
    store
        .restore_to(
            S,
            &target,
            RestoreKind::Rewind { undo_for: Some(S) },
            fx.restore_scope(S),
            &no_rules(),
        )
        .unwrap();

    // Somebody edits the file the rewind put back. An undo would delete it.
    fx.write("a.txt", "someone else's work");

    assert_eq!(
        store.undo_conflicts(S, &no_rules()).unwrap(),
        vec![fx.key("a.txt")]
    );
}

/// **Two ids differing only in case are two sessions, on every filesystem.**
///
/// Records are named for the hash of their id rather than for the id, because
/// APFS and NTFS are case-insensitive by default: `Session-A` and `session-a`
/// were one file there and two on ext4, so two conversations shared one log
/// and one undo stack on two of the three platforms — and deleting either
/// destroyed the other's history. That is the collision D7 says a *mapping*
/// must never cause, arriving by way of the filesystem instead.
///
/// The digest is lowercase hex, so ids that differ only in case produce names
/// that differ in more than case. This passes on Linux either way; it is the
/// other two platforms it exists for, and CI runs there.
#[test]
fn ids_differing_only_in_case_do_not_share_a_record() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    let store = fx.store();

    store
        .checkpoint("Session-A", "Turn-1", fx.all_files())
        .unwrap();
    fx.write("a.txt", "two");
    store
        .checkpoint("session-a", "turn-1", fx.all_files())
        .unwrap();

    assert_eq!(store.thread_history("Session-A").unwrap().len(), 1);
    assert_eq!(store.thread_history("session-a").unwrap().len(), 1);

    let mut sessions = store.sessions().unwrap();
    sessions.sort();
    assert_eq!(
        sessions,
        vec!["Session-A".to_string(), "session-a".to_string()]
    );

    // And the turns are distinct too, which is what a rewind resolves.
    let upper = store.target_for_turn("Turn-1").unwrap().unwrap();
    let lower = store.target_for_turn("turn-1").unwrap().unwrap();
    assert_ne!(upper, lower, "one turn resolved to the other's state");

    // Deleting one leaves the other whole. This was the destructive half.
    store.delete_sessions(&["session-a".to_string()]);
    assert!(!store.session_exists("session-a"));
    assert!(
        store.session_exists("Session-A"),
        "deleting one case destroyed the other's history"
    );
}

/// The id is in the record, so enumeration reports what a caller can use.
///
/// With the filename a digest, a listing of the directory says nothing. Had
/// `sessions()` kept deriving ids from filenames it would have reported
/// hashes — names no other API accepts.
#[test]
fn sessions_are_reported_by_id_not_by_the_name_on_disk() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    let store = fx.store();
    store
        .checkpoint("my.session-1", "turn-1", fx.all_files())
        .unwrap();

    assert_eq!(store.sessions().unwrap(), vec!["my.session-1".to_string()]);
    // The reported id round-trips through the API that consumes it.
    assert_eq!(store.thread_history("my.session-1").unwrap().len(), 1);
}

/// **One file has one manifest key, however the caller spelled the path.**
///
/// The partition key is canonicalized so two spellings of a directory are one
/// partition — `/var` → `/private/var` is the ordinary case on macOS, not an
/// exotic one. The contents were not, so the partition was
/// spelling-independent while its keys were not: the same file reached through
/// a symlinked root got two keys, and since keys are compared as strings the
/// stat cache never hit across a change of spelling, a restore rewrote content
/// that already matched, and `undo_conflicts` could not warn about a file it
/// was about to overwrite.
#[cfg(unix)]
#[test]
fn one_file_has_one_key_whichever_spelling_reaches_it() {
    let fx = Fixture::new();
    let real = fx.path("real");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("a.txt"), "one").unwrap();
    let linked = fx.path("linked");
    std::os::unix::fs::symlink(&real, &linked).unwrap();

    let store = fx.store();
    store
        .checkpoint(S, "turn-real", vec![real.join("a.txt")])
        .unwrap();
    store
        .checkpoint(S, "turn-link", vec![linked.join("a.txt")])
        .unwrap();

    let keys = |turn: &str| {
        let target = store.target_for_turn(turn).unwrap().unwrap();
        store
            .manifest(target.manifest_id())
            .unwrap()
            .entries
            .into_keys()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys("turn-real"),
        keys("turn-link"),
        "the same file was recorded under two keys"
    );

    // Which spelling wins, not merely that one does. The equality above
    // would hold just as well if both captures had been recorded under the
    // link, and a key that follows whichever path the caller happened to use
    // is the defect itself.
    assert_eq!(
        keys("turn-link"),
        vec![fx.key("real/a.txt")],
        "the key is the real spelling, not the one the caller reached it by"
    );

    // **The second minting site.** `checkpoint` canonicalizes inside
    // `capture`; `attach_pre_edit` canonicalizes independently, and everything
    // above leaves that one uncovered — the comment beside it says it "has to
    // agree with the first", which nothing was checking.
    //
    // The dedup is the visible half: this capture already covers the file, so
    // the link spelling must resolve to the same key or a second entry is
    // appended for one file.
    assert_eq!(
        store
            .attach_pre_edit(
                S,
                "turn-real",
                &linked.join("a.txt").to_string_lossy(),
                &PreEditImage::Existed(b"one".to_vec()),
            )
            .unwrap(),
        None,
        "the capture already covers this file; the link is the same key"
    );

    // And the tombstone half, which is the worse failure: a tombstone keyed by
    // a spelling `plan_restore` cannot match is one the restore never acts on,
    // so the file it licensed removing is silently never removed.
    let attached = store
        .attach_pre_edit(
            S,
            "turn-attach",
            &linked.join("b.txt").to_string_lossy(),
            &PreEditImage::DidNotExist,
        )
        .unwrap()
        .expect("a created path records that it did not exist");
    assert_eq!(
        store
            .manifest(&attached)
            .unwrap()
            .absent
            .into_iter()
            .collect::<Vec<_>>(),
        vec![fx.key("real/b.txt")],
        "the tombstone is keyed by the real spelling, not the link"
    );
}

/// A session another invocation is holding is **refused** — left exactly as
/// it was — while the rest of the batch is deleted.
///
/// This lives here rather than in `tests/concurrency.rs` because holding a
/// lock across a `delete_sessions` call is not something the public API can
/// express: every operation that takes a session lock releases it before it
/// returns. Reaching `crate::lock` directly is the only way to construct the
/// contention, and a test that cannot construct it can only pretend to.
///
/// It costs `LOCK_BUDGET` of wall clock, which is the point twice over: the
/// refusal is what `refused` promises, and the bound is what stops a busy
/// session from hanging a delete forever.
#[test]
fn deleting_refuses_a_session_another_invocation_is_using() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture("held", "turn-1");
    fx.capture("free", "turn-2");
    let store = fx.store();

    let guard = crate::lock::acquire(&store.partition, "held", crate::lock::LOCK_BUDGET)
        .unwrap()
        .expect("the lock was free");
    assert!(
        guard.is_enforced(),
        "this filesystem does not lock, so the refusal under test cannot happen"
    );

    let started = std::time::Instant::now();
    let outcome = store.delete_sessions(&["held".to_string(), "free".to_string()]);
    let waited = started.elapsed();
    drop(guard);

    assert_eq!(
        outcome
            .refused
            .iter()
            .map(|(session, _)| session.clone())
            .collect::<Vec<_>>(),
        vec!["held".to_string()]
    );
    assert!(outcome.incomplete.is_empty(), "{:?}", outcome.incomplete);
    assert!(store.session_exists("held"), "a refused session is intact");
    assert!(!store.session_exists("free"), "the rest of the batch went");
    assert!(
        waited < crate::lock::LOCK_BUDGET * 3,
        "the wait is meant to be bounded by LOCK_BUDGET, and took {waited:?}"
    );
}

/// The lock probe answers without inventing a session.
///
/// `doctor` asks whether this filesystem enforces locks, and the only way to
/// find out is to take one. The id it takes it under is internal, so a probe
/// must never show up as a session the user did not start.
#[test]
fn the_lock_probe_reports_enforcement_without_inventing_a_session() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    fx.capture("real", "turn-1");
    let store = fx.store();

    assert!(
        store.locking_is_enforced().unwrap(),
        "this filesystem does not lock, which would also silently weaken \
         every refusal test in this crate"
    );
    assert_eq!(
        store.sessions().unwrap(),
        vec!["real".to_string()],
        "the probe left a session behind"
    );
}
