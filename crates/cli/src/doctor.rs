use std::{path::Path, process::Stdio};

use agentctl_core::{
    AgentEvent, AgentProvider, AuthMode, CanonicalEvent, EventId, EventVisibility, ProviderKind,
    SessionContext, SyncBatch, TurnId, TurnRequest, TurnStatus, UnifiedSessionId,
};
use agentctl_provider_codex::{detect_installation, generate_schema_cache_under};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::{fs, process::Command};

use crate::{config::Config, paths::AgentctlPaths};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CompatibilityReport {
    pub generated_at: DateTime<Utc>,
    pub live_requested: bool,
    pub checks: Vec<CompatibilityCheck>,
}

impl CompatibilityReport {
    pub(crate) fn compatible(&self) -> bool {
        self.checks
            .iter()
            .all(|check| check.optional || check.status != CheckStatus::Failed)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CompatibilityCheck {
    pub provider: Option<String>,
    pub name: String,
    pub status: CheckStatus,
    pub optional: bool,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckStatus {
    Passed,
    Warning,
    Failed,
    Skipped,
}

pub(crate) async fn inspect(
    paths: &AgentctlPaths,
    config: &Config,
    live_requested: bool,
) -> CompatibilityReport {
    let mut checks = Vec::new();
    checks.extend(inspect_codex(paths, &config.providers.codex_binary).await);
    checks.extend(inspect_claude(&config.providers.claude_binary).await);
    checks.push(check_local_permissions(paths));
    if live_requested {
        checks.push(CompatibilityCheck {
            provider: None,
            name: "live protocol turns".to_owned(),
            status: CheckStatus::Skipped,
            optional: true,
            detail: "live turns are executed by the runtime after static compatibility succeeds"
                .to_owned(),
        });
    }
    CompatibilityReport {
        generated_at: Utc::now(),
        live_requested,
        checks,
    }
}

pub(crate) async fn write_report(report: &CompatibilityReport, path: &Path) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    fs::write(path, bytes)
        .await
        .with_context(|| format!("failed to write {}", path.display()))?;
    set_private_file(path)?;
    Ok(())
}

/// Executes one disposable, context-bearing model turn. This is intentionally
/// called only by `doctor --live` because it consumes provider quota.
pub(crate) async fn run_live_turn(
    provider: &dyn AgentProvider,
    workspace: &Path,
) -> CompatibilityCheck {
    let provider_name = provider.kind().to_string();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        perform_live_turn(provider, workspace),
    )
    .await;
    let _ = provider.shutdown().await;

    match result {
        Ok(Ok(())) => CompatibilityCheck {
            provider: Some(provider_name),
            name: "live projection, resume, and interrupt".to_owned(),
            status: CheckStatus::Passed,
            optional: false,
            detail: "context injection, restart/resume, and turn interruption succeeded".to_owned(),
        },
        Ok(Err(error)) => CompatibilityCheck {
            provider: Some(provider_name),
            name: "live projection, resume, and interrupt".to_owned(),
            status: CheckStatus::Failed,
            optional: false,
            detail: error.to_string(),
        },
        Err(_) => CompatibilityCheck {
            provider: Some(provider_name),
            name: "live projection, resume, and interrupt".to_owned(),
            status: CheckStatus::Failed,
            optional: false,
            detail: "live probe timed out after 180 seconds".to_owned(),
        },
    }
}

async fn perform_live_turn(
    provider: &dyn AgentProvider,
    workspace: &Path,
) -> std::result::Result<(), agentctl_core::ProviderError> {
    let session_id = UnifiedSessionId::new();
    let context = SessionContext {
        unified_session_id: session_id,
        workspace_root: workspace.to_path_buf(),
        workspace_fingerprint: "agentctl-live-probe".to_owned(),
        auth_mode: AuthMode::NativeLocal,
    };
    let native = provider.ensure_session(&context).await?;
    let nonce = format!("AGENTCTL_LIVE_{}", uuid::Uuid::new_v4().simple());
    inject_probe_nonce(provider, &native, session_id, 1, &nonce).await?;
    run_nonce_turn(provider, &native, session_id, workspace, &nonce).await?;

    provider.shutdown().await?;
    let native = provider.restore_session(&context, native).await?;
    let resumed_nonce = format!("AGENTCTL_RESUME_{}", uuid::Uuid::new_v4().simple());
    inject_probe_nonce(provider, &native, session_id, 2, &resumed_nonce).await?;
    run_nonce_turn(provider, &native, session_id, workspace, &resumed_nonce).await?;
    run_interrupt_probe(provider, &native, session_id, workspace).await
}

async fn inject_probe_nonce(
    provider: &dyn AgentProvider,
    native: &agentctl_core::NativeSession,
    session_id: UnifiedSessionId,
    seq: u64,
    nonce: &str,
) -> std::result::Result<(), agentctl_core::ProviderError> {
    let source = match provider.kind() {
        ProviderKind::Codex => ProviderKind::Claude,
        _ => ProviderKind::Codex,
    };
    let payload = serde_json::json!({"text": format!("External agent context nonce: {nonce}")});
    let event = CanonicalEvent {
        schema_version: 1,
        session_id,
        seq,
        event_id: EventId::new(),
        turn_id: None,
        origin_provider: Some(source),
        kind: "assistant_final".to_owned(),
        visibility: EventVisibility::Projection,
        payload,
        content_hash: format!("probe:{nonce}"),
        raw_event_id: None,
        created_at: Utc::now(),
    };
    provider
        .sync_context(
            native,
            SyncBatch {
                from_seq_exclusive: seq.saturating_sub(1),
                through_seq_inclusive: seq,
                projection_version: 1,
                events: vec![event],
                handoff: Some(format!(
                    "<agent-handoff version=\"1\"><assistant-result>External agent context nonce: {nonce}</assistant-result></agent-handoff>"
                )),
            },
        )
        .await?;
    Ok(())
}

async fn run_nonce_turn(
    provider: &dyn AgentProvider,
    native: &agentctl_core::NativeSession,
    session_id: UnifiedSessionId,
    workspace: &Path,
    nonce: &str,
) -> std::result::Result<(), agentctl_core::ProviderError> {
    let mut stream = provider
        .run_turn(
            native,
            TurnRequest {
                session_id,
                turn_id: TurnId::new(),
                prompt:
                    "Reply with exactly the external context nonce you received. Do not use tools."
                        .to_owned(),
                cwd: workspace.to_path_buf(),
                continuation: false,
                execution_mode: agentctl_core::TurnExecutionMode::ReadWrite,
                metadata: serde_json::json!({"doctor_live": true}),
            },
        )
        .await?;
    let mut text = String::new();
    let mut completed = false;
    while let Some(event) = stream.next().await {
        match event? {
            AgentEvent::AssistantTextDelta { text: delta }
            | AgentEvent::AssistantFinal { text: delta } => text.push_str(&delta),
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
            } => completed = true,
            AgentEvent::Error { error } => {
                return Err(agentctl_core::ProviderError::Protocol(error.message));
            }
            _ => {}
        }
    }
    if !completed {
        return Err(agentctl_core::ProviderError::Process(
            "live turn ended without completion".to_owned(),
        ));
    }
    if !text.contains(nonce) {
        return Err(agentctl_core::ProviderError::Protocol(format!(
            "provider did not recover projected nonce; response was {text:?}"
        )));
    }
    Ok(())
}

async fn run_interrupt_probe(
    provider: &dyn AgentProvider,
    native: &agentctl_core::NativeSession,
    session_id: UnifiedSessionId,
    workspace: &Path,
) -> std::result::Result<(), agentctl_core::ProviderError> {
    let turn_id = TurnId::new();
    let mut stream = provider
        .run_turn(
            native,
            TurnRequest {
                session_id,
                turn_id,
                prompt: "Write a very long numbered list. Do not use tools.".to_owned(),
                cwd: workspace.to_path_buf(),
                continuation: false,
                execution_mode: agentctl_core::TurnExecutionMode::ReadWrite,
                metadata: serde_json::json!({"doctor_interrupt": true}),
            },
        )
        .await?;
    provider.interrupt(native, &turn_id.to_string()).await?;
    let interrupted = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(event) = stream.next().await {
            match event {
                Ok(AgentEvent::TurnCompleted {
                    status: TurnStatus::Interrupted,
                })
                | Err(_) => return true,
                Ok(AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                }) => return false,
                _ => {}
            }
        }
        true
    })
    .await
    .map_err(|_| {
        agentctl_core::ProviderError::Process(
            "interrupted turn did not terminate within 30 seconds".to_owned(),
        )
    })?;
    if !interrupted {
        return Err(agentctl_core::ProviderError::Protocol(
            "turn completed normally after interrupt acknowledgement".to_owned(),
        ));
    }
    Ok(())
}

async fn inspect_codex(paths: &AgentctlPaths, binary: &str) -> Vec<CompatibilityCheck> {
    let mut checks = Vec::new();
    let installation = match detect_installation(Path::new(binary)).await {
        Ok(installation) => installation,
        Err(error) => {
            checks.push(failed("codex", "binary", error.to_string()));
            return checks;
        }
    };
    checks.push(passed("codex", "binary", installation.version.clone()));

    let schema = generate_schema_cache_under(&installation, &paths.codex_protocol_root()).await;
    match schema {
        Ok(schema_dir) => {
            if let Err(error) = set_private_tree(&schema_dir) {
                checks.push(failed(
                    "codex",
                    "schema cache permissions",
                    error.to_string(),
                ));
                return checks;
            }
            checks.push(passed(
                "codex",
                "installed-version schema",
                schema_dir.display().to_string(),
            ));
            let request_schema = schema_dir.join("ClientRequest.json");
            match fs::read_to_string(&request_schema).await {
                Ok(contents) => {
                    for method in [
                        "thread/start",
                        "thread/resume",
                        "thread/list",
                        "thread/read",
                        "thread/fork",
                        "thread/inject_items",
                        "turn/start",
                        "turn/interrupt",
                        "account/rateLimits/read",
                    ] {
                        let status = if contents.contains(method) {
                            passed("codex", method, "advertised by generated schema".to_owned())
                        } else {
                            failed("codex", method, "missing from generated schema".to_owned())
                        };
                        checks.push(status);
                    }
                }
                Err(error) => checks.push(failed("codex", "request schema", error.to_string())),
            }
        }
        Err(error) => checks.push(failed(
            "codex",
            "installed-version schema",
            error.to_string(),
        )),
    }
    checks
}

async fn inspect_claude(binary: &str) -> Vec<CompatibilityCheck> {
    let mut checks = Vec::new();
    let version = match command_output(binary, &["--version"]).await {
        Ok(version) => version,
        Err(error) => {
            checks.push(failed("claude", "binary", error.to_string()));
            return checks;
        }
    };
    checks.push(passed("claude", "binary", version.trim().to_owned()));
    checks.push(match crate::native_hooks::parse_claude_semver(&version) {
        Some(version) if crate::native_hooks::supports_exec_hooks(version) => passed(
            "claude",
            "native exec-form hooks",
            format!(
                "{}.{}.{} supports safe hook arguments",
                version.0, version.1, version.2
            ),
        ),
        Some(version) => failed(
            "claude",
            "native exec-form hooks",
            format!(
                "{}.{}.{} is older than the required 2.1.139",
                version.0, version.1, version.2
            ),
        ),
        None => failed(
            "claude",
            "native exec-form hooks",
            "could not parse the installed Claude Code version".to_owned(),
        ),
    });
    match command_output(binary, &["--help"]).await {
        Ok(help) => {
            for flag in [
                "--print",
                "--input-format",
                "--output-format",
                "--session-id",
                "--resume",
                "--settings",
                "--include-partial-messages",
                "--replay-user-messages",
                "--append-system-prompt",
            ] {
                checks.push(if help.contains(flag) {
                    passed("claude", flag, "advertised by installed CLI".to_owned())
                } else {
                    failed("claude", flag, "missing from installed CLI".to_owned())
                });
            }
            checks.push(CompatibilityCheck {
                provider: Some("claude".to_owned()),
                name: "permission prompt bridge".to_owned(),
                status: if help.contains("--permission-prompt-tool") {
                    CheckStatus::Passed
                } else {
                    CheckStatus::Warning
                },
                optional: true,
                detail: if help.contains("--permission-prompt-tool") {
                    "advertised by installed CLI".to_owned()
                } else {
                    "not advertised; MCP/hook capability probe required".to_owned()
                },
            });
        }
        Err(error) => checks.push(failed("claude", "help", error.to_string())),
    }
    checks
}

async fn command_output(binary: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to execute {binary}"))?;
    if !output.status.success() {
        bail!(
            "{} exited {}: {}",
            binary,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("command emitted non-UTF-8 output")
}

fn passed(provider: &str, name: &str, detail: String) -> CompatibilityCheck {
    CompatibilityCheck {
        provider: Some(provider.to_owned()),
        name: name.to_owned(),
        status: CheckStatus::Passed,
        optional: false,
        detail,
    }
}

fn failed(provider: &str, name: &str, detail: String) -> CompatibilityCheck {
    CompatibilityCheck {
        provider: Some(provider.to_owned()),
        name: name.to_owned(),
        status: CheckStatus::Failed,
        optional: false,
        detail,
    }
}

fn check_local_permissions(paths: &AgentctlPaths) -> CompatibilityCheck {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let result = std::fs::metadata(&paths.home).map(|metadata| metadata.permissions().mode());
        match result {
            Ok(mode) if mode.trailing_zeros() >= 6 => CompatibilityCheck {
                provider: None,
                name: "state directory permissions".to_owned(),
                status: CheckStatus::Passed,
                optional: false,
                detail: format!("{} is private", paths.home.display()),
            },
            Ok(mode) => CompatibilityCheck {
                provider: None,
                name: "state directory permissions".to_owned(),
                status: CheckStatus::Failed,
                optional: false,
                detail: format!("unexpected mode {:o}", mode & 0o777),
            },
            Err(error) => CompatibilityCheck {
                provider: None,
                name: "state directory permissions".to_owned(),
                status: CheckStatus::Failed,
                optional: false,
                detail: error.to_string(),
            },
        }
    }
    #[cfg(not(unix))]
    CompatibilityCheck {
        provider: None,
        name: "state directory permissions".to_owned(),
        status: CheckStatus::Skipped,
        optional: true,
        detail: "Unix permissions are not available".to_owned(),
    }
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_tree(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let item = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::set_permissions(&item, std::fs::Permissions::from_mode(0o700))?;
            set_private_tree(&item)?;
        } else {
            std::fs::set_permissions(&item, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_tree(_path: &Path) -> Result<()> {
    Ok(())
}
