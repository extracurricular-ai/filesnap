//! One module per command. Each returns an exit code from [`crate::exit`] and
//! writes its events to the writer it is given, so a test can read the stream
//! without spawning a process.

pub mod capture;
pub mod declare;
pub mod log;
pub mod restore;
pub mod status;
