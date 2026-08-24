//! `filesnap log` — what a session can rewind to.
//!
//! The output is a list of **turn ids**, because a turn id is the only thing
//! `restore` accepts (D35). Printing anything a caller cannot then pass back
//! would be a listing of options that are not options.

use std::collections::BTreeMap;
use std::io::Write;

use filesnap::WorkspaceStore;
use serde::Serialize;

use crate::event;
use crate::exit;

#[derive(Serialize)]
struct Entry<'a> {
    turn: &'a str,
    manifest: &'a str,
    at: u64,
    files: usize,
    absent: usize,
}

#[derive(Serialize)]
struct Done {
    turns: usize,
}

pub fn run(
    out: &mut impl Write,
    data_dir: &std::path::Path,
    cwd: &std::path::Path,
    session: &str,
    limit: Option<usize>,
) -> u8 {
    let store = match WorkspaceStore::open(data_dir, cwd) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("filesnap: cannot open the store: {err}");
            return exit::FAILED;
        }
    };

    let history = match store.thread_history(session) {
        Ok(history) => history,
        Err(err) => {
            eprintln!("filesnap: cannot read the session log: {err}");
            return exit::FAILED;
        }
    };

    // One line per **turn**, not per log entry. A turn holds several entries —
    // the turn-start scan plus every pre-edit attach — and a restore resolves
    // a turn to the last of them, which is the most complete. Listing the
    // entries would make one turn look like several rewind points that all go
    // to the same place, and would make `--limit N` mean an unpredictable
    // number of turns (D6).
    //
    // `BTreeMap` keyed by turn id, and later entries overwrite earlier ones,
    // so what is kept is the same entry `restore` would resolve to.
    let mut by_turn: BTreeMap<&str, (&filesnap::SnapshotRef, &filesnap::Manifest)> =
        BTreeMap::new();
    for (entry, manifest) in &history {
        // Safety checkpoints live in this log too, under a reserved turn id
        // the caller is not allowed to pass to `restore`. Showing one would
        // offer a rewind point that is refused the moment it is used.
        if entry.turn_id.starts_with(filesnap::SAFETY_TURN_PREFIX) {
            continue;
        }
        by_turn.insert(&entry.turn_id, (entry, manifest));
    }

    // Ordered by when the turn was first seen, which is the order the session
    // ran in — a map ordered by id would sort by whatever the host's ids
    // happen to look like.
    let mut turns: Vec<_> = by_turn.into_values().collect();
    turns.sort_by_key(|(entry, _)| entry.at);

    // The most recent, since that is what someone asking for "the last few"
    // means. Taken from the end rather than the start.
    let shown = match limit {
        Some(n) if n < turns.len() => &turns[turns.len() - n..],
        _ => &turns[..],
    };

    for (entry, manifest) in shown {
        event::emit(
            out,
            "log.entry",
            Entry {
                turn: &entry.turn_id,
                manifest: &entry.manifest_id,
                at: entry.at,
                files: manifest.entries.len(),
                absent: manifest.absent.len(),
            },
        );
    }
    event::emit(out, "log.done", Done { turns: shown.len() });
    exit::OK
}
