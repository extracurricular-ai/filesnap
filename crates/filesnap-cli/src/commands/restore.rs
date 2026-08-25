//! `filesnap restore` and `filesnap undo` — the two commands that write to
//! the user's own files.
//!
//! **Addressed by turn id only** (D35). "Go back N steps" was considered and
//! refused: the cost of getting a restore wrong is the user's work, and an id
//! is the one form that cannot quietly come to mean something else between
//! being read and being used.
//!
//! **Every restore is reversible.** A safety checkpoint of the current state
//! is captured before anything is written, and its manifest id is in the
//! terminal event — so a caller who does not like the result has somewhere to
//! go. That id arriving in a fixed place is the whole of C20: the
//! reversibility existed before and was out of reach at the moment it was
//! needed.

use std::io::Write;
use std::path::Path;

use filesnap::RestoreKind;
use filesnap::TurnScope;
use filesnap::WorkspaceStore;
use serde::Serialize;

use crate::event;
use crate::exit;

#[derive(Serialize)]
struct Planned<'a> {
    turn: Option<&'a str>,
    manifest: &'a str,
    writes: usize,
    deletes: usize,
}

#[derive(Serialize)]
struct Path_<'a> {
    path: &'a str,
}

#[derive(Serialize)]
struct Failed<'a> {
    path: String,
    error: &'a str,
}

#[derive(Serialize)]
struct Done<'a> {
    written: usize,
    deleted: usize,
    failed: usize,
    /// Where to go back to. Present even when files failed — especially then.
    safety: &'a str,
}

#[derive(Serialize)]
struct Conflict<'a> {
    path: &'a str,
}

fn open(data_dir: &Path, cwd: &Path) -> Result<WorkspaceStore, u8> {
    WorkspaceStore::open(data_dir, cwd).map_err(|err| {
        eprintln!("filesnap: cannot open the store: {err}");
        exit::FAILED
    })
}

pub fn restore(
    out: &mut impl Write,
    data_dir: &Path,
    cwd: &Path,
    session: &str,
    turn: &str,
    undo_for: Option<&str>,
) -> u8 {
    let store = match open(data_dir, cwd) {
        Ok(store) => store,
        Err(code) => return code,
    };

    let target = match store.target_for_turn(turn) {
        Ok(Some(target)) => target,
        Ok(None) => {
            eprintln!("filesnap: no snapshot for turn {turn}");
            return exit::USAGE;
        }
        Err(err) => {
            eprintln!("filesnap: cannot resolve turn {turn}: {err}");
            return exit::FAILED;
        }
    };

    apply(
        out,
        &store,
        cwd,
        session,
        Some(turn),
        &target,
        RestoreKind::Rewind { undo_for },
    )
}

pub fn undo(out: &mut impl Write, data_dir: &Path, cwd: &Path, session: &str) -> u8 {
    let store = match open(data_dir, cwd) {
        Ok(store) => store,
        Err(code) => return code,
    };

    let target = match store.last_restore_target(session) {
        Ok(Some(target)) => target,
        Ok(None) => {
            eprintln!("filesnap: session {session} has no restore to undo");
            return exit::USAGE;
        }
        Err(err) => {
            eprintln!("filesnap: cannot read the undo record: {err}");
            return exit::FAILED;
        }
    };

    // **Report what has moved since the rewind, before overwriting it.**
    //
    // The undo records are private to a session but the files are not, so
    // this is the only thing standing between a concurrent edit — another
    // session, the user's own editor — and work disappearing without a word.
    //
    // It reports rather than refuses, because the safety checkpoint below
    // captures that work *before* the undo writes over it: the change is
    // recoverable from the `safety` id in the terminal event. What must not
    // happen is this reading as an uneventful success, so the exit code says
    // otherwise.
    let rules = filesnap::load_ignore(cwd);
    let conflicts = store.undo_conflicts(session, &rules).unwrap_or_default();
    for path in &conflicts {
        event::emit(out, "undo.conflict", Conflict { path });
    }

    let code = apply(
        out,
        &store,
        cwd,
        session,
        None,
        &target,
        RestoreKind::Undo { spending: session },
    );
    if code == exit::OK && !conflicts.is_empty() {
        exit::PARTIAL
    } else {
        code
    }
}

fn apply(
    out: &mut impl Write,
    store: &WorkspaceStore,
    cwd: &Path,
    session: &str,
    turn: Option<&str>,
    target: &filesnap::RestoreTarget,
    kind: RestoreKind<'_>,
) -> u8 {
    let scope = TurnScope::at(cwd);
    let files = match filesnap::restore_scope(store, session, &scope) {
        Ok(files) => files,
        Err(err) => {
            eprintln!("filesnap: cannot work out what to look at: {err}");
            return exit::FAILED;
        }
    };
    // Read fresh, so newly ignoring a path protects it retroactively (II.3).
    let rules = filesnap::load_ignore(&scope.ignore_root());

    let outcome = match store.restore_to(session, target, kind, files, &rules) {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!("filesnap: restore failed: {err}");
            return exit::FAILED;
        }
    };

    event::emit(
        out,
        "restore.planned",
        Planned {
            turn,
            manifest: target.manifest_id(),
            writes: outcome.plan.writes.len(),
            deletes: outcome.plan.deletes.len(),
        },
    );
    for write in &outcome.plan.writes {
        event::emit(out, "restore.written", Path_ { path: &write.path });
    }
    for path in &outcome.plan.deletes {
        event::emit(out, "restore.deleted", Path_ { path });
    }
    // Per file, and streaming, because a failure list inside one terminal
    // event is the only unbounded fragment anywhere in the system (D40).
    for (path, err) in &outcome.stats.failed {
        event::emit(
            out,
            "restore.failed",
            Failed {
                path: path.to_string_lossy().into_owned(),
                error: &err.to_string(),
            },
        );
    }

    event::emit(
        out,
        "restore.done",
        Done {
            written: outcome.stats.written,
            deleted: outcome.stats.deleted,
            failed: outcome.stats.failed.len(),
            safety: outcome.safety.manifest_id(),
        },
    );

    // D28: a restore with files it could not write must not read as success
    // anywhere, and that includes here.
    if outcome.stats.failed.is_empty() {
        exit::OK
    } else {
        exit::PARTIAL
    }
}
