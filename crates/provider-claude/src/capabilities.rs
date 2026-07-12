use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use agentctl_core::ProviderError;
use agentctl_workspace::{ProcessTree, configure_tokio_process_group};
use tokio::{process::Command, time::timeout};

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct ClaudeInstallation {
    pub binary: PathBuf,
    pub version: String,
    pub flags: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct ClaudeAuthStatus {
    pub logged_in: bool,
    pub auth_method: Option<String>,
    pub api_provider: Option<String>,
}

impl ClaudeInstallation {
    pub fn supports_flag(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }

    pub fn supports_stream_json(&self) -> bool {
        [
            "--input-format",
            "--output-format",
            "--session-id",
            "--resume",
        ]
        .into_iter()
        .all(|flag| self.supports_flag(flag))
    }
}

/// Inspects version/help only. No Claude session or model request is created.
pub async fn detect_installation(binary: &Path) -> Result<ClaudeInstallation, ProviderError> {
    let version_output = run(binary, &["--version"]).await?;
    let version = String::from_utf8_lossy(&version_output.stdout)
        .trim()
        .to_owned();
    if version.is_empty() {
        return Err(ProviderError::Incompatible(
            "`claude --version` returned an empty version".to_owned(),
        ));
    }
    let help_output = run(binary, &["--help"]).await?;
    let help = String::from_utf8_lossy(&help_output.stdout);
    let known_flags = [
        "--input-format",
        "--output-format",
        "--include-partial-messages",
        "--replay-user-messages",
        "--session-id",
        "--resume",
        "--append-system-prompt",
        "--append-system-prompt-file",
        "--include-hook-events",
        "--settings",
    ];
    let flags = known_flags
        .into_iter()
        .filter(|flag| help.contains(flag))
        .map(ToOwned::to_owned)
        .collect();
    Ok(ClaudeInstallation {
        binary: binary.to_path_buf(),
        version,
        flags,
    })
}

/// Reads Claude Code's official local auth-status command without accessing credentials.
pub async fn detect_auth_status(binary: &Path) -> Result<ClaudeAuthStatus, ProviderError> {
    let output = run(binary, &["auth", "status", "--json"]).await?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| ProviderError::Protocol(format!("invalid Claude auth status: {error}")))?;
    Ok(ClaudeAuthStatus {
        logged_in: value
            .get("loggedIn")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        auth_method: value
            .get("authMethod")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        api_provider: value
            .get("apiProvider")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
    })
}

async fn run(binary: &Path, arguments: &[&str]) -> Result<std::process::Output, ProviderError> {
    let mut command = Command::new(binary);
    command
        .args(arguments)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    configure_tokio_process_group(&mut command);
    let mut child = command.spawn().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ProviderError::BinaryNotFound(binary.display().to_string()),
        _ => ProviderError::Io(error),
    })?;
    let process_tree = match ProcessTree::attach(&child) {
        Ok(process_tree) => process_tree,
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(ProviderError::Process(error.to_string()));
        }
    };
    let Ok(output) = timeout(PROBE_TIMEOUT, child.wait_with_output()).await else {
        let _ = process_tree.terminate();
        return Err(ProviderError::Process(format!(
            "`claude {}` timed out",
            arguments.join(" ")
        )));
    };
    let output = output.map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ProviderError::BinaryNotFound(binary.display().to_string()),
        _ => ProviderError::Io(error),
    })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(ProviderError::Process(format!(
            "`claude {}` exited with {}: {}",
            arguments.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::ClaudeInstallation;
    use std::{collections::BTreeSet, path::PathBuf};

    #[test]
    fn stream_support_requires_both_flags() {
        let mut installation = ClaudeInstallation {
            binary: PathBuf::from("claude"),
            version: "test".to_owned(),
            flags: BTreeSet::from(["--input-format".to_owned()]),
        };
        assert!(!installation.supports_stream_json());
        installation.flags.extend([
            "--output-format".to_owned(),
            "--session-id".to_owned(),
            "--resume".to_owned(),
        ]);
        assert!(installation.supports_stream_json());
    }
}
