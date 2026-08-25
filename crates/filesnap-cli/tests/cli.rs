//! The command surface as a consumer meets it: a real process, real argv, a
//! real exit code, and JSONL on stdout.
//!
//! These spawn the binary rather than calling the command functions, because
//! argument parsing and the exit code are as much of the contract as the
//! events are — and neither is exercised by calling `run` directly.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::process::Output;

use pretty_assertions::assert_eq;
use serde_json::Value;

struct Run {
    out: Vec<Value>,
    stderr: String,
    code: i32,
}

impl Run {
    fn of(output: Output) -> Self {
        let stdout = String::from_utf8(output.stdout).unwrap();
        Self {
            out: stdout
                .lines()
                .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l:?}: {e}")))
                .collect(),
            stderr: String::from_utf8(output.stderr).unwrap(),
            code: output.status.code().unwrap_or(-1),
        }
    }

    fn kinds(&self) -> Vec<&str> {
        self.out
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect()
    }

    fn find(&self, kind: &str) -> &Value {
        self.out
            .iter()
            .find(|e| e["type"] == kind)
            .unwrap_or_else(|| panic!("no {kind} in {:?}", self.kinds()))
    }
}

fn filesnap(data: &Path, args: &[&str]) -> Run {
    Run::of(
        Command::new(env!("CARGO_BIN_EXE_filesnap"))
            .arg("--data-dir")
            .arg(data)
            .args(args)
            .output()
            .unwrap(),
    )
}

/// A workspace with one file, and a store directory beside it.
fn workspace() -> (tempfile::TempDir, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("a.txt"), "one").unwrap();
    (data, ws)
}

#[test]
fn a_capture_reports_a_manifest_and_succeeds() {
    let (data, ws) = workspace();
    let run = filesnap(
        data.path(),
        &[
            "capture",
            "--session",
            "s1",
            "--turn",
            "t1",
            "--cwd",
            &ws.path().to_string_lossy(),
        ],
    );

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.kinds(), vec!["capture.started", "capture.done"]);
    assert_eq!(run.find("capture.done")["hashed"], 1);
    assert!(run.find("capture.done")["manifest"].as_str().unwrap().len() == 64);
}

/// Every line carries the version, because a consumer may hold exactly one
/// line — JSONL gets grepped, tailed and split (D39).
#[test]
fn every_line_carries_the_schema_version() {
    let (data, ws) = workspace();
    let run = filesnap(
        data.path(),
        &[
            "capture",
            "--session",
            "s1",
            "--turn",
            "t1",
            "--cwd",
            &ws.path().to_string_lossy(),
        ],
    );

    assert!(!run.out.is_empty());
    for line in &run.out {
        assert_eq!(line["v"], 1, "{line}");
    }
}

/// Prose never reaches stdout. A consumer parses every line without having to
/// tell the contract from commentary (D32).
#[test]
fn stdout_is_only_the_contract() {
    let (data, ws) = workspace();
    let run = filesnap(
        data.path(),
        &[
            "capture",
            "--session",
            "s1",
            "--turn",
            "t1",
            "--cwd",
            &ws.path().to_string_lossy(),
        ],
    );
    // Run::of already parsed every line as JSON; this asserts there was
    // something to parse and that nothing else crept in.
    assert!(run.out.len() >= 2);
    assert!(run.stderr.is_empty(), "stderr: {}", run.stderr);
}

/// An id that cannot be a filename is refused rather than rewritten, and the
/// refusal reaches the caller as a failing exit rather than a silent success.
#[test]
fn a_bad_id_fails_rather_than_being_rewritten() {
    let (data, ws) = workspace();
    let run = filesnap(
        data.path(),
        &[
            "capture",
            "--session",
            "my session",
            "--turn",
            "t1",
            "--cwd",
            &ws.path().to_string_lossy(),
        ],
    );

    assert_ne!(run.code, 0);
    assert!(run.stderr.contains("invalid"), "stderr: {}", run.stderr);
}

/// Declare reads the pre-image itself, and says which paths the ignore rules
/// kept out — an ignored file must not enter the store through the edit API.
#[test]
fn declare_reads_pre_images_and_honours_the_ignore_rules() {
    let (data, ws) = workspace();
    std::fs::write(ws.path().join(".filesnapignore"), "*.key\n").unwrap();
    std::fs::write(ws.path().join("private.key"), "material").unwrap();

    let run = filesnap(
        data.path(),
        &[
            "declare",
            "--session",
            "s1",
            "--turn",
            "t1",
            "--cwd",
            &ws.path().to_string_lossy(),
            "--path",
            &ws.path().join("a.txt").to_string_lossy(),
            "--path",
            &ws.path().join("private.key").to_string_lossy(),
        ],
    );

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.find("declare.done")["recorded"], 1);
    assert_eq!(run.find("declare.done")["ignored"], 1);
    assert!(
        run.find("declare.ignored")["path"]
            .as_str()
            .unwrap()
            .ends_with("private.key")
    );
}

/// A path the edit is about to *create* is the case that most needs
/// recording: the tombstone is the only thing that ever licenses a rewind to
/// remove the file again.
#[test]
fn declaring_a_path_that_does_not_exist_yet_is_not_an_error() {
    let (data, ws) = workspace();
    let unborn = ws.path().join("will-be-created.txt");

    let run = filesnap(
        data.path(),
        &[
            "declare",
            "--session",
            "s1",
            "--turn",
            "t1",
            "--cwd",
            &ws.path().to_string_lossy(),
            "--path",
            &unborn.to_string_lossy(),
        ],
    );

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.find("declare.recorded")["existed"], false);
}

/// Two processes, no shared memory: what one declares, the next captures.
/// That is the whole of D38 as a consumer experiences it.
#[test]
fn a_later_process_captures_what_an_earlier_one_declared() {
    let (data, ws) = workspace();
    // A path outside the workspace, so only the declared set can carry it.
    let outside = data.path().join("outside.cfg");
    std::fs::write(&outside, "before").unwrap();

    let declared = filesnap(
        data.path(),
        &[
            "declare",
            "--session",
            "s1",
            "--turn",
            "t1",
            "--cwd",
            &ws.path().to_string_lossy(),
            "--path",
            &outside.to_string_lossy(),
        ],
    );
    assert_eq!(declared.code, 0, "{}", declared.stderr);

    let captured = filesnap(
        data.path(),
        &[
            "capture",
            "--session",
            "s1",
            "--turn",
            "t1",
            "--cwd",
            &ws.path().to_string_lossy(),
        ],
    );
    assert_eq!(captured.code, 0, "{}", captured.stderr);
    assert_eq!(
        captured.find("capture.done")["hashed"],
        2,
        "the declared path outside the workspace was not captured"
    );
}

/// Usage errors are their own exit code, because they are the one non-zero
/// case meaning nothing was attempted.
#[test]
fn a_missing_required_argument_is_a_usage_error() {
    let run = Run::of(
        Command::new(env!("CARGO_BIN_EXE_filesnap"))
            .args(["capture", "--session", "s1"])
            .output()
            .unwrap(),
    );
    assert_ne!(run.code, 0);
    assert!(
        run.out.is_empty(),
        "nothing was attempted, so nothing is reported"
    );
}

// --- log ---

/// One line per **turn**, not per log entry.
///
/// A turn holds several entries — the turn-start scan plus every pre-edit
/// attach — and a restore resolves a turn to the last of them. Listing the
/// entries would make one turn look like several rewind points that all go to
/// the same place, and would make `--limit N` mean an unpredictable number of
/// turns (D6).
#[test]
fn log_lists_turns_rather_than_log_entries() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();

    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );
    // Three attaches, all inside turn t1: three more log entries, one turn.
    for name in ["x.txt", "y.txt", "z.txt"] {
        std::fs::write(ws.path().join(name), "before").unwrap();
        filesnap(
            data.path(),
            &[
                "declare",
                "--session",
                "s1",
                "--turn",
                "t1",
                "--cwd",
                &cwd,
                "--path",
                &ws.path().join(name).to_string_lossy(),
            ],
        );
    }

    let run = filesnap(data.path(), &["log", "--session", "s1", "--cwd", &cwd]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.find("log.done")["turns"], 1, "{:?}", run.kinds());
}

/// A safety checkpoint is in the log but is not a rewind point a caller can
/// name: its turn id is in the reserved namespace, and `restore` refuses one.
/// Listing it would offer an option that fails the moment it is taken.
#[test]
fn log_hides_the_safety_checkpoints_a_caller_cannot_use() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );

    // A restore writes a safety checkpoint into this same session's log.
    let store = filesnap::WorkspaceStore::open(data.path(), ws.path()).unwrap();
    let target = store.target_for_turn("t1").unwrap().unwrap();
    store
        .restore_to(
            "s1",
            &target,
            filesnap::RestoreKind::Rewind { undo_for: None },
            vec![ws.path().join("a.txt")],
            &filesnap::fixture::no_rules(),
        )
        .unwrap();
    drop(store);

    let run = filesnap(data.path(), &["log", "--session", "s1", "--cwd", &cwd]);
    let turns: Vec<&str> = run
        .out
        .iter()
        .filter(|e| e["type"] == "log.entry")
        .map(|e| e["turn"].as_str().unwrap())
        .collect();
    assert_eq!(
        turns,
        vec!["t1"],
        "a reserved turn id was offered as a rewind point"
    );
}

/// `--limit N` shows the most recent N, because that is what "the last few"
/// means.
#[test]
fn log_limits_from_the_recent_end() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    for turn in ["t1", "t2", "t3"] {
        std::fs::write(ws.path().join("a.txt"), turn).unwrap();
        filesnap(
            data.path(),
            &["capture", "--session", "s1", "--turn", turn, "--cwd", &cwd],
        );
    }

    let run = filesnap(
        data.path(),
        &["log", "--session", "s1", "--limit", "2", "--cwd", &cwd],
    );
    let turns: Vec<&str> = run
        .out
        .iter()
        .filter(|e| e["type"] == "log.entry")
        .map(|e| e["turn"].as_str().unwrap())
        .collect();
    assert_eq!(turns, vec!["t2", "t3"]);
}

#[test]
fn log_of_a_session_with_no_captures_is_empty_rather_than_an_error() {
    let (data, ws) = workspace();
    let run = filesnap(
        data.path(),
        &[
            "log",
            "--session",
            "never-used",
            "--cwd",
            &ws.path().to_string_lossy(),
        ],
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.find("log.done")["turns"], 0);
}

// --- status ---

/// The dashboard answers three separate questions, and reports disk usage
/// *split* the way the store is split rather than summed (D19, D34).
#[test]
fn status_reports_sessions_unprotected_files_and_split_usage() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    std::fs::write(ws.path().join("huge.bin"), vec![0u8; 17 * 1024 * 1024]).unwrap();
    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );

    let run = filesnap(data.path(), &["status", "--cwd", &cwd]);
    assert_eq!(run.code, 0, "{}", run.stderr);

    assert_eq!(run.find("status.session")["session"], "s1");
    assert_eq!(run.find("status.session")["turns"], 1);
    assert_eq!(run.find("status.unprotected")["reason"], "overSizeLimit");

    let usage = run.find("status.usage");
    assert!(usage["recordsBytes"].as_u64().unwrap() > 0);
    assert!(
        usage["sharedContentBytes"].is_u64(),
        "content is reported beside the records, never folded into them"
    );
}

/// Read-only means read-only: running it twice changes nothing, and it does
/// not take a session lock, because a lock creates a file (D34).
#[test]
fn status_changes_nothing() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );

    let fingerprint = |root: &Path| {
        let mut seen = Vec::new();
        fn walk(dir: &Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(format!(
                        "{}:{}",
                        path.display(),
                        entry.metadata().unwrap().len()
                    ));
                }
            }
        }
        walk(root, &mut seen);
        seen.sort();
        seen
    };

    let before = fingerprint(data.path());
    filesnap(data.path(), &["status", "--cwd", &cwd]);
    filesnap(data.path(), &["status", "--cwd", &cwd]);
    assert_eq!(
        fingerprint(data.path()),
        before,
        "status wrote to the store"
    );
}

/// Field names on the wire are camelCase, like the enum values. The first
/// multi-word field is where a convention gets set by accident; this pins it.
#[test]
fn wire_field_names_are_camel_case() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );

    let run = filesnap(data.path(), &["status", "--cwd", &cwd]);
    for line in &run.out {
        for key in line.as_object().unwrap().keys() {
            assert!(
                !key.contains('_'),
                "{key} is snake_case; the contract is camelCase"
            );
        }
    }
}

// --- restore and undo ---

/// A rewind puts the workspace back, and hands back the point it can be
/// reversed to. That id arriving in a fixed place is the whole of C20.
#[test]
fn a_restore_puts_files_back_and_reports_its_safety_point() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    std::fs::write(ws.path().join("gone-later.txt"), "here").unwrap();
    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );

    std::fs::write(ws.path().join("a.txt"), "changed").unwrap();
    std::fs::remove_file(ws.path().join("gone-later.txt")).unwrap();

    let run = filesnap(
        data.path(),
        &[
            "restore",
            "--session",
            "s1",
            "--turn",
            "t1",
            "--undo-for",
            "s1",
            "--cwd",
            &cwd,
        ],
    );

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        std::fs::read_to_string(ws.path().join("a.txt")).unwrap(),
        "one"
    );
    assert_eq!(
        std::fs::read_to_string(ws.path().join("gone-later.txt")).unwrap(),
        "here",
        "a file the turn had is put back"
    );
    assert!(run.find("restore.done")["safety"].as_str().unwrap().len() == 64);
}

/// The round trip, and it must be quiet: an ordinary undo of an ordinary
/// rewind reports nothing moved and exits clean.
#[test]
fn an_undo_reverses_the_rewind_without_crying_wolf() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    std::fs::write(ws.path().join("gone-later.txt"), "here").unwrap();
    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );

    std::fs::write(ws.path().join("a.txt"), "changed").unwrap();
    std::fs::remove_file(ws.path().join("gone-later.txt")).unwrap();
    filesnap(
        data.path(),
        &[
            "restore",
            "--session",
            "s1",
            "--turn",
            "t1",
            "--undo-for",
            "s1",
            "--cwd",
            &cwd,
        ],
    );

    let run = filesnap(data.path(), &["undo", "--session", "s1", "--cwd", &cwd]);

    assert_eq!(
        run.code,
        0,
        "an ordinary round trip reported a conflict: {:?}",
        run.kinds()
    );
    assert!(!run.kinds().contains(&"undo.conflict"));
    assert_eq!(
        std::fs::read_to_string(ws.path().join("a.txt")).unwrap(),
        "changed"
    );
    assert!(!ws.path().join("gone-later.txt").exists(), "removed again");
}

/// A change made after the rewind is reported, and the exit says so — an undo
/// that quietly overwrote someone's work would be the failure `undo_conflicts`
/// exists to prevent.
#[test]
fn an_undo_that_would_overwrite_a_change_says_so() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );
    std::fs::write(ws.path().join("a.txt"), "changed").unwrap();
    filesnap(
        data.path(),
        &[
            "restore",
            "--session",
            "s1",
            "--turn",
            "t1",
            "--undo-for",
            "s1",
            "--cwd",
            &cwd,
        ],
    );

    // Somebody else edits the file the rewind just wrote.
    std::fs::write(ws.path().join("a.txt"), "someone else's work").unwrap();

    let run = filesnap(data.path(), &["undo", "--session", "s1", "--cwd", &cwd]);

    assert_eq!(run.code, 1, "a conflicting undo read as a clean success");
    assert!(
        run.find("undo.conflict")["path"]
            .as_str()
            .unwrap()
            .ends_with("a.txt")
    );
    // It still happened, and the work is recoverable from the safety point.
    assert!(run.find("restore.done")["safety"].as_str().unwrap().len() == 64);
}

/// A turn that was never captured is a usage error, not a store failure:
/// nothing was attempted and the caller should fix the call.
#[test]
fn restoring_an_unknown_turn_is_a_usage_error() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    let run = filesnap(
        data.path(),
        &[
            "restore",
            "--session",
            "s1",
            "--turn",
            "never-happened",
            "--cwd",
            &cwd,
        ],
    );
    assert_eq!(run.code, 3);
    assert!(run.out.is_empty());
}

#[test]
fn undoing_with_nothing_to_undo_is_a_usage_error() {
    let (data, ws) = workspace();
    let run = filesnap(
        data.path(),
        &[
            "undo",
            "--session",
            "s1",
            "--cwd",
            &ws.path().to_string_lossy(),
        ],
    );
    assert_eq!(run.code, 3);
}

/// **One unwritable file does not strand the rest, and does not read as
/// success** (D28, D40). Each failure is its own event; the terminal event
/// still carries the counts and the safety id.
///
/// The blocker is a destination occupied by a non-empty directory rather
/// than a directory with writes denied, so this runs everywhere. The
/// permission version was `#[cfg(unix)]`, which left `exit::PARTIAL` — the
/// difference between a restore that worked and one that did not — executed
/// on no Windows or macOS-with-a-different-umask machine anywhere. The
/// errno is not asserted, only the degradation, because it differs by
/// platform: `EISDIR` on unix, an access denial on Windows.
#[test]
fn a_restore_that_cannot_write_everything_reports_per_file_and_exits_nonzero() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    std::fs::write(ws.path().join("blocked.txt"), "before").unwrap();
    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );

    std::fs::write(ws.path().join("a.txt"), "changed").unwrap();
    // The destination this restore must put back is now a non-empty
    // directory: what a workspace looks like after a module became a package.
    std::fs::remove_file(ws.path().join("blocked.txt")).unwrap();
    std::fs::create_dir(ws.path().join("blocked.txt")).unwrap();
    std::fs::write(ws.path().join("blocked.txt").join("inner.txt"), "occupied").unwrap();

    let run = filesnap(
        data.path(),
        &["restore", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );

    assert_eq!(run.code, 1, "a partial restore read as success");
    assert_eq!(run.find("restore.done")["failed"], 1);
    assert!(
        run.find("restore.failed")["path"]
            .as_str()
            .unwrap()
            .ends_with("blocked.txt")
    );
    assert_eq!(
        std::fs::read_to_string(ws.path().join("a.txt")).unwrap(),
        "one",
        "the file that could be written still was"
    );
    assert!(
        run.find("restore.done")["safety"].as_str().unwrap().len() == 64,
        "the point it can be reversed to is reported even on a partial restore"
    );
}

// --- delete, gc, doctor ---

/// Delete's promise is unreachability, and it keeps it immediately. Reclaiming
/// the bytes is the separate half, and belongs to `gc` (D19, VIII.3).
#[test]
fn delete_makes_a_session_unreachable_without_freeing_content() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );
    filesnap(
        data.path(),
        &["capture", "--session", "s2", "--turn", "t2", "--cwd", &cwd],
    );

    let run = filesnap(data.path(), &["delete", "--session", "s1", "--cwd", &cwd]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.find("delete.done")["deleted"], 1);

    // Gone.
    let gone = filesnap(data.path(), &["log", "--session", "s1", "--cwd", &cwd]);
    assert_eq!(gone.find("log.done")["turns"], 0);
    // And the neighbour is untouched.
    let neighbour = filesnap(data.path(), &["log", "--session", "s2", "--cwd", &cwd]);
    assert_eq!(neighbour.find("log.done")["turns"], 1);
}

/// Deleting what was never tracked is not an error — delete is idempotent and
/// has no preconditions (D9) — but it is reported as absent rather than
/// deleted, because a caller counting removals should not be told a lie.
#[test]
fn deleting_a_session_that_never_existed_is_reported_as_absent() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );

    let run = filesnap(
        data.path(),
        &[
            "delete",
            "--session",
            "s1",
            "--session",
            "never-existed",
            "--cwd",
            &cwd,
        ],
    );

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.find("delete.done")["deleted"], 1);
    assert_eq!(run.find("delete.done")["absent"], 1);
    assert_eq!(run.find("delete.absent")["session"], "never-existed");

    // And it is idempotent: doing it again says the same thing.
    let again = filesnap(data.path(), &["delete", "--session", "s1", "--cwd", &cwd]);
    assert_eq!(again.code, 0);
    assert_eq!(again.find("delete.done")["absent"], 1);
}

/// **Collecting changes nothing anyone can observe.** Every turn still
/// resolves and every session still rewinds exactly as far; only unreachable
/// bytes go.
#[test]
fn gc_leaves_every_session_able_to_rewind() {
    let (data, ws) = workspace();
    let cwd = ws.path().to_string_lossy().into_owned();
    filesnap(
        data.path(),
        &["capture", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );
    std::fs::write(ws.path().join("a.txt"), "changed").unwrap();

    let run = filesnap(data.path(), &["gc"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.find("gc.done")["blobsKept"].as_u64().unwrap() > 0);

    // The proof: the rewind still works after collecting.
    let restored = filesnap(
        data.path(),
        &["restore", "--session", "s1", "--turn", "t1", "--cwd", &cwd],
    );
    assert_eq!(restored.code, 0, "{}", restored.stderr);
    assert_eq!(
        std::fs::read_to_string(ws.path().join("a.txt")).unwrap(),
        "one"
    );
}

/// `gc` spans the whole store, so it takes no workspace at all.
#[test]
fn gc_takes_no_workspace() {
    let (data, _ws) = workspace();
    let run = filesnap(data.path(), &["gc"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
}

/// `doctor` clears what an interrupted restore left in the user's own
/// project — the corner the self-healing path never reaches, because it only
/// cleans directories a later restore writes into (D21).
#[test]
fn doctor_clears_settled_restore_residue() {
    let (_data, ws) = workspace();
    let stray = ws.path().join("a.txt.filesnap-restore-tmp");
    std::fs::write(&stray, "half a restore").unwrap();
    let fresh = ws.path().join("nested/b.txt.filesnap-restore-tmp");
    std::fs::create_dir_all(ws.path().join("nested")).unwrap();
    std::fs::write(&fresh, "still being written").unwrap();

    // Age only the first: the second may be a restore that is running now.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&stray)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old))
        .unwrap();

    let run = Run::of(
        std::process::Command::new(env!("CARGO_BIN_EXE_filesnap"))
            .args(["doctor", "--workdir", &ws.path().to_string_lossy()])
            .output()
            .unwrap(),
    );

    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.find("doctor.done")["removed"], 1);
    assert!(!stray.exists());
    assert!(
        fresh.exists(),
        "fresh residue may belong to a running restore"
    );
    assert!(
        ws.path().join("a.txt").exists(),
        "doctor touched a real file"
    );
}

/// Nothing to clear is a clean, quiet success.
#[test]
fn doctor_on_a_tidy_workspace_reports_nothing() {
    let (_data, ws) = workspace();
    let run = Run::of(
        std::process::Command::new(env!("CARGO_BIN_EXE_filesnap"))
            .args(["doctor", "--workdir", &ws.path().to_string_lossy()])
            .output()
            .unwrap(),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.find("doctor.done")["removed"], 0);
    assert_eq!(run.kinds(), vec!["doctor.done"]);
}
