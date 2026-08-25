//! Behaviour that only exists on Windows, tested where it exists.
//!
//! These were nearly filed as issues on the grounds that they could not be
//! checked from a Linux machine. They can: the CI matrix has a Windows runner,
//! and every scenario below is constructible. An untested platform-specific
//! branch is a branch nobody has ever run.
//!
//! The unix equivalents live in `tests/permissions.rs`. Read as a pair: each
//! file covers what a restore does to file attributes on its platform, and
//! each carries one degradation test that reaches `write_one`'s failure
//! branch through a blocker the other platform does not have — denied
//! directory permissions on unix, an occupied destination here.

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
///
/// Note what is *not* being tested: the destination's own attributes never
/// survive a restore, because `write_one` renames a fresh sibling over it.
/// The bit set on the destination mid-test is not a candidate to carry over
/// — the recorded mode alone decides what the replacement gets, which is
/// what an inverted `0o444`/`0o644` mapping would break.
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

/// **A destination that cannot be replaced is reported per file, and the
/// rest of the restore still lands.**
///
/// The unix half of this (`tests/permissions.rs`) denies write permission on
/// the containing directory. Windows has no equivalent — permission to
/// replace a file does not live on its directory there — so without this test
/// the `write_one` failure branch is never executed on Windows at all.
///
/// The blocker here is a destination whose name is occupied by a non-empty
/// directory: what a workspace looks like after a module has been turned into
/// a package. Nothing renames a file over that, on any Windows version or
/// filesystem, and nothing does on unix either.
///
/// Two more obvious constructions do not work, and are deliberately not used:
///
/// - Marking the destination read-only. `write_one` clears it itself, one
///   line before the rename, on purpose.
/// - Holding a handle open. `std::fs::File::open` shares read, write **and**
///   delete, so it holds nothing — which is why the first version of this
///   test asserted a failure that never happened. Opening without
///   `FILE_SHARE_DELETE` does block the rename, but `std` re-raises the
///   original `ERROR_ACCESS_DENIED` and discards the sharing violation, and
///   whether the `FILE_RENAME_FLAG_POSIX_SEMANTICS` retry refuses such a
///   target is not documented anywhere primary. There is nothing
///   deterministic to assert, and a probably-fails test is a flaky one.
///
/// What is asserted is the D28 degradation contract: the file that could be
/// written is written, the one that could not is named, and the outcome does
/// not read as success.
#[test]
fn a_destination_that_cannot_be_replaced_is_reported_and_does_not_strand_the_others() {
    let fx = Fixture::new();
    fx.write("blocked.txt", "before");
    fx.write("ordinary.txt", "before");
    fx.capture(SESSION, "turn-1");

    fx.write("ordinary.txt", "after");
    fx.remove("blocked.txt");
    fx.write("blocked.txt/inner.txt", "occupied");

    let outcome = rewind(&fx, "turn-1");

    assert_eq!(outcome.stats.written, 1, "the writable file did not land");
    assert_eq!(
        fx.read("ordinary.txt"),
        "before",
        "one unreplaceable destination stranded the rest"
    );
    assert_eq!(
        outcome.stats.failed.len(),
        1,
        "a restore that could not write a file must not read as success: {:?}",
        outcome.stats.failed
    );
    assert!(outcome.stats.failed[0].0.ends_with("blocked.txt"));
    // Untouched, not half-replaced.
    assert_eq!(fx.read("blocked.txt/inner.txt"), "occupied");
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

    // The premise, stated rather than assumed. Nothing here measures a path
    // on its own, so a shorter TMP or a flatter store layout would quietly
    // turn this into an ordinary round-trip that proves nothing.
    let deepest = longest_path_under(&deep);
    assert!(
        deepest.as_os_str().len() > 260,
        "only {} characters, so the limit was never approached: {}",
        deepest.as_os_str().len(),
        deepest.display()
    );
    assert!(store.manifest(&checkpoint.id).is_ok());
}

/// The longest path the store actually built underneath `root`.
fn longest_path_under(root: &std::path::Path) -> std::path::PathBuf {
    let mut best = root.to_path_buf();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.as_os_str().len() > best.as_os_str().len() {
                best = path.clone();
            }
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    best
}
