//! `filesnap doctor` — clear what an interrupted operation left behind.
//!
//! **The one command that writes to the user's own directory**, which is why
//! it is separate from `status`: a plugin can ask what the state is without
//! risking a write, and a person can look before deciding (D34).
//!
//! A restore writes a temporary beside each file and renames it into place, so
//! a process killed between the two leaves a stray in the project — somewhere
//! store collection can never reach, because it knows the store and not the
//! workspace. A later restore into the same directory clears it; a workspace
//! nothing restores into again keeps it forever, and whoever finds a
//! `.filesnap-restore-tmp` in their project is more likely to delete it by
//! hand or file a bug than to know what it is (D21).

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::event;
use crate::exit;

#[derive(Serialize)]
struct Residue {
    path: String,
    removed: bool,
}

#[derive(Serialize)]
struct Done {
    removed: usize,
    failed: usize,
}

pub fn run(out: &mut impl Write, workdir: &Path) -> u8 {
    let mut removed = 0;
    let mut failed = 0;
    for path in filesnap::residue_under(workdir) {
        // A stray we cannot remove is reported rather than swallowed: it is
        // in the user's own project, and they can act on a name.
        let ok = std::fs::remove_file(&path).is_ok();
        if ok {
            removed += 1;
        } else {
            failed += 1;
        }
        event::emit(
            out,
            "doctor.residue",
            Residue {
                path: path.to_string_lossy().into_owned(),
                removed: ok,
            },
        );
    }
    event::emit(out, "doctor.done", Done { removed, failed });
    if failed == 0 { exit::OK } else { exit::PARTIAL }
}
