//! Behaviour that only exists on Windows, tested where it exists.
//!
//! These were nearly filed as issues on the grounds that they could not be
//! checked from a Linux machine. They can: the CI matrix has a Windows runner,
//! and every scenario below is constructible. An untested platform-specific
//! branch is a branch nobody has ever run.
//!
//! The unix equivalents live in `tests/permissions.rs`; the two files together
//! are the whole of what a restore does to permissions.

#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use filesnap::RestoreKind;
use filesnap::fixture::Fixture;
use filesnap::fixture::no_rules;
use pretty_assertions::assert_eq;

const SESSION: &str = "s1";

fn readonly(fx: &Fixture, rel: &str) -> bool {
    std::fs::metadata(fx.path(rel))
        .unwrap()
        .permissions()
        .readonly()
}

fn set_readonly(fx: &Fixture, rel: &str, value: bool) {
    let mut perms = std::fs::metadata(fx.path(rel)).unwrap().permissions();
    perms.set_readonly(value);
    std::fs::set_permissions(fx.path(rel), perms).unwrap();
}

fn rewind(fx: &Fixture, turn: &str) -> filesnap::RestoreOutcome {
    let store = fx.store();
    let target = store.target_for_turn(turn).unwrap().unwrap();
    store
        .restore_to(
            SESSION,
            &target,
            RestoreKind::Rewind { undo_for: None },
            fx.restore_scope(SESSION),
            &no_rules(),
        )
        .unwrap()
}

/// **A restore can replace a read-only file.**
///
/// `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` must delete the
/// destination, and it refuses to delete a read-only one — so a rewind failed
/// with access denied on exactly the files a user had marked as
/// not-to-be-changed. Unix has no equivalent, because permission to replace a
/// file lives on its directory rather than on the file.
#[test]
fn a_restore_can_replace_a_file_marked_read_only() {
    let fx = Fixture::new();
    fx.write("notes.txt", "before");
    fx.capture(SESSION, "turn-1");

    fx.write("notes.txt", "after");
    set_readonly(&fx, "notes.txt", true);

    let outcome = rewind(&fx, "turn-1");

    assert!(
        outcome.stats.failed.is_empty(),
        "a read-only destination stopped the restore: {:?}",
        outcome.stats.failed
    );
    assert_eq!(fx.read("notes.txt"), "before");
}

/// **The read-only bit is recorded and put back.**
///
/// It is the one permission Windows exposes, and `mode` was `None` there — so
/// it was thrown away in both directions, and a rewind silently made a
/// read-only file writable. `mode` carries it now, mapped onto `0o444` /
/// `0o644` so a mode recorded on unix still reads as "writable or not".
#[test]
fn a_rewind_puts_the_read_only_bit_back() {
    let fx = Fixture::new();
    fx.write("locked.txt", "v1");
    set_readonly(&fx, "locked.txt", true);
    fx.capture(SESSION, "turn-1");

    set_readonly(&fx, "locked.txt", false);
    fx.write("locked.txt", "v2");
    fx.capture(SESSION, "turn-2");
    assert!(!readonly(&fx, "locked.txt"));

    rewind(&fx, "turn-1");

    assert_eq!(fx.read("locked.txt"), "v1");
    assert!(
        readonly(&fx, "locked.txt"),
        "the bit the capture recorded was not restored"
    );
}

/// And the other direction: a file that was writable does not come back
/// read-only.
#[test]
fn a_rewind_does_not_invent_a_read_only_bit() {
    let fx = Fixture::new();
    fx.write("ordinary.txt", "v1");
    fx.capture(SESSION, "turn-1");

    fx.write("ordinary.txt", "v2");
    set_readonly(&fx, "ordinary.txt", true);

    rewind(&fx, "turn-1");

    assert_eq!(fx.read("ordinary.txt"), "v1");
    assert!(!readonly(&fx, "ordinary.txt"));
}

/// **A file held open cannot be replaced, and that is reported per file
/// rather than losing the rest of the restore.**
///
/// `MoveFileExW` fails with a sharing violation when a handle to the
/// destination is open without `FILE_SHARE_DELETE` — the ordinary state of a
/// file open in an editor, or being scanned at that instant. Unix has no such
/// rule: the old inode simply lives on for existing readers.
///
/// This is a real limitation and not a defect, so what is asserted is that it
/// degrades the way D28 requires: the file that could be written is written,
/// the one that could not is named, and the exit is not a success.
#[test]
fn a_file_held_open_is_reported_and_does_not_strand_the_others() {
    let fx = Fixture::new();
    fx.write("open.txt", "before");
    fx.write("closed.txt", "before");
    fx.capture(SESSION, "turn-1");

    fx.write("open.txt", "after");
    fx.write("closed.txt", "after");

    // A plain `File::open` on Windows shares read and write but not delete,
    // which is what an editor holding a file looks like.
    let held = std::fs::File::open(fx.path("open.txt")).unwrap();
    let outcome = rewind(&fx, "turn-1");
    drop(held);

    assert_eq!(
        fx.read("closed.txt"),
        "before",
        "one unreplaceable file stranded the rest"
    );
    assert_eq!(outcome.stats.failed.len(), 1, "{:?}", outcome.stats.failed);
    assert!(outcome.stats.failed[0].0.ends_with("open.txt"));
    // The point it can be reversed to is still reported.
    assert!(fx.store().manifest(outcome.safety.manifest_id()).is_ok());
}

/// A store path near the classic 260-character limit still works.
///
/// The store nests two 64-character digests under the data directory, so a
/// deep-enough `--data-dir` gets close. Rust's `std` applies the `\\?\` prefix
/// to most filesystem calls, which lifts the limit — this asserts that holds
/// for the paths this crate actually builds.
#[test]
fn a_deep_store_path_still_works() {
    let base = tempfile::tempdir().unwrap();
    // Enough nesting to push the deepest record past 260 with the digests.
    let deep = base.path().join("a".repeat(60)).join("b".repeat(60));
    std::fs::create_dir_all(&deep).unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("a.txt"), "one").unwrap();

    let store = filesnap::WorkspaceStore::open(&deep, ws.path()).unwrap();
    let checkpoint = store
        .checkpoint("s1", "t1", vec![ws.path().join("a.txt")])
        .unwrap();

    assert!(store.manifest(&checkpoint.id).is_ok());
}
