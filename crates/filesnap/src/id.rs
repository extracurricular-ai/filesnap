//! Which ids may become filenames, and which are refused.
//!
//! **Refused, not rewritten.** The engine used to map every character outside
//! `[A-Za-z0-9._-]` to `_`, so `my session`, `my/session` and `my:session` all
//! became `my_session` — three distinct sessions quietly merging into one log
//! and one undo stack. Silent rewriting is what turns a typo into a collision,
//! and a collision between conversations is unrecoverable: by the time anyone
//! notices, two histories are interleaved in one file (D7).
//!
//! Rejection is honest about the same situation, and it is checkable. Once an
//! id is accepted it *is* its filename — no mapping, so no two ids can share
//! one.
//!
//! # The two namespaces
//!
//! Ids reach the store from two places: the host, which passes external
//! conversation and turn ids through unchanged (D6), and the engine, which
//! mints a turn id for the safety checkpoint taken before every restore. These
//! must not be able to collide, and previously did: the engine minted
//! `safety-restore:<id>`, which sanitized to `safety-restore_<id>` — a name a
//! user turn could hold exactly.
//!
//! They are disjoint by construction now. An internal id begins with
//! [`INTERNAL_PREFIX`]; an external one may not. Nothing is reserved by
//! convention or by luck.

use crate::error::Result;
use crate::error::SnapshotError;

/// What every internally minted id starts with, and no external id may.
///
/// A filename-safe character deliberately: the prefix has to survive to the
/// filesystem, so reserving one that needed escaping would reintroduce the
/// mapping this module exists to remove.
pub(crate) const INTERNAL_PREFIX: char = '_';

/// Whether `c` may appear in an id that becomes a filename.
///
/// Deliberately narrow. It excludes the path separators on every platform we
/// support, the drive-letter colon, and everything a shell would need quoted —
/// so an accepted id is safe to put in a path, print in a log line, and pass
/// to a command without further thought.
fn is_admissible(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// Prove `id` cannot resolve outside the directory it will live in.
///
/// This is the guarantee D5 asks for, and it is enforced where paths are
/// *built* rather than where they are accepted. An entry-point check is a
/// promise every future method has to remember to keep; a check inside the
/// path builder is one the type system asks for on every call.
///
/// `.` and `..` are the interesting cases. Both survive any character-level
/// filter — they contain nothing illegal — and both resolve to a directory
/// rather than a file in it. `..` reaches the partition root, and the empty
/// string resolves to the enclosing directory itself, which `with_extension`
/// then turns into a sibling of it: writing `turns/` as an id produced
/// `<partition>/turns.tmp`, one level above where it belonged.
pub(crate) fn validate_stored(kind: &'static str, id: &str) -> Result<()> {
    let reason = if id.is_empty() {
        "an id may not be empty"
    } else if id == "." || id == ".." {
        "an id may not be `.` or `..`, which name a directory rather than a record"
    } else if !id.chars().all(is_admissible) {
        "an id may hold only letters, digits, `-`, `_` and `.`"
    } else if id.len() > MAX_ID_BYTES {
        "an id is longer than any filesystem will store"
    } else {
        return Ok(());
    };
    Err(SnapshotError::InvalidId {
        kind,
        id: id.to_string(),
        reason,
    })
}

/// The longest id that is portable as a single path component. Every
/// filesystem we target allows at least 255 bytes per component; the record
/// suffix and the `.tmp` a write adds have to fit alongside it.
const MAX_ID_BYTES: usize = 200;

/// Prove `id` came from outside, and so may not claim the engine's namespace.
///
/// Everything [`validate_stored`] requires, plus the prefix rule. Applied at
/// the public API boundary: a host that hands us an id beginning with `_` is
/// told so rather than being allowed to shadow a safety checkpoint.
pub(crate) fn validate_external(kind: &'static str, id: &str) -> Result<()> {
    validate_stored(kind, id)?;
    if id.starts_with(INTERNAL_PREFIX) {
        return Err(SnapshotError::InvalidId {
            kind,
            id: id.to_string(),
            reason: "an id may not begin with `_`, which is reserved for ids filesnap mints",
        });
    }
    Ok(())
}

/// Whether `name` is the shape of a content-addressed object: 64 lowercase
/// hex characters.
///
/// The whitelist D9 asks every enumeration to use. A blacklist of `.tmp` is
/// the same idea stated the fragile way round: it admits anything nobody
/// thought to exclude, which is how a half-written file became readable as a
/// record and then an uncollectable GC root (C4).
pub(crate) fn is_object_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Refuse an object id that is not the shape we mint.
///
/// Object ids are ours — a SHA-256 of content — so a malformed one means a
/// record on disk has been corrupted or forged. It matters because those ids
/// are read back out of manifests and handed straight to `remove`: an id of
/// `/etc/passwd` would resolve there, since joining an absolute path replaces
/// everything before it.
pub(crate) fn validate_object(kind: &'static str, id: &str) -> Result<()> {
    if is_object_name(id) {
        return Ok(());
    }
    Err(SnapshotError::InvalidId {
        kind,
        id: id.to_string(),
        reason: "an object id must be 64 lowercase hex characters",
    })
}

#[cfg(test)]
#[path = "id_tests.rs"]
mod tests;
