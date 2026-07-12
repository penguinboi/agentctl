use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid canonical sequence: expected {expected}, received {actual}")]
    InvalidSequence { expected: u64, actual: u64 },
    #[error("content digest does not match: expected {expected}, received {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("invalid blob digest: {0}")]
    InvalidDigest(String),
    #[error("blob exceeds configured decompressed size limit of {0} bytes")]
    BlobTooLarge(u64),
    #[error("event JSON exceeds configured size limit of {0} bytes")]
    EventTooLarge(usize),
    #[error("event JSON exceeds configured nesting depth of {0}")]
    JsonTooDeep(usize),
    #[error("unsupported database schema version {found}; maximum supported is {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("record not found: {0}")]
    NotFound(String),
    #[error("invalid stored data: {0}")]
    InvalidData(String),
    #[error("refusing to use symbolic link for sensitive storage: {}", .0.display())]
    UnsafeSymlink(PathBuf),
}

pub type Result<T> = std::result::Result<T, StorageError>;

pub(crate) fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> StorageError {
    StorageError::Io {
        path: path.into(),
        source,
    }
}
