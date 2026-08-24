//! `filesnap capture` — the state of a workspace at the start of a turn.

use std::io::Write;
use std::path::PathBuf;

use filesnap::TurnScope;
use filesnap::WorkspaceStore;
use serde::Serialize;

use crate::event;
use crate::event::DropReason;
use crate::exit;

#[derive(Serialize)]
struct Started<'a> {
    session: &'a str,
    turn: &'a str,
    roots: Vec<String>,
}

#[derive(Serialize)]
struct Dropped {
    path: String,
    reason: DropReason,
}

#[derive(Serialize)]
struct Done<'a> {
    manifest: &'a str,
    reused: usize,
    hashed: usize,
    dropped: usize,
}

pub fn run(
    out: &mut impl Write,
    data_dir: &std::path::Path,
    session: &str,
    turn: &str,
    cwd: PathBuf,
    roots: Vec<PathBuf>,
) -> u8 {
    let store = match WorkspaceStore::open(data_dir, &cwd) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("filesnap: cannot open the store: {err}");
            return exit::FAILED;
        }
    };
    let scope = TurnScope {
        cwd,
        roots,
        hidden: filesnap::HiddenFiles::Skip,
        limits: filesnap::ScanLimits::default(),
    };

    event::emit(
        out,
        "capture.started",
        Started {
            session,
            turn,
            roots: scope
                .scan_roots()
                .iter()
                .map(|r| r.to_string_lossy().into_owned())
                .collect(),
        },
    );

    let checkpoint = match filesnap::capture_turn(&store, session, turn, &scope) {
        Ok(checkpoint) => checkpoint,
        Err(err) => {
            eprintln!("filesnap: capture failed: {err}");
            return exit::FAILED;
        }
    };

    // The sample, not the count: what a person can act on is a filename, and
    // the count is in the terminal event for anyone totalling them up (D23).
    for (path, reason) in &checkpoint.stats.sample {
        event::emit(
            out,
            "capture.dropped",
            Dropped {
                path: path.to_string_lossy().into_owned(),
                reason: (*reason).into(),
            },
        );
    }

    event::emit(
        out,
        "capture.done",
        Done {
            manifest: &checkpoint.id,
            reused: checkpoint.stats.reused,
            hashed: checkpoint.stats.hashed,
            dropped: checkpoint.stats.dropped,
        },
    );
    exit::OK
}
