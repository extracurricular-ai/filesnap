//! Parsing `--declared-window`.
//!
//! The one bound this CLI exposes, and the exception D14 argues against
//! exposing scan limits does not cover it: a scan limit answers "how much of
//! this tree can we afford", which is the engine's problem, while this answers
//! "how long should a file the agent touched stay reversible", which is the
//! host's product and cannot be guessed from here. A host that drives the
//! binary has nowhere else to say it (D25).

use std::num::NonZeroU64;

use filesnap::DeclaredWindow;

/// The word that means "no window at all".
///
/// Matched case-insensitively on the way in and produced by the type's own
/// `Display` on the way out; `window_tests` pins the two together.
const UNLIMITED: &str = "unlimited";

/// A turn count of one or more, or [`UNLIMITED`].
///
/// **Zero is refused rather than interpreted.** It is the one value with no
/// sensible reading: a window shorter than a turn leaves the file out of the
/// manifest a user rewinding to just after their edit lands on, and silently
/// promoting it to one would hide a caller's mistake in the place its cost is
/// invisible.
pub fn parse(raw: &str) -> Result<DeclaredWindow, String> {
    if raw.eq_ignore_ascii_case(UNLIMITED) {
        return Ok(DeclaredWindow::Unlimited);
    }
    raw.parse::<NonZeroU64>()
        .map(DeclaredWindow::Turns)
        .map_err(|_| format!("expected a turn count of 1 or more, or `{UNLIMITED}`"))
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod tests;
