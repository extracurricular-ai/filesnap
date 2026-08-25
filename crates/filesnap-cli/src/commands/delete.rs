//! `filesnap delete` — end a session's data.
//!
//! **The single source of truth**, and it depends on nothing else having run
//! first (D9). Its promise is unreachability, not disk: the records go and the
//! session can no longer be reached, while the bytes wait for the next `gc`.
//! Content is deduplicated across every workspace, so "is anyone else still
//! using this" is a question only a whole-store sweep can answer (D19).

use std::io::Write;
use std::path::Path;

use filesnap::WorkspaceStore;
use serde::Serialize;

use crate::event;
use crate::exit;

#[derive(Serialize)]
struct Session<'a> {
    session: &'a str,
}

#[derive(Serialize)]
struct Problem<'a> {
    session: &'a str,
    error: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Done {
    deleted: usize,
    /// Sessions that were not there to begin with. The postcondition holds
    /// for them too — they are unreachable — but nothing was removed.
    absent: usize,
    refused: usize,
    incomplete: usize,
    manifests_removed: usize,
}

pub fn run(out: &mut impl Write, data_dir: &Path, cwd: &Path, sessions: &[String]) -> u8 {
    let store = match WorkspaceStore::open(data_dir, cwd) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("filesnap: cannot open the store: {err}");
            return exit::FAILED;
        }
    };

    // Which of these were there at all, asked before the delete because
    // afterwards the answer is the same either way. Deleting something that
    // was never tracked is not an error — D9 makes delete idempotent and
    // precondition-free — but reporting it as *deleted* would be a small lie
    // to a caller that is counting.
    let existed: Vec<bool> = sessions
        .iter()
        .map(|session| store.session_exists(session))
        .collect();

    let outcome = store.delete_sessions(sessions);

    // Everything not named in `refused` or `incomplete` went.
    let mut deleted = 0;
    let mut absent = 0;
    for (session, was_there) in sessions.iter().zip(existed) {
        let named =
            |list: &[(String, filesnap::SnapshotError)]| list.iter().any(|(id, _)| id == session);
        if named(&outcome.refused) || named(&outcome.incomplete) {
            continue;
        }
        if was_there {
            deleted += 1;
            event::emit(out, "delete.deleted", Session { session });
        } else {
            absent += 1;
            event::emit(out, "delete.absent", Session { session });
        }
    }

    // **Left exactly as it was**, and the call can be retried.
    for (session, err) in &outcome.refused {
        event::emit(
            out,
            "delete.refused",
            Problem {
                session,
                error: err.to_string(),
            },
        );
    }
    // Began and did not finish — a different claim, and one a caller must not
    // read as "untouched". Retrying is still right: delete is idempotent.
    for (session, err) in &outcome.incomplete {
        event::emit(
            out,
            "delete.incomplete",
            Problem {
                session,
                error: err.to_string(),
            },
        );
    }
    if let Some(err) = &outcome.sweep_error {
        // Reclamation was never part of delete's success criterion (VIII.3):
        // the sessions above are unreachable either way, and the records wait
        // for the next collection.
        eprintln!("filesnap: records were not reclaimed: {err}");
    }

    event::emit(
        out,
        "delete.done",
        Done {
            deleted,
            absent,
            refused: outcome.refused.len(),
            incomplete: outcome.incomplete.len(),
            manifests_removed: outcome.reclaimed.manifests_removed,
        },
    );

    if outcome.refused.is_empty() && outcome.incomplete.is_empty() {
        exit::OK
    } else {
        exit::PARTIAL
    }
}
