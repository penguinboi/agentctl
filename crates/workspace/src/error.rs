use std::path::PathBuf;

use thiserror::Error;

use crate::LeaseMetadata;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("git command failed: {command}: {stderr}")]
    Git { command: String, stderr: String },
    #[error("workspace is already leased by process {}", .0.owner_pid)]
    LeaseBusy(LeaseMetadata),
    #[error("lease metadata is invalid: {0}")]
    InvalidLease(#[from] serde_json::Error),
    #[error("process group id {0} does not fit the platform pid type")]
    InvalidProcessGroup(u32),
    #[cfg(unix)]
    #[error("process group operation failed: {0}")]
    ProcessGroup(#[from] nix::errno::Errno),
    #[error("process tree operation failed: {0}")]
    ProcessTree(String),
    #[error("workspace identity cannot be computed: {0}")]
    Identity(String),
}

pub type Result<T> = std::result::Result<T, WorkspaceError>;

pub(crate) fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> WorkspaceError {
    WorkspaceError::Io {
        path: path.into(),
        source,
    }
}
