//! `filesnap gc` — reclaim what nothing references.
//!
//! **Spans every workspace**, which is why it takes no `--cwd`: content
//! liveness is a whole-store question, because a blob written for one
//! workspace may be named by a manifest in another (D19).
//!
//! **Running it changes nothing anyone can observe.** Collect, or do not
//! collect, any number of times: every turn still resolves to the same
//! manifest and every session can still rewind exactly as far. Only bytes
//! nothing can reach go away.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::event;
use crate::exit;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Done {
    manifests_removed: usize,
    manifests_kept: usize,
    blobs_removed: usize,
    blobs_kept: usize,
}

pub fn run(out: &mut impl Write, data_dir: &Path) -> u8 {
    let stats = match filesnap::collect_garbage(data_dir) {
        Ok(stats) => stats,
        Err(err) => {
            eprintln!("filesnap: collection failed: {err}");
            return exit::FAILED;
        }
    };
    event::emit(
        out,
        "gc.done",
        Done {
            manifests_removed: stats.manifests_removed,
            manifests_kept: stats.manifests_kept,
            blobs_removed: stats.blobs_removed,
            blobs_kept: stats.blobs_kept,
        },
    );
    exit::OK
}
