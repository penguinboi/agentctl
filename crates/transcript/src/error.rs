use thiserror::Error;

#[derive(Debug, Error)]
pub enum TranscriptError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("canonical events are not strictly monotonic at sequence {0}")]
    NonMonotonic(u64),
    #[error("export checksum mismatch: expected {expected}, received {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("invalid export: {0}")]
    InvalidExport(String),
    #[error("JSONL line exceeds the limit of {0} bytes")]
    LineTooLarge(usize),
    #[error("projection through sequence {through_seq} exceeds latest event {latest_seq}")]
    ProjectionOutOfRange { through_seq: u64, latest_seq: u64 },
    #[error("sync progress cannot move backwards from {current} to {proposed}")]
    SyncRegression { current: u64, proposed: u64 },
}

pub type Result<T> = std::result::Result<T, TranscriptError>;
