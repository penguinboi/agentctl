//! Canonical persistence for provider-owned Claude Code interactive sessions.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command as StdCommand,
};

use agentctl_core::{
    CanonicalEvent, EventId, EventVisibility, ProviderHealth, ProviderKind, ProviderSessionId,
    ProviderStatus, RateLimitSnapshot, SessionStatus, SideEffectState, TurnId, TurnStatus,
    UnifiedSession, UnifiedSessionId,
};
use agentctl_provider_claude::native_hooks::{
    NativeHookCommand, NativeHookEvent, SessionEndReason, SessionStartSource, StopFailureKind,
    merge_interactive_hook_settings, parse_hook_payload_for_session,
    user_prompt_submit_additional_context,
};
use agentctl_storage::{
    AgentctlStore, NativeHandoffRecord, NativeHandoffState, NativeLaunchRecord, NativeLaunchState,
    ProviderSessionRecord, RawProviderEvent, TurnRecord, canonical_content_hash,
};
use agentctl_telemetry::PayloadGuard;
use agentctl_transcript::HandoffCapsule;
use agentctl_workspace::WorkspaceIdentity;
use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use crate::{args::ClaudeHookArgs, config::Config, native, paths::AgentctlPaths};

const MAX_STDIN_BYTES: u64 = 8 * 1024 * 1024 + 1;
const MAX_HANDOFF_CONTEXT_CHARS: usize = 9_500;
const EVENT_PAGE_SIZE: usize = 512;
const TOOL_SUMMARY_BYTES: usize = 8 * 1024;
const CLAUDE_MATERIALIZED_KEY: &str = "native_materialized";
const CLAUDE_LEGACY_STARTED_KEY: &str = "native_started";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ClaudeHandoffStage {
    version: u32,
    session_id: UnifiedSessionId,
    native_session_id: String,
    through_seq: u64,
    capsule: String,
    digest: String,
}

impl ClaudeHandoffStage {
    fn new(
        session_id: UnifiedSessionId,
        native_session_id: String,
        through_seq: u64,
        capsule: String,
    ) -> Self {
        let digest = stage_digest(session_id, &native_session_id, through_seq, &capsule);
        Self {
            version: 1,
            session_id,
            native_session_id,
            through_seq,
            capsule,
            digest,
        }
    }
}

/// Files and native session identity required to open the provider-owned CLI.
#[derive(Clone, Debug, Serialize)]
pub struct PreparedClaudeNative {
    pub launch_id: Uuid,
    pub native_session_id: String,
    pub resume: bool,
    pub settings_path: PathBuf,
    pub handoff_through: Option<u64>,
}

/// Prepares a native Claude Code launch without spawning it.
#[allow(clippy::too_many_lines)]
pub fn prepare_native_claude(
    store: &AgentctlStore,
    paths: &AgentctlPaths,
    config: &Config,
    session: &UnifiedSession,
    identity: &WorkspaceIdentity,
) -> Result<PreparedClaudeNative> {
    ensure!(
        identity.execution_root() == session.workspace_path,
        "native Claude workspace identity no longer matches the canonical session"
    );
    let resume = reconcile_claude_materialization(store, session.id)?;
    let existing = store.provider_session(session.id, &ProviderKind::Claude)?;
    let native_session_id = existing.as_ref().map_or_else(
        || Uuid::new_v4().to_string(),
        |record| record.native_session_id.clone(),
    );
    let previous_cursor = existing.as_ref().map_or(0, |record| record.last_synced_seq);
    if let Some(provider) = &existing {
        ensure!(
            store.pending_sync_intents(provider.id)?.is_empty(),
            "Claude projection has an uncertain send intent; run `agentctl repair --rebuild-projections`"
        );
    }

    let runtime_dir = paths.home.join("native-runtime");
    create_private_dir(&runtime_dir)?;
    ensure!(
        !config.providers.claude_binary.trim().is_empty(),
        "Claude binary cannot be empty"
    );
    let claude_version = ensure_exec_hook_support(&config.providers.claude_binary)?;
    let latest = store.next_seq(session.id)?.saturating_sub(1);
    let handoff = (latest > previous_cursor)
        .then(|| {
            build_handoff_stage(
                store,
                session.id,
                &native_session_id,
                previous_cursor,
                latest,
            )
        })
        .transpose()?
        .flatten();
    let launch_id = Uuid::new_v4();
    let executable = std::env::current_exe().context("failed to locate agentctl executable")?;
    let hook_args = vec![
        "--home".to_owned(),
        utf8_path(&paths.home, "agentctl home")?,
        "hook".to_owned(),
        "claude".to_owned(),
        "--session".to_owned(),
        session.id.to_string(),
        "--expected-native-session".to_owned(),
        native_session_id.clone(),
        "--launch-id".to_owned(),
        launch_id.to_string(),
    ];
    let command = NativeHookCommand::new(executable, hook_args, 10)?;
    let settings = merge_interactive_hook_settings(None, &command)?;
    let settings_path =
        runtime_dir.join(format!("claude-{}-{launch_id}-settings.json", session.id));

    let now = Utc::now();
    let mut provider = if let Some(provider) = existing {
        provider
    } else {
        let record = ProviderSessionRecord {
            id: ProviderSessionId::new(),
            unified_session_id: session.id,
            provider: ProviderKind::Claude,
            native_session_id: native_session_id.clone(),
            native_version: None,
            last_synced_seq: 0,
            status: ProviderStatus::Unknown,
            reset_at: None,
            capabilities: BTreeMap::from([("native_hooks".to_owned(), true)]),
            metadata: json!({
                "mode": "native_interactive",
                "native_materialized": false,
            }),
            created_at: now,
            updated_at: now,
        };
        store.upsert_provider_session(&record)?;
        record
    };
    provider.native_version = Some(claude_version);
    provider.updated_at = Utc::now();
    store.upsert_provider_session(&provider)?;
    if latest > provider.last_synced_seq && handoff.is_none() {
        // Only internal/non-projectable events were pending.
        store.advance_provider_cursor(provider.id, latest)?;
    }
    store.start_native_launch(&NativeLaunchRecord {
        id: launch_id,
        session_id: session.id,
        provider: ProviderKind::Claude,
        native_session_id: native_session_id.clone(),
        workspace_lease_key: identity.lease_key.clone(),
        child_pid: None,
        state: NativeLaunchState::Started,
        exit_code: None,
        error: None,
        metadata: json!({}),
        started_at: now,
        updated_at: now,
    })?;
    let prepared = (|| -> Result<()> {
        if let Some(stage) = &handoff {
            store.stage_native_handoff(&NativeHandoffRecord {
                launch_id,
                provider_session_id: provider.id,
                session_id: session.id,
                native_session_id: native_session_id.clone(),
                through_seq: stage.through_seq,
                capsule: stage.capsule.clone(),
                content_digest: stage.digest.clone(),
                state: NativeHandoffState::Staged,
                created_at: now,
                updated_at: now,
            })?;
        }
        write_private_json(&settings_path, &settings)
    })();
    if let Err(error) = prepared {
        let safe = config
            .payload_guard()?
            .process_text(&error.to_string())
            .unwrap_or_else(|_| "native preparation failed".to_owned());
        let _ = store.update_native_launch(
            launch_id,
            NativeLaunchState::Failed,
            None,
            Some(&safe),
            Utc::now(),
        );
        return Err(error);
    }
    Ok(PreparedClaudeNative {
        launch_id,
        native_session_id,
        resume,
        settings_path,
        handoff_through: handoff.map(|stage| stage.through_seq),
    })
}

/// Opens Claude Code's native interactive CLI with lifecycle capture enabled.
#[allow(clippy::too_many_lines)]
pub async fn run_native_claude(
    store: &AgentctlStore,
    paths: &AgentctlPaths,
    config: &Config,
    session: &UnifiedSession,
    identity: &WorkspaceIdentity,
    native_args: &[OsString],
    explicit_provider: bool,
) -> Result<Value> {
    let prepared = prepare_native_claude(store, paths, config, session, identity)?;
    if let Err(error) = store.update_session_routing(
        session.id,
        Some(&ProviderKind::Claude),
        if explicit_provider {
            "manual"
        } else {
            &session.routing_policy
        },
        SessionStatus::Active,
        Utc::now(),
    ) {
        let _ = remove_regular_file(&prepared.settings_path);
        let _ = store.update_native_launch(
            prepared.launch_id,
            NativeLaunchState::Failed,
            None,
            Some("failed to activate prepared Claude launch"),
            Utc::now(),
        );
        return Err(error.into());
    }
    let launched = native::launch_claude_with_spawn(
        &config.providers.claude_binary,
        identity.execution_root(),
        &prepared.native_session_id,
        prepared.resume,
        &prepared.settings_path,
        native_args,
        |spawn| {
            store
                .record_native_launch_pid(prepared.launch_id, spawn.pid)
                .map_err(Into::into)
        },
    )
    .await;
    let _ = remove_regular_file(&prepared.settings_path);
    let native_exit = match launched {
        Ok(exit) => exit,
        Err(error) => {
            mark_claude_launch_after_error(
                store,
                config,
                prepared.launch_id,
                &error,
                native::error_happened_after_spawn(&error),
                None,
            )?;
            return Err(error);
        }
    };
    let finalized = (|| -> Result<Value> {
        record_native_child_exit(
            store,
            prepared.launch_id,
            native_exit.propagated_exit_code(),
        )?;
        let provider = store
            .provider_session(session.id, &ProviderKind::Claude)?
            .context("Claude provider session disappeared after native exit")?;
        let handoff = store.native_handoff(prepared.launch_id)?;
        if handoff
            .as_ref()
            .is_some_and(|handoff| handoff.state == NativeHandoffState::Delivering)
        {
            store.mark_native_handoff_uncertain(prepared.launch_id)?;
        }
        if let Some(through) = prepared.handoff_through {
            let handoff = handoff.context("prepared Claude handoff journal disappeared")?;
            match handoff.state {
                NativeHandoffState::Staged => {
                    // No prompt was submitted. The delta remains pending and
                    // will be staged again for the next native launch.
                }
                NativeHandoffState::Delivered => ensure!(
                    provider.last_synced_seq >= through,
                    "Claude handoff was delivered without advancing its projection cursor"
                ),
                NativeHandoffState::Delivering | NativeHandoffState::Uncertain => {
                    bail!("Claude handoff delivery is uncertain; run agentctl repair")
                }
            }
        }
        let unfinished = active_claude_turns(store, session.id)?;
        if !unfinished.is_empty() {
            for turn in &unfinished {
                store.update_turn_state(
                    turn.id,
                    TurnStatus::Uncertain,
                    turn.side_effect_state.observe(SideEffectState::Possible),
                    None,
                    Utc::now(),
                )?;
            }
            bail!(
                "Claude exited with {} unfinished native turn(s); run agentctl repair before switching",
                unfinished.len()
            );
        }
        ensure!(
            store
                .native_launch(prepared.launch_id)?
                .is_some_and(|launch| launch.state == NativeLaunchState::Captured),
            "Claude exited without a captured SessionEnd hook; run agentctl repair before switching"
        );
        store.update_native_launch(
            prepared.launch_id,
            NativeLaunchState::Captured,
            native_exit.propagated_exit_code(),
            None,
            Utc::now(),
        )?;
        Ok(json!({
            "mode": "native_cli",
            "session_id": session.id,
            "launch_id": prepared.launch_id,
            "native_exit": native_exit,
            "capture": {
                "provider": "claude",
                "native_hooks": true,
                "synced_through": provider.last_synced_seq,
            }
        }))
    })();
    match finalized {
        Ok(report) => Ok(report),
        Err(error) => {
            mark_claude_launch_after_error(
                store,
                config,
                prepared.launch_id,
                &error,
                true,
                native_exit.propagated_exit_code(),
            )?;
            Err(error)
        }
    }
}

fn mark_claude_launch_after_error(
    store: &AgentctlStore,
    config: &Config,
    launch_id: Uuid,
    error: &anyhow::Error,
    happened_after_spawn: bool,
    exit_code: Option<i32>,
) -> Result<()> {
    let launch = store
        .native_launch(launch_id)?
        .context("native Claude launch journal disappeared")?;
    let target = if happened_after_spawn || launch.child_pid.is_some() {
        NativeLaunchState::Uncertain
    } else {
        NativeLaunchState::Failed
    };
    if launch.state == target
        || launch.state == NativeLaunchState::Uncertain
        || matches!(
            launch.state,
            NativeLaunchState::Captured | NativeLaunchState::Failed
        )
    {
        return Ok(());
    }
    let safe = config
        .payload_guard()
        .ok()
        .and_then(|guard| guard.process_text(&format!("{error:#}")).ok())
        .unwrap_or_else(|| "native Claude launch failed; inspect debug logs".to_owned());
    store.update_native_launch(
        launch_id,
        target,
        exit_code.or(launch.exit_code),
        Some(&safe),
        Utc::now(),
    )?;
    Ok(())
}

/// Records an ordinary child exit without manufacturing capture evidence.
/// Returns true only when `SessionEnd` had already committed `CaptureReady`.
fn record_native_child_exit(
    store: &AgentctlStore,
    launch_id: Uuid,
    exit_code: Option<i32>,
) -> Result<bool> {
    let launch = store
        .native_launch(launch_id)?
        .context("native Claude launch journal disappeared")?;
    match launch.state {
        NativeLaunchState::Started => {
            store.update_native_launch(
                launch_id,
                NativeLaunchState::Exited,
                exit_code,
                None,
                Utc::now(),
            )?;
            Ok(false)
        }
        NativeLaunchState::CaptureReady => {
            store.update_native_launch(
                launch_id,
                NativeLaunchState::Captured,
                exit_code,
                None,
                Utc::now(),
            )?;
            Ok(true)
        }
        NativeLaunchState::Captured => Ok(true),
        _ => bail!("native Claude launch was invalidated by a hook; run agentctl repair"),
    }
}

/// Internal command invoked by Claude Code's documented command hooks.
pub async fn handle_claude_hook(
    _paths: &AgentctlPaths,
    config: &Config,
    store: &AgentctlStore,
    args: ClaudeHookArgs,
) -> Result<()> {
    let launch_id = args.launch_id.parse::<Uuid>();
    let result = handle_claude_hook_inner(config, store, &args).await;
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Ok(launch_id) = launch_id {
                if store
                    .native_handoff(launch_id)
                    .ok()
                    .flatten()
                    .is_some_and(|handoff| handoff.state == NativeHandoffState::Delivering)
                {
                    let _ = store.mark_native_handoff_uncertain(launch_id);
                }
                let safe = config
                    .payload_guard()?
                    .process_text(&error.to_string())
                    .unwrap_or_else(|_| "native hook bridge failed".to_owned());
                if store
                    .native_launch(launch_id)
                    .ok()
                    .flatten()
                    .is_some_and(|launch| launch.state == NativeLaunchState::Started)
                {
                    let _ = store.update_native_launch(
                        launch_id,
                        NativeLaunchState::Uncertain,
                        None,
                        Some(&safe),
                        Utc::now(),
                    );
                }
            }
            tracing::error!(%error, "native Claude hook failed closed");
            write_hook_json(&json!({
                "continue": false,
                "stopReason": "agentctl bridge failed; exit the native CLI and run agentctl repair"
            }))
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_claude_hook_inner(
    config: &Config,
    store: &AgentctlStore,
    args: &ClaudeHookArgs,
) -> Result<()> {
    ensure!(
        args.handoff.is_none(),
        "file-based Claude handoff arguments are not trusted"
    );
    let session_id: UnifiedSessionId = args
        .session
        .parse()
        .context("invalid canonical session id in Claude hook")?;
    let session = store
        .get_session(session_id)?
        .context("canonical session for Claude hook does not exist")?;
    let mut bytes = Vec::new();
    tokio::io::stdin()
        .take(MAX_STDIN_BYTES)
        .read_to_end(&mut bytes)
        .await
        .context("failed reading Claude hook stdin")?;
    ensure!(
        (bytes.len() as u64) < MAX_STDIN_BYTES,
        "Claude hook stdin exceeds the supported payload limit"
    );
    let launch_id: Uuid = args
        .launch_id
        .parse()
        .context("invalid native launch id in Claude hook")?;
    let launch =
        validate_launch_binding(store, &session, launch_id, &args.expected_native_session)?;
    let event = match parse_hook_payload_for_session(&bytes, &args.expected_native_session) {
        Ok(event) => event,
        Err(agentctl_provider_claude::native_hooks::NativeHookError::SessionMismatch {
            ..
        }) => {
            let untrusted = agentctl_provider_claude::native_hooks::parse_hook_payload(&bytes)?;
            record_session_divergence(
                store,
                &config.payload_guard()?,
                &session,
                &untrusted,
                &serde_json::from_slice(&bytes)?,
            );
            if store
                .native_handoff(launch_id)?
                .is_some_and(|handoff| handoff.state == NativeHandoffState::Delivering)
            {
                let _ = store.mark_native_handoff_uncertain(launch_id);
            }
            if matches!(
                launch.state,
                NativeLaunchState::Started
                    | NativeLaunchState::CaptureReady
                    | NativeLaunchState::Captured
            ) {
                let _ = store.update_native_launch(
                    launch_id,
                    NativeLaunchState::Uncertain,
                    None,
                    Some("Claude changed native session identity"),
                    Utc::now(),
                );
            }
            return write_hook_json(&session_mismatch_output(&untrusted));
        }
        Err(error) => return Err(error.into()),
    };
    if is_native_session_reset(&event) {
        validate_workspace(&session, event.common().cwd.as_path())?;
        let raw: Value = serde_json::from_slice(&bytes)?;
        let guard = config.payload_guard()?;
        let normalized = sanitize_hook_json(&guard, &serde_json::to_value(&event)?)?;
        let raw = sanitize_hook_json(&guard, &raw)?;
        let base = digest(format!("native_clear\0{launch_id}").as_bytes());
        let key = stage_key(&base, None, "native_clear_blocked");
        append_hook_event(
            store,
            session.id,
            None,
            "native_session_reset_blocked",
            EventVisibility::Internal,
            with_hook_metadata(
                json!({
                    "launch_id": launch_id,
                    "source": "clear",
                    "normalized_hook_digest": digest(&serde_json::to_vec(&normalized)?),
                }),
                "session_start",
                &base,
                &key,
            ),
            Some(("claude_hook_session_reset_blocked", &raw)),
        )?;
        store.update_native_launch(
            launch_id,
            NativeLaunchState::Uncertain,
            None,
            Some("Claude cleared the mapped native transcript"),
            Utc::now(),
        )?;
        return write_hook_json(&native_session_reset_output());
    }
    ensure!(
        launch.state == NativeLaunchState::Started
            || (launch.state == NativeLaunchState::CaptureReady
                && matches!(event, NativeHookEvent::SessionEnd(_))),
        "native Claude launch is not accepting hook events"
    );
    ensure_handoff_allows_event(store, launch_id, &event)?;
    validate_workspace(&session, event.common().cwd.as_path())?;
    let raw: Value = serde_json::from_slice(&bytes)?;
    let guard = config.payload_guard()?;
    let outcome = persist_hook_event(
        store,
        &guard,
        &session,
        &args.expected_native_session,
        launch_id,
        &event,
        &raw,
    )?;

    if matches!(event, NativeHookEvent::UserPromptSubmit(_)) {
        let mut stdout = std::io::stdout().lock();
        deliver_staged_handoff(
            store,
            session.id,
            &args.expected_native_session,
            launch_id,
            &mut stdout,
        )?;
    }
    tracing::debug!(
        session_id = %session.id,
        turn_id = ?outcome.turn_id,
        events_written = outcome.events_written,
        duplicate = outcome.duplicate,
        "captured native Claude hook"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct PersistOutcome {
    turn_id: Option<TurnId>,
    events_written: usize,
    duplicate: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HandoffDelivery {
    None,
    Delivered,
    ReceiptUncertain,
}

fn ensure_handoff_allows_event(
    store: &AgentctlStore,
    launch_id: Uuid,
    event: &NativeHookEvent,
) -> Result<()> {
    let Some(handoff) = store.native_handoff(launch_id)? else {
        return Ok(());
    };
    match handoff.state {
        NativeHandoffState::Delivered => Ok(()),
        NativeHandoffState::Staged
            if matches!(
                event,
                NativeHookEvent::SessionStart(_)
                    | NativeHookEvent::UserPromptSubmit(_)
                    | NativeHookEvent::SessionEnd(_)
            ) =>
        {
            Ok(())
        }
        NativeHandoffState::Staged => {
            bail!("Claude emitted effects before its staged handoff reached UserPromptSubmit")
        }
        NativeHandoffState::Delivering | NativeHandoffState::Uncertain => {
            bail!("Claude handoff delivery is uncertain; exit the native CLI and repair")
        }
    }
}

fn deliver_staged_handoff(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    native_session_id: &str,
    launch_id: Uuid,
    writer: &mut impl Write,
) -> Result<HandoffDelivery> {
    let Some(staged) = store.native_handoff(launch_id)? else {
        return Ok(HandoffDelivery::None);
    };
    match staged.state {
        NativeHandoffState::Delivered => return Ok(HandoffDelivery::None),
        NativeHandoffState::Staged => {}
        NativeHandoffState::Delivering | NativeHandoffState::Uncertain => {
            bail!("Claude handoff delivery is uncertain; exit the native CLI and repair")
        }
    }
    staged.validate_for_delivery(session_id, native_session_id)?;
    ensure!(
        staged.capsule.chars().count() <= MAX_HANDOFF_CONTEXT_CHARS,
        "Claude handoff exceeds the native hook context budget; compact the session"
    );
    ensure!(
        staged.content_digest
            == stage_digest(
                staged.session_id,
                &staged.native_session_id,
                staged.through_seq,
                &staged.capsule,
            ),
        "journaled Claude handoff digest is invalid"
    );
    let output = user_prompt_submit_additional_context(&staged.capsule)?;
    store.begin_native_handoff_delivery(launch_id)?;
    if let Err(error) = write_hook_json_to(writer, &output) {
        let _ = store.mark_native_handoff_uncertain(launch_id);
        return Err(error);
    }
    if let Err(error) = store.complete_native_handoff_delivery(launch_id, Utc::now()) {
        let _ = store.mark_native_handoff_uncertain(launch_id);
        let _ = store.update_native_launch(
            launch_id,
            NativeLaunchState::Uncertain,
            None,
            Some("Claude received handoff output but the delivery receipt was not committed"),
            Utc::now(),
        );
        tracing::error!(%error, %launch_id, "handoff was written to Claude but cursor commit failed");
        // Claude received the context. Returning another JSON document would
        // corrupt hook stdout; the uncertain journal blocks future transfer.
        return Ok(HandoffDelivery::ReceiptUncertain);
    }
    Ok(HandoffDelivery::Delivered)
}

#[allow(clippy::too_many_lines)]
fn persist_hook_event(
    store: &AgentctlStore,
    guard: &PayloadGuard,
    session: &UnifiedSession,
    expected_native_session_id: &str,
    launch_id: Uuid,
    event: &NativeHookEvent,
    raw: &Value,
) -> Result<PersistOutcome> {
    ensure!(
        event.session_id() == expected_native_session_id,
        "native Claude hook session mismatch"
    );
    let normalized = sanitize_hook_json(guard, &serde_json::to_value(event)?)?;
    let raw = sanitize_hook_json(guard, raw)?;
    match event {
        NativeHookEvent::SessionStart(_start) => {
            let mut provider =
                ensure_provider_binding(store, session.id, expected_native_session_id, true)?;
            if let Some(metadata) = provider.metadata.as_object_mut() {
                metadata.insert(
                    "mode".to_owned(),
                    Value::String("native_interactive".to_owned()),
                );
            }
            provider.status = ProviderStatus::Ready;
            provider.updated_at = Utc::now();
            store.upsert_provider_session(&provider)?;
            store.update_provider_session_health(
                session.id,
                &ProviderKind::Claude,
                &ProviderStatus::Ready,
                None,
                Utc::now(),
            )?;
            let base =
                digest(format!("{}\0{launch_id}", hook_base_key(event, &normalized)).as_bytes());
            let key = stage_key(&base, None, "session_start");
            let inserted = append_hook_event(
                store,
                session.id,
                None,
                "native_session_started",
                EventVisibility::Internal,
                with_hook_metadata(
                    json!({
                        "launch_id": launch_id,
                        "native_session_digest": digest(expected_native_session_id.as_bytes()),
                        "source": normalized.pointer("/payload/source").cloned(),
                        "model": normalized.pointer("/payload/model").cloned(),
                        "agent_type": normalized.pointer("/payload/agent_type").cloned(),
                    }),
                    "session_start",
                    &base,
                    &key,
                ),
                Some(("claude_hook_session_start", &raw)),
            )?;
            store.update_session_state(
                session.id,
                Some(&ProviderKind::Claude),
                SessionStatus::Active,
                Utc::now(),
            )?;
            Ok(PersistOutcome {
                events_written: usize::from(inserted),
                duplicate: !inserted,
                ..PersistOutcome::default()
            })
        }
        NativeHookEvent::UserPromptSubmit(_prompt) => {
            ensure_provider_binding(store, session.id, expected_native_session_id, false)?;
            let safe_prompt = sanitized_string(&normalized, "/payload/prompt")?.to_owned();
            let base =
                digest(format!("{}\0{launch_id}", hook_base_key(event, &normalized)).as_bytes());
            let turn_id = deterministic_turn_id(session.id, &base);
            if let Some(turn) = active_claude_turn(store, session.id)? {
                let key = stage_key(&base, Some(turn.id), "prompt");
                if turn.id == turn_id {
                    let inserted = append_hook_event(
                        store,
                        session.id,
                        Some(turn.id),
                        "user_prompt",
                        EventVisibility::User,
                        with_hook_metadata(
                            json!({"text": safe_prompt}),
                            "user_prompt_submit",
                            &base,
                            &key,
                        ),
                        Some(("claude_hook_user_prompt_submit", &raw)),
                    )?;
                    mark_claude_materialized(store, session.id, "user_prompt_submit")?;
                    return Ok(PersistOutcome {
                        turn_id: Some(turn.id),
                        events_written: usize::from(inserted),
                        duplicate: !inserted,
                    });
                }
                bail!(
                    "Claude native session already has active turn {}; refusing another prompt",
                    turn.id
                );
            }
            let now = Utc::now();
            let turn = TurnRecord {
                id: turn_id,
                session_id: session.id,
                provider: Some(ProviderKind::Claude),
                // Allocated atomically with the first event below.
                prompt_seq: 1,
                status: TurnStatus::Running,
                side_effect_state: SideEffectState::None,
                native_turn_id: normalized
                    .pointer("/payload/common/prompt_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| Some(format!("claude-hook:{turn_id}"))),
                continuation: false,
                started_at: Some(now),
                completed_at: None,
                created_at: now,
                updated_at: now,
            };
            let key = stage_key(&base, Some(turn_id), "prompt");
            let payload = with_hook_metadata(
                json!({"text": safe_prompt}),
                "user_prompt_submit",
                &base,
                &key,
            );
            let raw = RawProviderEvent {
                event_id: deterministic_event_id(session.id, &format!("{key}\0raw")),
                session_id: session.id,
                turn_id: Some(turn_id),
                provider: ProviderKind::Claude,
                kind: "claude_hook_user_prompt_submit".to_owned(),
                content_hash: digest(&serde_json::to_vec(&raw)?),
                payload: raw,
                created_at: now,
            };
            let event = CanonicalEvent {
                schema_version: 1,
                session_id: session.id,
                seq: 0,
                event_id: deterministic_event_id(session.id, &key),
                turn_id: Some(turn_id),
                origin_provider: Some(ProviderKind::Claude),
                kind: "user_prompt".to_owned(),
                visibility: EventVisibility::User,
                content_hash: canonical_content_hash(
                    "user_prompt",
                    EventVisibility::User,
                    &payload,
                )?,
                payload,
                raw_event_id: Some(raw.event_id),
                created_at: now,
            };
            let expected_event = event.clone();
            let (_, stored_event, duplicate) =
                store.create_turn_with_allocated_event(turn, event, Some(&raw))?;
            if duplicate {
                validate_idempotent_event(store, &stored_event, &expected_event, Some(&raw))?;
            }
            mark_claude_materialized(store, session.id, "user_prompt_submit")?;
            Ok(PersistOutcome {
                turn_id: Some(turn_id),
                events_written: usize::from(!duplicate),
                duplicate,
            })
        }
        NativeHookEvent::PreToolUse(_tool) => persist_tool_started(
            store,
            session.id,
            launch_id,
            &normalized,
            &raw,
            sanitized_string(&normalized, "/payload/tool_use_id")?,
            sanitized_string(&normalized, "/payload/tool_name")?,
            normalized
                .pointer("/payload/tool_input")
                .unwrap_or(&Value::Null),
            event.common().cwd.as_path(),
        ),
        NativeHookEvent::PostToolUse(_tool) => persist_tool(
            store,
            session.id,
            launch_id,
            &normalized,
            &raw,
            sanitized_string(&normalized, "/payload/tool_use_id")?,
            sanitized_string(&normalized, "/payload/tool_name")?,
            normalized
                .pointer("/payload/tool_input")
                .unwrap_or(&Value::Null),
            normalized
                .pointer("/payload/tool_response")
                .unwrap_or(&Value::Null),
            true,
        ),
        NativeHookEvent::PostToolUseFailure(_tool) => persist_tool(
            store,
            session.id,
            launch_id,
            &normalized,
            &raw,
            sanitized_string(&normalized, "/payload/tool_use_id")?,
            sanitized_string(&normalized, "/payload/tool_name")?,
            normalized
                .pointer("/payload/tool_input")
                .unwrap_or(&Value::Null),
            normalized.pointer("/payload/error").unwrap_or(&Value::Null),
            false,
        ),
        NativeHookEvent::Stop(_stop) => persist_stop(
            store,
            session.id,
            launch_id,
            &normalized,
            &raw,
            sanitized_string(&normalized, "/payload/last_assistant_message")?,
        ),
        NativeHookEvent::StopFailure(failure) => persist_stop_failure(
            store,
            session.id,
            launch_id,
            &normalized,
            &raw,
            failure.error,
            normalized
                .pointer("/payload/error_details")
                .and_then(Value::as_str),
            normalized
                .pointer("/payload/last_assistant_message")
                .and_then(Value::as_str),
        ),
        NativeHookEvent::SessionEnd(end) => persist_session_end(
            store,
            session.id,
            expected_native_session_id,
            launch_id,
            &end.common.transcript_path,
            &normalized,
            &raw,
            end.reason,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn persist_tool_started(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    launch_id: Uuid,
    normalized: &Value,
    raw: &Value,
    tool_use_id: &str,
    tool_name: &str,
    input: &Value,
    cwd: &Path,
) -> Result<PersistOutcome> {
    let base = tool_hook_base(session_id, launch_id, tool_use_id);
    let Some(turn) = active_claude_turn(store, session_id)? else {
        if event_base_key_exists(store, session_id, &base)? {
            return Ok(PersistOutcome {
                duplicate: true,
                ..PersistOutcome::default()
            });
        }
        bail!("received Claude PreToolUse hook without an active native turn");
    };
    let encoded = serde_json::to_vec(input)?;
    let key = stage_key(&base, Some(turn.id), "tool_started");
    let mut written = usize::from(append_hook_event(
        store,
        session_id,
        Some(turn.id),
        "tool_started",
        EventVisibility::User,
        with_hook_metadata(
            json!({
                "type": "tool_started",
                "id": tool_use_id,
                "name": tool_name,
                "input": {
                    "summary": summarize_value(input, TOOL_SUMMARY_BYTES),
                    "digest": digest(&encoded),
                    "size": encoded.len(),
                },
                "normalized_hook_digest": digest(&serde_json::to_vec(normalized)?),
            }),
            "pre_tool_use",
            &base,
            &key,
        ),
        Some(("claude_hook_pre_tool_use", raw)),
    )?);
    if is_command_tool(tool_name)
        && let Some(command) = tool_input_string(input, "command")
    {
        let key = stage_key(&base, Some(turn.id), "command_started");
        written += usize::from(append_hook_event(
            store,
            session_id,
            Some(turn.id),
            "command_started",
            EventVisibility::User,
            with_hook_metadata(
                json!({
                    "type": "command_started",
                    "id": tool_use_id,
                    "command": command,
                    "cwd": cwd,
                }),
                "pre_tool_use",
                &base,
                &key,
            ),
            None,
        )?);
    }
    store.update_turn_state(
        turn.id,
        TurnStatus::Running,
        turn.side_effect_state.observe(SideEffectState::Possible),
        None,
        Utc::now(),
    )?;
    Ok(PersistOutcome {
        turn_id: Some(turn.id),
        events_written: written,
        duplicate: written == 0,
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn persist_tool(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    launch_id: Uuid,
    normalized: &Value,
    raw: &Value,
    tool_use_id: &str,
    tool_name: &str,
    input: &Value,
    output: &Value,
    succeeded: bool,
) -> Result<PersistOutcome> {
    let base = tool_hook_base(session_id, launch_id, tool_use_id);
    let Some(turn) = active_claude_turn(store, session_id)? else {
        if event_base_key_exists(store, session_id, &base)? {
            return Ok(PersistOutcome {
                duplicate: true,
                ..PersistOutcome::default()
            });
        }
        bail!("received Claude tool hook without an active native turn");
    };
    let stage = if succeeded {
        "tool_completed"
    } else {
        "tool_failed"
    };
    let key = stage_key(&base, Some(turn.id), stage);
    let encoded = serde_json::to_vec(output)?;
    let output_digest = digest(&encoded);
    let summary = summarize_value(output, TOOL_SUMMARY_BYTES);
    let event_kind = if succeeded {
        "tool_completed"
    } else {
        "tool_failed"
    };
    let hook_event = if succeeded {
        "post_tool_use"
    } else {
        "post_tool_use_failure"
    };
    let mut written = usize::from(append_hook_event(
        store,
        session_id,
        Some(turn.id),
        event_kind,
        EventVisibility::User,
        with_hook_metadata(
            json!({
                "type": event_kind,
                "id": tool_use_id,
                "name": tool_name,
                "status": if succeeded { "completed" } else { "failed" },
                "output": {"summary": summary, "digest": output_digest, "size": encoded.len()},
                "normalized_hook_digest": digest(&serde_json::to_vec(normalized)?),
            }),
            hook_event,
            &base,
            &key,
        ),
        Some((
            if succeeded {
                "claude_hook_post_tool_use"
            } else {
                "claude_hook_post_tool_use_failure"
            },
            raw,
        )),
    )?);
    if is_command_tool(tool_name) {
        let key = stage_key(&base, Some(turn.id), "command_completed");
        written += usize::from(append_hook_event(
            store,
            session_id,
            Some(turn.id),
            "command_completed",
            EventVisibility::User,
            with_hook_metadata(
                json!({
                    "type": "command_completed",
                    "id": tool_use_id,
                    "command": tool_input_string(input, "command"),
                    "exit_code": tool_exit_code(output),
                    "status": if succeeded { "completed" } else { "failed" },
                    "output_digest": output_digest,
                }),
                hook_event,
                &base,
                &key,
            ),
            None,
        )?);
    }
    if succeeded {
        let paths = changed_paths_from_tool(tool_name, input);
        if !paths.is_empty() {
            let key = stage_key(&base, Some(turn.id), "files_changed");
            written += usize::from(append_hook_event(
                store,
                session_id,
                Some(turn.id),
                "files_changed",
                EventVisibility::User,
                with_hook_metadata(
                    json!({
                        "type": "files_changed",
                        "changes": paths.into_iter().map(|path| json!({
                            "path": path,
                            "kind": "modified",
                            "digest": null,
                        })).collect::<Vec<_>>(),
                    }),
                    hook_event,
                    &base,
                    &key,
                ),
                None,
            )?);
        }
    }
    store.update_turn_state(
        turn.id,
        TurnStatus::Running,
        if succeeded {
            SideEffectState::Confirmed
        } else {
            SideEffectState::Possible
        },
        None,
        Utc::now(),
    )?;
    Ok(PersistOutcome {
        turn_id: Some(turn.id),
        events_written: written,
        duplicate: written == 0,
    })
}

fn tool_hook_base(session_id: UnifiedSessionId, launch_id: Uuid, tool_use_id: &str) -> String {
    digest(format!("tool\0{session_id}\0{launch_id}\0{tool_use_id}").as_bytes())
}

fn is_command_tool(tool_name: &str) -> bool {
    matches!(
        tool_name.trim().to_ascii_lowercase().as_str(),
        "bash" | "shell"
    )
}

fn tool_input_string<'a>(input: &'a Value, field: &str) -> Option<&'a str> {
    input
        .get(field)
        .or_else(|| input.pointer(&format!("/selected/{field}")))
        .and_then(Value::as_str)
}

fn tool_exit_code(output: &Value) -> Option<i32> {
    ["exit_code", "exitCode", "code"]
        .into_iter()
        .find_map(|field| {
            output
                .get(field)
                .or_else(|| output.pointer(&format!("/selected/{field}")))
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
        })
}

fn changed_paths_from_tool(tool_name: &str, input: &Value) -> Vec<String> {
    let normalized = tool_name.trim().to_ascii_lowercase();
    let fields: &[&str] = match normalized.as_str() {
        "write" | "edit" | "multiedit" | "write_file" | "edit_file" => &["file_path", "path"],
        "notebookedit" | "notebook_edit" => &["notebook_path", "path"],
        _ => &[],
    };
    let mut paths = fields
        .iter()
        .filter_map(|field| tool_input_string(input, field))
        .filter(|path| !path.is_empty() && !path.contains('\0'))
        .take(128)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

fn persist_stop(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    launch_id: Uuid,
    normalized: &Value,
    raw: &Value,
    assistant: &str,
) -> Result<PersistOutcome> {
    let base = digest(
        format!(
            "{}\0{launch_id}",
            hook_base_key_from_value("stop", normalized)
        )
        .as_bytes(),
    );
    let Some(turn) = active_claude_turn(store, session_id)? else {
        if event_base_key_exists(store, session_id, &base)? {
            return Ok(PersistOutcome {
                duplicate: true,
                ..PersistOutcome::default()
            });
        }
        bail!("received Claude Stop hook without an active native turn");
    };
    let assistant_key = stage_key(&base, Some(turn.id), "assistant");
    let terminal_key = stage_key(&base, Some(turn.id), "terminal");
    let mut written = 0;
    if !event_key_exists(store, session_id, Some(turn.id), &assistant_key)? {
        append_hook_event(
            store,
            session_id,
            Some(turn.id),
            "assistant_final",
            EventVisibility::User,
            with_hook_metadata(
                json!({"type": "assistant_final", "text": assistant}),
                "stop",
                &base,
                &assistant_key,
            ),
            Some(("claude_hook_stop", raw)),
        )?;
        written += 1;
    }
    if !event_key_exists(store, session_id, Some(turn.id), &terminal_key)? {
        append_hook_event(
            store,
            session_id,
            Some(turn.id),
            "turn_completed",
            EventVisibility::Internal,
            with_hook_metadata(
                json!({"type": "turn_completed", "status": TurnStatus::Completed}),
                "stop",
                &base,
                &terminal_key,
            ),
            None,
        )?;
        written += 1;
    }
    store.update_turn_state(
        turn.id,
        TurnStatus::Completed,
        turn.side_effect_state,
        None,
        Utc::now(),
    )?;
    Ok(PersistOutcome {
        turn_id: Some(turn.id),
        events_written: written,
        duplicate: written == 0,
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn persist_stop_failure(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    launch_id: Uuid,
    normalized: &Value,
    raw: &Value,
    failure: StopFailureKind,
    details: Option<&str>,
    assistant: Option<&str>,
) -> Result<PersistOutcome> {
    let base = digest(
        format!(
            "{}\0{launch_id}",
            hook_base_key_from_value("stop_failure", normalized)
        )
        .as_bytes(),
    );
    let Some(turn) = active_claude_turn(store, session_id)? else {
        if event_base_key_exists(store, session_id, &base)? {
            return Ok(PersistOutcome {
                duplicate: true,
                ..PersistOutcome::default()
            });
        }
        bail!("received Claude StopFailure hook without an active native turn");
    };
    let code = stop_failure_code(failure);
    let message = details.unwrap_or("Claude native turn failed");
    let rate_limited = failure == StopFailureKind::RateLimit;
    let error_key = stage_key(&base, Some(turn.id), "error");
    let terminal_key = stage_key(&base, Some(turn.id), "terminal");
    let mut written = 0;
    if let Some(text) = assistant.filter(|text| !text.is_empty()) {
        let key = stage_key(&base, Some(turn.id), "assistant");
        if !event_key_exists(store, session_id, Some(turn.id), &key)? {
            append_hook_event(
                store,
                session_id,
                Some(turn.id),
                "assistant_final",
                EventVisibility::User,
                with_hook_metadata(
                    json!({"type": "assistant_final", "text": text, "partial": true}),
                    "stop_failure",
                    &base,
                    &key,
                ),
                None,
            )?;
            written += 1;
        }
    }
    if !event_key_exists(store, session_id, Some(turn.id), &error_key)? {
        append_hook_event(
            store,
            session_id,
            Some(turn.id),
            "error",
            EventVisibility::User,
            with_hook_metadata(
                json!({
                    "type": "error",
                    "error": {"code": code, "message": message, "retryable": rate_limited}
                }),
                "stop_failure",
                &base,
                &error_key,
            ),
            Some(("claude_hook_stop_failure", raw)),
        )?;
        written += 1;
    }
    if rate_limited {
        let key = stage_key(&base, Some(turn.id), "rate_limit");
        if !event_key_exists(store, session_id, Some(turn.id), &key)? {
            append_hook_event(
                store,
                session_id,
                Some(turn.id),
                "rate_limit_updated",
                EventVisibility::User,
                with_hook_metadata(
                    json!({
                        "type": "rate_limit_updated",
                        "limit": {"utilization": 1.0, "source": "claude_native_hook"}
                    }),
                    "stop_failure",
                    &base,
                    &key,
                ),
                None,
            )?;
            record_provider_failure_health(store, session_id, failure, message)?;
            written += 1;
        }
    } else {
        record_provider_failure_health(store, session_id, failure, message)?;
    }
    if !event_key_exists(store, session_id, Some(turn.id), &terminal_key)? {
        append_hook_event(
            store,
            session_id,
            Some(turn.id),
            "turn_completed",
            EventVisibility::Internal,
            with_hook_metadata(
                json!({"type": "turn_completed", "status": TurnStatus::Failed}),
                "stop_failure",
                &base,
                &terminal_key,
            ),
            None,
        )?;
        written += 1;
    }
    store.update_turn_state(
        turn.id,
        TurnStatus::Failed,
        turn.side_effect_state.observe(SideEffectState::Possible),
        None,
        Utc::now(),
    )?;
    Ok(PersistOutcome {
        turn_id: Some(turn.id),
        events_written: written,
        duplicate: written == 0,
    })
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn persist_session_end(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    native_session_id: &str,
    launch_id: Uuid,
    transcript_path: &Path,
    normalized: &Value,
    raw: &Value,
    reason: SessionEndReason,
) -> Result<PersistOutcome> {
    let base = digest(
        format!(
            "{}\0{launch_id}",
            hook_base_key_from_value("session_end", normalized)
        )
        .as_bytes(),
    );
    let turns = active_claude_turns(store, session_id)?;
    let status = if reason == SessionEndReason::Other {
        TurnStatus::Uncertain
    } else {
        TurnStatus::Interrupted
    };
    let mut written = 0;
    let mut last_turn = None;
    for turn in turns {
        let key = stage_key(&base, Some(turn.id), "terminal");
        if !event_key_exists(store, session_id, Some(turn.id), &key)? {
            append_hook_event(
                store,
                session_id,
                Some(turn.id),
                "turn_completed",
                EventVisibility::Internal,
                with_hook_metadata(
                    json!({
                        "type": "turn_completed",
                        "status": status,
                        "reason": reason,
                        "launch_id": launch_id,
                    }),
                    "session_end",
                    &base,
                    &key,
                ),
                Some(("claude_hook_session_end", raw)),
            )?;
            written += 1;
        }
        store.update_turn_state(
            turn.id,
            status,
            if status == TurnStatus::Uncertain {
                turn.side_effect_state.observe(SideEffectState::Possible)
            } else {
                turn.side_effect_state
            },
            None,
            Utc::now(),
        )?;
        last_turn = Some(turn.id);
    }
    if last_turn.is_none() {
        let key = stage_key(&base, None, "session_end");
        if !event_key_exists(store, session_id, None, &key)? {
            append_hook_event(
                store,
                session_id,
                None,
                "native_session_ended",
                EventVisibility::Internal,
                with_hook_metadata(
                    json!({"reason": reason, "launch_id": launch_id}),
                    "session_end",
                    &base,
                    &key,
                ),
                Some(("claude_hook_session_end", raw)),
            )?;
            written += 1;
        }
    }
    if native_transcript_materialized(transcript_path, native_session_id)? {
        mark_claude_materialized(store, session_id, "nonempty_transcript_path")?;
    }
    let provider = store
        .provider_session(session_id, &ProviderKind::Claude)?
        .context("SessionEnd could not find the bound Claude provider session")?;
    store.update_session_state(
        session_id,
        Some(&ProviderKind::Claude),
        SessionStatus::Idle,
        Utc::now(),
    )?;
    let launch = store
        .native_launch(launch_id)?
        .context("SessionEnd referenced a missing native launch")?;
    ensure!(
        launch.session_id == session_id && launch.provider == ProviderKind::Claude,
        "SessionEnd native launch binding mismatch"
    );
    let handoff = store.native_handoff(launch_id)?;
    if let Some(handoff) = &handoff {
        ensure!(
            matches!(
                handoff.state,
                NativeHandoffState::Staged | NativeHandoffState::Delivered
            ),
            "SessionEnd arrived with an uncertain Claude handoff delivery"
        );
    }
    if handoff
        .as_ref()
        .is_none_or(|handoff| handoff.state == NativeHandoffState::Delivered)
    {
        let captured_through = store.next_seq(session_id)?.saturating_sub(1);
        store.advance_provider_cursor(provider.id, captured_through)?;
    }
    store.update_native_launch(
        launch_id,
        NativeLaunchState::CaptureReady,
        None,
        None,
        Utc::now(),
    )?;
    Ok(PersistOutcome {
        turn_id: last_turn,
        events_written: written,
        duplicate: written == 0,
    })
}

fn build_handoff_stage(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    native_session_id: &str,
    after_seq: u64,
    through_seq: u64,
) -> Result<Option<ClaudeHandoffStage>> {
    let events = all_events(store, session_id)?;
    let delta = events
        .into_iter()
        .filter(|event| event.seq > after_seq && event.seq <= through_seq)
        .collect::<Vec<_>>();
    let source = delta
        .iter()
        .rev()
        .find_map(|event| {
            (event.visibility != EventVisibility::Internal
                && event.origin_provider.as_ref() != Some(&ProviderKind::Claude))
            .then(|| event.origin_provider.clone())
            .flatten()
        })
        .unwrap_or(ProviderKind::Codex);
    let mut capsule = HandoffCapsule::from_events(session_id, &source, &delta)?;
    capsule.through_seq = through_seq;
    if !capsule.has_projectable_context() {
        return Ok(None);
    }
    let rendered = capsule.render_xml();
    ensure!(
        rendered.chars().count() <= MAX_HANDOFF_CONTEXT_CHARS,
        "canonical delta is too large for a native Claude handoff; run `agentctl compact {session_id}`"
    );
    Ok(Some(ClaudeHandoffStage::new(
        session_id,
        native_session_id.to_owned(),
        through_seq,
        rendered,
    )))
}

fn append_hook_event(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    turn_id: Option<TurnId>,
    kind: &str,
    visibility: EventVisibility,
    payload: Value,
    raw: Option<(&str, &Value)>,
) -> Result<bool> {
    let key = payload
        .pointer("/native_hook/key")
        .and_then(Value::as_str)
        .unwrap_or(kind)
        .to_owned();
    let raw = raw
        .map(|(raw_kind, payload)| {
            let encoded = serde_json::to_vec(payload)?;
            Ok::<_, anyhow::Error>(RawProviderEvent {
                event_id: deterministic_event_id(session_id, &format!("{key}\0raw")),
                session_id,
                turn_id,
                provider: ProviderKind::Claude,
                kind: raw_kind.to_owned(),
                payload: payload.clone(),
                content_hash: digest(&encoded),
                created_at: Utc::now(),
            })
        })
        .transpose()?;
    let event = CanonicalEvent {
        schema_version: 1,
        session_id,
        seq: 0,
        event_id: deterministic_event_id(session_id, &key),
        turn_id,
        origin_provider: Some(ProviderKind::Claude),
        kind: kind.to_owned(),
        visibility,
        content_hash: canonical_content_hash(kind, visibility, &payload)?,
        payload,
        raw_event_id: raw.as_ref().map(|event| event.event_id),
        created_at: Utc::now(),
    };
    if let Some(existing) = find_event_by_key(store, session_id, turn_id, &key)? {
        validate_idempotent_event(store, &existing, &event, raw.as_ref())?;
        return Ok(false);
    }
    match store.append_event_allocating_seq(event.clone(), raw.as_ref()) {
        Ok(_) => Ok(true),
        Err(error) => {
            if let Some(existing) = find_event_by_key(store, session_id, turn_id, &key)? {
                validate_idempotent_event(store, &existing, &event, raw.as_ref())?;
                Ok(false)
            } else {
                Err(error.into())
            }
        }
    }
}

fn validate_idempotent_event(
    store: &AgentctlStore,
    existing: &CanonicalEvent,
    expected: &CanonicalEvent,
    expected_raw: Option<&RawProviderEvent>,
) -> Result<()> {
    ensure!(
        existing.event_id == expected.event_id
            && existing.session_id == expected.session_id
            && existing.turn_id == expected.turn_id
            && existing.origin_provider == expected.origin_provider
            && existing.kind == expected.kind
            && existing.visibility == expected.visibility
            && existing.content_hash == expected.content_hash
            && existing.raw_event_id == expected.raw_event_id,
        "native Claude hook idempotency key was reused with different canonical content"
    );
    match (existing.raw_event_id, expected_raw) {
        (Some(raw_id), Some(expected_raw)) => {
            let actual = store
                .raw_event(raw_id)?
                .context("native hook canonical event references missing raw payload")?;
            ensure!(
                actual.provider == expected_raw.provider
                    && actual.turn_id == expected_raw.turn_id
                    && actual.kind == expected_raw.kind
                    && actual.content_hash == expected_raw.content_hash,
                "native Claude hook idempotency key was reused with different raw content"
            );
        }
        (None, None) => {}
        _ => bail!("native Claude hook raw-event idempotency mismatch"),
    }
    Ok(())
}

fn ensure_provider_binding(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    expected_native_session_id: &str,
    allow_create: bool,
) -> Result<ProviderSessionRecord> {
    if let Some(record) = store.provider_session(session_id, &ProviderKind::Claude)? {
        ensure!(
            record.native_session_id == expected_native_session_id,
            "canonical session is bound to a different Claude session"
        );
        return Ok(record);
    }
    ensure!(allow_create, "Claude SessionStart hook was not captured");
    let now = Utc::now();
    let record = ProviderSessionRecord {
        id: ProviderSessionId::new(),
        unified_session_id: session_id,
        provider: ProviderKind::Claude,
        native_session_id: expected_native_session_id.to_owned(),
        native_version: None,
        last_synced_seq: 0,
        status: ProviderStatus::Ready,
        reset_at: None,
        capabilities: BTreeMap::from([("native_hooks".to_owned(), true)]),
        metadata: json!({
            "mode": "native_interactive",
            "native_materialized": false,
        }),
        created_at: now,
        updated_at: now,
    };
    store.upsert_provider_session(&record)?;
    Ok(record)
}

/// Reconciles the durable-session marker used to choose Claude's `--resume`.
///
/// Older agentctl versions recorded `native_started` at `SessionStart`, even
/// though Claude does not always persist an interrupted empty conversation.
/// That legacy bit is deliberately ignored. A native transcript is known to
/// exist after a captured Claude prompt, an explicit attachment, or a
/// previously captured `SessionEnd` whose official transcript path still
/// points to a non-empty transcript for this exact native session id.
pub(crate) fn reconcile_claude_materialization(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
) -> Result<bool> {
    let Some(mut provider) = store.provider_session(session_id, &ProviderKind::Claude)? else {
        return Ok(false);
    };
    let metadata_materialized = provider
        .metadata
        .get(CLAUDE_MATERIALIZED_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let attached = provider
        .metadata
        .get("attached")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let canonical_prompt =
        !metadata_materialized && !attached && canonical_claude_prompt_exists(store, session_id)?;
    let canonical_transcript = !metadata_materialized
        && !attached
        && !canonical_prompt
        && canonical_claude_transcript_exists(store, session_id, &provider.native_session_id)?;
    let materialized =
        metadata_materialized || attached || canonical_prompt || canonical_transcript;

    let metadata = provider
        .metadata
        .as_object_mut()
        .context("Claude provider metadata must be a JSON object")?;
    let needs_update = metadata
        .get(CLAUDE_MATERIALIZED_KEY)
        .and_then(Value::as_bool)
        != Some(materialized)
        || metadata.contains_key(CLAUDE_LEGACY_STARTED_KEY);
    if needs_update {
        metadata.insert(
            CLAUDE_MATERIALIZED_KEY.to_owned(),
            Value::Bool(materialized),
        );
        if materialized && !metadata.contains_key("native_materialized_by") {
            metadata.insert(
                "native_materialized_by".to_owned(),
                Value::String(
                    if attached {
                        "official_resume_interface"
                    } else if canonical_prompt {
                        "canonical_user_prompt"
                    } else if canonical_transcript {
                        "canonical_session_end_transcript"
                    } else {
                        "persisted_metadata"
                    }
                    .to_owned(),
                ),
            );
        }
        metadata.remove(CLAUDE_LEGACY_STARTED_KEY);
        provider.updated_at = Utc::now();
        store.upsert_provider_session(&provider)?;
    }
    Ok(materialized)
}

fn mark_claude_materialized(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    evidence: &'static str,
) -> Result<()> {
    let mut provider = store
        .provider_session(session_id, &ProviderKind::Claude)?
        .context("Claude materialization evidence could not find its bound provider session")?;
    let metadata = provider
        .metadata
        .as_object_mut()
        .context("Claude provider metadata must be a JSON object")?;
    metadata.insert(CLAUDE_MATERIALIZED_KEY.to_owned(), Value::Bool(true));
    metadata
        .entry("native_materialized_by".to_owned())
        .or_insert_with(|| Value::String(evidence.to_owned()));
    metadata.remove(CLAUDE_LEGACY_STARTED_KEY);
    provider.updated_at = Utc::now();
    store.upsert_provider_session(&provider)?;
    Ok(())
}

fn native_transcript_materialized(path: &Path, native_session_id: &str) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect Claude transcript {}", path.display())
            });
        }
    };
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "Claude transcript path is not a regular non-symlink file"
    );
    if metadata.len() == 0 {
        return Ok(false);
    }
    let expected_name = format!("{native_session_id}.jsonl");
    ensure!(
        path.file_name().and_then(|name| name.to_str()) == Some(expected_name.as_str()),
        "Claude transcript filename does not match its native session id"
    );
    Ok(true)
}

fn canonical_claude_prompt_exists(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
) -> Result<bool> {
    let mut after_seq = 0;
    loop {
        let page = store.list_events(session_id, after_seq, EVENT_PAGE_SIZE)?;
        if page.iter().any(|event| {
            event.kind == "user_prompt"
                && event.origin_provider.as_ref() == Some(&ProviderKind::Claude)
        }) {
            return Ok(true);
        }
        let Some(last) = page.last() else {
            return Ok(false);
        };
        after_seq = last.seq;
        if page.len() < EVENT_PAGE_SIZE {
            return Ok(false);
        }
    }
}

fn canonical_claude_transcript_exists(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    native_session_id: &str,
) -> Result<bool> {
    let mut after_seq = 0;
    loop {
        let page = store.list_events(session_id, after_seq, EVENT_PAGE_SIZE)?;
        for event in &page {
            if event.origin_provider.as_ref() != Some(&ProviderKind::Claude) {
                continue;
            }
            let Some(raw_id) = event.raw_event_id else {
                continue;
            };
            let Some(raw) = store.raw_event(raw_id)? else {
                continue;
            };
            if raw.kind != "claude_hook_session_end" {
                continue;
            }
            let Some(path) = raw.payload.get("transcript_path").and_then(Value::as_str) else {
                continue;
            };
            if native_transcript_materialized(Path::new(path), native_session_id)? {
                return Ok(true);
            }
        }
        let Some(last) = page.last() else {
            return Ok(false);
        };
        after_seq = last.seq;
        if page.len() < EVENT_PAGE_SIZE {
            return Ok(false);
        }
    }
}

pub(crate) fn active_claude_turn(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
) -> Result<Option<TurnRecord>> {
    Ok(active_claude_turns(store, session_id)?.pop())
}

fn active_claude_turns(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
) -> Result<Vec<TurnRecord>> {
    let mut turns = store.recovery_candidates_for(session_id, &ProviderKind::Claude)?;
    turns.sort_by_key(|turn| (turn.updated_at, turn.id));
    Ok(turns)
}

fn event_key_exists(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    turn_id: Option<TurnId>,
    key: &str,
) -> Result<bool> {
    Ok(find_event_by_key(store, session_id, turn_id, key)?.is_some())
}

fn find_event_by_key(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    turn_id: Option<TurnId>,
    key: &str,
) -> Result<Option<CanonicalEvent>> {
    let Some(event) = store.event_by_id(deterministic_event_id(session_id, key))? else {
        return Ok(None);
    };
    ensure!(
        event.session_id == session_id
            && event.turn_id == turn_id
            && event
                .payload
                .pointer("/native_hook/key")
                .and_then(Value::as_str)
                == Some(key),
        "native Claude hook deterministic event id collision"
    );
    Ok(Some(event))
}

fn event_base_key_exists(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    base_key: &str,
) -> Result<bool> {
    Ok(store.has_native_hook_base_key(session_id, base_key)?)
}

fn all_events(store: &AgentctlStore, session_id: UnifiedSessionId) -> Result<Vec<CanonicalEvent>> {
    let mut events = Vec::new();
    let mut after = 0;
    loop {
        let page = store.list_events(session_id, after, EVENT_PAGE_SIZE)?;
        let Some(last) = page.last() else {
            break;
        };
        after = last.seq;
        let finished = page.len() < EVENT_PAGE_SIZE;
        events.extend(page);
        if finished {
            break;
        }
    }
    Ok(events)
}

fn with_hook_metadata(mut payload: Value, event: &str, base_key: &str, key: &str) -> Value {
    payload
        .as_object_mut()
        .expect("canonical hook payloads are always objects")
        .insert(
            "native_hook".to_owned(),
            json!({"provider": "claude", "event": event, "base_key": base_key, "key": key}),
        );
    payload
}

fn hook_base_key(event: &NativeHookEvent, normalized: &Value) -> String {
    let common = event.common();
    let identity = common.prompt_id.as_deref().map_or_else(
        || serde_json::to_vec(normalized).unwrap_or_default(),
        |id| id.as_bytes().to_vec(),
    );
    hook_base_key_bytes(event_name(event), &identity)
}

fn hook_base_key_from_value(name: &str, normalized: &Value) -> String {
    hook_base_key_bytes(name, &serde_json::to_vec(normalized).unwrap_or_default())
}

fn hook_base_key_bytes(name: &str, identity: &[u8]) -> String {
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(identity);
    digest(&bytes)
}

fn event_name(event: &NativeHookEvent) -> &'static str {
    match event {
        NativeHookEvent::SessionStart(_) => "session_start",
        NativeHookEvent::UserPromptSubmit(_) => "user_prompt_submit",
        NativeHookEvent::PreToolUse(_) => "pre_tool_use",
        NativeHookEvent::PostToolUse(_) => "post_tool_use",
        NativeHookEvent::PostToolUseFailure(_) => "post_tool_use_failure",
        NativeHookEvent::Stop(_) => "stop",
        NativeHookEvent::StopFailure(_) => "stop_failure",
        NativeHookEvent::SessionEnd(_) => "session_end",
    }
}

fn stage_key(base: &str, turn_id: Option<TurnId>, stage: &str) -> String {
    digest(
        format!(
            "{base}\0{}\0{stage}",
            turn_id.map_or_else(|| "session".to_owned(), |id| id.to_string())
        )
        .as_bytes(),
    )
}

fn deterministic_event_id(session_id: UnifiedSessionId, key: &str) -> EventId {
    EventId(deterministic_uuid(session_id, key))
}

fn deterministic_turn_id(session_id: UnifiedSessionId, key: &str) -> TurnId {
    TurnId(deterministic_uuid(session_id, &format!("turn\0{key}")))
}

fn deterministic_uuid(session_id: UnifiedSessionId, key: &str) -> Uuid {
    let mut hash = Sha256::new();
    hash.update(session_id.to_string().as_bytes());
    hash.update([0]);
    hash.update(key.as_bytes());
    let digest = hash.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn stage_digest(
    session_id: UnifiedSessionId,
    native_session_id: &str,
    through_seq: u64,
    capsule: &str,
) -> String {
    digest(
        serde_json::to_string(&(session_id, native_session_id, through_seq, capsule))
            .unwrap_or_default()
            .as_bytes(),
    )
}

fn summarize_value(value: &Value, limit: usize) -> String {
    let rendered = value.as_str().map_or_else(
        || serde_json::to_string(value).unwrap_or_default(),
        ToOwned::to_owned,
    );
    if rendered.len() <= limit {
        return rendered;
    }
    let mut end = limit;
    while !rendered.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &rendered[..end])
}

fn sanitized_string<'a>(value: &'a Value, pointer: &str) -> Result<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("sanitized Claude hook field {pointer} is missing"))
}

fn sanitize_hook_json(guard: &PayloadGuard, value: &Value) -> Result<Value> {
    let mut guarded = match guard.process_json(value) {
        Ok(guarded) => guarded,
        Err(original) => {
            let compacted = compact_oversized_hook_payload(value)?;
            guard.process_json(&compacted).with_context(|| {
                format!(
                    "Claude hook payload remained unsafe after bounded compaction (original: {original})"
                )
            })?
        }
    };
    redact_sensitive_object_values(&mut guarded);
    Ok(guarded)
}

fn compact_oversized_hook_payload(value: &Value) -> Result<Value> {
    const RETAINED_TEXT_BYTES: usize = 64 * 1024;
    let mut compacted = value.clone();
    for pointer in ["/payload/tool_input", "/tool_input"] {
        if let Some(target) = compacted.pointer_mut(pointer) {
            *target = compact_tool_input(target)?;
        }
    }
    for pointer in ["/payload/tool_response", "/tool_response"] {
        if let Some(target) = compacted.pointer_mut(pointer) {
            *target = compact_hook_value(target)?;
        }
    }
    for pointer in [
        "/payload/prompt",
        "/prompt",
        "/payload/last_assistant_message",
        "/last_assistant_message",
        "/payload/error",
        "/error",
        "/payload/error_details",
        "/error_details",
    ] {
        if let Some(Value::String(text)) = compacted.pointer_mut(pointer)
            && text.len() > RETAINED_TEXT_BYTES
        {
            *text = compact_text(text, RETAINED_TEXT_BYTES);
        }
    }
    Ok(compacted)
}

fn compact_tool_input(value: &Value) -> Result<Value> {
    const RETAINED_KEYS: &[&str] = &[
        "command",
        "cwd",
        "file_path",
        "path",
        "notebook_path",
        "cell_id",
        "description",
    ];
    let encoded = serde_json::to_vec(value)?;
    let selected = value
        .as_object()
        .map_or_else(serde_json::Map::new, |object| {
            RETAINED_KEYS
                .iter()
                .filter_map(|key| {
                    object
                        .get(*key)
                        .map(|value| ((*key).to_owned(), compact_scalar(value)))
                })
                .collect()
        });
    let keys = value
        .as_object()
        .into_iter()
        .flat_map(|object| object.keys())
        .take(128)
        .cloned()
        .collect::<Vec<_>>();
    Ok(json!({
        "agentctl_compacted": true,
        "selected": selected,
        "keys": keys,
        "digest": digest(&encoded),
        "size": encoded.len(),
    }))
}

fn compact_hook_value(value: &Value) -> Result<Value> {
    let encoded = serde_json::to_vec(value)?;
    let selected = value
        .as_object()
        .map_or_else(serde_json::Map::new, |object| {
            ["exit_code", "exitCode", "code", "status"]
                .into_iter()
                .filter_map(|key| {
                    object
                        .get(key)
                        .map(|value| (key.to_owned(), compact_scalar(value)))
                })
                .collect()
        });
    Ok(json!({
        "agentctl_compacted": true,
        "summary": summarize_value(value, TOOL_SUMMARY_BYTES),
        "selected": selected,
        "digest": digest(&encoded),
        "size": encoded.len(),
    }))
}

fn compact_scalar(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(compact_text(text, 64 * 1024)),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
        _ => json!({
            "digest": digest(&serde_json::to_vec(value).unwrap_or_default()),
            "omitted": true,
        }),
    }
}

fn compact_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n<agentctl-omitted bytes=\"{}\" digest=\"{}\" />",
        &text[..end],
        text.len() - end,
        digest(text.as_bytes())
    )
}

fn redact_sensitive_object_values(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let key = key.to_ascii_lowercase().replace('-', "_");
                if [
                    "authorization",
                    "api_key",
                    "apikey",
                    "token",
                    "access_token",
                    "refresh_token",
                    "password",
                    "secret",
                    "private_key",
                    "cookie",
                    "set_cookie",
                ]
                .iter()
                .any(|secret| key == *secret || key.ends_with(&format!("_{secret}")))
                {
                    *value = Value::String("[REDACTED]".to_owned());
                } else {
                    redact_sensitive_object_values(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_sensitive_object_values(item);
            }
        }
        _ => {}
    }
}

fn stop_failure_code(kind: StopFailureKind) -> &'static str {
    match kind {
        StopFailureKind::RateLimit => "rate_limit",
        StopFailureKind::Overloaded => "overloaded",
        StopFailureKind::AuthenticationFailed => "authentication_failed",
        StopFailureKind::OauthOrgNotAllowed => "oauth_org_not_allowed",
        StopFailureKind::BillingError => "billing_error",
        StopFailureKind::InvalidRequest => "invalid_request",
        StopFailureKind::ModelNotFound => "model_not_found",
        StopFailureKind::ServerError => "server_error",
        StopFailureKind::MaxOutputTokens => "max_output_tokens",
        StopFailureKind::Unknown => "unknown",
    }
}

fn record_provider_failure_health(
    store: &AgentctlStore,
    session_id: UnifiedSessionId,
    failure: StopFailureKind,
    message: &str,
) -> Result<()> {
    let status = match failure {
        StopFailureKind::RateLimit => ProviderStatus::Exhausted { resets_at: None },
        StopFailureKind::Overloaded | StopFailureKind::ServerError => ProviderStatus::Overloaded,
        StopFailureKind::AuthenticationFailed
        | StopFailureKind::OauthOrgNotAllowed
        | StopFailureKind::BillingError => ProviderStatus::AuthError,
        _ => ProviderStatus::Warning,
    };
    let now = Utc::now();
    let health = ProviderHealth {
        provider: ProviderKind::Claude,
        status: status.clone(),
        version: None,
        capabilities: BTreeMap::from([("native_hooks".to_owned(), true)]),
        usage: None,
        rate_limit: (failure == StopFailureKind::RateLimit).then_some(RateLimitSnapshot {
            utilization: Some(1.0),
            window_seconds: None,
            resets_at: None,
            source: "claude_native_hook".to_owned(),
        }),
        checked_at: now,
        message: Some(message.to_owned()),
    };
    store.record_health(Some(session_id), &health)?;
    store.update_provider_session_health(session_id, &ProviderKind::Claude, &status, None, now)?;
    Ok(())
}

fn validate_workspace(session: &UnifiedSession, cwd: &Path) -> Result<()> {
    let expected = WorkspaceIdentity::discover(&session.workspace_path)?;
    let actual = WorkspaceIdentity::discover(cwd)?;
    ensure!(
        actual.lease_key == expected.lease_key,
        "Claude hook originated from a different workspace/worktree"
    );
    Ok(())
}

fn validate_launch_binding(
    store: &AgentctlStore,
    session: &UnifiedSession,
    launch_id: Uuid,
    native_session_id: &str,
) -> Result<NativeLaunchRecord> {
    let expected_workspace = WorkspaceIdentity::discover(&session.workspace_path)?;
    let launch = store
        .native_launch(launch_id)?
        .context("Claude hook does not belong to a known native launch")?;
    ensure!(
        launch.session_id == session.id,
        "native launch canonical session mismatch"
    );
    ensure!(
        launch.provider == ProviderKind::Claude,
        "native launch provider mismatch"
    );
    ensure!(
        launch.native_session_id == native_session_id,
        "native launch session mismatch"
    );
    ensure!(
        launch.workspace_lease_key == expected_workspace.lease_key,
        "native launch workspace mismatch"
    );
    Ok(launch)
}

trait NativeHandoffValidation {
    fn validate_for_delivery(
        &self,
        session_id: UnifiedSessionId,
        native_session_id: &str,
    ) -> Result<()>;
}

impl NativeHandoffValidation for NativeHandoffRecord {
    fn validate_for_delivery(
        &self,
        session_id: UnifiedSessionId,
        native_session_id: &str,
    ) -> Result<()> {
        ensure!(
            self.state == NativeHandoffState::Staged,
            "Claude handoff is not staged"
        );
        ensure!(
            self.session_id == session_id,
            "Claude handoff session mismatch"
        );
        ensure!(
            self.native_session_id == native_session_id,
            "Claude handoff native session mismatch"
        );
        Ok(())
    }
}

fn record_session_divergence(
    store: &AgentctlStore,
    guard: &PayloadGuard,
    session: &UnifiedSession,
    event: &NativeHookEvent,
    raw: &Value,
) {
    let result = (|| -> Result<()> {
        let raw = sanitize_hook_json(guard, raw)?;
        let actual_digest = digest(event.session_id().as_bytes());
        let base = digest(format!("divergence\0{}\0{actual_digest}", event_name(event)).as_bytes());
        let key = stage_key(&base, None, "session_divergence");
        append_hook_event(
            store,
            session.id,
            None,
            "native_session_diverged",
            EventVisibility::Internal,
            with_hook_metadata(
                json!({
                    "expected_session_digest": store
                        .provider_session(session.id, &ProviderKind::Claude)?
                        .map(|record| digest(record.native_session_id.as_bytes())),
                    "actual_session_digest": actual_digest,
                    "event": event_name(event),
                }),
                "session_divergence",
                &base,
                &key,
            ),
            Some(("claude_hook_session_divergence", &raw)),
        )?;
        Ok(())
    })();
    if let Err(error) = result {
        tracing::error!(%error, "failed to journal Claude native session divergence");
    }
}

fn write_hook_json(value: &Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write_hook_json_to(&mut stdout, value)?;
    Ok(())
}

fn write_hook_json_to(writer: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn session_mismatch_output(event: &NativeHookEvent) -> Value {
    if matches!(event, NativeHookEvent::UserPromptSubmit(_)) {
        json!({
            "decision": "block",
            "reason": "This session is owned by another agentctl mapping; exit and use agentctl open/switch"
        })
    } else {
        json!({
            "continue": false,
            "stopReason": "Claude changed to a session outside the active agentctl mapping; exit and use agentctl open/switch",
            "systemMessage": "agentctl blocked a native session identity change"
        })
    }
}

fn is_native_session_reset(event: &NativeHookEvent) -> bool {
    matches!(
        event,
        NativeHookEvent::SessionStart(start) if start.source == SessionStartSource::Clear
    )
}

fn native_session_reset_output() -> Value {
    json!({
        "continue": false,
        "stopReason": "Claude cleared the mapped session; exit and use agentctl new/fork so canonical and native history do not diverge",
        "systemMessage": "agentctl blocked a native transcript reset"
    })
}

fn create_private_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && parent.exists()
    {
        let metadata = fs::symlink_metadata(parent)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "native runtime parent must be a non-symlink directory"
        );
    }
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "native runtime path must be a non-symlink directory"
        );
    } else {
        fs::create_dir(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if path.exists() {
        bail!(
            "refusing to overwrite native runtime file {}",
            path.display()
        );
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn remove_regular_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "refusing to remove non-regular native runtime path"
    );
    fs::remove_file(path)?;
    Ok(())
}

fn utf8_path(path: &Path, label: &str) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("{label} path must be valid UTF-8"))
}

fn ensure_exec_hook_support(binary: &str) -> Result<String> {
    let output = StdCommand::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to execute `{binary} --version`"))?;
    ensure!(output.status.success(), "`{binary} --version` failed");
    ensure!(
        output.stdout.len().saturating_add(output.stderr.len()) <= 64 * 1024,
        "Claude version output is unexpectedly large"
    );
    let rendered = format!(
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let version = parse_claude_semver(&rendered)
        .context("Claude version is not a supported semantic version")?;
    ensure!(
        supports_exec_hooks(version),
        "Claude Code {}.{}.{} is too old for safe exec-form native hooks; version 2.1.139 or newer is required",
        version.0,
        version.1,
        version.2,
    );
    Ok(format!("{}.{}.{}", version.0, version.1, version.2))
}

pub(crate) fn supports_exec_hooks(version: (u64, u64, u64)) -> bool {
    version >= (2, 1, 139)
}

pub(crate) fn parse_claude_semver(value: &str) -> Option<(u64, u64, u64)> {
    value.split_whitespace().find_map(|token| {
        let token = token.trim_start_matches(['v', 'V']);
        let core = token.split(['-', '+', '(']).next()?;
        let mut components = core.split('.');
        let major = components.next()?.parse().ok()?;
        let minor = components.next()?.parse().ok()?;
        let patch = components.next()?.parse().ok()?;
        (components.next().is_none()).then_some((major, minor, patch))
    })
}

#[cfg(test)]
mod tests {
    use agentctl_core::{AuthMode, NativeSession, SessionStatus};
    use agentctl_provider_claude::native_hooks::parse_hook_payload;
    use agentctl_telemetry::{PayloadLimits, RedactionConfig, Redactor};
    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        root: TempDir,
        workspace: TempDir,
        paths: AgentctlPaths,
        store: AgentctlStore,
        guard: PayloadGuard,
        session: UnifiedSession,
        native_session_id: String,
        launch_id: Uuid,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let paths = AgentctlPaths::resolve(Some(root.path().join("home"))).unwrap();
            let store = AgentctlStore::open(&paths.database, &paths.blobs).unwrap();
            let identity = WorkspaceIdentity::discover(workspace.path()).unwrap();
            let now = Utc::now();
            let session = UnifiedSession {
                id: UnifiedSessionId::new(),
                name: "native-hooks".to_owned(),
                workspace_path: identity.execution_root().to_path_buf(),
                workspace_fingerprint: identity.fingerprint,
                active_provider: None,
                routing_policy: "manual".to_owned(),
                auth_mode: AuthMode::NativeLocal,
                status: SessionStatus::Idle,
                parent_session_id: None,
                created_at: now,
                updated_at: now,
                schema_version: 1,
            };
            store.create_session(&session).unwrap();
            Self {
                root,
                workspace,
                paths,
                store,
                guard: PayloadGuard::new(
                    PayloadLimits::default(),
                    Redactor::new(&RedactionConfig::with_secret_defaults()).unwrap(),
                ),
                session,
                native_session_id: Uuid::new_v4().to_string(),
                launch_id: Uuid::new_v4(),
            }
        }

        #[allow(clippy::needless_pass_by_value)]
        fn raw(&self, event: &str, fields: Value) -> Value {
            let mut payload = json!({
                "session_id": self.native_session_id,
                "prompt_id": "550e8400-e29b-41d4-a716-446655440000",
                "transcript_path": "/untrusted/transcript.jsonl",
                "cwd": self.workspace.path(),
                "permission_mode": "default",
                "hook_event_name": event,
            });
            payload
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            payload
        }

        fn persist(&self, raw: &Value) -> Result<PersistOutcome> {
            self.persist_for_launch(self.launch_id, raw)
        }

        fn persist_for_launch(&self, launch_id: Uuid, raw: &Value) -> Result<PersistOutcome> {
            let bytes = serde_json::to_vec(raw)?;
            let event = parse_hook_payload(&bytes)?;
            persist_hook_event(
                &self.store,
                &self.guard,
                &self.session,
                &self.native_session_id,
                launch_id,
                &event,
                raw,
            )
        }

        fn start(&self) {
            self.journal_launch();
            self.persist(&self.raw(
                "SessionStart",
                json!({"source": "startup", "model": "claude-test"}),
            ))
            .unwrap();
        }

        fn journal_launch(&self) {
            let now = Utc::now();
            let identity = WorkspaceIdentity::discover(self.workspace.path()).unwrap();
            self.store
                .start_native_launch(&NativeLaunchRecord {
                    id: self.launch_id,
                    session_id: self.session.id,
                    provider: ProviderKind::Claude,
                    native_session_id: self.native_session_id.clone(),
                    workspace_lease_key: identity.lease_key,
                    child_pid: None,
                    state: NativeLaunchState::Started,
                    exit_code: None,
                    error: None,
                    metadata: json!({}),
                    started_at: now,
                    updated_at: now,
                })
                .unwrap();
        }

        fn prompt(&self, text: &str) -> PersistOutcome {
            self.persist(&self.raw(
                "UserPromptSubmit",
                json!({"prompt": text, "prompt_id": Uuid::new_v4().to_string()}),
            ))
            .unwrap()
        }

        #[cfg(unix)]
        fn claude_config(&self) -> Config {
            use std::os::unix::fs::PermissionsExt;

            let fake_claude = self.root.path().join("claude-materialization-test");
            fs::write(&fake_claude, "#!/bin/sh\necho '2.1.139 (Claude Code)'\n").unwrap();
            fs::set_permissions(&fake_claude, fs::Permissions::from_mode(0o700)).unwrap();
            let mut config = Config::default();
            config.providers.claude_binary = fake_claude.to_string_lossy().into_owned();
            config
        }

        fn end_and_capture(&self) {
            self.persist(&self.raw("SessionEnd", json!({"reason": "prompt_input_exit"})))
                .unwrap();
            assert!(record_native_child_exit(&self.store, self.launch_id, Some(0)).unwrap());
        }

        fn end_with_nonempty_transcript_and_capture(&self) -> PathBuf {
            let transcript = self
                .root
                .path()
                .join(format!("{}.jsonl", self.native_session_id));
            fs::write(&transcript, "{\"type\":\"session\"}\n").unwrap();
            self.persist(&self.raw(
                "SessionEnd",
                json!({
                    "reason": "prompt_input_exit",
                    "transcript_path": transcript,
                }),
            ))
            .unwrap();
            assert!(record_native_child_exit(&self.store, self.launch_id, Some(0)).unwrap());
            transcript
        }

        fn rewrite_as_legacy_started_metadata(&self) {
            let mut provider = self
                .store
                .provider_session(self.session.id, &ProviderKind::Claude)
                .unwrap()
                .unwrap();
            let metadata = provider.metadata.as_object_mut().unwrap();
            metadata.remove(CLAUDE_MATERIALIZED_KEY);
            metadata.remove("native_materialized_by");
            metadata.insert(CLAUDE_LEGACY_STARTED_KEY.to_owned(), Value::Bool(true));
            provider.updated_at = Utc::now();
            self.store.upsert_provider_session(&provider).unwrap();
        }

        fn append_codex_result(&self, text: &str) {
            let payload = json!({"text": text});
            self.store
                .append_event_allocating_seq(
                    CanonicalEvent {
                        schema_version: 1,
                        session_id: self.session.id,
                        seq: 0,
                        event_id: EventId::new(),
                        turn_id: None,
                        origin_provider: Some(ProviderKind::Codex),
                        kind: "assistant_final".to_owned(),
                        visibility: EventVisibility::User,
                        content_hash: canonical_content_hash(
                            "assistant_final",
                            EventVisibility::User,
                            &payload,
                        )
                        .unwrap(),
                        payload,
                        raw_event_id: None,
                        created_at: Utc::now(),
                    },
                    None,
                )
                .unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn empty_claude_launch_does_not_resume_an_unmaterialized_transcript() {
        let fixture = Fixture::new();
        fixture.start();
        assert_eq!(
            fixture
                .store
                .provider_session(fixture.session.id, &ProviderKind::Claude)
                .unwrap()
                .unwrap()
                .metadata[CLAUDE_MATERIALIZED_KEY],
            false
        );
        fixture.end_and_capture();
        fixture.rewrite_as_legacy_started_metadata();

        let prepared = prepare_native_claude(
            &fixture.store,
            &fixture.paths,
            &fixture.claude_config(),
            &fixture.session,
            &WorkspaceIdentity::discover(fixture.workspace.path()).unwrap(),
        )
        .unwrap();

        assert_eq!(prepared.native_session_id, fixture.native_session_id);
        assert!(
            !prepared.resume,
            "empty Claude launch must use --session-id"
        );
        let metadata = fixture
            .store
            .provider_session(fixture.session.id, &ProviderKind::Claude)
            .unwrap()
            .unwrap()
            .metadata;
        assert_eq!(metadata[CLAUDE_MATERIALIZED_KEY], false);
        assert!(metadata.get(CLAUDE_LEGACY_STARTED_KEY).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn claude_launch_resumes_after_the_first_canonical_prompt() {
        let fixture = Fixture::new();
        fixture.start();
        fixture.prompt("Materialize this conversation");
        fixture.end_and_capture();
        fixture.rewrite_as_legacy_started_metadata();

        let prepared = prepare_native_claude(
            &fixture.store,
            &fixture.paths,
            &fixture.claude_config(),
            &fixture.session,
            &WorkspaceIdentity::discover(fixture.workspace.path()).unwrap(),
        )
        .unwrap();

        assert!(prepared.resume, "prompted Claude launch must use --resume");
        let metadata = fixture
            .store
            .provider_session(fixture.session.id, &ProviderKind::Claude)
            .unwrap()
            .unwrap()
            .metadata;
        assert_eq!(metadata[CLAUDE_MATERIALIZED_KEY], true);
        assert!(metadata.get(CLAUDE_LEGACY_STARTED_KEY).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn nonempty_exit_transcript_materializes_and_backfills_legacy_metadata() {
        let fixture = Fixture::new();
        fixture.start();
        let transcript = fixture.end_with_nonempty_transcript_and_capture();
        assert!(transcript.exists());
        let current = fixture
            .store
            .provider_session(fixture.session.id, &ProviderKind::Claude)
            .unwrap()
            .unwrap();
        assert_eq!(current.metadata[CLAUDE_MATERIALIZED_KEY], true);
        assert_eq!(
            current.metadata["native_materialized_by"],
            "nonempty_transcript_path"
        );

        // Simulate a database written by the prior release: SessionEnd raw
        // exists, but only the incorrect SessionStart marker was retained.
        fixture.rewrite_as_legacy_started_metadata();
        let prepared = prepare_native_claude(
            &fixture.store,
            &fixture.paths,
            &fixture.claude_config(),
            &fixture.session,
            &WorkspaceIdentity::discover(fixture.workspace.path()).unwrap(),
        )
        .unwrap();

        assert!(
            prepared.resume,
            "non-empty /exit transcript must use --resume"
        );
        let migrated = fixture
            .store
            .provider_session(fixture.session.id, &ProviderKind::Claude)
            .unwrap()
            .unwrap();
        assert_eq!(migrated.metadata[CLAUDE_MATERIALIZED_KEY], true);
        assert_eq!(
            migrated.metadata["native_materialized_by"],
            "canonical_session_end_transcript"
        );
        assert!(migrated.metadata.get(CLAUDE_LEGACY_STARTED_KEY).is_none());
    }

    #[test]
    fn terminal_crash_turn_does_not_block_a_fresh_prompt_when_claude_returns() {
        let fixture = Fixture::new();
        fixture.start();
        let crashed = fixture.prompt("Start work before the crash");
        let crashed_turn = crashed.turn_id.unwrap();
        fixture
            .store
            .update_turn_state(
                crashed_turn,
                TurnStatus::Failed,
                SideEffectState::Possible,
                Some("crashed-native-turn"),
                Utc::now(),
            )
            .unwrap();
        fixture
            .store
            .update_native_launch(
                fixture.launch_id,
                NativeLaunchState::Failed,
                Some(1),
                Some("cross-provider continuation committed"),
                Utc::now(),
            )
            .unwrap();

        let returned_launch = Uuid::now_v7();
        let now = Utc::now();
        let identity = WorkspaceIdentity::discover(fixture.workspace.path()).unwrap();
        fixture
            .store
            .start_native_launch(&NativeLaunchRecord {
                id: returned_launch,
                session_id: fixture.session.id,
                provider: ProviderKind::Claude,
                native_session_id: fixture.native_session_id.clone(),
                workspace_lease_key: identity.lease_key,
                child_pid: None,
                state: NativeLaunchState::Started,
                exit_code: None,
                error: None,
                metadata: json!({}),
                started_at: now,
                updated_at: now,
            })
            .unwrap();
        let start_raw = fixture.raw(
            "SessionStart",
            json!({"source": "resume", "model": "claude-test"}),
        );
        let start = parse_hook_payload(&serde_json::to_vec(&start_raw).unwrap()).unwrap();
        persist_hook_event(
            &fixture.store,
            &fixture.guard,
            &fixture.session,
            &fixture.native_session_id,
            returned_launch,
            &start,
            &start_raw,
        )
        .unwrap();
        let prompt_raw = fixture.raw(
            "UserPromptSubmit",
            json!({
                "prompt": "Continue from the preserved workspace",
                "prompt_id": Uuid::new_v4().to_string(),
            }),
        );
        let prompt = parse_hook_payload(&serde_json::to_vec(&prompt_raw).unwrap()).unwrap();

        let accepted = persist_hook_event(
            &fixture.store,
            &fixture.guard,
            &fixture.session,
            &fixture.native_session_id,
            returned_launch,
            &prompt,
            &prompt_raw,
        )
        .unwrap();

        assert_ne!(accepted.turn_id, Some(crashed_turn));
        assert_eq!(
            active_claude_turn(&fixture.store, fixture.session.id)
                .unwrap()
                .map(|turn| turn.id),
            accepted.turn_id
        );
    }

    #[cfg(unix)]
    #[test]
    fn attached_claude_session_always_uses_resume() {
        let fixture = Fixture::new();
        crate::operations::persist_native_attachment(
            &fixture.store,
            &fixture.session,
            &NativeSession {
                id: ProviderSessionId::new(),
                provider: ProviderKind::Claude,
                native_session_id: fixture.native_session_id.clone(),
                native_version: Some("2.1.139".to_owned()),
                capabilities: BTreeMap::new(),
            },
            false,
        )
        .unwrap();

        let prepared = prepare_native_claude(
            &fixture.store,
            &fixture.paths,
            &fixture.claude_config(),
            &fixture.session,
            &WorkspaceIdentity::discover(fixture.workspace.path()).unwrap(),
        )
        .unwrap();

        assert!(prepared.resume, "attached Claude session must use --resume");
    }

    #[test]
    fn native_hook_lifecycle_is_canonical_raw_and_idempotent() {
        let fixture = Fixture::new();
        fixture.start();
        let prompt = fixture.prompt("Implement the bridge");
        let pre_tool = fixture.raw(
            "PreToolUse",
            json!({
                "tool_name": "Write",
                "tool_input": {"file_path": "src/lib.rs", "content": "safe"},
                "tool_use_id": "tool-1"
            }),
        );
        assert_eq!(fixture.persist(&pre_tool).unwrap().events_written, 1);
        let tool = fixture.raw(
            "PostToolUse",
            json!({
                "tool_name": "Write",
                "tool_input": {"file_path": "src/lib.rs"},
                "tool_response": {"ok": true},
                "tool_use_id": "tool-1"
            }),
        );
        assert_eq!(fixture.persist(&tool).unwrap().events_written, 2);
        assert!(fixture.persist(&tool).unwrap().duplicate);
        let stop = fixture.raw(
            "Stop",
            json!({"stop_hook_active": false, "last_assistant_message": "Done"}),
        );
        assert_eq!(fixture.persist(&stop).unwrap().events_written, 2);
        assert!(fixture.persist(&stop).unwrap().duplicate);

        let turn = fixture
            .store
            .get_turn(prompt.turn_id.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(turn.status, TurnStatus::Completed);
        assert_eq!(turn.side_effect_state, SideEffectState::Confirmed);
        let events = all_events(&fixture.store, fixture.session.id).unwrap();
        assert_eq!(
            turn.prompt_seq,
            events
                .iter()
                .find(|event| event.kind == "user_prompt")
                .unwrap()
                .seq
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.kind.as_str())
                .collect::<Vec<_>>(),
            vec![
                "native_session_started",
                "user_prompt",
                "tool_started",
                "tool_completed",
                "files_changed",
                "assistant_final",
                "turn_completed"
            ]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.raw_event_id.is_some())
                .count(),
            5
        );
    }

    #[test]
    fn command_hooks_and_large_outputs_remain_bounded_and_projectable() {
        let fixture = Fixture::new();
        fixture.start();
        fixture.prompt("Run the checks");
        fixture
            .persist(&fixture.raw(
                "PreToolUse",
                json!({
                    "tool_name": "Bash",
                    "tool_input": {"command": "cargo test --workspace"},
                    "tool_use_id": "tool-large-command"
                }),
            ))
            .unwrap();
        fixture
            .persist(&fixture.raw(
                "PostToolUse",
                json!({
                    "tool_name": "Bash",
                    "tool_input": {"command": "cargo test --workspace"},
                    "tool_response": {
                        "stdout": "x".repeat(2 * 1024 * 1024),
                        "exit_code": 0
                    },
                    "tool_use_id": "tool-large-command"
                }),
            ))
            .unwrap();

        let events = all_events(&fixture.store, fixture.session.id).unwrap();
        assert!(events.iter().any(|event| {
            event.kind == "command_started" && event.payload["command"] == "cargo test --workspace"
        }));
        assert!(
            events.iter().any(|event| {
                event.kind == "command_completed" && event.payload["exit_code"] == 0
            })
        );
        let raw = events
            .iter()
            .find(|event| event.kind == "tool_completed")
            .and_then(|event| event.raw_event_id)
            .and_then(|id| fixture.store.raw_event(id).unwrap())
            .unwrap();
        assert_eq!(raw.payload["tool_response"]["agentctl_compacted"], true);
        assert!(serde_json::to_vec(&raw.payload).unwrap().len() < 1024 * 1024);
    }

    #[test]
    fn secrets_are_redacted_in_normalized_raw_and_turn_metadata() {
        let fixture = Fixture::new();
        fixture.start();
        fixture.prompt("token=prompt-super-secret");
        fixture
            .persist(&fixture.raw(
                "PostToolUse",
                json!({
                    "tool_name": "token=tool-name-secret",
                    "tool_input": {
                        "token": "input-secret",
                        "headers": {"Cookie": "sessionid=cookie-secret"}
                    },
                    "tool_response": {
                        "body": "Bearer abcdefghijklmnop",
                        "headers": {"Set-Cookie": "auth=response-cookie-secret"}
                    },
                    "tool_use_id": "tool-secret"
                }),
            ))
            .unwrap();
        fixture
            .persist(&fixture.raw(
                "Stop",
                json!({
                    "stop_hook_active": false,
                    "last_assistant_message": "api_key=assistant-secret"
                }),
            ))
            .unwrap();
        for event in all_events(&fixture.store, fixture.session.id).unwrap() {
            let encoded = serde_json::to_string(&event.payload).unwrap();
            assert!(!encoded.contains("prompt-super-secret"));
            assert!(!encoded.contains("tool-name-secret"));
            assert!(!encoded.contains("abcdefghijklmnop"));
            assert!(!encoded.contains("assistant-secret"));
            if let Some(raw_id) = event.raw_event_id {
                let raw = fixture.store.raw_event(raw_id).unwrap().unwrap();
                let encoded = serde_json::to_string(&raw.payload).unwrap();
                assert!(!encoded.contains("prompt-super-secret"));
                assert!(!encoded.contains("input-secret"));
                assert!(!encoded.contains("abcdefghijklmnop"));
                assert!(!encoded.contains("assistant-secret"));
                assert!(!encoded.contains("cookie-secret"));
                assert!(!encoded.contains("response-cookie-secret"));
            }
        }
        let turn = active_claude_turns(&fixture.store, fixture.session.id).unwrap();
        assert!(turn.is_empty());
    }

    #[test]
    fn changed_payload_with_same_native_id_fails_closed() {
        let fixture = Fixture::new();
        fixture.start();
        fixture.prompt("Work");
        let first = fixture.raw(
            "PostToolUse",
            json!({
                "tool_name": "Bash",
                "tool_input": {"command": "true"},
                "tool_response": "first",
                "tool_use_id": "stable-tool-id"
            }),
        );
        fixture.persist(&first).unwrap();
        let changed = fixture.raw(
            "PostToolUse",
            json!({
                "tool_name": "Bash",
                "tool_input": {"command": "true"},
                "tool_response": "changed",
                "tool_use_id": "stable-tool-id"
            }),
        );
        let error = fixture.persist(&changed).unwrap_err();
        assert!(error.to_string().contains("idempotency key"));
    }

    #[test]
    fn concurrent_tool_hooks_allocate_distinct_monotonic_sequences() {
        let fixture = Fixture::new();
        fixture.start();
        fixture.prompt("Parallel tools");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for index in 0..2 {
            let raw = fixture.raw(
                "PostToolUse",
                json!({
                    "tool_name": "Read",
                    "tool_input": {"path": format!("file-{index}")},
                    "tool_response": {"index": index},
                    "tool_use_id": format!("parallel-tool-{index}")
                }),
            );
            let event = parse_hook_payload(&serde_json::to_vec(&raw).unwrap()).unwrap();
            let store = fixture.store.clone();
            let guard = fixture.guard.clone();
            let session = fixture.session.clone();
            let native_session_id = fixture.native_session_id.clone();
            let barrier = barrier.clone();
            let launch_id = fixture.launch_id;
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                persist_hook_event(
                    &store,
                    &guard,
                    &session,
                    &native_session_id,
                    launch_id,
                    &event,
                    &raw,
                )
                .unwrap();
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        let events = all_events(&fixture.store, fixture.session.id).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "tool_completed")
                .count(),
            2
        );
        assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    }

    #[test]
    fn stop_failure_records_health_and_session_end_marks_unknown_turn_uncertain() {
        let fixture = Fixture::new();
        fixture.start();
        let first = fixture.prompt("Hit quota");
        fixture
            .persist(&fixture.raw(
                "StopFailure",
                json!({"error": "rate_limit", "error_details": "quota exhausted"}),
            ))
            .unwrap();
        assert_eq!(
            fixture
                .store
                .get_turn(first.turn_id.unwrap())
                .unwrap()
                .unwrap()
                .status,
            TurnStatus::Failed
        );
        assert!(matches!(
            fixture
                .store
                .latest_health(&ProviderKind::Claude)
                .unwrap()
                .unwrap()
                .status,
            ProviderStatus::Exhausted { .. }
        ));

        // A second exact prompt is a distinct turn after the terminal hook.
        let second = fixture.prompt("Hit quota");
        fixture
            .persist(&fixture.raw("SessionEnd", json!({"reason": "other"})))
            .unwrap();
        let turn = fixture
            .store
            .get_turn(second.turn_id.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(turn.status, TurnStatus::Uncertain);
        assert_eq!(turn.side_effect_state, SideEffectState::Possible);
        assert_eq!(
            fixture
                .store
                .native_launch(fixture.launch_id)
                .unwrap()
                .unwrap()
                .state,
            NativeLaunchState::CaptureReady
        );
        assert!(record_native_child_exit(&fixture.store, fixture.launch_id, Some(0)).unwrap());
        assert_eq!(
            fixture
                .store
                .native_launch(fixture.launch_id)
                .unwrap()
                .unwrap()
                .state,
            NativeLaunchState::Captured
        );
    }

    #[cfg(unix)]
    #[test]
    fn preparation_journals_authorized_handoff_and_private_settings() {
        let fixture = Fixture::new();
        let payload = json!({"text": "Codex result"});
        fixture
            .store
            .append_event_allocating_seq(
                CanonicalEvent {
                    schema_version: 1,
                    session_id: fixture.session.id,
                    seq: 0,
                    event_id: EventId::new(),
                    turn_id: None,
                    origin_provider: Some(ProviderKind::Codex),
                    kind: "assistant_final".to_owned(),
                    visibility: EventVisibility::User,
                    content_hash: canonical_content_hash(
                        "assistant_final",
                        EventVisibility::User,
                        &payload,
                    )
                    .unwrap(),
                    payload,
                    raw_event_id: None,
                    created_at: Utc::now(),
                },
                None,
            )
            .unwrap();
        let identity = WorkspaceIdentity::discover(fixture.workspace.path()).unwrap();
        let fake_claude = fixture.root.path().join("claude-test");
        fs::write(&fake_claude, "#!/bin/sh\necho '2.1.139 (Claude Code)'\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_claude, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut config = Config::default();
        config.providers.claude_binary = fake_claude.to_string_lossy().into_owned();
        let prepared = prepare_native_claude(
            &fixture.store,
            &fixture.paths,
            &config,
            &fixture.session,
            &identity,
        )
        .unwrap();
        let staged = fixture
            .store
            .native_handoff(prepared.launch_id)
            .unwrap()
            .unwrap();
        assert_eq!(staged.state, NativeHandoffState::Staged);
        assert!(staged.capsule.contains("Codex result"));
        assert_eq!(
            staged.content_digest,
            stage_digest(
                staged.session_id,
                &staged.native_session_id,
                staged.through_seq,
                &staged.capsule,
            )
        );
        let settings: Value =
            serde_json::from_slice(&fs::read(&prepared.settings_path).unwrap()).unwrap();
        let encoded = settings.to_string();
        assert!(encoded.contains(&prepared.launch_id.to_string()));
        assert!(!encoded.contains("Codex result"));
        let output = user_prompt_submit_additional_context(&staged.capsule).unwrap();
        assert_eq!(
            output["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        let cursor = fixture
            .store
            .provider_session(fixture.session.id, &ProviderKind::Claude)
            .unwrap();
        assert_eq!(cursor.unwrap().last_synced_seq, 0);
        assert_eq!(
            fixture
                .store
                .native_handoff(prepared.launch_id)
                .unwrap()
                .unwrap()
                .state,
            NativeHandoffState::Staged
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&prepared.settings_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn staged_handoff_survives_empty_exit_and_delivers_once_on_first_prompt() {
        let fixture = Fixture::new();
        fixture.start();
        fixture.end_and_capture();
        let cursor_before = fixture
            .store
            .provider_session(fixture.session.id, &ProviderKind::Claude)
            .unwrap()
            .unwrap()
            .last_synced_seq;
        fixture.append_codex_result("Codex delta that must survive an idle Claude launch");
        let identity = WorkspaceIdentity::discover(fixture.workspace.path()).unwrap();
        let config = fixture.claude_config();

        let first = prepare_native_claude(
            &fixture.store,
            &fixture.paths,
            &config,
            &fixture.session,
            &identity,
        )
        .unwrap();
        fixture
            .persist_for_launch(
                first.launch_id,
                &fixture.raw(
                    "SessionStart",
                    json!({"source": "startup", "model": "claude-test"}),
                ),
            )
            .unwrap();
        assert_eq!(
            fixture
                .store
                .native_handoff(first.launch_id)
                .unwrap()
                .unwrap()
                .state,
            NativeHandoffState::Staged
        );
        fixture
            .persist_for_launch(
                first.launch_id,
                &fixture.raw("SessionEnd", json!({"reason": "prompt_input_exit"})),
            )
            .unwrap();
        assert!(record_native_child_exit(&fixture.store, first.launch_id, Some(0)).unwrap());
        assert_eq!(
            fixture
                .store
                .provider_session(fixture.session.id, &ProviderKind::Claude)
                .unwrap()
                .unwrap()
                .last_synced_seq,
            cursor_before,
            "idle exit must not advance past an undelivered handoff"
        );

        let second = prepare_native_claude(
            &fixture.store,
            &fixture.paths,
            &config,
            &fixture.session,
            &identity,
        )
        .unwrap();
        let restaged = fixture
            .store
            .native_handoff(second.launch_id)
            .unwrap()
            .unwrap();
        assert_eq!(restaged.state, NativeHandoffState::Staged);
        assert!(restaged.capsule.contains("Codex delta that must survive"));
        fixture
            .persist_for_launch(
                second.launch_id,
                &fixture.raw(
                    "SessionStart",
                    json!({"source": "startup", "model": "claude-test"}),
                ),
            )
            .unwrap();
        fixture
            .persist_for_launch(
                second.launch_id,
                &fixture.raw(
                    "UserPromptSubmit",
                    json!({
                        "prompt": "Continue with the transferred context",
                        "prompt_id": Uuid::new_v4().to_string(),
                    }),
                ),
            )
            .unwrap();

        let mut stdout = Vec::new();
        assert_eq!(
            deliver_staged_handoff(
                &fixture.store,
                fixture.session.id,
                &fixture.native_session_id,
                second.launch_id,
                &mut stdout,
            )
            .unwrap(),
            HandoffDelivery::Delivered
        );
        let output: Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(
            output["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert!(
            output["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("Codex delta that must survive")
        );
        assert_eq!(
            fixture
                .store
                .native_handoff(second.launch_id)
                .unwrap()
                .unwrap()
                .state,
            NativeHandoffState::Delivered
        );
        assert!(
            fixture
                .store
                .provider_session(fixture.session.id, &ProviderKind::Claude)
                .unwrap()
                .unwrap()
                .last_synced_seq
                >= restaged.through_seq
        );
        assert_eq!(
            fixture
                .store
                .provider_session(fixture.session.id, &ProviderKind::Claude)
                .unwrap()
                .unwrap()
                .metadata[CLAUDE_MATERIALIZED_KEY],
            true
        );

        let output_len = stdout.len();
        assert_eq!(
            deliver_staged_handoff(
                &fixture.store,
                fixture.session.id,
                &fixture.native_session_id,
                second.launch_id,
                &mut stdout,
            )
            .unwrap(),
            HandoffDelivery::None
        );
        assert_eq!(stdout.len(), output_len, "delivery must be idempotent");
    }

    #[test]
    fn parses_and_orders_claude_exec_hook_version_boundary() {
        assert_eq!(
            parse_claude_semver("2.1.139 (Claude Code)"),
            Some((2, 1, 139))
        );
        assert_eq!(parse_claude_semver("v2.2.0+build"), Some((2, 2, 0)));
        assert_eq!(parse_claude_semver("Claude Code unknown"), None);
        assert!(!supports_exec_hooks((2, 1, 138)));
        assert!(supports_exec_hooks((2, 1, 139)));
    }

    #[test]
    fn native_session_escape_response_blocks_before_model_turn() {
        let fixture = Fixture::new();
        let prompt = parse_hook_payload(
            &serde_json::to_vec(&fixture.raw(
                "UserPromptSubmit",
                json!({"prompt": "escape", "prompt_id": Uuid::new_v4().to_string()}),
            ))
            .unwrap(),
        )
        .unwrap();
        let prompt_output = session_mismatch_output(&prompt);
        assert_eq!(prompt_output["decision"], "block");
        assert!(prompt_output.get("reason").is_some());

        let start = parse_hook_payload(
            &serde_json::to_vec(&fixture.raw("SessionStart", json!({"source": "clear"}))).unwrap(),
        )
        .unwrap();
        assert!(is_native_session_reset(&start));
        let start_output = native_session_reset_output();
        assert_eq!(start_output["continue"], false);
        assert!(start_output.get("systemMessage").is_some());
    }

    #[test]
    fn child_exit_without_session_end_stays_open_and_is_not_falsely_captured() {
        let fixture = Fixture::new();
        fixture.journal_launch();
        assert!(!record_native_child_exit(&fixture.store, fixture.launch_id, Some(0)).unwrap());
        let launch = fixture
            .store
            .native_launch(fixture.launch_id)
            .unwrap()
            .unwrap();
        assert_eq!(launch.state, NativeLaunchState::Exited);
        assert!(
            fixture
                .store
                .open_native_launches(fixture.session.id)
                .unwrap()
                .iter()
                .any(|open| open.id == fixture.launch_id)
        );
    }

    #[test]
    fn post_spawn_claude_error_without_pid_receipt_stays_uncertain() {
        let fixture = Fixture::new();
        fixture.journal_launch();
        let error = anyhow!("simulated post-spawn containment failure");

        mark_claude_launch_after_error(
            &fixture.store,
            &Config::default(),
            fixture.launch_id,
            &error,
            true,
            None,
        )
        .unwrap();

        let launch = fixture
            .store
            .native_launch(fixture.launch_id)
            .unwrap()
            .unwrap();
        assert_eq!(launch.state, NativeLaunchState::Uncertain);
        assert!(launch.child_pid.is_none());
        assert!(
            fixture
                .store
                .open_native_launches(fixture.session.id)
                .unwrap()
                .iter()
                .any(|open| open.id == fixture.launch_id)
        );
    }

    #[test]
    fn recorded_pid_forces_uncertain_even_without_error_marker() {
        let fixture = Fixture::new();
        fixture.journal_launch();
        fixture
            .store
            .record_native_launch_pid(fixture.launch_id, 424_242)
            .unwrap();
        let error = anyhow!("simulated unclassified failure");

        mark_claude_launch_after_error(
            &fixture.store,
            &Config::default(),
            fixture.launch_id,
            &error,
            false,
            None,
        )
        .unwrap();

        assert_eq!(
            fixture
                .store
                .native_launch(fixture.launch_id)
                .unwrap()
                .unwrap()
                .state,
            NativeLaunchState::Uncertain
        );
    }
}
