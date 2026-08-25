//! `filesnap` — the command line interface to the snapshot engine.
//!
//! **Stateless per invocation** (D3). Every command opens the store, does one
//! operation, and exits; nothing is carried between calls in memory, and there
//! is no resident process to reconnect to (D29). What a long-lived host would
//! have kept in memory is persisted instead — see the engine's `declared`
//! module.
//!
//! **JSONL on stdout, prose on stderr** (D32). The event stream is the
//! contract; see [`event`]. A human running `filesnap doctor` gets JSON, and
//! that cost is accepted so that a consumer never has to tell output from
//! commentary.
//!
//! The command surface is settled and closed. There is no `rewind` — the
//! engine has no opinion about conversations (D27) — and scan limits are not
//! exposed, because a bound a user has to find is not a bound (D14).

mod commands;
mod event;
mod exit;

use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;

#[derive(Parser)]
#[command(
    name = "filesnap",
    version,
    about = "Git-free file snapshots and rewind",
    long_about = "Snapshots a working directory and puts it back, without a repository \
                  and without touching your version control.\n\n\
                  Every command writes JSON Lines to stdout; anything meant for a person \
                  goes to stderr."
)]
struct Cli {
    /// Where the store lives. Defaults to the platform data directory.
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Capture the state of a workspace at the start of a turn.
    Capture {
        #[arg(long, value_name = "ID")]
        session: String,
        #[arg(long, value_name = "ID")]
        turn: String,
        /// The directory this turn runs in. Defaults to the current one.
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// Extra roots to scan. Repeatable.
        #[arg(long = "root", value_name = "DIR")]
        roots: Vec<PathBuf>,
    },

    /// Record what a path holds **before** an edit changes it.
    ///
    /// Called before the edit, not after: the pre-image is the only thing that
    /// can be recovered, and after the write it is gone (D30). Takes many
    /// paths in one call so a turn with twenty edits is one process, not
    /// twenty (D29).
    Declare {
        #[arg(long, value_name = "ID")]
        session: String,
        #[arg(long, value_name = "ID")]
        turn: String,
        #[arg(long = "path", value_name = "FILE", required = true)]
        paths: Vec<PathBuf>,
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// List what a session can rewind to.
    Log {
        #[arg(long, value_name = "ID")]
        session: String,
        /// Show only the most recent N turns.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// Put the workspace back to the state captured at the start of a turn.
    ///
    /// Addressed by turn id only. A positional "N steps back" was considered
    /// and refused: the cost of getting it wrong is the user's files, and an
    /// id is the one form that cannot silently mean something else (D35).
    Restore {
        #[arg(long, value_name = "ID")]
        session: String,
        #[arg(long, value_name = "ID")]
        turn: String,
        /// The session this hands the workspace to, which is where the undo
        /// record is filed. Omit for a restore that is not undoable.
        #[arg(long, value_name = "ID")]
        undo_for: Option<String>,
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// Reverse this session's most recent restore.
    Undo {
        #[arg(long, value_name = "ID")]
        session: String,
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// Delete sessions and everything they hold.
    ///
    /// The single source of truth for ending a session's data, and it depends
    /// on nothing else having run first (D9).
    Delete {
        #[arg(long = "session", value_name = "ID", required = true)]
        sessions: Vec<String>,
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// Reclaim orphaned records and unreferenced content, across every
    /// workspace in the store.
    Gc,

    /// What the state of this workspace is. Read-only.
    Status {
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// Find and clear what an interrupted operation left in a workspace.
    /// The one command that writes to the user's own directory.
    Doctor {
        #[arg(long, value_name = "DIR")]
        workdir: Option<PathBuf>,
    },
}

/// Where the store lives when the caller does not say.
///
/// Deliberately not read from an environment variable: a store that moves
/// because a variable happened to be set is a store whose contents nobody can
/// account for. The flag is explicit or the platform default applies.
fn default_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    base
}

fn here(cwd: Option<PathBuf>) -> std::io::Result<PathBuf> {
    match cwd {
        Some(dir) => Ok(dir),
        None => std::env::current_dir(),
    }
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    let Some(data_dir) = cli.data_dir.or_else(default_data_dir) else {
        eprintln!("filesnap: no data directory; pass --data-dir");
        return std::process::ExitCode::from(exit::USAGE);
    };

    let mut out = std::io::stdout().lock();
    let code = match cli.command {
        Command::Capture {
            session,
            turn,
            cwd,
            roots,
        } => match here(cwd) {
            Ok(cwd) => commands::capture::run(&mut out, &data_dir, &session, &turn, cwd, roots),
            Err(err) => {
                eprintln!("filesnap: cannot resolve the working directory: {err}");
                exit::USAGE
            }
        },
        Command::Declare {
            session,
            turn,
            paths,
            cwd,
        } => match here(cwd) {
            Ok(cwd) => commands::declare::run(&mut out, &data_dir, &session, &turn, cwd, paths),
            Err(err) => {
                eprintln!("filesnap: cannot resolve the working directory: {err}");
                exit::USAGE
            }
        },
        Command::Log {
            session,
            limit,
            cwd,
        } => match here(cwd) {
            Ok(cwd) => commands::log::run(&mut out, &data_dir, &cwd, &session, limit),
            Err(err) => {
                eprintln!("filesnap: cannot resolve the working directory: {err}");
                exit::USAGE
            }
        },
        Command::Status { cwd } => match here(cwd) {
            Ok(cwd) => commands::status::run(&mut out, &data_dir, &cwd),
            Err(err) => {
                eprintln!("filesnap: cannot resolve the working directory: {err}");
                exit::USAGE
            }
        },
        Command::Restore {
            session,
            turn,
            undo_for,
            cwd,
        } => match here(cwd) {
            Ok(cwd) => commands::restore::restore(
                &mut out,
                &data_dir,
                &cwd,
                &session,
                &turn,
                undo_for.as_deref(),
            ),
            Err(err) => {
                eprintln!("filesnap: cannot resolve the working directory: {err}");
                exit::USAGE
            }
        },
        Command::Undo { session, cwd } => match here(cwd) {
            Ok(cwd) => commands::restore::undo(&mut out, &data_dir, &cwd, &session),
            Err(err) => {
                eprintln!("filesnap: cannot resolve the working directory: {err}");
                exit::USAGE
            }
        },
        Command::Delete { sessions, cwd } => match here(cwd) {
            Ok(cwd) => commands::delete::run(&mut out, &data_dir, &cwd, &sessions),
            Err(err) => {
                eprintln!("filesnap: cannot resolve the working directory: {err}");
                exit::USAGE
            }
        },
        Command::Gc => commands::gc::run(&mut out, &data_dir),
        Command::Doctor { workdir } => match here(workdir) {
            Ok(workdir) => commands::doctor::run(&mut out, &workdir),
            Err(err) => {
                eprintln!("filesnap: cannot resolve the working directory: {err}");
                exit::USAGE
            }
        },
    };
    std::process::ExitCode::from(code)
}
