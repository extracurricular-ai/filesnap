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
