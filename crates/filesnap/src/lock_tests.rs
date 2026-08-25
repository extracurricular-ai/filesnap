//! The lock, and the classifier that decides what a failure meant.
//!
//! The classifier is a pure function taking the platform, so the Windows and
//! network branches run here on Linux. That is the point of the parameter:
//! otherwise those branches are code nobody can execute until it is in a
//! user's hands.

#![allow(clippy::unwrap_used)]

use std::io::ErrorKind;

use super::*;
use pretty_assertions::assert_eq;

fn store() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn a_session_can_take_its_own_lock() {
    let dir = store();
    let guard = acquire(dir.path(), "s1", LOCK_BUDGET).unwrap();
    assert!(guard.is_some());
    assert!(guard.unwrap().is_enforced());
}

/// The whole of D18's first half: one session cannot run twice at once.
#[test]
fn a_second_holder_of_the_same_session_is_refused() {
    let dir = store();
    let _held = acquire(dir.path(), "s1", LOCK_BUDGET).unwrap().unwrap();

    // A short budget, because the test is the refusal and not the wait.
    let second = acquire(dir.path(), "s1", Duration::from_millis(30)).unwrap();
    assert!(
        second.is_none(),
        "two invocations of one session ran at once"
    );
}

/// And the whole of its second half: nothing wider is locked. Two sessions in
/// one workspace do not wait on each other.
#[test]
fn a_different_session_is_not_blocked() {
    let dir = store();
    let _held = acquire(dir.path(), "s1", LOCK_BUDGET).unwrap().unwrap();

    let other = acquire(dir.path(), "s2", Duration::from_millis(30)).unwrap();
    assert!(other.is_some(), "an unrelated session was serialized");
}

/// Releasing is the drop, so there is no way to forget it and no way for a
/// panic to skip it.
#[test]
fn dropping_the_guard_releases_it() {
    let dir = store();
    {
        let _held = acquire(dir.path(), "s1", LOCK_BUDGET).unwrap().unwrap();
    }
    assert!(
        acquire(dir.path(), "s1", Duration::from_millis(30))
            .unwrap()
            .is_some()
    );
}

/// **A lock held by a process that is killed is released with it.**
///
/// The central claim of the design, and the reason the kernel holds the lock
/// rather than a file's existence: git's `O_EXCL` convention leaves the
/// sentinel behind on `SIGKILL` and the resource stays unusable until a human
/// deletes it. Here there is nothing to delete.
///
/// The test re-invokes its own binary as the holder, because the property is
/// about a *process* dying and cannot be observed within one.
#[test]
#[cfg(unix)]
fn a_lock_dies_with_the_process_holding_it() {
    const HOLDER: &str = "FILESNAP_LOCK_HOLDER";

    // Re-invoked as the holder: take the lock and wait to be killed.
    if let Ok(dir) = std::env::var(HOLDER) {
        let _held = acquire(Path::new(&dir), "s1", LOCK_BUDGET)
            .unwrap()
            .unwrap();
        std::thread::sleep(Duration::from_secs(60));
        return;
    }

    let dir = store();
    let mut holder = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lock::tests::a_lock_dies_with_the_process_holding_it",
        ])
        .env(HOLDER, dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // Wait until it really holds the lock, so the assertion below cannot pass
    // for the trivial reason that nothing was ever taken.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "the holder never took the lock");
        if acquire(dir.path(), "s1", Duration::from_millis(20))
            .unwrap()
            .is_none()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    holder.kill().unwrap();
    holder.wait().unwrap();

    assert!(
        acquire(dir.path(), "s1", Duration::from_secs(5))
            .unwrap()
            .is_some(),
        "a killed holder wedged the session"
    );
}

/// The lock file is never unlinked, and that is deliberate: unlinking is what
/// reintroduces the races the kernel lock avoids.
#[test]
fn the_sentinel_outlives_the_lock() {
    let dir = store();
    drop(acquire(dir.path(), "s1", LOCK_BUDGET).unwrap());
    assert!(
        dir_in(dir.path())
            .join(format!("{}.lock", crate::id::record_name("s1")))
            .exists()
    );
}

/// An id that could escape its directory is refused before a path is built,
/// like every other path builder in the crate (D5).
#[test]
fn a_forged_session_id_cannot_place_the_sentinel() {
    let dir = store();
    for forged in ["../../etc/passwd", "..", "", "my session"] {
        assert!(
            matches!(
                acquire(dir.path(), forged, LOCK_BUDGET),
                Err(SnapshotError::InvalidId { .. })
            ),
            "{forged:?}"
        );
    }
}

// --- the classifier, on both platforms, from whichever one is running ---

#[test]
fn contention_is_recognised_on_both_platforms() {
    for platform in [Platform::Unix, Platform::Windows] {
        assert_eq!(
            classify(ErrorKind::WouldBlock, None, Site::Lock, platform),
            Disposition::Contended
        );
    }
    // Windows reports a delete-pending or unshared handle on the *open*.
    assert_eq!(
        classify(
            ErrorKind::PermissionDenied,
            Some(WINDOWS_SHARING_VIOLATION),
            Site::Open,
            Platform::Windows
        ),
        Disposition::Contended
    );
}

/// The reason the platform is a parameter rather than a `cfg!`: the numbers
/// collide, and merging the tables would read an ordinary `EPERM` as "this
/// filesystem has no locks" and proceed unlocked.
#[test]
fn code_one_means_opposite_things_on_the_two_platforms() {
    assert_eq!(
        classify(
            ErrorKind::PermissionDenied,
            Some(1),
            Site::Lock,
            Platform::Windows
        ),
        Disposition::Unsupported,
        "ERROR_INVALID_FUNCTION: this filesystem does not do locks"
    );
    assert_eq!(
        classify(
            ErrorKind::PermissionDenied,
            Some(1),
            Site::Lock,
            Platform::Unix
        ),
        Disposition::Fatal,
        "EPERM: a real failure, and proceeding unlocked would hide it"
    );
}

#[test]
fn a_filesystem_without_locking_is_proceeded_past_rather_than_refused() {
    for code in [95, 38, 37, 45] {
        assert_eq!(
            classify(ErrorKind::Other, Some(code), Site::Lock, Platform::Unix),
            Disposition::Unsupported,
            "unix code {code}"
        );
    }
    for code in [1, 50] {
        assert_eq!(
            classify(ErrorKind::Other, Some(code), Site::Lock, Platform::Windows),
            Disposition::Unsupported,
            "windows code {code}"
        );
    }
    assert_eq!(
        classify(ErrorKind::Unsupported, None, Site::Lock, Platform::Unix),
        Disposition::Unsupported
    );
}

/// A stale NFS handle is retried, not read as "no locking here" — reading it
/// that way would silently drop the lock on exactly the filesystem where
/// concurrent invocations are most likely.
#[test]
fn a_stale_network_handle_is_retried() {
    for platform in [Platform::Unix, Platform::Windows] {
        assert_eq!(
            classify(
                ErrorKind::StaleNetworkFileHandle,
                None,
                Site::Lock,
                platform
            ),
            Disposition::Transient
        );
        assert_eq!(
            classify(ErrorKind::Interrupted, None, Site::Lock, platform),
            Disposition::Transient
        );
    }
}

#[test]
fn an_unrecognised_failure_is_fatal_rather_than_ignored() {
    assert_eq!(
        classify(ErrorKind::NotFound, None, Site::Lock, Platform::Unix),
        Disposition::Fatal
    );
    assert_eq!(
        classify(ErrorKind::OutOfMemory, None, Site::Open, Platform::Windows),
        Disposition::Fatal
    );
}
