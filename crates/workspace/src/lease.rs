use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use agentctl_core::{TurnId, UnifiedSessionId};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Result, WorkspaceError, WorkspaceIdentity, error::io_error};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LeaseMetadata {
    pub workspace_fingerprint: String,
    pub owner_pid: u32,
    pub session_id: UnifiedSessionId,
    pub turn_id: TurnId,
    pub acquired_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct WorkspaceLease {
    path: PathBuf,
    file: File,
    metadata: LeaseMetadata,
}

#[derive(Debug)]
pub struct LeaseRecovery {
    pub lease: WorkspaceLease,
    pub previous: Option<LeaseMetadata>,
    pub was_stale: bool,
}

impl WorkspaceLease {
    /// Acquires the stable per-worktree writer key from a discovered identity.
    pub fn acquire_for_identity(
        lock_directory: impl AsRef<Path>,
        identity: &WorkspaceIdentity,
        session_id: UnifiedSessionId,
        turn_id: TurnId,
    ) -> Result<Self> {
        Self::acquire(
            lock_directory,
            identity.lease_key.clone(),
            session_id,
            turn_id,
        )
    }

    pub fn acquire(
        lock_directory: impl AsRef<Path>,
        workspace_fingerprint: impl Into<String>,
        session_id: UnifiedSessionId,
        turn_id: TurnId,
    ) -> Result<Self> {
        let workspace_fingerprint = workspace_fingerprint.into();
        let lock_directory = lock_directory.as_ref();
        create_private_dir(lock_directory)?;
        let path = lock_path(lock_directory, &workspace_fingerprint);
        let mut file = open_lock_file(&path)?;
        if FileExt::try_lock_exclusive(&file).is_err() {
            let metadata = read_metadata(&mut file).unwrap_or_else(|_| LeaseMetadata {
                workspace_fingerprint: workspace_fingerprint.clone(),
                owner_pid: 0,
                session_id,
                turn_id,
                acquired_at: Utc::now(),
                heartbeat_at: Utc::now(),
            });
            return Err(WorkspaceError::LeaseBusy(metadata));
        }
        let now = Utc::now();
        let metadata = LeaseMetadata {
            workspace_fingerprint,
            owner_pid: std::process::id(),
            session_id,
            turn_id,
            acquired_at: now,
            heartbeat_at: now,
        };
        write_metadata(&mut file, &path, &metadata)?;
        Ok(Self {
            path,
            file,
            metadata,
        })
    }

    pub fn recover(
        lock_directory: impl AsRef<Path>,
        workspace_fingerprint: impl Into<String>,
        session_id: UnifiedSessionId,
        turn_id: TurnId,
        stale_after: Duration,
    ) -> Result<LeaseRecovery> {
        let workspace_fingerprint = workspace_fingerprint.into();
        let lock_directory = lock_directory.as_ref();
        create_private_dir(lock_directory)?;
        let path = lock_path(lock_directory, &workspace_fingerprint);
        let previous = if path.exists() {
            let mut file = open_lock_file(&path)?;
            read_metadata(&mut file).ok()
        } else {
            None
        };
        let was_stale = previous.as_ref().is_some_and(|metadata| {
            Utc::now()
                .signed_duration_since(metadata.heartbeat_at)
                .to_std()
                .is_ok_and(|age| age >= stale_after)
        });
        let lease = Self::acquire(lock_directory, workspace_fingerprint, session_id, turn_id)?;
        Ok(LeaseRecovery {
            lease,
            previous,
            was_stale,
        })
    }

    pub fn metadata(&self) -> &LeaseMetadata {
        &self.metadata
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn heartbeat(&mut self) -> Result<()> {
        self.metadata.heartbeat_at = Utc::now();
        write_metadata(&mut self.file, &self.path, &self.metadata)
    }

    pub fn release(self) -> Result<()> {
        FileExt::unlock(&self.file).map_err(|error| io_error(&self.path, error))
    }
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn lock_path(directory: &Path, fingerprint: &str) -> PathBuf {
    let safe_name = hex::encode(Sha256::digest(fingerprint.as_bytes()));
    directory.join(format!("{safe_name}.lock"))
}

fn read_metadata(file: &mut File) -> Result<LeaseMetadata> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io_error("lease", error))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|error| io_error("lease", error))?;
    serde_json::from_str(&contents).map_err(Into::into)
}

fn write_metadata(file: &mut File, path: &Path, metadata: &LeaseMetadata) -> Result<()> {
    let encoded = serde_json::to_vec(metadata)?;
    file.set_len(0).map_err(|error| io_error(path, error))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io_error(path, error))?;
    file.write_all(&encoded)
        .map_err(|error| io_error(path, error))?;
    file.sync_data().map_err(|error| io_error(path, error))
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| io_error(path, error))?;
    set_mode(path, 0o700)
}

fn open_lock_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|error| io_error(path, error))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| io_error(path, error))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_writer_can_hold_a_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let session = UnifiedSessionId::new();
        let turn = TurnId::new();
        let first = WorkspaceLease::acquire(directory.path(), "workspace", session, turn).unwrap();
        let second = WorkspaceLease::acquire(directory.path(), "workspace", session, turn);
        assert!(matches!(second, Err(WorkspaceError::LeaseBusy(_))));
        drop(first);
        WorkspaceLease::acquire(directory.path(), "workspace", session, turn).unwrap();
    }

    #[test]
    fn heartbeat_is_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let mut lease = WorkspaceLease::acquire(
            directory.path(),
            "workspace",
            UnifiedSessionId::new(),
            TurnId::new(),
        )
        .unwrap();
        let before = lease.metadata().heartbeat_at;
        lease.heartbeat().unwrap();
        assert!(lease.metadata().heartbeat_at >= before);
    }
}
