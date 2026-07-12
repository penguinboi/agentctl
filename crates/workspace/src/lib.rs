//! Workspace identity, serialization and process safety boundaries.

mod error;
mod git;
mod identity;
mod lease;
mod process_group;

pub use error::{Result, WorkspaceError};
pub use git::{ChangedPath, GitSnapshot, capture_git_snapshot, git_diff, git_diff_summary};
pub use identity::WorkspaceIdentity;
pub use lease::{LeaseMetadata, WorkspaceLease};
pub use process_group::*;
