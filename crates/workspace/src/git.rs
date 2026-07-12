use std::{
    path::{Path, PathBuf},
    process::Command,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Result, WorkspaceError, error::io_error};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangedPath {
    pub path: PathBuf,
    pub original_path: Option<PathBuf>,
    pub index_status: char,
    pub worktree_status: char,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GitSnapshot {
    pub root: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub changed_paths: Vec<ChangedPath>,
    pub dirty: bool,
    pub diff_digest: String,
    /// Whether Git accounts for every writable path in the workspace.
    ///
    /// Untracked/ignored files and non-Git workspaces make a snapshot
    /// conservative: an unchanged digest cannot prove that no side effect
    /// happened. Callers must treat an incomplete snapshot as at least a
    /// possible side effect before replaying a failed turn.
    #[serde(default = "snapshot_coverage_default")]
    pub coverage_complete: bool,
    pub captured_at: DateTime<Utc>,
}

const fn snapshot_coverage_default() -> bool {
    // Legacy snapshots predate coverage accounting. Unknown coverage must
    // never be upgraded into proof that replay is safe.
    false
}

impl GitSnapshot {
    pub fn changed_since(&self, earlier: &Self) -> bool {
        self.head != earlier.head
            || self.diff_digest != earlier.diff_digest
            || self.changed_paths != earlier.changed_paths
            || self.coverage_complete != earlier.coverage_complete
    }
}

pub fn capture_git_snapshot(path: impl AsRef<Path>) -> Result<GitSnapshot> {
    let requested = path.as_ref();
    let git_root = git_optional(requested, &["rev-parse", "--show-toplevel"])?;
    let root = git_root.as_ref().map_or_else(
        || std::fs::canonicalize(requested).map_err(|error| io_error(requested, error)),
        |root| Ok(PathBuf::from(root.trim())),
    )?;
    if git_root.is_none() {
        let material = format!("non-git\0{}", root.display());
        return Ok(GitSnapshot {
            root,
            head: None,
            branch: None,
            changed_paths: Vec::new(),
            dirty: false,
            diff_digest: format!("sha256:{}", hex::encode(Sha256::digest(material))),
            coverage_complete: false,
            captured_at: Utc::now(),
        });
    }
    let head = git_optional(&root, &["rev-parse", "--verify", "HEAD"])?;
    let branch = git_optional(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let status = git_bytes(
        &root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        true,
    )?;
    let changed_paths = parse_porcelain(&status);
    let ignored = git_bytes(
        &root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored=matching",
            "--untracked-files=all",
        ],
        true,
    )?;
    let has_untracked = changed_paths
        .iter()
        .any(|change| change.index_status == '?' && change.worktree_status == '?');
    let has_ignored = ignored
        .split(|byte| *byte == 0)
        .any(|record| record.starts_with(b"!! "));
    let diff_material = git_diff_material(&root)?;
    let diff_digest = format!("sha256:{}", hex::encode(Sha256::digest(&diff_material)));
    Ok(GitSnapshot {
        root,
        head,
        branch,
        dirty: !changed_paths.is_empty(),
        changed_paths,
        diff_digest,
        coverage_complete: !has_untracked && !has_ignored,
        captured_at: Utc::now(),
    })
}

pub fn git_diff(path: impl AsRef<Path>, max_bytes: usize) -> Result<String> {
    let root = git_required(path.as_ref(), &["rev-parse", "--show-toplevel"])?;
    let mut output = git_patch_material(Path::new(root.trim()))?;
    let truncated = output.len() > max_bytes;
    output.truncate(max_bytes);
    let mut text = String::from_utf8_lossy(&output).into_owned();
    if truncated {
        text.push_str("\n[agentctl: diff truncated]\n");
    }
    Ok(text)
}

pub fn git_diff_summary(path: impl AsRef<Path>, max_bytes: usize) -> Result<String> {
    let root = git_required(path.as_ref(), &["rev-parse", "--show-toplevel"])?;
    let root = Path::new(root.trim());
    let mut output = if git_optional(root, &["rev-parse", "--verify", "HEAD"])?.is_some() {
        git_bytes(
            root,
            &["diff", "--no-ext-diff", "--stat", "HEAD", "--"],
            false,
        )?
    } else {
        let mut staged = git_bytes(
            root,
            &["diff", "--cached", "--no-ext-diff", "--stat", "--"],
            false,
        )?;
        staged.extend(git_bytes(
            root,
            &["diff", "--no-ext-diff", "--stat", "--"],
            false,
        )?);
        staged
    };
    let status = git_bytes(
        root,
        &["status", "--porcelain=v1", "--untracked-files=all"],
        true,
    )?;
    if !status.is_empty() {
        output.extend_from_slice(b"\nWorkspace status:\n");
        output.extend_from_slice(&status);
    }
    let truncated = output.len() > max_bytes;
    let mut text = String::from_utf8_lossy(&output[..output.len().min(max_bytes)]).into_owned();
    if truncated {
        text.push_str("\n[agentctl: diff summary truncated]\n");
    }
    Ok(text)
}

fn git_diff_material(root: &Path) -> Result<Vec<u8>> {
    let mut tracked = git_patch_material(root)?;
    // Untracked file contents are deliberately excluded; their paths are still part of status.
    let status = git_bytes(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        true,
    )?;
    tracked.extend_from_slice(b"\0agentctl-status\0");
    tracked.extend_from_slice(&status);
    Ok(tracked)
}

fn git_patch_material(root: &Path) -> Result<Vec<u8>> {
    if git_optional(root, &["rev-parse", "--verify", "HEAD"])?.is_some() {
        git_bytes(
            root,
            &["diff", "--no-ext-diff", "--binary", "HEAD", "--"],
            false,
        )
    } else {
        let mut staged = git_bytes(
            root,
            &["diff", "--cached", "--no-ext-diff", "--binary", "--"],
            false,
        )?;
        staged.extend(git_bytes(
            root,
            &["diff", "--no-ext-diff", "--binary", "--"],
            false,
        )?);
        Ok(staged)
    }
}

fn git_required(cwd: &Path, arguments: &[&str]) -> Result<String> {
    let output = git_bytes(cwd, arguments, false)?;
    Ok(String::from_utf8_lossy(&output).trim().to_owned())
}

fn git_optional(cwd: &Path, arguments: &[&str]) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(arguments)
        .output()
        .map_err(|error| io_error(cwd, error))?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

fn git_bytes(cwd: &Path, arguments: &[&str], accept_unborn_head: bool) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(arguments)
        .output()
        .map_err(|error| io_error(cwd, error))?;
    if output.status.success()
        || (accept_unborn_head
            && output.status.code() == Some(128)
            && String::from_utf8_lossy(&output.stderr).contains("ambiguous argument 'HEAD'"))
    {
        Ok(output.stdout)
    } else {
        Err(WorkspaceError::Git {
            command: format!("git -C {} {}", cwd.display(), arguments.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn parse_porcelain(bytes: &[u8]) -> Vec<ChangedPath> {
    let records: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
        .collect();
    let mut changes = Vec::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        if record.len() < 4 {
            index += 1;
            continue;
        }
        let index_status = record[0] as char;
        let worktree_status = record[1] as char;
        let path = PathBuf::from(String::from_utf8_lossy(&record[3..]).into_owned());
        let renamed = matches!(index_status, 'R' | 'C') || matches!(worktree_status, 'R' | 'C');
        let original_path = if renamed && index + 1 < records.len() {
            index += 1;
            Some(PathBuf::from(
                String::from_utf8_lossy(records[index]).into_owned(),
            ))
        } else {
            None
        };
        changes.push(ChangedPath {
            path,
            original_path,
            index_status,
            worktree_status,
        });
        index += 1;
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    changes
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn snapshots_detect_workspace_changes() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "--quiet"]);
        git(
            directory.path(),
            &["config", "user.email", "agentctl@example.invalid"],
        );
        git(directory.path(), &["config", "user.name", "agentctl"]);
        fs::write(directory.path().join("file.txt"), "one").unwrap();
        git(directory.path(), &["add", "file.txt"]);
        git(directory.path(), &["commit", "--quiet", "-m", "initial"]);
        let before = capture_git_snapshot(directory.path()).unwrap();
        fs::write(directory.path().join("file.txt"), "two").unwrap();
        let after = capture_git_snapshot(directory.path()).unwrap();
        assert!(!before.dirty);
        assert!(after.dirty);
        assert!(after.changed_since(&before));
        assert!(git_diff(directory.path(), 1024).unwrap().contains("two"));
    }

    #[test]
    fn unborn_repository_can_be_snapshotted() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "--quiet"]);
        fs::write(directory.path().join("new.txt"), "new").unwrap();
        let snapshot = capture_git_snapshot(directory.path()).unwrap();
        assert!(snapshot.dirty);
        assert!(git_diff(directory.path(), 1024).is_ok());
        assert!(
            git_diff_summary(directory.path(), 1024)
                .unwrap()
                .contains("new.txt")
        );
        assert!(!snapshot.coverage_complete);
    }

    #[test]
    fn ignored_and_untracked_paths_make_replay_coverage_conservative() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "--quiet"]);
        fs::write(directory.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(directory.path().join("tracked.txt"), "tracked").unwrap();
        git(directory.path(), &["add", ".gitignore", "tracked.txt"]);
        git(
            directory.path(),
            &["config", "user.email", "agentctl@example.invalid"],
        );
        git(directory.path(), &["config", "user.name", "agentctl"]);
        git(directory.path(), &["commit", "--quiet", "-m", "initial"]);

        assert!(
            capture_git_snapshot(directory.path())
                .unwrap()
                .coverage_complete
        );
        fs::write(directory.path().join("ignored.txt"), "secret state").unwrap();
        assert!(
            !capture_git_snapshot(directory.path())
                .unwrap()
                .coverage_complete
        );
        fs::write(directory.path().join("untracked.txt"), "other state").unwrap();
        assert!(
            !capture_git_snapshot(directory.path())
                .unwrap()
                .coverage_complete
        );
    }

    #[test]
    fn non_git_workspace_has_a_conservative_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("state.txt"), "one").unwrap();
        let snapshot = capture_git_snapshot(directory.path()).unwrap();
        assert_eq!(snapshot.root, directory.path().canonicalize().unwrap());
        assert!(!snapshot.coverage_complete);
    }

    #[test]
    fn legacy_snapshot_without_coverage_field_is_conservative() {
        let snapshot: GitSnapshot = serde_json::from_value(serde_json::json!({
            "root": "/repo",
            "head": null,
            "branch": null,
            "changed_paths": [],
            "dirty": false,
            "diff_digest": "sha256:legacy",
            "captured_at": Utc::now(),
        }))
        .unwrap();
        assert!(!snapshot.coverage_complete);
    }
}
