//! What the exit code means.
//!
//! D28 is explicit that a partial failure must not read as success **anywhere**
//! — not in the output, not in the exit code. That makes the code part of the
//! contract rather than an afterthought, so it lives here with the reasoning
//! rather than as integer literals scattered through the commands.
//!
// The whole contract is defined here at once, including the codes the
// commands that land later will return. Defining half of it now and the rest
// piecemeal is how an exit code ends up meaning two things.
#![allow(dead_code)]

/// Everything the command was asked to do, it did.
pub const OK: u8 = 0;

/// The command ran and reported, but not everything it was asked to do
/// happened: a restore that could not write some files, a delete that refused
/// a session.
///
/// **Distinct from [`FAILED`] on purpose.** stdout is still a valid, complete
/// event stream — the caller should read it, because it says exactly which
/// files and which sessions. A caller that treats any non-zero code as "no
/// output worth reading" would throw away the only record of what happened.
pub const PARTIAL: u8 = 1;

/// The command did not run, or could not report. Nothing useful is on stdout.
pub const FAILED: u8 = 2;

/// The arguments were wrong. Separate from [`FAILED`] because it is the one
/// non-zero code that means "nothing was attempted", so a script can retry
/// after fixing the call rather than investigating the store.
pub const USAGE: u8 = 3;
