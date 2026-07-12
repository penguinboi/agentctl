use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Result, WorkspaceError, error::io_error};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceIdentity {
    pub canonical_path: PathBuf,
    pub repository_root: Option<PathBuf>,
    pub git_common_dir: Option<PathBuf>,
    pub worktree_path: Option<PathBuf>,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub remote_hash: Option<String>,
    pub filesystem_id: Option<String>,
    /// Stable writer-lock key for this worktree. Unlike `fingerprint`, this excludes HEAD/branch.
    pub lease_key: String,
    /// Full point-in-time identity, including HEAD and branch, used for session/snapshot auditing.
    pub fingerprint: String,
}

#[derive(Serialize)]
struct StableInput<'a> {
    canonical_path: &'a Path,
    repository_root: Option<&'a Path>,
    git_common_dir: Option<&'a Path>,
    worktree_path: Option<&'a Path>,
    filesystem_id: &'a Option<String>,
}

#[derive(Serialize)]
struct FingerprintInput<'a> {
    #[serde(flatten)]
    stable: StableInput<'a>,
    head: &'a Option<String>,
    branch: &'a Option<String>,
    remote_hash: &'a Option<String>,
}

impl WorkspaceIdentity {
    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        let canonical_path =
            fs::canonicalize(path.as_ref()).map_err(|error| io_error(path.as_ref(), error))?;
        let repository_root = git_value(&canonical_path, &["rev-parse", "--show-toplevel"])?
            .map(PathBuf::from)
            .map(canonicalize_if_possible);
        let worktree_path = repository_root.clone();
        let git_common_dir = match git_value(
            &canonical_path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )? {
            Some(value) => Some(PathBuf::from(value)),
            None => git_value(&canonical_path, &["rev-parse", "--git-common-dir"])?
                .map(PathBuf::from)
                // Git documents relative rev-parse paths relative to the command's cwd.
                .map(|value| {
                    if value.is_absolute() {
                        value
                    } else {
                        canonical_path.join(value)
                    }
                }),
        }
        .map(canonicalize_if_possible);
        let head = git_value(&canonical_path, &["rev-parse", "--verify", "HEAD"])?;
        let branch = git_value(
            &canonical_path,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        )?;
        let remote_hash = git_value(&canonical_path, &["remote", "get-url", "origin"])?
            .map(|remote| sha256(remote.as_bytes()));
        let filesystem_id = filesystem_id(&canonical_path)?;

        let stable_root = worktree_path
            .as_deref()
            .or(repository_root.as_deref())
            .unwrap_or(&canonical_path);
        let stable = StableInput {
            canonical_path: stable_root,
            repository_root: repository_root.as_deref(),
            git_common_dir: git_common_dir.as_deref(),
            worktree_path: worktree_path.as_deref(),
            filesystem_id: &filesystem_id,
        };
        let lease_key = sha256(
            &serde_json::to_vec(&stable)
                .map_err(|error| WorkspaceError::Identity(error.to_string()))?,
        );
        let encoded = serde_json::to_vec(&FingerprintInput {
            stable,
            head: &head,
            branch: &branch,
            remote_hash: &remote_hash,
        })
        .map_err(|error| WorkspaceError::Identity(error.to_string()))?;
        let fingerprint = sha256(&encoded);
        Ok(Self {
            canonical_path,
            repository_root,
            git_common_dir,
            worktree_path,
            head,
            branch,
            remote_hash,
            filesystem_id,
            lease_key,
            fingerprint,
        })
    }

    pub fn execution_root(&self) -> &Path {
        self.worktree_path
            .as_deref()
            .unwrap_or(&self.canonical_path)
    }
}

fn git_value(cwd: &Path, arguments: &[&str]) -> Result<Option<String>> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(arguments)
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(cwd, error)),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

fn canonicalize_if_possible(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

#[cfg(unix)]
fn filesystem_id(path: &Path) -> Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path).map_err(|error| io_error(path, error))?;
    Ok(Some(format!("unix-dev:{}", metadata.dev())))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn filesystem_id(_path: &Path) -> Result<Option<String>> {
    Ok(None)
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_for_same_directory() {
        let directory = tempfile::tempdir().unwrap();
        let first = WorkspaceIdentity::discover(directory.path()).unwrap();
        let second = WorkspaceIdentity::discover(directory.path()).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(
            first.execution_root(),
            directory.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn git_identity_includes_worktree_and_head() {
        let directory = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        let identity = WorkspaceIdentity::discover(directory.path()).unwrap();
        assert!(identity.repository_root.is_some());
        assert!(identity.git_common_dir.is_some());
        assert!(identity.remote_hash.is_none());
    }

    #[test]
    fn lease_key_survives_head_changes_and_subdirectories() {
        let directory = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "agentctl@example.invalid"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "agentctl"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        let child = directory.path().join("nested");
        fs::create_dir(&child).unwrap();
        let before = WorkspaceIdentity::discover(&child).unwrap();
        fs::write(directory.path().join("file"), "content").unwrap();
        Command::new("git")
            .args(["add", "file"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "--quiet", "-m", "first"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        let after = WorkspaceIdentity::discover(directory.path()).unwrap();
        assert_eq!(before.lease_key, after.lease_key);
        assert_ne!(before.fingerprint, after.fingerprint);
    }
}
