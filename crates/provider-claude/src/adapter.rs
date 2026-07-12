use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use agentctl_core::{
    AgentEvent, AgentProvider, ApprovalAction, ApprovalDecision, ApprovalId, ApprovalRequest,
    NativeSession, ProviderError, ProviderEventStream, ProviderHealth, ProviderKind,
    ProviderSessionId, ProviderStatus, RiskLevel, SessionContext, SyncBatch, SyncReceipt,
    TurnExecutionMode, TurnRequest, UnifiedSessionId,
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::{
    capabilities::{ClaudeInstallation, detect_auth_status, detect_installation},
    hooks::HANDOFF_POLICY,
    mapping::{classify_result_error, map_message},
    process::{ClaudeProcess, ProcessFeatures, SpawnOptions},
    protocol::{ClaudeInit, control_success, user_message},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ShouldQuerySupport {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UserPromptSubmitHookSupport {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Clone, Debug)]
pub struct ShouldQueryProbeReport {
    pub supported: bool,
    pub session_id: String,
    pub synthetic_triggered_turn: bool,
    pub nonce_observed: bool,
    pub persisted_after_resume: Option<bool>,
    pub result_messages: usize,
}

#[derive(Clone, Debug)]
pub struct UserPromptSubmitHookProbeReport {
    pub supported: bool,
    pub session_id: String,
    pub nonce_observed: bool,
    pub persisted_after_resume: Option<bool>,
    pub result_messages: usize,
}

#[derive(Clone, Debug)]
pub struct ClaudeConfig {
    pub binary: PathBuf,
    pub runtime_root: Option<PathBuf>,
    pub append_system_prompt_file: Option<PathBuf>,
    pub channel_capacity: usize,
    pub control_timeout: Duration,
    pub init_timeout: Duration,
    pub should_query: ShouldQuerySupport,
    pub user_prompt_submit_hook: UserPromptSubmitHookSupport,
    pub enable_hook_fallback: bool,
    pub allow_prompt_capsule: bool,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            runtime_root: None,
            append_system_prompt_file: None,
            channel_capacity: 256,
            control_timeout: Duration::from_secs(15),
            init_timeout: Duration::from_secs(3),
            should_query: ShouldQuerySupport::Unknown,
            user_prompt_submit_hook: UserPromptSubmitHookSupport::Unknown,
            enable_hook_fallback: true,
            allow_prompt_capsule: true,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingApproval {
    native_session_id: String,
    process_instance_id: Option<Uuid>,
    request_id: String,
    tool_name: String,
    input: Value,
    suggestions: Option<Value>,
    tool_use_id: Option<String>,
}

/// Provider adapter backed by persistent Claude Code stream-json processes.
#[derive(Debug)]
pub struct ClaudeAdapter {
    config: ClaudeConfig,
    installation: RwLock<Option<ClaudeInstallation>>,
    sessions: RwLock<HashMap<UnifiedSessionId, String>>,
    contexts: RwLock<HashMap<String, PathBuf>>,
    processes: RwLock<HashMap<String, Arc<ClaudeProcess>>>,
    pending_capsules: Arc<Mutex<HashMap<String, String>>>,
    pending_hook_context: Arc<Mutex<HashMap<String, String>>>,
    pending_approvals: Arc<Mutex<HashMap<ApprovalId, PendingApproval>>>,
    active_turns: Arc<Mutex<HashMap<String, String>>>,
    applied_syncs: Mutex<HashMap<(String, u64, u32), Option<String>>>,
}

impl ClaudeAdapter {
    /// Creates an adapter with optional policy file and private runtime directory.
    pub fn new(
        binary: impl Into<PathBuf>,
        append_system_prompt_file: Option<PathBuf>,
        runtime_root: Option<PathBuf>,
    ) -> Self {
        Self::from_config(ClaudeConfig {
            binary: binary.into(),
            append_system_prompt_file,
            runtime_root,
            ..ClaudeConfig::default()
        })
    }

    pub fn from_config(config: ClaudeConfig) -> Self {
        Self {
            config,
            installation: RwLock::new(None),
            sessions: RwLock::new(HashMap::new()),
            contexts: RwLock::new(HashMap::new()),
            processes: RwLock::new(HashMap::new()),
            pending_capsules: Arc::new(Mutex::new(HashMap::new())),
            pending_hook_context: Arc::new(Mutex::new(HashMap::new())),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            active_turns: Arc::new(Mutex::new(HashMap::new())),
            applied_syncs: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a persisted Claude session. Its next process is spawned with `--resume`.
    pub async fn attach_native_session(
        &self,
        unified_session_id: UnifiedSessionId,
        native_session_id: impl Into<String>,
        workspace_root: impl Into<PathBuf>,
    ) -> Result<(), ProviderError> {
        let native_session_id = native_session_id.into();
        Uuid::parse_str(&native_session_id).map_err(|error| {
            ProviderError::Incompatible(format!("invalid Claude session UUID: {error}"))
        })?;
        self.sessions
            .write()
            .await
            .insert(unified_session_id, native_session_id.clone());
        self.contexts
            .write()
            .await
            .insert(native_session_id, workspace_root.into());
        Ok(())
    }

    /// Version/help probe only; never creates a session or invokes a model.
    pub async fn probe_static(&self) -> Result<ProviderHealth, ProviderError> {
        let installation = self.installation().await?;
        let compatible = installation.supports_stream_json();
        let auth = detect_auth_status(&self.config.binary).await.ok();
        let authenticated = auth.as_ref().is_none_or(|auth| auth.logged_in);
        Ok(ProviderHealth {
            provider: ProviderKind::Claude,
            status: if !authenticated {
                ProviderStatus::AuthError
            } else if compatible {
                ProviderStatus::Ready
            } else {
                ProviderStatus::Incompatible
            },
            version: Some(installation.version.clone()),
            capabilities: self.capabilities(None, &installation),
            usage: None,
            rate_limit: None,
            checked_at: Utc::now(),
            message: if !compatible {
                Some("Claude binary lacks required stream-json flags".to_owned())
            } else if !authenticated {
                Some("Claude Code is not authenticated".to_owned())
            } else {
                auth.map(|auth| {
                    format!(
                        "authenticated via {} ({})",
                        auth.auth_method.as_deref().unwrap_or("unknown"),
                        auth.api_provider.as_deref().unwrap_or("unknown provider")
                    )
                })
            },
        })
    }

    /// Exercises the SDK control handshake with an empty disposable session; no model turn runs.
    pub async fn probe_live(&self, cwd: &Path) -> Result<ProviderHealth, ProviderError> {
        let installation = self.installation().await?;
        if !installation.supports_stream_json() {
            return Err(ProviderError::Incompatible(
                "Claude binary lacks stream-json support".to_owned(),
            ));
        }
        let session_id = Uuid::new_v4().to_string();
        let process = self.spawn_process(&session_id, cwd, false).await?;
        let init = process.wait_init(self.config.init_timeout).await;
        process.shutdown().await?;
        let auth = detect_auth_status(&self.config.binary).await.ok();
        let authenticated = auth.as_ref().is_none_or(|auth| auth.logged_in);
        Ok(ProviderHealth {
            provider: ProviderKind::Claude,
            status: if authenticated {
                ProviderStatus::Ready
            } else {
                ProviderStatus::AuthError
            },
            version: Some(installation.version.clone()),
            capabilities: self.capabilities(init.as_ref(), &installation),
            usage: None,
            rate_limit: None,
            checked_at: Utc::now(),
            message: Some(if init.is_some() {
                "stream-json control handshake and system/init succeeded".to_owned()
            } else {
                "control handshake succeeded; system/init is deferred until first turn".to_owned()
            }),
        })
    }

    /// Opt-in behavioral probe for `shouldQuery:false`.
    ///
    /// This intentionally performs one model turn, or two when `verify_resume`
    /// is true, and can consume the user's Claude quota. Ordinary tests and
    /// `probe_static`/`probe_live` never call it.
    pub async fn probe_should_query(
        &self,
        cwd: &Path,
        verify_resume: bool,
    ) -> Result<ShouldQueryProbeReport, ProviderError> {
        let session_id = Uuid::new_v4().to_string();
        let nonce = format!("agentctl-should-query-{}", Uuid::new_v4());
        let process = self.spawn_process(&session_id, cwd, false).await?;
        let mut inbound = process.subscribe();
        process
            .send(user_message(&session_id, &nonce, false))
            .await?;
        let synthetic_triggered_turn =
            observes_model_activity(&mut inbound, &session_id, Duration::from_secs(3)).await?;
        if synthetic_triggered_turn {
            process.shutdown().await?;
            return Ok(ShouldQueryProbeReport {
                supported: false,
                session_id,
                synthetic_triggered_turn: true,
                nonce_observed: false,
                persisted_after_resume: None,
                result_messages: 1,
            });
        }

        let prompt = "Reply with only the nonce supplied in the immediately preceding synthetic context message. The nonce starts with agentctl-should-query-.";
        process
            .send(user_message(&session_id, prompt, true))
            .await?;
        let (result, result_messages) = wait_probe_result(&mut inbound, &session_id).await?;
        let nonce_observed = result.contains(&nonce);
        process.shutdown().await?;

        let persisted_after_resume = if verify_resume && nonce_observed {
            let resumed = self.spawn_process(&session_id, cwd, true).await?;
            let mut resumed_inbound = resumed.subscribe();
            resumed
                .send(user_message(
                    &session_id,
                    "Reply only with the agentctl-should-query nonce from the prior turn.",
                    true,
                ))
                .await?;
            let (result, _) = wait_probe_result(&mut resumed_inbound, &session_id).await?;
            resumed.shutdown().await?;
            Some(result.contains(&nonce))
        } else {
            None
        };
        Ok(ShouldQueryProbeReport {
            supported: nonce_observed && persisted_after_resume.unwrap_or(true),
            session_id,
            synthetic_triggered_turn: false,
            nonce_observed,
            persisted_after_resume,
            result_messages,
        })
    }

    /// Opt-in behavioral probe for `UserPromptSubmit` `additionalContext`.
    ///
    /// Flag discovery alone is not capability evidence. This probe forces the
    /// hook on in a disposable session, verifies that its nonce reaches the
    /// model, and can verify persistence after resume. It consumes one or two
    /// Claude model turns and is therefore only called by `doctor --live`.
    pub async fn probe_user_prompt_submit_hook(
        &self,
        cwd: &Path,
        verify_resume: bool,
    ) -> Result<UserPromptSubmitHookProbeReport, ProviderError> {
        if !self.installation().await?.supports_flag("--settings") {
            return Err(ProviderError::Incompatible(
                "installed Claude CLI does not advertise --settings".to_owned(),
            ));
        }
        let session_id = Uuid::new_v4().to_string();
        let nonce = format!("agentctl-user-prompt-submit-{}", Uuid::new_v4());
        let process = self
            .spawn_process_with_hook(&session_id, cwd, false, true)
            .await?;
        let mut inbound = process.subscribe();
        process
            .stage_hook_context(&format!(
                "<agent-handoff><hook-probe-nonce>{nonce}</hook-probe-nonce></agent-handoff>"
            ))
            .await?;
        process
            .send(user_message(
                &session_id,
                "Reply with only the nonce supplied through UserPromptSubmit additional context. The nonce starts with agentctl-user-prompt-submit-.",
                true,
            ))
            .await?;
        let (result, result_messages) = wait_probe_result(&mut inbound, &session_id).await?;
        let nonce_observed = result.contains(&nonce);
        process.shutdown().await?;

        let persisted_after_resume = if verify_resume && nonce_observed {
            let resumed = self
                .spawn_process_with_hook(&session_id, cwd, true, true)
                .await?;
            let mut resumed_inbound = resumed.subscribe();
            resumed
                .send(user_message(
                    &session_id,
                    "Reply only with the agentctl-user-prompt-submit nonce from the prior turn.",
                    true,
                ))
                .await?;
            let (result, _) = wait_probe_result(&mut resumed_inbound, &session_id).await?;
            resumed.shutdown().await?;
            Some(result.contains(&nonce))
        } else {
            None
        };
        Ok(UserPromptSubmitHookProbeReport {
            supported: nonce_observed && persisted_after_resume.unwrap_or(true),
            session_id,
            nonce_observed,
            persisted_after_resume,
            result_messages,
        })
    }

    async fn installation(&self) -> Result<ClaudeInstallation, ProviderError> {
        if let Some(installation) = self.installation.read().await.clone() {
            return Ok(installation);
        }
        let installation = detect_installation(&self.config.binary).await?;
        *self.installation.write().await = Some(installation.clone());
        Ok(installation)
    }

    async fn spawn_process(
        &self,
        session_id: &str,
        cwd: &Path,
        resume: bool,
    ) -> Result<Arc<ClaudeProcess>, ProviderError> {
        let hook_enabled = self.config.enable_hook_fallback
            && self.config.user_prompt_submit_hook == UserPromptSubmitHookSupport::Supported;
        self.spawn_process_with_hook(session_id, cwd, resume, hook_enabled)
            .await
    }

    async fn spawn_process_with_hook(
        &self,
        session_id: &str,
        cwd: &Path,
        resume: bool,
        hook_enabled: bool,
    ) -> Result<Arc<ClaudeProcess>, ProviderError> {
        let installation = self.installation().await?;
        ClaudeProcess::spawn(SpawnOptions {
            binary: &self.config.binary,
            session_id,
            cwd,
            resume,
            channel_capacity: self.config.channel_capacity,
            control_timeout: self.config.control_timeout,
            runtime_root: self.config.runtime_root.as_deref(),
            features: ProcessFeatures::empty()
                .with_hook(hook_enabled && installation.supports_flag("--settings"))
                .with_partial_messages(installation.supports_flag("--include-partial-messages"))
                .with_replay_user_messages(installation.supports_flag("--replay-user-messages"))
                .with_hook_events(installation.supports_flag("--include-hook-events")),
            policy_file: self.config.append_system_prompt_file.as_deref(),
        })
        .await
    }

    async fn process_for(
        &self,
        session_id: &str,
        cwd: &Path,
        resume_if_spawned: bool,
    ) -> Result<Arc<ClaudeProcess>, ProviderError> {
        if let Some(process) = self.processes.read().await.get(session_id).cloned()
            && !process.is_closed()
        {
            return Ok(process);
        }
        let process = self
            .spawn_process(session_id, cwd, resume_if_spawned)
            .await?;
        self.processes
            .write()
            .await
            .insert(session_id.to_owned(), Arc::clone(&process));
        Ok(process)
    }

    fn capabilities(
        &self,
        init: Option<&ClaudeInit>,
        installation: &ClaudeInstallation,
    ) -> BTreeMap<String, bool> {
        let mut capabilities = init
            .map(|init| init.capabilities.clone())
            .unwrap_or_default();
        for capability in [
            "streaming",
            "resume",
            "interrupt",
            "approvals",
            "rate_limits",
            "raw_events",
        ] {
            capabilities.insert(capability.to_owned(), true);
        }
        capabilities.insert(
            "partial_messages".to_owned(),
            installation.supports_flag("--include-partial-messages"),
        );
        capabilities.insert(
            "should_query".to_owned(),
            self.config.should_query == ShouldQuerySupport::Supported,
        );
        capabilities.insert(
            "user_prompt_submit_hook".to_owned(),
            self.config.enable_hook_fallback
                && self.config.user_prompt_submit_hook == UserPromptSubmitHookSupport::Supported
                && installation.supports_flag("--settings"),
        );
        capabilities.insert(
            "prompt_capsule".to_owned(),
            self.config.allow_prompt_capsule,
        );
        capabilities
    }

    async fn respond_to_approval(
        &self,
        session: &NativeSession,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), ProviderError> {
        let pending = self
            .pending_approvals
            .lock()
            .await
            .remove(&approval_id)
            .ok_or_else(|| ProviderError::Protocol(format!("unknown approval {approval_id}")))?;
        if pending.native_session_id != session.native_session_id {
            return Err(ProviderError::Protocol(
                "approval belongs to another Claude session".to_owned(),
            ));
        }
        let Some(process) = self
            .processes
            .read()
            .await
            .get(&session.native_session_id)
            .cloned()
        else {
            return Err(ProviderError::Incompatible(
                "Claude approval bridge process is no longer available".to_owned(),
            ));
        };
        if pending.process_instance_id != Some(process.instance_id()) {
            return Err(ProviderError::Process(
                "approval's Claude process was replaced".to_owned(),
            ));
        }
        let response = permission_response(&pending, decision);
        process
            .respond_control(control_success(&pending.request_id, &response))
            .await?;
        if decision == ApprovalDecision::CancelTurn {
            let _ = process.interrupt().await;
        }
        Ok(())
    }
}

#[async_trait]
#[allow(clippy::too_many_lines)]
impl AgentProvider for ClaudeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    fn supports_turn_mode(&self, mode: TurnExecutionMode) -> bool {
        mode == TurnExecutionMode::ReadWrite
    }

    async fn probe(&self) -> Result<ProviderHealth, ProviderError> {
        self.probe_static().await
    }

    async fn ensure_session(
        &self,
        context: &SessionContext,
    ) -> Result<NativeSession, ProviderError> {
        let installation = self.installation().await?;
        if !installation.supports_stream_json() {
            return Err(ProviderError::Incompatible(
                "Claude binary lacks required stream-json flags".to_owned(),
            ));
        }
        let existing = self
            .sessions
            .read()
            .await
            .get(&context.unified_session_id)
            .cloned();
        let native_session_id = existing
            .clone()
            .unwrap_or_else(|| context.unified_session_id.to_string());
        let process = self
            .process_for(
                &native_session_id,
                &context.workspace_root,
                existing.is_some(),
            )
            .await?;
        self.sessions
            .write()
            .await
            .insert(context.unified_session_id, native_session_id.clone());
        self.contexts
            .write()
            .await
            .insert(native_session_id.clone(), context.workspace_root.clone());
        let init = process.wait_init(self.config.init_timeout).await;
        Ok(NativeSession {
            id: ProviderSessionId::new(),
            provider: ProviderKind::Claude,
            native_session_id,
            native_version: init
                .as_ref()
                .and_then(|init| init.version.clone())
                .or(Some(installation.version.clone())),
            capabilities: self.capabilities(init.as_ref(), &installation),
        })
    }

    async fn restore_session(
        &self,
        context: &SessionContext,
        persisted: NativeSession,
    ) -> Result<NativeSession, ProviderError> {
        validate_session(&persisted)?;
        self.attach_native_session(
            context.unified_session_id,
            persisted.native_session_id.clone(),
            context.workspace_root.clone(),
        )
        .await?;
        Ok(persisted)
    }

    async fn attach_existing_session(
        &self,
        context: &SessionContext,
        native_session_id: &str,
    ) -> Result<NativeSession, ProviderError> {
        validate_native_session_id(native_session_id)?;
        let installation = self.installation().await?;
        if !installation.supports_stream_json() {
            return Err(ProviderError::Incompatible(
                "Claude binary lacks required stream-json flags".to_owned(),
            ));
        }

        // A resume control handshake is provider-owned and does not invoke a
        // model. Fail closed if this Claude version defers `system/init`, since
        // agentctl cannot then prove that the requested transcript exists.
        let process = self
            .spawn_process(native_session_id, &context.workspace_root, true)
            .await?;
        let init = process.wait_init(self.config.init_timeout).await;
        let shutdown = process.shutdown().await;
        let init = init.ok_or_else(|| {
            ProviderError::Incompatible(
                "Claude resume did not emit system/init; refusing an unverified attachment"
                    .to_owned(),
            )
        })?;
        shutdown?;
        if init.session_id != native_session_id {
            return Err(ProviderError::Protocol(format!(
                "Claude resumed unexpected session {}",
                init.session_id
            )));
        }
        self.attach_native_session(
            context.unified_session_id,
            native_session_id,
            context.workspace_root.clone(),
        )
        .await?;
        let capabilities = self.capabilities(Some(&init), &installation);
        let native_version = init.version.clone().or(Some(installation.version.clone()));
        Ok(NativeSession {
            id: ProviderSessionId::new(),
            provider: ProviderKind::Claude,
            native_session_id: native_session_id.to_owned(),
            native_version,
            capabilities,
        })
    }

    async fn sync_context(
        &self,
        session: &NativeSession,
        batch: SyncBatch,
    ) -> Result<SyncReceipt, ProviderError> {
        validate_session(session)?;
        let key = (
            session.native_session_id.clone(),
            batch.through_seq_inclusive,
            batch.projection_version,
        );
        if let Some(native_receipt) = self.applied_syncs.lock().await.get(&key).cloned() {
            return Ok(SyncReceipt {
                through_seq: batch.through_seq_inclusive,
                projection_version: batch.projection_version,
                native_receipt,
            });
        }
        let handoff = build_handoff(&batch);
        if handoff.is_empty() {
            self.applied_syncs.lock().await.insert(key, None);
            return Ok(SyncReceipt {
                through_seq: batch.through_seq_inclusive,
                projection_version: batch.projection_version,
                native_receipt: None,
            });
        }
        let cwd = self
            .contexts
            .read()
            .await
            .get(&session.native_session_id)
            .cloned()
            .ok_or_else(|| {
                ProviderError::Protocol("Claude session has no workspace context".to_owned())
            })?;
        let receipt = match self.config.should_query {
            ShouldQuerySupport::Supported => {
                let process = self
                    .process_for(&session.native_session_id, &cwd, true)
                    .await?;
                process
                    .send(user_message(&session.native_session_id, &handoff, false))
                    .await?;
                "claude:should-query-false"
            }
            ShouldQuerySupport::Unsupported | ShouldQuerySupport::Unknown
                if self.config.enable_hook_fallback
                    && self.config.user_prompt_submit_hook
                        == UserPromptSubmitHookSupport::Supported
                    && self.installation().await?.supports_flag("--settings") =>
            {
                let process = self
                    .process_for(&session.native_session_id, &cwd, true)
                    .await?;
                process.stage_hook_context(&handoff).await?;
                self.pending_hook_context
                    .lock()
                    .await
                    .insert(session.native_session_id.clone(), handoff);
                "claude:user-prompt-submit-hook"
            }
            _ if self.config.allow_prompt_capsule => {
                let mut pending = self.pending_capsules.lock().await;
                pending
                    .entry(session.native_session_id.clone())
                    .and_modify(|existing| {
                        existing.push_str("\n\n");
                        existing.push_str(&handoff);
                    })
                    .or_insert(handoff);
                "claude:next-prompt-capsule"
            }
            _ => {
                return Err(ProviderError::Incompatible(
                    "Claude exposes no validated context synchronization mechanism".to_owned(),
                ));
            }
        };
        self.applied_syncs
            .lock()
            .await
            .insert(key, Some(receipt.to_owned()));
        Ok(SyncReceipt {
            through_seq: batch.through_seq_inclusive,
            projection_version: batch.projection_version,
            native_receipt: Some(receipt.to_owned()),
        })
    }

    async fn run_turn(
        &self,
        session: &NativeSession,
        request: TurnRequest,
    ) -> Result<ProviderEventStream, ProviderError> {
        validate_session(session)?;
        if request.execution_mode == TurnExecutionMode::ReviewReadOnly {
            return Err(ProviderError::Incompatible(
                "Claude stream-json does not provide a per-turn read-only sandbox guarantee; review fails closed"
                    .to_owned(),
            ));
        }
        let process = self
            .process_for(&session.native_session_id, &request.cwd, true)
            .await?;
        let mut inbound = process.subscribe();
        if let Some(handoff) = self
            .pending_hook_context
            .lock()
            .await
            .get(&session.native_session_id)
            .cloned()
        {
            process.stage_hook_context(&handoff).await?;
        }
        let pending_capsule = self
            .pending_capsules
            .lock()
            .await
            .get(&session.native_session_id)
            .cloned();
        let prompt = pending_capsule.map_or_else(
            || request.prompt.clone(),
            |capsule| prompt_with_capsule(&capsule, &request.prompt),
        );
        process
            .send(user_message(&session.native_session_id, &prompt, true))
            .await?;
        let session_id = session.native_session_id.clone();
        self.active_turns
            .lock()
            .await
            .insert(session_id.clone(), request.turn_id.to_string());
        let process_for_task = Arc::clone(&process);
        let approvals = Arc::clone(&self.pending_approvals);
        let active_turns = Arc::clone(&self.active_turns);
        let pending_capsules = Arc::clone(&self.pending_capsules);
        let pending_hook_context = Arc::clone(&self.pending_hook_context);
        let (sender, receiver) = mpsc::channel(self.config.channel_capacity.max(1));
        tokio::spawn(async move {
            loop {
                match inbound.recv().await {
                    Ok(frame) => {
                        if !frame_matches_session(&frame, &session_id) {
                            continue;
                        }
                        let is_result = frame.get("type").and_then(Value::as_str) == Some("result");
                        let result_error = classify_result_error(&frame);
                        let is_process_error = frame.get("type").and_then(Value::as_str)
                            == Some("agentctl_process_error");
                        if is_permission_request(&frame) {
                            let raw = AgentEvent::ProviderSpecific {
                                provider: ProviderKind::Claude,
                                kind: "raw:control_request:can_use_tool".to_owned(),
                                payload: frame.clone(),
                            };
                            if sender.send(Ok(raw)).await.is_err() {
                                return;
                            }
                            let approval_id = ApprovalId::new();
                            let pending = pending_approval(
                                &session_id,
                                process_for_task.instance_id(),
                                &frame,
                            );
                            let approval = approval_request(approval_id, &pending, &request);
                            approvals.lock().await.insert(approval_id, pending);
                            if sender
                                .send(Ok(AgentEvent::ApprovalRequested { request: approval }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        } else {
                            for event in map_message(&frame) {
                                if sender.send(Ok(event)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        if is_result || is_process_error {
                            active_turns.lock().await.remove(&session_id);
                            if is_result && result_error.is_none() {
                                pending_capsules.lock().await.remove(&session_id);
                                pending_hook_context.lock().await.remove(&session_id);
                            }
                            if let Some(error) = result_error {
                                let _ = sender.send(Err(error)).await;
                            }
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let _ = sender
                            .send(Err(ProviderError::Protocol(format!(
                                "Claude event consumer lagged by {skipped} frames"
                            ))))
                            .await;
                        active_turns.lock().await.remove(&session_id);
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = sender
                            .send(Err(ProviderError::Process(
                                "Claude event stream closed".to_owned(),
                            )))
                            .await;
                        active_turns.lock().await.remove(&session_id);
                        return;
                    }
                }
            }
        });
        Ok(Box::pin(ReceiverStream::new(receiver)))
    }

    async fn interrupt(
        &self,
        session: &NativeSession,
        native_turn_id: &str,
    ) -> Result<(), ProviderError> {
        validate_session(session)?;
        let Some(process) = self
            .processes
            .read()
            .await
            .get(&session.native_session_id)
            .cloned()
        else {
            return Ok(());
        };
        let _active_turn_id = self
            .active_turns
            .lock()
            .await
            .get(&session.native_session_id)
            .cloned()
            .unwrap_or_else(|| native_turn_id.to_owned());
        if let Err(error) = process.interrupt().await {
            tracing::warn!(provider = %ProviderKind::Claude, %error, "control interrupt failed; killing process group root");
            process.shutdown().await?;
        }
        self.active_turns
            .lock()
            .await
            .remove(&session.native_session_id);
        Ok(())
    }

    async fn respond_approval(
        &self,
        session: &NativeSession,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), ProviderError> {
        validate_session(session)?;
        self.respond_to_approval(session, approval_id, decision)
            .await
    }

    async fn shutdown(&self) -> Result<(), ProviderError> {
        self.pending_approvals.lock().await.clear();
        self.active_turns.lock().await.clear();
        self.pending_capsules.lock().await.clear();
        self.pending_hook_context.lock().await.clear();
        let processes = std::mem::take(&mut *self.processes.write().await);
        let mut first_error = None;
        for process in processes.into_values() {
            if let Err(error) = process.shutdown().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }
}

fn validate_session(session: &NativeSession) -> Result<(), ProviderError> {
    if session.provider == ProviderKind::Claude {
        validate_native_session_id(&session.native_session_id)
    } else {
        Err(ProviderError::Protocol(format!(
            "Claude adapter received a {} session",
            session.provider
        )))
    }
}

fn validate_native_session_id(native_session_id: &str) -> Result<(), ProviderError> {
    Uuid::parse_str(native_session_id).map_err(|error| {
        ProviderError::Incompatible(format!("invalid Claude session UUID: {error}"))
    })?;
    Ok(())
}

fn build_handoff(batch: &SyncBatch) -> String {
    if let Some(handoff) = &batch.handoff {
        return handoff.clone();
    }
    let turns = batch
        .events
        .iter()
        .filter(|event| {
            let kind = event.kind.to_ascii_lowercase();
            kind.contains("user_prompt")
                || kind.contains("assistant_final")
                || kind.contains("checkpoint")
                || kind.contains("files_changed")
                || kind.contains("command_completed")
        })
        .map(|event| {
            json!({
                "seq": event.seq,
                "kind": event.kind,
                "payload": event.payload,
                "content_hash": event.content_hash
            })
        })
        .collect::<Vec<_>>();
    if turns.is_empty() {
        String::new()
    } else {
        format!(
            "<agent-handoff version=\"1\" through-seq=\"{}\">\n<handling-policy>{}</handling-policy>\n<history>{}</history>\n</agent-handoff>",
            batch.through_seq_inclusive,
            HANDOFF_POLICY,
            Value::Array(turns)
        )
    }
}

fn prompt_with_capsule(capsule: &str, prompt: &str) -> String {
    format!("{capsule}\n\n<current-user-request>\n{prompt}\n</current-user-request>")
}

fn frame_matches_session(frame: &Value, session_id: &str) -> bool {
    frame
        .get("session_id")
        .and_then(Value::as_str)
        .is_none_or(|value| value == session_id)
}

fn is_permission_request(frame: &Value) -> bool {
    frame.get("type").and_then(Value::as_str) == Some("control_request")
        && frame.pointer("/request/subtype").and_then(Value::as_str) == Some("can_use_tool")
}

fn pending_approval(session_id: &str, process_instance_id: Uuid, frame: &Value) -> PendingApproval {
    PendingApproval {
        native_session_id: session_id.to_owned(),
        process_instance_id: Some(process_instance_id),
        request_id: frame
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        tool_name: frame
            .pointer("/request/tool_name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        input: frame
            .pointer("/request/input")
            .cloned()
            .unwrap_or(Value::Null),
        suggestions: frame.pointer("/request/permission_suggestions").cloned(),
        tool_use_id: frame
            .pointer("/request/tool_use_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    }
}

fn approval_request(
    id: ApprovalId,
    pending: &PendingApproval,
    turn: &TurnRequest,
) -> ApprovalRequest {
    let tool = pending.tool_name.as_str();
    let action = if matches!(tool, "Bash" | "Shell" | "Terminal") {
        ApprovalAction::Command
    } else if matches!(tool, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
        ApprovalAction::FileChange
    } else if tool.starts_with("mcp__") {
        let mut parts = tool.splitn(3, "__");
        let _ = parts.next();
        ApprovalAction::McpTool {
            server: parts.next().unwrap_or("unknown").to_owned(),
            tool: parts.next().unwrap_or("unknown").to_owned(),
        }
    } else if matches!(tool, "WebFetch" | "WebSearch") {
        ApprovalAction::Network {
            host: pending
                .input
                .get("url")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        }
    } else {
        ApprovalAction::Permission {
            name: tool.to_owned(),
        }
    };
    let command = pending
        .input
        .get("command")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let files = ["file_path", "path", "notebook_path"]
        .into_iter()
        .filter_map(|key| pending.input.get(key).and_then(Value::as_str))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    ApprovalRequest {
        id,
        provider: ProviderKind::Claude,
        session_id: turn.session_id,
        turn_id: turn.turn_id,
        action,
        risk: if command.is_some() || !files.is_empty() {
            RiskLevel::High
        } else {
            RiskLevel::Medium
        },
        cwd: pending
            .input
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from),
        command,
        files,
        reason: None,
    }
}

fn permission_response(pending: &PendingApproval, decision: ApprovalDecision) -> Value {
    match decision {
        ApprovalDecision::AllowOnce => json!({
            "behavior": "allow",
            "updatedInput": pending.input,
            "toolUseID": pending.tool_use_id,
            "decisionClassification": "user_temporary"
        }),
        ApprovalDecision::AllowSession => {
            let mut response = json!({
                "behavior": "allow",
                "updatedInput": pending.input,
                "decisionClassification": "user_permanent"
            });
            if let Some(suggestions) = &pending.suggestions {
                response["updatedPermissions"] = suggestions.clone();
            }
            response
        }
        ApprovalDecision::Deny => json!({
            "behavior": "deny",
            "message": "Denied by user through agentctl",
            "interrupt": false,
            "decisionClassification": "user_reject"
        }),
        ApprovalDecision::CancelTurn => json!({
            "behavior": "deny",
            "message": "Turn cancelled by user through agentctl",
            "interrupt": true,
            "decisionClassification": "user_reject"
        }),
    }
}

async fn observes_model_activity(
    inbound: &mut broadcast::Receiver<Value>,
    session_id: &str,
    duration: Duration,
) -> Result<bool, ProviderError> {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let frame = match tokio::time::timeout(remaining, inbound.recv()).await {
            Err(_) => return Ok(false),
            Ok(Ok(frame)) => frame,
            Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                return Err(ProviderError::Protocol(format!(
                    "Claude capability probe lagged by {skipped} frames"
                )));
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Err(ProviderError::Process(
                    "Claude capability probe stream closed".to_owned(),
                ));
            }
        };
        if !frame_matches_session(&frame, session_id) {
            continue;
        }
        let frame_type = frame.get("type").and_then(Value::as_str);
        let requesting = frame_type == Some("system")
            && frame.get("subtype").and_then(Value::as_str) == Some("status")
            && frame.get("status").and_then(Value::as_str) == Some("requesting");
        if requesting || matches!(frame_type, Some("assistant" | "stream_event" | "result")) {
            return Ok(true);
        }
        if frame_type == Some("agentctl_process_error") {
            return Err(ProviderError::Process(
                frame
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Claude probe process failed")
                    .to_owned(),
            ));
        }
    }
}

async fn wait_probe_result(
    inbound: &mut broadcast::Receiver<Value>,
    session_id: &str,
) -> Result<(String, usize), ProviderError> {
    tokio::time::timeout(Duration::from_secs(180), async {
        let mut result_messages = 0;
        loop {
            let frame = inbound.recv().await.map_err(|error| {
                ProviderError::Process(format!("Claude probe stream closed: {error}"))
            })?;
            if !frame_matches_session(&frame, session_id) {
                continue;
            }
            if frame.get("type").and_then(Value::as_str) == Some("agentctl_process_error") {
                return Err(ProviderError::Process(
                    frame
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Claude probe process failed")
                        .to_owned(),
                ));
            }
            if frame.get("type").and_then(Value::as_str) == Some("result") {
                result_messages += 1;
                if frame.get("subtype").and_then(Value::as_str) != Some("success") {
                    return Err(ProviderError::Protocol(format!(
                        "Claude capability probe failed: {}",
                        frame.get("errors").cloned().unwrap_or(Value::Null)
                    )));
                }
                return Ok((
                    frame
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    result_messages,
                ));
            }
        }
    })
    .await
    .map_err(|_| ProviderError::Process("Claude capability probe timed out".to_owned()))?
}

#[cfg(test)]
mod tests {
    use super::{
        ClaudeAdapter, ClaudeConfig, PendingApproval, ShouldQuerySupport, build_handoff,
        permission_response, validate_native_session_id,
    };
    use agentctl_core::{
        AgentProvider, ApprovalDecision, NativeSession, ProviderKind, ProviderSessionId, SyncBatch,
        TurnExecutionMode,
    };
    use serde_json::{Value, json};
    use std::{collections::BTreeMap, path::PathBuf};

    #[test]
    fn supplied_handoff_is_not_rewritten() {
        let handoff = build_handoff(&SyncBatch {
            from_seq_exclusive: 1,
            through_seq_inclusive: 2,
            projection_version: 1,
            events: Vec::new(),
            handoff: Some("<agent-handoff/>".to_owned()),
        });
        assert_eq!(handoff, "<agent-handoff/>");
    }

    #[tokio::test]
    async fn repeated_empty_sync_preserves_its_original_receipt_without_spawning_claude() {
        let adapter = ClaudeAdapter::new("binary-must-not-run", None, None);
        let session = NativeSession {
            id: ProviderSessionId::new(),
            provider: ProviderKind::Claude,
            native_session_id: uuid::Uuid::new_v4().to_string(),
            native_version: Some("test".to_owned()),
            capabilities: BTreeMap::new(),
        };
        let batch = SyncBatch {
            from_seq_exclusive: 0,
            through_seq_inclusive: 7,
            projection_version: 3,
            events: Vec::new(),
            handoff: None,
        };
        let first = adapter.sync_context(&session, batch.clone()).await.unwrap();
        let repeated = adapter.sync_context(&session, batch).await.unwrap();
        assert_eq!(first.through_seq, 7);
        assert_eq!(first.native_receipt, None);
        assert_eq!(repeated.native_receipt, first.native_receipt);
    }

    #[tokio::test]
    async fn unprobed_hook_fails_closed_to_capsule_and_preserves_deferred_receipt() {
        let adapter = ClaudeAdapter::from_config(ClaudeConfig {
            binary: PathBuf::from("binary-must-not-run"),
            should_query: ShouldQuerySupport::Unsupported,
            enable_hook_fallback: true,
            allow_prompt_capsule: true,
            ..ClaudeConfig::default()
        });
        let native_session_id = uuid::Uuid::new_v4().to_string();
        let workspace = tempfile::tempdir().unwrap();
        adapter
            .attach_native_session(
                agentctl_core::UnifiedSessionId::new(),
                native_session_id.clone(),
                workspace.path(),
            )
            .await
            .unwrap();
        let session = NativeSession {
            id: ProviderSessionId::new(),
            provider: ProviderKind::Claude,
            native_session_id: native_session_id.clone(),
            native_version: Some("test".to_owned()),
            capabilities: BTreeMap::new(),
        };
        let batch = SyncBatch {
            from_seq_exclusive: 0,
            through_seq_inclusive: 9,
            projection_version: 4,
            events: Vec::new(),
            handoff: Some("<agent-handoff/>".to_owned()),
        };

        let first = adapter.sync_context(&session, batch.clone()).await.unwrap();
        let repeated = adapter.sync_context(&session, batch).await.unwrap();

        assert_eq!(
            first.native_receipt.as_deref(),
            Some("claude:next-prompt-capsule")
        );
        assert_eq!(repeated.native_receipt, first.native_receipt);
        assert_eq!(
            adapter
                .pending_capsules
                .lock()
                .await
                .get(&native_session_id)
                .map(String::as_str),
            Some("<agent-handoff/>")
        );
    }

    #[test]
    fn denial_fails_closed() {
        let pending = PendingApproval {
            native_session_id: "session".to_owned(),
            process_instance_id: None,
            request_id: "request".to_owned(),
            tool_name: "Bash".to_owned(),
            input: json!({"command": "rm -rf /tmp/example"}),
            suggestions: None,
            tool_use_id: Some("tool-1".to_owned()),
        };
        let response = permission_response(&pending, ApprovalDecision::Deny);
        assert_eq!(response["behavior"], "deny");
        assert_eq!(response["interrupt"], false);
        assert_ne!(response, Value::Null);

        let cancelled = permission_response(&pending, ApprovalDecision::CancelTurn);
        assert_eq!(cancelled["behavior"], "deny");
        assert_eq!(cancelled["interrupt"], true);
    }

    #[test]
    fn malformed_native_ids_and_read_only_reviews_fail_closed() {
        assert!(validate_native_session_id("not-a-uuid").is_err());
        assert!(validate_native_session_id("01900000-0000-7000-8000-000000000000").is_ok());
        let adapter = ClaudeAdapter::from_config(ClaudeConfig::default());
        assert!(adapter.supports_turn_mode(TurnExecutionMode::ReadWrite));
        assert!(!adapter.supports_turn_mode(TurnExecutionMode::ReviewReadOnly));
    }
}
