//! `filesnap status` — what the state of this workspace is.
//!
//! **Read-only, and that is the point** (D34). `doctor` is the one that
//! changes things, so a plugin can ask without risking a write and a person
//! can look before deciding. Nothing here takes a session lock either: a lock
//! creates a file, and a command that promises to change nothing must not.
//!
//! Three things, because they answer three different questions:
//!
//! - **what is not protected** — the complete drop list, re-scanned now (D23);
//! - **what it costs** — split the way the store is split (D19), never summed;
//! - **how far back each session goes** — which is what a person is usually
//!   about to act on.

use std::io::Write;
use std::path::Path;

use filesnap::WorkspaceStore;
use serde::Serialize;

use crate::event;
use crate::event::DropReason;
use crate::exit;

#[derive(Serialize)]
struct Workspace<'a> {
    workspace: String,
    partition: &'a str,
}

#[derive(Serialize)]
struct Session<'a> {
    session: &'a str,
    turns: usize,
    /// The oldest turn still reachable — how far back this session can go.
    earliest: Option<&'a str>,
    latest: Option<&'a str>,
}

#[derive(Serialize)]
struct Unprotected {
    path: String,
    reason: DropReason,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    /// This workspace's records.
    records_bytes: u64,
    /// The content store, which every workspace shares.
    ///
    /// Reported *beside* the records rather than added to them: a blob is
    /// named by however many manifests name it, so charging it to one
    /// workspace would report the same bytes once per reference (D19).
    shared_content_bytes: u64,
}

pub fn run(out: &mut impl Write, data_dir: &Path, cwd: &Path) -> u8 {
    let store = match WorkspaceStore::open(data_dir, cwd) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("filesnap: cannot open the store: {err}");
            return exit::FAILED;
        }
    };

    event::emit(
        out,
        "status.workspace",
        Workspace {
            workspace: cwd.to_string_lossy().into_owned(),
            partition: store.key().as_str(),
        },
    );

    let sessions = match store.sessions() {
        Ok(sessions) => sessions,
        Err(err) => {
            eprintln!("filesnap: cannot list sessions: {err}");
            return exit::FAILED;
        }
    };
    for session in &sessions {
        // A session whose log will not parse is reported as unreadable rather
        // than skipped. Silently omitting it would make a damaged record look
        // like a session that was never there, which is the one thing a
        // dashboard must not do.
        let Ok(history) = store.thread_history(session) else {
            event::emit(
                out,
                "status.sessionUnreadable",
                Session {
                    session,
                    turns: 0,
                    earliest: None,
                    latest: None,
                },
            );
            continue;
        };
        let mut turns: Vec<&str> = history
            .iter()
            .map(|(entry, _)| entry.turn_id.as_str())
            .filter(|id| !id.starts_with(filesnap::SAFETY_TURN_PREFIX))
            .collect();
        turns.dedup();
        event::emit(
            out,
            "status.session",
            Session {
                session,
                turns: turns.len(),
                earliest: turns.first().copied(),
                latest: turns.last().copied(),
            },
        );
    }

    // Complete, not the per-turn sample: this is the diagnostic half of D23,
    // and it re-scans rather than reading anything a capture stored, so the
    // answer is about the project as it stands now.
    for (path, reason) in filesnap::scan_report(
        &[cwd.to_path_buf()],
        filesnap::HiddenFiles::Skip,
        filesnap::ScanLimits::default(),
    ) {
        event::emit(
            out,
            "status.unprotected",
            Unprotected {
                path: path.to_string_lossy().into_owned(),
                reason: reason.into(),
            },
        );
    }

    let records = store.records_disk_usage().unwrap_or(0);
    let content = filesnap::content_disk_usage(data_dir).unwrap_or(0);
    event::emit(
        out,
        "status.usage",
        Usage {
            records_bytes: records,
            shared_content_bytes: content,
        },
    );
    exit::OK
}
