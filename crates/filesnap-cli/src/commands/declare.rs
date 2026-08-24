//! `filesnap declare` — what a path holds *before* an edit changes it.
//!
//! Called before the edit, which is the whole point: the pre-image is the only
//! thing that can be recovered, and after the write it is gone (D30). The
//! caller names the paths; this reads them.
//!
//! Many paths in one invocation, because a turn with twenty edits would
//! otherwise be twenty process spawns. The expensive operation is `capture`, a
//! stat walk measured in hundreds of milliseconds, against which startup does
//! not register — but twenty startups do (D29).

use std::io::Write;
use std::path::PathBuf;

use filesnap::PreEditImage;
use filesnap::TurnScope;
use filesnap::WorkspaceStore;
use serde::Serialize;

use crate::event;
use crate::exit;

#[derive(Serialize)]
struct Recorded {
    path: String,
    existed: bool,
}

#[derive(Serialize)]
struct Ignored {
    path: String,
}

#[derive(Serialize)]
struct Done {
    recorded: usize,
    ignored: usize,
}

pub fn run(
    out: &mut impl Write,
    data_dir: &std::path::Path,
    session: &str,
    turn: &str,
    cwd: PathBuf,
    paths: Vec<PathBuf>,
) -> u8 {
    let store = match WorkspaceStore::open(data_dir, &cwd) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("filesnap: cannot open the store: {err}");
            return exit::FAILED;
        }
    };

    // Read each pre-image here rather than taking it from the caller. A caller
    // that passed the content could pass the *post*-edit content by mistake,
    // and nothing downstream could tell — the store would hold a snapshot of
    // the change rather than of what preceded it.
    let images: Vec<(PathBuf, PreEditImage)> = paths
        .into_iter()
        .map(|path| {
            let image = match std::fs::read(&path) {
                Ok(bytes) => PreEditImage::Existed(bytes),
                // Not an error: a path the edit is about to *create* is the
                // case that most needs recording, because the tombstone is the
                // only thing that ever licenses a rewind to remove the file
                // again.
                Err(_) => PreEditImage::DidNotExist,
            };
            (path, image)
        })
        .collect();

    let scope = TurnScope::at(cwd);
    let outcome = match filesnap::declare_edits(&store, session, turn, &scope, images) {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!("filesnap: declare failed: {err}");
            return exit::FAILED;
        }
    };

    for path in &outcome.recorded {
        event::emit(
            out,
            "declare.recorded",
            Recorded {
                path: path.to_string_lossy().into_owned(),
                existed: path.exists(),
            },
        );
    }
    for path in &outcome.ignored {
        event::emit(
            out,
            "declare.ignored",
            Ignored {
                path: path.to_string_lossy().into_owned(),
            },
        );
    }
    event::emit(
        out,
        "declare.done",
        Done {
            recorded: outcome.recorded.len(),
            ignored: outcome.ignored.len(),
        },
    );
    exit::OK
}
