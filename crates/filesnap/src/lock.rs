//! One session, serialized against itself (D18).
//!
//! Several of the store's files are read-modify-write — a log append reads,
//! pushes and rewrites — and a session can race *itself*: a hook that fires
//! twice, or a user running a command while one is already going. A long-lived
//! library could not hit that; a CLI invoked twice can, and does.
//!
//! **Nothing wider is locked, deliberately.** Two sessions in one directory,
//! or in nested directories, is not a configuration the engine tries to make
//! coherent: each capture is still a truthful record of what was on disk at
//! that moment, and interleaved edits make the *user's* mental model harder
//! rather than making a snapshot wrong.
//!
//! # Why the kernel holds it, and not a file's existence
//!
//! The obvious design is git's: create `<name>.lock` with `O_CREAT|O_EXCL` and
//! unlink it after. Git's own header is candid that this has no stale-lock
//! detection at all — cleanup is an `atexit` handler plus a signal handler, so
//! a `SIGKILL` leaves the file on disk and the resource wedged until a human
//! deletes it. Even git's optional PID sidecar only improves the *message*; it
//! never breaks the lock, because "PIDs can be reused".
//!
//! That is unacceptable here. A hook that fires and dies would leave a session
//! permanently unusable, and the person hitting it has no reason to know which
//! file to delete.
//!
//! An OS advisory lock has no staleness concept because it needs none: the
//! kernel drops it when the holder's descriptor closes, `SIGKILL` included.
//! Cargo takes this route for the same reason and has no stale-lock code
//! anywhere. It is strictly the smaller design — no PID, no hostname, no
//! clock, no grace window, and nothing to break by hand.
//!
//! **`GC_GRACE` is deliberately not reused here.** `flock` never touches the
//! file's mtime, so an age test would be wrong in both directions: a lock held
//! for an hour looks ancient, and one taken a second ago on a file created
//! last week looks settled.
//!
//! # Why std rather than a crate
//!
//! `File::try_lock` and `File::unlock` are stable since Rust 1.89 and this
//! workspace pins 1.95, so `fs4`/`fd-lock`/`fs2` would buy nothing. The
//! dependency list is deliberately small and two of its entries (`ignore`,
//! `gix`) are already public-API commitments.

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use crate::error::Result;
use crate::error::SnapshotError;

/// How long an acquire waits before reporting the session busy.
///
/// Neither of git's extremes fits. `index.lock` fails immediately, which would
/// turn an ordinary double-fired hook into a user-visible error; blocking
/// forever would hang the hook instead. A capture holds the lock for the
/// length of a stat walk — hundreds of milliseconds on a large project — so
/// the budget is a small multiple of that.
pub(crate) const LOCK_BUDGET: Duration = Duration::from_secs(5);

/// Where a session's lock file lives inside a partition.
pub(crate) fn dir_in(partition: &Path) -> PathBuf {
    partition.join("locks")
}

/// A held session lock. Releasing is the drop of the file handle, so there is
/// no way to forget it and no way for a panic to skip it.
///
/// The lock file is **never unlinked.** Unlinking is what reintroduces every
/// problem the kernel lock avoids: on unix a second process can be holding a
/// descriptor to a file whose name is gone and think it holds the lock, and on
/// Windows the unlink can fail outright while another handle is open. An empty
/// file per session is a rounding error against the manifests beside it, and
/// collection reclaims one whose session is gone.
#[derive(Debug)]
pub(crate) struct SessionGuard {
    /// `None` when the filesystem has no locks and we proceeded without one.
    /// Representable rather than pretended: a guard that silently means
    /// nothing is worse than one that says so.
    _file: Option<File>,
}

impl SessionGuard {
    /// Whether the kernel is actually holding anything. False on a filesystem
    /// with no locking, where the operation went ahead unprotected.
    ///
    /// For `status` and `doctor` to report, once they exist: a store on a
    /// filesystem that cannot lock is a fact about the user's setup, and one
    /// they should be able to see rather than infer.
    #[allow(dead_code)]
    pub(crate) fn is_enforced(&self) -> bool {
        self._file.is_some()
    }

    fn held(file: File) -> Self {
        Self { _file: Some(file) }
    }

    fn unenforced() -> Self {
        Self { _file: None }
    }
}

/// What an error while locking means, decided without touching the
/// filesystem.
///
/// Pure and platform-independent **on purpose**: both tables are compiled on
/// every target, so the Windows and network-filesystem branches are reachable
/// from a Linux test. Otherwise they are code nobody can run until it is in a
/// user's hands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// Somebody else holds it. Wait and try again.
    Contended,
    /// Worth retrying: the handle went stale under a network filesystem, or
    /// the call was interrupted.
    Transient,
    /// This filesystem does not do locks. Proceed **unlocked**.
    ///
    /// Cargo's precedent, and the right call: refusing to work on a
    /// filesystem without `flock` would break a user who has no other
    /// machine, to prevent a race that needs two concurrent invocations of
    /// one session to matter at all.
    Unsupported,
    /// Something else. The caller decides.
    Fatal,
}

/// Where the error came from. The same code means different things depending
/// on whether we were opening the sentinel or locking it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Site {
    Open,
    Lock,
}

/// Which error-code table applies.
///
/// An explicit parameter rather than a `cfg!`, so both branches are reachable
/// from a test on either platform. It has to be *selected* rather than
/// merged: the numbers collide. Code 1 is `ERROR_INVALID_FUNCTION` on Windows
/// and `EPERM` on unix, so a merged table would read an ordinary permission
/// error as "this filesystem has no locks" and silently proceed unlocked —
/// which is the one outcome this module exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Unix,
    Windows,
}

impl Platform {
    pub(crate) const CURRENT: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Unix
    };
}

/// Codes meaning "this filesystem does not do locks".
const UNSUPPORTED_UNIX: [i32; 4] = [
    95, // ENOTSUP / EOPNOTSUPP on Linux
    38, // ENOSYS
    37, // ENOLCK
    45, // EOPNOTSUPP on the BSDs and macOS
];
const UNSUPPORTED_WINDOWS: [i32; 2] = [
    1,  // ERROR_INVALID_FUNCTION — what a filesystem without locking returns
    50, // ERROR_NOT_SUPPORTED
];
/// `ERROR_SHARING_VIOLATION`. On Windows a delete-pending name, or a handle
/// held without shared access, fails the *open* rather than the lock.
const WINDOWS_SHARING_VIOLATION: i32 = 32;

/// Classify a locking failure. See [`Disposition`].
pub(crate) fn classify(
    kind: io::ErrorKind,
    raw: Option<i32>,
    site: Site,
    platform: Platform,
) -> Disposition {
    if let Some(code) = raw {
        let unsupported = match platform {
            Platform::Unix => UNSUPPORTED_UNIX.contains(&code),
            Platform::Windows => UNSUPPORTED_WINDOWS.contains(&code),
        };
        if unsupported {
            return Disposition::Unsupported;
        }
        if platform == Platform::Windows && code == WINDOWS_SHARING_VIOLATION && site == Site::Open
        {
            return Disposition::Contended;
        }
    }
    match kind {
        io::ErrorKind::WouldBlock => Disposition::Contended,
        // NFS's "your handle died, reopen the path". Retrying is right, and
        // reading it as unsupported would silently drop the lock entirely.
        io::ErrorKind::StaleNetworkFileHandle | io::ErrorKind::Interrupted => {
            Disposition::Transient
        }
        io::ErrorKind::Unsupported => Disposition::Unsupported,
        _ => Disposition::Fatal,
    }
}

/// Take `session_id`'s lock, waiting up to `budget`.
///
/// `Ok(None)` means another invocation of this same session holds it — the
/// caller reports the session busy rather than proceeding, because proceeding
/// is what loses a log entry.
pub(crate) fn acquire(
    partition: &Path,
    session_id: &str,
    budget: Duration,
) -> Result<Option<SessionGuard>> {
    crate::id::validate_stored("session id", session_id)?;
    let dir = dir_in(partition);
    std::fs::create_dir_all(&dir).map_err(|e| SnapshotError::io(&dir, e))?;
    // Named for the digest like every other record, so two ids differing only
    // in case cannot share one sentinel on a case-insensitive filesystem —
    // which would serialize two unrelated sessions against each other, the
    // one thing D18 says is never done.
    let path = dir.join(format!("{}.lock", crate::id::record_name(session_id)));

    let deadline = Instant::now() + budget;
    let mut backoff = Duration::from_millis(2);
    loop {
        // `.read(true).write(true)`, never append: on Windows a file opened
        // only for append cannot be locked at all, and that is the one
        // platform this is least likely to be noticed on.
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
        {
            Ok(file) => match file.try_lock() {
                Ok(()) => return Ok(Some(SessionGuard::held(file))),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(err)) => {
                    match classify(
                        err.kind(),
                        err.raw_os_error(),
                        Site::Lock,
                        Platform::CURRENT,
                    ) {
                        // Held without the lock, which is the honest outcome
                        // on a filesystem that has none.
                        Disposition::Unsupported => {
                            return Ok(Some(SessionGuard::unenforced()));
                        }
                        Disposition::Contended | Disposition::Transient => {}
                        Disposition::Fatal => return Err(SnapshotError::io(&path, err)),
                    }
                }
            },
            Err(err) => match classify(
                err.kind(),
                err.raw_os_error(),
                Site::Open,
                Platform::CURRENT,
            ) {
                Disposition::Unsupported => return Ok(Some(SessionGuard::unenforced())),
                Disposition::Contended | Disposition::Transient => {}
                Disposition::Fatal => return Err(SnapshotError::io(&path, err)),
            },
        }

        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(100));
    }
}

#[cfg(test)]
#[path = "lock_tests.rs"]
mod tests;
