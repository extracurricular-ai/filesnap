//! The wire shape, pinned. These tests exist to make a change to the contract
//! impossible to land by accident — which is what "freezes on first publish"
//! has to mean in practice.

#![allow(clippy::unwrap_used)]

use super::*;
use pretty_assertions::assert_eq;

fn line<T: Serialize>(kind: &str, payload: T) -> String {
    let mut out = Vec::new();
    emit(&mut out, kind, payload);
    String::from_utf8(out).unwrap()
}

#[derive(Serialize)]
struct Done<'a> {
    manifest: &'a str,
    reused: usize,
}

/// Field order, flatness, the version, and the trailing newline — all four are
/// the contract, so all four are asserted literally rather than by round-trip.
#[test]
fn an_event_is_one_flat_line_with_the_version_first() {
    assert_eq!(
        line(
            "capture.done",
            Done {
                manifest: "a1b2c3",
                reused: 412,
            }
        ),
        "{\"v\":1,\"type\":\"capture.done\",\"manifest\":\"a1b2c3\",\"reused\":412}\n"
    );
}

/// One object per line, and nothing pretty-printed: a consumer splits on `\n`
/// and parses each piece on its own.
#[test]
fn every_event_is_exactly_one_line() {
    let mut out = Vec::new();
    emit(
        &mut out,
        "capture.started",
        Done {
            manifest: "a",
            reused: 0,
        },
    );
    emit(
        &mut out,
        "capture.done",
        Done {
            manifest: "b",
            reused: 1,
        },
    );
    let text = String::from_utf8(out).unwrap();

    assert_eq!(text.lines().count(), 2);
    for l in text.lines() {
        let parsed: serde_json::Value = serde_json::from_str(l).unwrap();
        assert_eq!(parsed["v"], 1);
        assert!(parsed["type"].is_string());
    }
}

/// The version rides on every line, not in a header.
///
/// JSONL gets grepped, tailed and split; a reader may hold exactly one line,
/// and it has to be able to refuse a shape it does not understand.
#[test]
fn a_single_line_carries_enough_to_be_refused() {
    let text = line(
        "restore.failed",
        Done {
            manifest: "x",
            reused: 0,
        },
    );
    let alone: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();

    assert_eq!(alone["v"], SCHEMA_VERSION);
    assert_eq!(alone["type"], "restore.failed");
}

/// Drop reasons are camelCase on the wire and are this crate's own type, so a
/// new engine-side variant cannot silently appear in a pinned consumer's
/// stream.
#[test]
fn drop_reasons_are_camel_case_on_the_wire() {
    #[derive(Serialize)]
    struct Dropped {
        reason: DropReason,
    }

    for (engine, wire) in [
        (filesnap::DropReason::OverSizeLimit, "overSizeLimit"),
        (filesnap::DropReason::Unreadable, "unreadable"),
        (filesnap::DropReason::NotARegularFile, "notARegularFile"),
    ] {
        assert_eq!(
            line(
                "capture.dropped",
                Dropped {
                    reason: engine.into()
                }
            ),
            format!("{{\"v\":1,\"type\":\"capture.dropped\",\"reason\":\"{wire}\"}}\n")
        );
    }
}

/// A closed pipe is not an error. `filesnap log | head -1` must not turn a
/// successful read into a failure, because stdout is a report rather than an
/// acknowledgement.
#[test]
fn a_write_failure_does_not_propagate() {
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // The point is that this returns at all.
    emit(
        &mut Closed,
        "capture.done",
        Done {
            manifest: "a",
            reused: 0,
        },
    );
}
