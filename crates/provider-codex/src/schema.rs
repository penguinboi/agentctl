use std::path::{Path, PathBuf};

use agentctl_core::ProviderError;
use agentctl_workspace::{ProcessTree, configure_tokio_process_group};
use directories::ProjectDirs;
use tokio::{process::Command, time::timeout};

const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct CodexInstallation {
    pub binary: PathBuf,
    pub version: String,
}

pub async fn detect_installation(binary: &Path) -> Result<CodexInstallation, ProviderError> {
    let mut command = Command::new(binary);
    command.arg("--version");
    let output = run_output(command, binary, "`codex --version`").await?;
    if !output.status.success() {
        return Err(ProviderError::Process(format!(
            "`codex --version` exited with {}",
            output.status
        )));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version.is_empty() {
        return Err(ProviderError::Incompatible(
            "`codex --version` returned an empty version".to_owned(),
        ));
    }
    Ok(CodexInstallation {
        binary: binary.to_path_buf(),
        version,
    })
}

pub async fn generate_schema_cache(
    installation: &CodexInstallation,
) -> Result<PathBuf, ProviderError> {
    let project = ProjectDirs::from("dev", "agentctl", "agentctl").ok_or_else(|| {
        ProviderError::Process("could not determine agentctl data directory".to_owned())
    })?;
    let safe_version = installation
        .version
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let directory = project
        .cache_dir()
        .join("protocols")
        .join("codex")
        .join(safe_version);
    tokio::fs::create_dir_all(&directory).await?;

    let mut command = Command::new(&installation.binary);
    command
        .args(["app-server", "generate-json-schema", "--out"])
        .arg(&directory);
    let output = run_output(command, &installation.binary, "Codex schema generation").await?;
    if !output.status.success() {
        return Err(ProviderError::Incompatible(format!(
            "Codex schema generation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(directory)
}

async fn run_output(
    mut command: Command,
    binary: &Path,
    description: &str,
) -> Result<std::process::Output, ProviderError> {
    command
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
    let Ok(output) = timeout(COMMAND_TIMEOUT, child.wait_with_output()).await else {
        let _ = process_tree.terminate();
        return Err(ProviderError::Process(format!("{description} timed out")));
    };
    output.map_err(ProviderError::Io)
}
