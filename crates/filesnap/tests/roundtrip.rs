//! End-to-end scenario from the RFC: checkpoint → agent edits →
//! checkpoint → user edits → rewind (with safety checkpoint,
//! tombstone-licensed deletion, symmetric ignore) → redo → GC.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use filesnap::HiddenFiles;
use filesnap::PreEditImage;
use filesnap::RestoreKind;
use filesnap::SNAPSHOT_IGNORE_FILENAME;
use filesnap::WorkspaceStore;
use filesnap::fixture::no_rules;
use filesnap::fixture::rules_for;
use filesnap::is_ignored;
use filesnap::load_ignore;
use filesnap::tracked_files;
use pretty_assertions::assert_eq;

const THREAD: &str = "thread-1";
/// The thread a rewind hands the workspace to, and therefore the one its undo
/// record is filed under. Distinct from `THREAD`, which performs the restore.
const BRANCH: &str = "branch-thread";

/// Every regular file under `root`, skipping `.git` and dot-entries.
///
/// The library deliberately no longer offers a subtree walk — bounding
/// tracking by the project rather than by the tree is the whole point of the
/// three partitions — so a test that wants one spells it out rather than
/// keeping a production API alive for its own convenience.
fn all_files(root: &Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, ignore: &ignore::gitignore::Gitignore, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            // Ignore rules belong to enumeration, exactly as they do in
            // production: an ignored path is never offered to a capture.
            if is_ignored(ignore, &path) {
                continue;
            }
            if path.is_dir() {
                walk(&path, ignore, out);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &load_ignore(root), &mut out);
    out.sort();
    out
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn full_rewind_redo_gc_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();

    // Workspace: two files, one ignored log, the (versioned) ignore file.
    fs::write(ws.join("a.txt"), "alpha v1").unwrap();
    fs::write(ws.join("b.txt"), "bravo v1").unwrap();
    fs::write(ws.join("build.log"), "log v1").unwrap();
    fs::write(ws.join(SNAPSHOT_IGNORE_FILENAME), "*.log\n").unwrap();

    // Turn 1 checkpoint.
    let cp1 = store.checkpoint(THREAD, "turn-1", all_files(&ws)).unwrap();
    assert_eq!(
        cp1.manifest.entries.len(),
        2,
        "the ignored log and the (hidden) ignore file are both out of scope"
    );

    // Agent work during turn 1: modify a, delete b, create c. Creating c goes
    // through the edit hook, as it does in the running system — that is what
    // records "this path did not exist at turn 1", and nothing else can.
    fs::write(ws.join("a.txt"), "alpha v2 (agent)").unwrap();
    fs::remove_file(ws.join("b.txt")).unwrap();
    store
        .attach_pre_edit(
            THREAD,
            "turn-1",
            &ws.join("c.txt").to_string_lossy(),
            &PreEditImage::DidNotExist,
        )
        .unwrap()
        .expect("creating a file records that it did not exist");
    fs::write(ws.join("c.txt"), "charlie (agent)").unwrap();

    // Turn 2 checkpoint observes the agent's changes.
    let cp2 = store.checkpoint(THREAD, "turn-2", all_files(&ws)).unwrap();
    assert_eq!(
        cp2.manifest.entries.len(),
        2,
        "a.txt modified, b.txt deleted, c.txt created"
    );

    // Between checkpoints: the user creates a file (never yet captured)
    // and edits the ignored log.
    fs::write(ws.join("user-note.txt"), "user data").unwrap();
    fs::write(ws.join("build.log"), "log v2 (user)").unwrap();

    // What a restore looks at: the workspace as it stands now, plus every path
    // this thread has ever observed. The second half is not optional — a file
    // the agent deleted is on no walk of the directory, so without it the
    // safety checkpoint could not record that it was gone, and the redo could
    // never take it away again.
    let current = || {
        let mut files = all_files(&ws);
        files.extend(
            store
                .tracked_paths(THREAD)
                .unwrap()
                .into_iter()
                .map(Into::into),
        );
        files
    };

    // Rewind to turn 1. Protection = current ignore rules.
    let protect = load_ignore(&ws);
    let outcome = store
        .restore_to(
            THREAD,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            current(),
            &protect,
        )
        .unwrap();

    // Disk now matches turn 1 for tracked files…
    assert_eq!(read(&ws.join("a.txt")), "alpha v1");
    assert_eq!(
        read(&ws.join("b.txt")),
        "bravo v1",
        "deleted file recreated"
    );
    assert!(!ws.join("c.txt").exists(), "agent-born file deleted");
    assert_eq!(
        read(&ws.join("user-note.txt")),
        "user data",
        "a file nothing ever looked at is left alone: the rewind has no \
         evidence it was absent at turn 1, and guessing costs the user data \
         that was never the agent's to remove"
    );
    // …while ignored paths were untouched in every direction.
    assert_eq!(read(&ws.join("build.log")), "log v2 (user)");
    assert_eq!(outcome.stats.written, 2);
    assert_eq!(outcome.stats.deleted, 1);
    assert!(
        store.last_restore_target(BRANCH).unwrap().is_some(),
        "the rewind left an undo behind"
    );

    // Redo: restore the safety checkpoint → pre-rewind state returns.
    let protect2 = load_ignore(&ws);
    store
        .restore_to(
            THREAD,
            &outcome.safety,
            RestoreKind::Undo { spending: BRANCH },
            current(),
            &protect2,
        )
        .unwrap();
    assert_eq!(read(&ws.join("a.txt")), "alpha v2 (agent)");
    assert!(!ws.join("b.txt").exists());
    assert_eq!(read(&ws.join("c.txt")), "charlie (agent)");
    assert_eq!(read(&ws.join("user-note.txt")), "user data");
    assert_eq!(read(&ws.join("build.log")), "log v2 (user)");
    assert!(
        store.last_restore_target(BRANCH).unwrap().is_none(),
        "the undo consumed the rewind it reversed, so there is nothing left to undo"
    );

    // Session lifetime: with the undo spent, deleting the sessions leaves
    // nothing reachable. Delete reclaims *records* — the content they named
    // is shared with every other workspace, so only a whole-store collection
    // can decide whether any of it is still wanted.
    assert!(store.records_disk_usage().unwrap() > 0);
    let outcome = store.delete_sessions(&[THREAD.to_string(), BRANCH.to_string()]);
    assert!(
        outcome.refused.is_empty(),
        "nothing to refuse here: {:?}",
        outcome.refused
    );
    assert!(
        !store.session_exists(THREAD),
        "unreachable is what delete promises, and it promises it now"
    );
    assert!(
        store.target_for_turn("turn-1").unwrap().is_none(),
        "and nothing resolves through the turn index either"
    );

    // Reclamation is the separate half. Everything here was written seconds
    // ago, and a sweep spares what it cannot yet age — a capture publishes
    // its content before the manifest naming it, so recent bytes may belong
    // to a manifest that has not landed. Age the store and it goes.
    filesnap::fixture::age_store(dir.path());
    let swept = filesnap::collect_garbage(dir.path()).unwrap();
    assert_eq!(
        swept.manifests_kept, 0,
        "no manifest survives the sessions that produced it"
    );
    assert!(swept.blobs_removed > 0, "and their contents go with them");
}

#[cfg(unix)]
#[test]
fn restore_preserves_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();

    let script = ws.join("run.sh");
    fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    store.checkpoint(THREAD, "turn-1", all_files(&ws)).unwrap();

    fs::write(&script, "#!/bin/sh\necho changed\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o600)).unwrap();
    store.checkpoint(THREAD, "turn-2", all_files(&ws)).unwrap();

    store
        .restore_to(
            THREAD,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            all_files(&ws),
            &no_rules(),
        )
        .unwrap();

    assert_eq!(read(&script), "#!/bin/sh\necho hi\n");
    let mode = fs::metadata(&script).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o755, "executable bit restored");
}

#[test]
fn thread_marker_and_pre_edit_attach() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();

    // Log existence is the session-scoped "tracking on" marker.
    assert!(!store.session_exists(THREAD));
    store.ensure_session(THREAD).unwrap();
    assert!(store.session_exists(THREAD));
    store.ensure_session(THREAD).unwrap(); // idempotent

    // Turn-start scan sees only a.txt.
    fs::write(ws.join("a.txt"), "alpha").unwrap();
    let cp1 = store.checkpoint(THREAD, "turn-1", all_files(&ws)).unwrap();

    // Agent edits a file OUTSIDE the workspace scan: pre-image attaches
    // retroactively under the same turn.
    let outside = filesnap::canonical_key(dir.path()).join("outside.cfg");
    let attached = store
        .attach_pre_edit(
            THREAD,
            "turn-1",
            &outside.to_string_lossy(),
            &PreEditImage::Existed(b"pre-edit state".to_vec()),
        )
        .unwrap()
        .expect("new path should attach");
    fs::write(&outside, "post-edit state").unwrap();

    // Already covered by the turn-start scan → nothing to add.
    assert!(
        store
            .attach_pre_edit(
                THREAD,
                "turn-1",
                &ws.join("a.txt").to_string_lossy(),
                &PreEditImage::Existed(b"x".to_vec())
            )
            .unwrap()
            .is_none()
    );
    // Born by this edit: there is no pre-image, but "it did not exist" is
    // itself worth recording — it is the evidence a later restore needs in
    // order to remove the file, and outside a complete scan nothing else
    // supplies it.
    let tombstoned = store
        .attach_pre_edit(
            THREAD,
            "turn-1",
            "/brand/new.txt",
            &PreEditImage::DidNotExist,
        )
        .unwrap()
        .expect("a created path is recorded as absent");
    // The literal separator is fine here, unlike in an assertion about a path
    // the filesystem produced: this key is the string the caller passed and is
    // stored verbatim, never resolved, so it reads the same on every platform.
    assert!(
        store
            .manifest(&tombstoned)
            .unwrap()
            .absent
            .contains("/brand/new.txt")
    );
    // Recording it twice adds nothing.
    assert!(
        store
            .attach_pre_edit(
                THREAD,
                "turn-1",
                "/brand/new.txt",
                &PreEditImage::DidNotExist
            )
            .unwrap()
            .is_none()
    );

    // Each supplement extends the previous one, and the turn resolves to the
    // most complete of them.
    let history = store.thread_history(THREAD).unwrap();
    assert_eq!(history.len(), 3, "turn-start scan, pre-image, tombstone");
    assert!(history.iter().all(|(r, _)| r.turn_id == "turn-1"));
    assert_eq!(history[1].0.manifest_id, attached);
    assert_eq!(history[1].1.entries.len(), cp1.manifest.entries.len() + 1);
    assert_eq!(history[2].0.manifest_id, tombstoned);
    assert_eq!(
        history[2].1.entries.len(),
        cp1.manifest.entries.len() + 1,
        "the tombstone carries the pre-image forward"
    );
    assert_eq!(
        store
            .target_for_turn("turn-1")
            .unwrap()
            .unwrap()
            .manifest_id(),
        tombstoned
    );

    // Restoring to the supplemental manifest recovers the pre-edit content.
    store
        .restore_to(
            THREAD,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            all_files(&ws).into_iter().chain([outside.clone()]),
            &no_rules(),
        )
        .unwrap();
    assert_eq!(read(&outside), "pre-edit state");
}

#[test]
fn turn_resolution_and_fork_inheritance() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();

    fs::write(ws.join("a.txt"), "v1").unwrap();
    store.checkpoint(THREAD, "turn-1", all_files(&ws)).unwrap();
    // Supplemental attach under the same turn: resolution must pick it.
    let outside = filesnap::canonical_key(dir.path()).join("ext.cfg");
    let supplemental = store
        .attach_pre_edit(
            THREAD,
            "turn-1",
            &outside.to_string_lossy(),
            &PreEditImage::Existed(b"pre".to_vec()),
        )
        .unwrap()
        .unwrap();
    fs::write(ws.join("a.txt"), "v2").unwrap();
    store.checkpoint(THREAD, "turn-2", all_files(&ws)).unwrap();

    assert_eq!(
        store
            .target_for_turn("turn-1")
            .unwrap()
            .map(|t| t.manifest_id().to_string()),
        Some(supplemental.clone()),
        "last entry for the turn wins"
    );
    assert!(store.target_for_turn("nope").unwrap().is_none());

    // tracked_paths covers the outside-workspace attach.
    let paths = store.tracked_paths(THREAD).unwrap();
    assert!(paths.contains(&outside.to_string_lossy().into_owned()));
    assert!(paths.iter().any(|p| p.ends_with("a.txt")));

    // Fork inherits entries through turn-1 (scan + supplemental), not turn-2.
    let inherited = store.inherit_log(THREAD, "fork-1", "turn-1").unwrap();
    assert_eq!(inherited, 2);
    assert!(store.session_exists("fork-1"));
    let fork_history = store.thread_history("fork-1").unwrap();
    assert_eq!(fork_history.len(), 2);
    assert_eq!(fork_history.last().unwrap().0.manifest_id, supplemental);

    // Unknown turn: log created (tracking marker) but nothing inherited.
    assert_eq!(store.inherit_log(THREAD, "fork-2", "nope").unwrap(), 0);
    assert!(store.session_exists("fork-2"));

    // Shared manifests survive a sweep while either session references them.
    let outcome = store.delete_sessions(&[THREAD.to_string()]);
    assert!(
        outcome.reclaimed.manifests_kept >= 2,
        "fork-1 still references manifests"
    );
}

#[test]
fn rewinding_twice_still_restores() {
    // Mirrors real use: rewind, undo it, rewind again. The second rewind must
    // put the files back, not quietly decide there is nothing to do.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();
    let scan = || all_files(&ws);

    fs::write(ws.join("a.txt"), "v1").unwrap();
    store.checkpoint(THREAD, "turn-1", scan()).unwrap();

    fs::write(ws.join("a.txt"), "v2").unwrap();
    store.checkpoint(THREAD, "turn-2", scan()).unwrap();

    // Rewind to turn 1.
    let turn1_target = store.target_for_turn("turn-1").unwrap().unwrap();
    store
        .restore_to(
            THREAD,
            &turn1_target,
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            scan(),
            &no_rules(),
        )
        .unwrap();
    assert_eq!(read(&ws.join("a.txt")), "v1", "first rewind");

    // Undo that rewind.
    let safety = store.last_restore_target(BRANCH).unwrap().unwrap();
    store
        .restore_to(
            THREAD,
            &safety,
            RestoreKind::Undo { spending: BRANCH },
            scan(),
            &no_rules(),
        )
        .unwrap();
    assert_eq!(
        read(&ws.join("a.txt")),
        "v2",
        "undo restores the newer state"
    );

    // Rewind to turn 1 again — the case that regressed in real use. The
    // target must resolve to the same entry as before, even though the undo
    // recorded that very state again further down the log.
    store
        .restore_to(
            THREAD,
            &turn1_target,
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            scan(),
            &no_rules(),
        )
        .unwrap();
    assert_eq!(
        read(&ws.join("a.txt")),
        "v1",
        "second rewind must still work"
    );
}

#[test]
fn an_undo_reports_what_moved_since_the_rewind() {
    // Undo records are private per thread, but the files are not. Anything
    // that changed after the rewind — another session, or the user's own
    // editor — would be overwritten without a word, so the undo has to be
    // able to say what it is about to clobber.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();
    let scan = || all_files(&ws);

    fs::write(ws.join("kept.txt"), "v1").unwrap();
    fs::write(ws.join("build.log"), "noise").unwrap();
    store.checkpoint(THREAD, "turn-1", scan()).unwrap();
    fs::write(ws.join("kept.txt"), "v2").unwrap();
    store.checkpoint(THREAD, "turn-2", scan()).unwrap();

    store
        .restore_to(
            THREAD,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            scan(),
            &no_rules(),
        )
        .unwrap();

    // Nothing has moved yet.
    assert!(
        store
            .undo_conflicts(BRANCH, &no_rules())
            .unwrap()
            .is_empty(),
        "the workspace still looks like what the rewind left"
    );

    // Someone else edits a file the undo would overwrite.
    fs::write(ws.join("kept.txt"), "theirs").unwrap();
    let conflicts = store.undo_conflicts(BRANCH, &no_rules()).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].ends_with("kept.txt"));

    // Ignored paths are not reported: the restore would not touch them.
    fs::write(ws.join("build.log"), "changed too").unwrap();
    let protect = rules_for(&ws, "build.log");
    let conflicts = store.undo_conflicts(BRANCH, &protect).unwrap();
    assert_eq!(conflicts.len(), 1, "only the path the undo would write");

    // A thread with nothing to undo has nothing to report.
    assert!(
        store
            .undo_conflicts(THREAD, &no_rules())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_restore_with_no_destination_leaves_no_undo() {
    // Rewinding to the first prompt restarts the conversation instead of
    // branching, so there is no thread for an undo record to live under and
    // be reached from. It records nothing rather than filing an orphan, and
    // the TUI says so before running it.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();
    let scan = || all_files(&ws);

    fs::write(ws.join("a.txt"), "v1").unwrap();
    store.checkpoint(THREAD, "turn-1", scan()).unwrap();
    fs::write(ws.join("a.txt"), "v2").unwrap();

    store
        .restore_to(
            THREAD,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind { undo_for: None },
            scan(),
            &no_rules(),
        )
        .unwrap();

    assert_eq!(read(&ws.join("a.txt")), "v1", "the files still go back");
    assert!(store.last_restore_target(THREAD).unwrap().is_none());
    assert!(store.last_restore_target(BRANCH).unwrap().is_none());
}

#[test]
fn two_sessions_in_one_workspace_do_not_share_undos() {
    // Undo records are filed under the thread a rewind hands the workspace
    // to, not under the workspace itself. Keyed by workspace, two sessions
    // working in one directory push onto the same stack, and whichever undoes
    // first spends the other's record — silently reverting work it never saw.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();
    let scan = || all_files(&ws);

    fs::write(ws.join("a.txt"), "v1").unwrap();
    store.checkpoint("session-a", "a-turn-1", scan()).unwrap();
    fs::write(ws.join("a.txt"), "v2").unwrap();
    store.checkpoint("session-b", "b-turn-1", scan()).unwrap();

    // Session A rewinds, handing the workspace to its branch.
    store
        .restore_to(
            "session-a",
            &store.target_for_turn("a-turn-1").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some("branch-a"),
            },
            scan(),
            &no_rules(),
        )
        .unwrap();

    // Session B rewinds too. Same directory, same files.
    store
        .restore_to(
            "session-b",
            &store.target_for_turn("b-turn-1").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some("branch-b"),
            },
            scan(),
            &no_rules(),
        )
        .unwrap();

    // Each branch sees exactly one undo: its own.
    let a = store.last_restore_target("branch-a").unwrap();
    let b = store.last_restore_target("branch-b").unwrap();
    assert!(a.is_some() && b.is_some());
    assert_ne!(a, b, "each branch undoes its own rewind, not the other's");

    // And spending one leaves the other untouched.
    store
        .restore_to(
            "branch-b",
            &b.unwrap(),
            RestoreKind::Undo {
                spending: "branch-b",
            },
            scan(),
            &no_rules(),
        )
        .unwrap();
    assert!(store.last_restore_target("branch-b").unwrap().is_none());
    assert_eq!(
        store.last_restore_target("branch-a").unwrap(),
        a,
        "one session's undo does not consume another's"
    );
}

#[test]
fn nested_rewinds_unwind_in_the_order_they_were_made() {
    // The case worth being sure about: rewind, keep working on the branch,
    // rewind again, then undo twice. Each undo has to land on the state that
    // existed just before the rewind it reverses — including the work done on
    // the branch in between, which the second undo must not skip past.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();
    let scan = || all_files(&ws);

    for (turn, contents) in [("turn-1", "v1"), ("turn-2", "v2")] {
        fs::write(ws.join("a.txt"), contents).unwrap();
        store.checkpoint(THREAD, turn, scan()).unwrap();
    }
    fs::write(ws.join("a.txt"), "v3 (unsaved)").unwrap();

    let rewind = |turn: &str| {
        let target = store.target_for_turn(turn).unwrap().unwrap();
        store
            .restore_to(
                THREAD,
                &target,
                RestoreKind::Rewind {
                    undo_for: Some(BRANCH),
                },
                scan(),
                &no_rules(),
            )
            .unwrap();
    };
    let undo = || {
        let target = store.last_restore_target(BRANCH).unwrap().unwrap();
        store
            .restore_to(
                THREAD,
                &target,
                RestoreKind::Undo { spending: BRANCH },
                scan(),
                &no_rules(),
            )
            .unwrap();
    };

    rewind("turn-2");
    assert_eq!(read(&ws.join("a.txt")), "v2");

    // Work on the branch, then rewind again from there.
    fs::write(ws.join("a.txt"), "branch work").unwrap();
    rewind("turn-1");
    assert_eq!(read(&ws.join("a.txt")), "v1");

    undo();
    assert_eq!(
        read(&ws.join("a.txt")),
        "branch work",
        "the first undo returns the branch's own work, not turn 2"
    );
    undo();
    assert_eq!(
        read(&ws.join("a.txt")),
        "v3 (unsaved)",
        "the second undo reaches back past the first rewind"
    );
    assert!(store.last_restore_target(BRANCH).unwrap().is_none());
}

#[test]
fn a_capture_covers_every_configured_root() {
    // Scope follows the session's workspace roots, which are plural. Scoping
    // to one guessed root would leave the others unprotected while the sandbox
    // happily lets the agent write to them.
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (
        filesnap::canonical_key(dir.path()).join("svc-a"),
        filesnap::canonical_key(dir.path()).join("svc-b"),
    );
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("main.rs"), "fn a() {}").unwrap();
    fs::write(b.join("main.rs"), "fn b() {}").unwrap();
    // Several roots are scanned per turn, but the session is bound to one
    // directory (D4) — here the parent that holds both services.
    let store = WorkspaceStore::open(dir.path(), dir.path()).unwrap();

    let roots = vec![a.clone(), b.clone()];
    let scan = || {
        tracked_files(
            &roots,
            [],
            HiddenFiles::Skip,
            filesnap::ScanLimits::default(),
        )
        .files
        .into_iter()
        .collect::<Vec<_>>()
    };
    let cp1 = store.checkpoint(THREAD, "turn-1", scan()).unwrap();
    assert_eq!(cp1.manifest.entries.len(), 2, "both roots captured");

    // Work in the second root only, then rewind. The new file goes through the
    // edit hook, which is what records the tombstone licensing its removal.
    fs::write(b.join("main.rs"), "fn b() { changed }").unwrap();
    let born = b.join("extra.rs");
    store
        .attach_pre_edit(
            THREAD,
            "turn-1",
            &born.to_string_lossy(),
            &PreEditImage::DidNotExist,
        )
        .unwrap()
        .expect("creating a file records that it did not exist");
    fs::write(&born, "born").unwrap();
    store
        .restore_to(
            THREAD,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            scan(),
            &no_rules(),
        )
        .unwrap();

    assert_eq!(
        read(&b.join("main.rs")),
        "fn b() {}",
        "second root reverted"
    );
    assert!(
        !born.exists(),
        "the tombstone works in every declared root, not just the first"
    );
    assert_eq!(read(&a.join("main.rs")), "fn a() {}");
}

#[test]
fn a_file_created_outside_the_scanned_scope_is_removed_by_a_rewind() {
    // The hard case for deletion: fallback mode (no project marker, so the
    // capture is bounded) and a file the agent creates *above* the scanned
    // directory. Absence from a bounded manifest proves nothing — the scan
    // never looked there — so the only thing that can license removing it is
    // the tombstone written when the edit created it.
    let dir = tempfile::tempdir().unwrap();
    let outer = filesnap::canonical_key(dir.path()).join("project");
    let cwd = outer.join("inner");
    fs::create_dir_all(&cwd).unwrap();
    // The session belongs to the project; the scan for this turn covers only
    // `inner/`, which is what makes the file created in `outer/` invisible to
    // it and the tombstone the only evidence there is.
    let store = WorkspaceStore::open(dir.path(), &outer).unwrap();

    // Turn 1 starts with an empty scope: `inner/` holds nothing.
    let cp1 = store.checkpoint(THREAD, "turn-1", all_files(&cwd)).unwrap();
    assert!(cp1.manifest.entries.is_empty());

    // The agent creates a file in the parent directory.
    let created = outer.join("hello.html");
    store
        .attach_pre_edit(
            THREAD,
            "turn-1",
            &created.to_string_lossy(),
            &PreEditImage::DidNotExist,
        )
        .unwrap()
        .expect("creating a file records that it did not exist");
    fs::write(&created, "generated").unwrap();

    // Rewind to turn 1. The safety scope has to include what the thread has
    // observed, or the new file is not even a candidate.
    let mut current: Vec<_> = all_files(&cwd);
    current.extend(
        store
            .tracked_paths(THREAD)
            .unwrap()
            .into_iter()
            .map(std::path::PathBuf::from),
    );
    let outcome = store
        .restore_to(
            THREAD,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            current,
            &no_rules(),
        )
        .unwrap();

    assert!(
        !created.exists(),
        "a tombstoned path is removed even though the capture was bounded"
    );
    assert_eq!(outcome.stats.deleted, 1);
}

#[test]
fn undo_walks_back_through_successive_rewinds() {
    // Rewinding twice in a row and then undoing twice has to retrace those
    // steps in reverse. If the undo target were simply "the last restore",
    // the second undo would reverse the first undo and the files would
    // oscillate between two states while the conversation kept moving back.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();
    let scan = || all_files(&ws);

    for (turn, contents) in [("turn-1", "v1"), ("turn-2", "v2"), ("turn-3", "v3")] {
        fs::write(ws.join("a.txt"), contents).unwrap();
        store.checkpoint(THREAD, turn, scan()).unwrap();
    }

    let rewind = |turn: &str| {
        let target = store.target_for_turn(turn).unwrap().unwrap();
        store
            .restore_to(
                THREAD,
                &target,
                RestoreKind::Rewind {
                    undo_for: Some(BRANCH),
                },
                scan(),
                &no_rules(),
            )
            .unwrap();
    };
    let undo = || {
        let target = store.last_restore_target(BRANCH).unwrap().unwrap();
        store
            .restore_to(
                THREAD,
                &target,
                RestoreKind::Undo { spending: BRANCH },
                scan(),
                &no_rules(),
            )
            .unwrap();
    };

    rewind("turn-2");
    assert_eq!(read(&ws.join("a.txt")), "v2");
    rewind("turn-1");
    assert_eq!(read(&ws.join("a.txt")), "v1");

    undo();
    assert_eq!(
        read(&ws.join("a.txt")),
        "v2",
        "first undo retraces one step"
    );
    undo();
    assert_eq!(
        read(&ws.join("a.txt")),
        "v3",
        "second undo retraces the earlier rewind, back to where we started"
    );
    assert!(
        store.last_restore_target(BRANCH).unwrap().is_none(),
        "both rewinds have been spent"
    );
}

#[test]
fn deleting_a_conversation_takes_its_snapshots_with_it() {
    // The whole of "snapshot lifetime = session lifetime". Until this ran,
    // deleting a conversation removed the rollout and left every file it had
    // captured on disk forever — contents included, which is not what someone
    // deleting a conversation is asking for.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let ws = home.join("ws");
    fs::create_dir_all(&ws).unwrap();
    let store = WorkspaceStore::open(home, &ws).unwrap();

    fs::write(ws.join("secret.txt"), "sensitive").unwrap();
    let scan = || all_files(&ws);
    store.checkpoint(THREAD, "turn-1", scan()).unwrap();

    // A rewind, so the thread also owns an undo record — a second file, under
    // a second lifetime, that dropping the log alone would strand as a GC root.
    store
        .restore_to(
            THREAD,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            scan(),
            &no_rules(),
        )
        .unwrap();
    assert!(store.last_restore_target(BRANCH).unwrap().is_some());

    let count = || filesnap::fixture::blob_count(home);
    assert!(count() > 0, "the file's content is on disk");

    // Delete both sessions, the way deleting a conversation does.
    store.delete_sessions(&[THREAD.to_string(), BRANCH.to_string()]);

    assert!(!store.session_exists(THREAD));
    assert!(
        store.last_restore_target(BRANCH).unwrap().is_none(),
        "the undo record goes too — it is a GC root, and left behind it would \
         pin the manifests it names for good"
    );

    // The content is now unreachable but still on disk: delete makes a
    // session unreachable, and reclaiming what only it held is a separate,
    // idempotent activity that no delete waits on. A collection is what frees
    // it — and it spares anything written recently, so the store has to be
    // aged before one will act.
    assert!(count() > 0, "delete does not reclaim content");
    filesnap::collect_garbage(home).unwrap();
    assert!(
        count() > 0,
        "and a collection still spares content written moments ago"
    );

    filesnap::fixture::age_store(home);
    let stats = filesnap::collect_garbage(home).unwrap();
    assert!(stats.blobs_removed > 0);
    assert_eq!(count(), 0, "once it can be aged, the contents are gone");
}

#[test]
fn deleting_what_was_never_tracked_is_not_an_error() {
    // Deleting must not fail because the store was never used, and a session
    // that captured nothing is not a session that failed — removing files
    // that are already absent is success, not an error to report.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();

    let outcome = store.delete_sessions(&["never-tracked".to_string()]);
    assert!(
        outcome.refused.is_empty(),
        "a session with nothing to delete is not refused: {:?}",
        outcome.refused
    );
    assert_eq!(outcome.reclaimed.manifests_removed, 0);

    // An empty batch does nothing at all, including no sweep.
    let empty = store.delete_sessions(&[]);
    assert!(empty.refused.is_empty());
    assert_eq!(empty.reclaimed, filesnap::GcStats::default());
}

#[test]
fn an_undo_removes_a_file_recreated_outside_the_workspace() {
    // The gap that survived every other guard. A path enters the thread's
    // records only *after* the turn a rewind forks at, so the fork never
    // inherits it; something other than the rewind puts it back on disk; and
    // it sits outside the workspace, where no scan can reach it. The undo
    // then held a tombstone for a file it could not see, and `plan_restore`
    // needs both to delete — so it silently survived.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let outside = filesnap::canonical_key(dir.path()).join("elsewhere");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();
    let scan = || all_files(&ws);

    fs::write(ws.join("a.txt"), "v1").unwrap();
    store.checkpoint(THREAD, "fork-here", scan()).unwrap();

    // A later turn deletes a file the workspace scan cannot see. The edit
    // hook records it, so the *source* thread knows about it from now on.
    let script = outside.join("deploy.sh");
    fs::write(&script, "old").unwrap();
    store.checkpoint(THREAD, "turn-2", scan()).unwrap();
    store
        .attach_pre_edit(
            THREAD,
            "turn-2",
            &script.to_string_lossy(),
            &PreEditImage::Existed(b"old".to_vec()),
        )
        .unwrap()
        .expect("the pre-image of a file about to be deleted is recorded");
    fs::remove_file(&script).unwrap();

    // Rewind to the earlier turn. The fork inherits only up to that turn, so
    // it never learns the path exists. The rewind itself scans the way the
    // app-server does — the workspace plus everything *this* thread has
    // observed — which is what lets the safety capture record the script as
    // absent.
    store.inherit_log(THREAD, BRANCH, "fork-here").unwrap();
    let mut source_scope = scan();
    source_scope.extend(
        store
            .tracked_paths(THREAD)
            .unwrap()
            .into_iter()
            .map(Into::into),
    );
    let outcome = store
        .restore_to(
            THREAD,
            &store.target_for_turn("fork-here").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            source_scope,
            &no_rules(),
        )
        .unwrap();
    assert!(!script.exists(), "still deleted; the target never saw it");

    // Something that leaves no trace in the fork's records puts it back.
    fs::write(&script, "resurrected by a shell command").unwrap();

    // Redo. The safety manifest recorded the path as absent, so it must go —
    // even though the fork's own history has never heard of it.
    store
        .restore_to(
            BRANCH,
            &outcome.safety,
            RestoreKind::Undo { spending: BRANCH },
            all_files(&ws),
            &no_rules(),
        )
        .unwrap();
    assert!(
        !script.exists(),
        "the undo must delete a path its target recorded as absent, whatever \
         the scan happened to cover"
    );
}

#[test]
fn a_turn_reports_what_it_cannot_put_back() {
    // Tracking is discovered, not retroactive: a turn from before a path was
    // ever seen has no content to restore for it, so rewinding *further back*
    // restores *less*. That is the right call — inventing content from a
    // later turn's pre-image would write bytes of unknown provenance — but it
    // is invisible, so it has to be said out loud.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let outside = filesnap::canonical_key(dir.path()).join("elsewhere");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();
    let scan = || all_files(&ws);

    fs::write(ws.join("a.txt"), "v1").unwrap();
    store.checkpoint(THREAD, "early", scan()).unwrap();

    let script = outside.join("deploy.sh");
    fs::write(&script, "old").unwrap();
    store.checkpoint(THREAD, "late", scan()).unwrap();
    store
        .attach_pre_edit(
            THREAD,
            "late",
            &script.to_string_lossy(),
            &PreEditImage::Existed(b"old".to_vec()),
        )
        .unwrap()
        .unwrap();

    let roots = vec![ws.clone()];
    let early = store.target_for_turn("early").unwrap().unwrap();
    assert_eq!(
        store.unrestorable_outside(THREAD, &early, &roots).unwrap(),
        vec![script.to_string_lossy().into_owned()],
        "the early turn predates the discovery, so it cannot put this back"
    );

    let late = store.target_for_turn("late").unwrap().unwrap();
    assert!(
        store
            .unrestorable_outside(THREAD, &late, &roots)
            .unwrap()
            .is_empty(),
        "the later turn holds the pre-image, so it can"
    );
}

#[test]
fn the_undo_warning_covers_what_it_will_delete() {
    // The warning reads two manifests because they answer different
    // questions. The rewind's target says where the workspace was left, so it
    // catches edits made since. Only the safety manifest — the state the undo
    // restores — knows that a file existing now was absent then and is about
    // to be removed, which is the case a user would most want stopped.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();

    fs::write(ws.join("kept.txt"), "v1").unwrap();
    store.checkpoint(THREAD, "turn-1", all_files(&ws)).unwrap();

    // A path the agent creates and then deletes, so the safety capture
    // records it as absent while the rewind target never mentions it.
    let scratch = ws.join("scratch.txt");
    store
        .attach_pre_edit(
            THREAD,
            "turn-1",
            &scratch.to_string_lossy(),
            &PreEditImage::DidNotExist,
        )
        .unwrap();
    store
        .restore_to(
            THREAD,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            all_files(&ws),
            &no_rules(),
        )
        .unwrap();

    assert!(
        store
            .undo_conflicts(BRANCH, &no_rules())
            .unwrap()
            .is_empty(),
        "nothing has moved yet"
    );

    // The user writes it back. The undo will delete it again.
    fs::write(&scratch, "written after the rewind").unwrap();
    assert_eq!(
        store.undo_conflicts(BRANCH, &no_rules()).unwrap(),
        vec![scratch.to_string_lossy().into_owned()],
        "a file the undo is about to remove must be reported, not just the \
         ones it will overwrite"
    );
}

#[test]
fn the_undo_warning_covers_files_the_rewind_never_touched() {
    // An undo restores the whole safety manifest, which is wider than the
    // rewind's target: it holds paths the rewind had no opinion about and
    // therefore left alone. The target cannot say what the rewind left for
    // those, so checking against it checks nothing — while the undo writes
    // over them regardless. Found in real use, where a second session had
    // created a file after the fork point.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    // Canonical: a capture records canonical keys, and on macOS the temp
    // directory is under `/var`, a symlink to `/private/var`. Comparing a raw
    // temp path against a recorded key compares two names for one file.
    let ws = filesnap::canonical_key(&ws);
    let store = WorkspaceStore::open(dir.path(), &ws).unwrap();

    fs::write(ws.join("a.txt"), "v1").unwrap();
    store
        .checkpoint(THREAD, "fork-here", all_files(&ws))
        .unwrap();

    // Appears only after the turn the rewind targets, so the target has no
    // entry for it — but the safety capture, taken later, does.
    let bystander = ws.join("bystander.txt");
    fs::write(&bystander, "untouched by the rewind").unwrap();

    store
        .restore_to(
            THREAD,
            &store.target_for_turn("fork-here").unwrap().unwrap(),
            RestoreKind::Rewind {
                undo_for: Some(BRANCH),
            },
            all_files(&ws),
            &no_rules(),
        )
        .unwrap();
    assert_eq!(
        read(&bystander),
        "untouched by the rewind",
        "the rewind leaves a path its target never mentioned"
    );
    assert!(
        store
            .undo_conflicts(BRANCH, &no_rules())
            .unwrap()
            .is_empty(),
        "and while it still matches, there is nothing to warn about"
    );

    // Someone edits it. The undo will write the old bytes back over this.
    fs::write(&bystander, "edited after the rewind").unwrap();
    assert_eq!(
        store.undo_conflicts(BRANCH, &no_rules()).unwrap(),
        vec![bystander.to_string_lossy().into_owned()],
        "a path the undo will overwrite must be reported even though the \
         rewind's target has never heard of it"
    );
}
