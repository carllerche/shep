use std::io;
use std::path::PathBuf;
use std::sync::Arc;

/// A storage error.
///
/// Cloneable because one failed `fdatasync` fails every flush waiting on it.
#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(Arc<io::Error>),

    /// Data on disk fails a checksum or breaks the log's invariants somewhere
    /// recovery can't repair it (anywhere but the tail of the last segment).
    #[error("corrupt data in {}: {reason}", path.display())]
    Corrupt { path: PathBuf, reason: String },

    /// An append whose index isn't the next one, or whose term goes back.
    #[error("out-of-order append: {0}")]
    OutOfOrder(String),

    /// A read or truncation below the log's start.
    #[error("index {index} is below the log start {start}")]
    BelowStart { index: u64, start: u64 },

    #[error("closed")]
    Closed,
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(Arc::new(e))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn corrupt(path: impl Into<PathBuf>, reason: impl ToString) -> Error {
    Error::Corrupt {
        path: path.into(),
        reason: reason.to_string(),
    }
}
