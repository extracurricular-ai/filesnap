//! The output contract: one JSON object per line on stdout (D32, D39).
//!
//! ```text
//! {"v":1,"type":"capture.started","session":"s1","turn":"t1"}
//! {"v":1,"type":"capture.dropped","path":"/w/dump.bin","reason":"overSizeLimit"}
//! {"v":1,"type":"capture.done","manifest":"a1b2c3…","reused":412,"hashed":7,"dropped":1}
//! ```
//!
//! **This freezes on the first publish.** npm has no rollback (D37), so a
//! consumer pinned to a version is pinned to this shape. Adding a field is
//! safe; renaming or removing one is a new `SCHEMA_VERSION`.
//!
//! # Why the version is on every line
//!
//! JSONL gets `grep`ed, `tail`ed, `split`, and piped through filters that keep
//! some lines and drop others. A version in a header line survives none of
//! that, and a reader here may be holding exactly one line. One integer per
//! line is cheap, and it is the same reasoning VII.1 applies to records on
//! disk: a reader must be able to refuse what it does not understand.
//!
//! # Why the fields are flat
//!
//! Nesting the payload under `data` costs every consumer a level on every
//! access and buys only collision-avoidance, which `<command>.<event>` already
//! provides.
//!
//! # Where prose goes
//!
//! Not here. Human-facing text belongs on stderr, where it cannot be mistaken
//! for the contract (D32).

use std::io::Write;

use serde::Serialize;

/// The `v` on every line. Bumped only for a change that breaks a reader —
/// a renamed or removed field, or a changed meaning. New fields do not bump it.
pub const SCHEMA_VERSION: u32 = 1;

/// One line of the contract.
///
/// The payload is flattened into the same object as `v` and `type`, so a
/// payload field named `v` or `type` would shadow the envelope. Nothing in
/// this crate has one, and `Event::write` is the only way to emit a line, so
/// the invariant has one place to hold.
#[derive(Debug, Serialize)]
struct Line<'a, T: Serialize> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(flatten)]
    payload: T,
}

/// Emit one event. `kind` is `<command>.<event>`, e.g. `capture.done`.
///
/// Failures to write are **ignored**, deliberately: a consumer that closes the
/// pipe early (`filesnap log | head`) would otherwise turn into a broken-pipe
/// error on an operation that actually succeeded. The exit code carries
/// success or failure; stdout is a report, not an acknowledgement.
pub fn emit<T: Serialize>(out: &mut impl Write, kind: &str, payload: T) {
    let line = Line {
        v: SCHEMA_VERSION,
        kind,
        payload,
    };
    if let Ok(json) = serde_json::to_string(&line) {
        let _ = writeln!(out, "{json}");
    }
}

/// Why a file the scan saw is not in the snapshot, on the wire.
///
/// Its own type rather than a re-serialization of [`filesnap::DropReason`]:
/// the engine's enum is Rust API and may gain variants for reasons that have
/// nothing to do with this contract, and a consumer pinned to `v1` must not
/// see a new string appear because an internal enum grew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DropReason {
    OverSizeLimit,
    Unreadable,
    NotARegularFile,
}

impl From<filesnap::DropReason> for DropReason {
    fn from(reason: filesnap::DropReason) -> Self {
        match reason {
            filesnap::DropReason::OverSizeLimit => Self::OverSizeLimit,
            filesnap::DropReason::Unreadable => Self::Unreadable,
            filesnap::DropReason::NotARegularFile => Self::NotARegularFile,
        }
    }
}

#[cfg(test)]
#[path = "event_tests.rs"]
mod tests;
