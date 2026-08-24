use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("blob {0} not found in store")]
    MissingBlob(String),
    #[error("manifest {0} not found in store")]
    MissingManifest(String),
    /// The store holds a format this build does not understand. Reading it
    /// anyway would mean guessing at a layout, which is the failure
    /// versioning exists to prevent.
    #[error(
        "the snapshot store at {path} was written by a newer filesnap \
         (format v{found}; this build supports v{supported})"
    )]
    UnknownStoreVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    /// A durable record declares a version this build cannot read. Distinct
    /// from `UnknownStoreVersion`: the store said one thing and a record
    /// inside it says another, which is what a migration interrupted halfway
    /// looks like.
    #[error("{kind} {id} declares format v{found}, which this build (v{supported}) cannot read")]
    UnknownRecordVersion {
        kind: &'static str,
        id: String,
        found: u32,
        supported: u32,
    },
    /// Another invocation of this same session is running.
    ///
    /// Not a failure of the store: the caller asked for something that would
    /// have raced a read-modify-write and silently lost one side of it. A
    /// retry after the other invocation finishes is the right response, and
    /// the operation is unchanged in the meantime (D18).
    #[error("session {session} is busy: another filesnap invocation for it is running")]
    SessionBusy { session: String },
    /// An id cannot become a filename. Refused rather than rewritten: mapping
    /// it onto something legal is what silently merges two distinct
    /// conversations into one record (D7).
    #[error("invalid {kind} {id:?}: {reason}")]
    InvalidId {
        kind: &'static str,
        id: String,
        reason: &'static str,
    },
}

impl SnapshotError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, SnapshotError>;
