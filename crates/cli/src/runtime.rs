//! Projection-only runtime used by the native CLI bridge.
//!
//! User prompts are never executed here. The runtime only restores provider
//! sessions, projects canonical context, and shuts provider protocol helpers
//! down before the terminal is handed to the provider's own interactive CLI.

use std::{collections::BTreeMap, sync::Arc};

use agentctl_core::{
    AgentProvider, CanonicalEvent, EventId, EventVisibility, NativeSession, ProviderError,
    ProviderHealth, ProviderKind, ProviderStatus, SessionContext, SideEffectState, TurnId,
    TurnStatus, UnifiedSession,
};
use agentctl_storage::{
    AgentctlStore, ProviderSessionRecord, SyncIntentEntry, SyncReceiptEntry, canonical_content_hash,
};
use agentctl_telemetry::PayloadGuard;
use agentctl_transcript::{
    ContextCheckpoint, HandoffCapsule, IdempotencyLedger, ProjectionRequest, ReceiptKey,
    build_sync_batch, projection_window,
};
use agentctl_workspace::WorkspaceIdentity;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::config::Config;

const EVENT_PAGE_SIZE: usize = 2_000;
pub(crate) const PROVIDER_SHUTDOWN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);

#[derive(Clone)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    store: AgentctlStore,
    session: RwLock<UnifiedSession>,
    providers: BTreeMap<ProviderKind, Arc<dyn AgentProvider>>,
    payload_guard: PayloadGuard,
    operation_guard: Mutex<()>,
}

impl Runtime {
    pub fn new(
        store: AgentctlStore,
        config: &Config,
        session: UnifiedSession,
        providers: BTreeMap<ProviderKind, Arc<dyn AgentProvider>>,
    ) -> Result<Self> {
        if store.has_incomplete_native_import(session.id)? {
            bail!(
                "session {} has an incomplete native transcript import; rerun `agentctl import-native` with the same native session before opening a provider",
                session.id
            );
        }
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                store,
                session: RwLock::new(session),
                providers,
                payload_guard: config.payload_guard()?,
                operation_guard: Mutex::new(()),
            }),
        })
    }

    async fn session(&self) -> UnifiedSession {
        self.inner.session.read().await.clone()
    }

    /// Tears down every helper process. Every provider is attempted even when
    /// another provider's shutdown fails.
    pub async fn shutdown(&self) -> Result<()> {
        let shutdowns = self.inner.providers.values().map(|provider| async move {
            (
                provider.kind(),
                tokio::time::timeout(PROVIDER_SHUTDOWN_TIMEOUT, provider.shutdown()).await,
            )
        });
        let mut failures = Vec::new();
        for (provider, result) in futures::future::join_all(shutdowns).await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("{provider}: {error}")),
                Err(_) => failures.push(format!("{provider}: shutdown timed out")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!("provider teardown failed: {}", failures.join("; "))
        }
    }

    /// Closes turns left by the removed headless engine. This is migration
    /// hygiene only; native turns are recovered through the native launch
    /// journal instead.
    pub fn recover_crashed_turns(&self) -> Result<Vec<TurnId>> {
        let session = self
            .inner
            .session
            .try_read()
            .map_err(|_| anyhow::anyhow!("session is busy during recovery"))?
            .clone();
        let events = self.all_events(session.id)?;
        let mut recovered = Vec::new();
        for turn in self
            .inner
            .store
            .recovery_candidates()?
            .into_iter()
            .filter(|turn| turn.session_id == session.id)
        {
            let effects = events
                .iter()
                .filter(|event| event.turn_id == Some(turn.id))
                .fold(turn.side_effect_state, |state, event| {
                    let observed = match event.kind.as_str() {
                        "files_changed" => SideEffectState::Confirmed,
                        "command_started" | "command_completed" | "tool_started"
                        | "tool_completed" => SideEffectState::Possible,
                        _ => SideEffectState::None,
                    };
                    state.observe(observed)
                });
            // A recovered process is no longer executing. `Failed` closes the
            // operational turn while `effects` and the audit event preserve
            // uncertainty. Leaving it as `Uncertain` would make every runtime
            // boot recover the same turn again and would block native hooks
            // from accepting a fresh prompt.
            let status = TurnStatus::Failed;
            let payload = serde_json::json!({
                "previous_status": turn.status,
                "recovered_status": status,
                "side_effect_state": effects,
                "native_bridge": false,
                "uncertainty_preserved": effects != SideEffectState::None,
                "replay_allowed": false,
            });
            let event_id = deterministic_legacy_recovery_event_id(session.id, turn.id);
            if let Some(existing) = self.inner.store.event_by_id(event_id)? {
                if existing.session_id != session.id
                    || existing.turn_id != Some(turn.id)
                    || existing.kind != "legacy_turn_recovered"
                {
                    bail!("deterministic legacy turn recovery event id collision");
                }
            } else {
                let event = CanonicalEvent {
                    schema_version: 1,
                    session_id: session.id,
                    seq: 0,
                    event_id,
                    turn_id: Some(turn.id),
                    origin_provider: turn.provider,
                    kind: "legacy_turn_recovered".to_owned(),
                    visibility: EventVisibility::Internal,
                    content_hash: canonical_content_hash(
                        "legacy_turn_recovered",
                        EventVisibility::Internal,
                        &payload,
                    )?,
                    payload,
                    raw_event_id: None,
                    created_at: Utc::now(),
                };
                self.inner.store.append_event_allocating_seq(event, None)?;
            }
            self.inner.store.update_turn_state(
                turn.id,
                status,
                effects,
                turn.native_turn_id.as_deref(),
                Utc::now(),
            )?;
            recovered.push(turn.id);
        }
        Ok(recovered)
    }

    /// Projects the canonical delta to every configured provider without
    /// starting a model turn.
    pub async fn sync_all(&self) -> Result<serde_json::Value> {
        let _guard = self.inner.operation_guard.lock().await;
        let session = self.session().await;
        let identity = WorkspaceIdentity::discover(&session.workspace_path)?;
        let latest = self.inner.store.next_seq(session.id)?.saturating_sub(1);
        let context = SessionContext {
            unified_session_id: session.id,
            workspace_root: identity.execution_root().to_path_buf(),
            workspace_fingerprint: identity.fingerprint,
            auth_mode: session.auth_mode,
        };
        let mut results = Vec::new();
        for (kind, provider) in &self.inner.providers {
            if kind == &ProviderKind::Claude {
                // Claude's documented `shouldQuery:false` input has no
                // provider receipt which proves that an enqueued frame reached
                // the native transcript. This runtime is torn down as soon as
                // `sync` returns, so treating the local enqueue as durable can
                // advance the canonical cursor over context that the shutdown
                // races and discards. Keep the delta canonical instead. The
                // next provider-native launch journals it in `native_handoffs`
                // and commits the cursor only after UserPromptSubmit writes
                // the additionalContext response.
                let synced_through = self
                    .inner
                    .store
                    .provider_session(session.id, kind)?
                    .map_or(0, |record| record.last_synced_seq);
                results.push(serde_json::json!({
                    "provider": kind,
                    "synced_through": synced_through,
                    "deferred_through": latest,
                    "delivery": "next_native_user_prompt",
                }));
                continue;
            }
            let result = async {
                let native = self.native_session(provider, kind, &context).await?;
                self.sync_provider(&session, provider, &native, latest)
                    .await?;
                self.inner
                    .store
                    .provider_session(session.id, kind)
                    .map_err(|error| ProviderError::Process(error.to_string()))?
                    .map(|record| record.last_synced_seq)
                    .ok_or_else(|| {
                        ProviderError::Process("provider session disappeared".to_owned())
                    })
            }
            .await;
            results.push(match result {
                Ok(synced) => serde_json::json!({"provider": kind, "synced_through": synced}),
                Err(error) => serde_json::json!({"provider": kind, "error": error.to_string()}),
            });
        }
        Ok(serde_json::json!({"latest_seq": latest, "providers": results}))
    }

    /// Restores one provider projection and applies the canonical delta. It
    /// never starts a model turn.
    pub async fn prepare_native_projection(
        &self,
        provider_kind: &ProviderKind,
    ) -> Result<NativeSession> {
        if provider_kind == &ProviderKind::Claude {
            bail!(
                "Claude native projection is prompt-bound; prepare it through the launch-bound UserPromptSubmit handoff"
            );
        }
        let _guard = self.inner.operation_guard.lock().await;
        let provider = self
            .inner
            .providers
            .get(provider_kind)
            .cloned()
            .context("requested provider is not configured")?;
        let probed = tokio::time::timeout(std::time::Duration::from_secs(20), provider.probe())
            .await
            .map_err(|_| anyhow::anyhow!("provider {provider_kind} health probe timed out"))??;
        let health = self.effective_health(&probed)?;
        let session = self.session().await;
        self.inner.store.record_health(Some(session.id), &health)?;
        if !health.status.available() {
            bail!(
                "provider {provider_kind} is not available for native launch: {:?}",
                health.status
            );
        }
        let identity = WorkspaceIdentity::discover(&session.workspace_path)?;
        let context = SessionContext {
            unified_session_id: session.id,
            workspace_root: identity.execution_root().to_path_buf(),
            workspace_fingerprint: identity.fingerprint,
            auth_mode: session.auth_mode,
        };
        let native = self
            .native_session(&provider, provider_kind, &context)
            .await?;
        let latest = self.inner.store.next_seq(session.id)?.saturating_sub(1);
        self.sync_provider(&session, &provider, &native, latest)
            .await?;
        Ok(native)
    }

    fn effective_health(&self, probed: &ProviderHealth) -> Result<ProviderHealth> {
        let payload = self
            .inner
            .payload_guard
            .process_json(&serde_json::to_value(probed)?)?;
        let mut health: ProviderHealth =
            serde_json::from_value(payload).context("sanitized provider health became invalid")?;
        if let Some(persisted) = self.inner.store.latest_health(&health.provider)?
            && let ProviderStatus::Exhausted { resets_at } = persisted.status
            && resets_at.is_none_or(|reset| reset > Utc::now())
        {
            health.status = ProviderStatus::Exhausted { resets_at };
            health.rate_limit = persisted.rate_limit;
            health.message = persisted.message;
        }
        Ok(health)
    }

    async fn native_session(
        &self,
        provider: &Arc<dyn AgentProvider>,
        kind: &ProviderKind,
        context: &SessionContext,
    ) -> std::result::Result<NativeSession, ProviderError> {
        if let Some(record) = self
            .inner
            .store
            .provider_session(context.unified_session_id, kind)
            .map_err(|error| ProviderError::Process(error.to_string()))?
        {
            let persisted = NativeSession {
                id: record.id,
                provider: record.provider,
                native_session_id: record.native_session_id,
                native_version: record.native_version,
                capabilities: record.capabilities,
            };
            let mut restored = provider.restore_session(context, persisted).await?;
            restored.id = record.id;
            return Ok(restored);
        }
        let native = provider.ensure_session(context).await?;
        let now = Utc::now();
        self.inner
            .store
            .upsert_provider_session(&ProviderSessionRecord {
                id: native.id,
                unified_session_id: context.unified_session_id,
                provider: kind.clone(),
                native_session_id: native.native_session_id.clone(),
                native_version: native.native_version.clone(),
                last_synced_seq: 0,
                status: ProviderStatus::Ready,
                reset_at: None,
                capabilities: native.capabilities.clone(),
                metadata: serde_json::json!({"mode": "native_bridge"}),
                created_at: now,
                updated_at: now,
            })
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        Ok(native)
    }

    #[allow(clippy::too_many_lines)]
    async fn sync_provider(
        &self,
        session: &UnifiedSession,
        provider: &Arc<dyn AgentProvider>,
        native: &NativeSession,
        through_seq: u64,
    ) -> std::result::Result<(), ProviderError> {
        let record = self
            .inner
            .store
            .provider_session(session.id, &native.provider)
            .map_err(|error| ProviderError::Process(error.to_string()))?
            .ok_or_else(|| {
                ProviderError::Process("provider session was not persisted".to_owned())
            })?;
        let pending = self
            .inner
            .store
            .pending_sync_intents(native.id)
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        if !pending.is_empty() {
            return Err(ProviderError::Incompatible(format!(
                "{} projection is uncertain after an unconfirmed send; run `agentctl repair --rebuild-projections`",
                native.provider
            )));
        }
        if through_seq <= record.last_synced_seq {
            return Ok(());
        }
        let events = self
            .all_events(session.id)
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        let checkpoint_record = self
            .inner
            .store
            .latest_checkpoint(session.id)
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        let projection_version = checkpoint_record
            .as_ref()
            .map_or(1, |checkpoint| checkpoint.projection_version);
        let checkpoint = checkpoint_record
            .as_ref()
            .map(|record| serde_json::from_value::<ContextCheckpoint>(record.checkpoint.clone()))
            .transpose()
            .map_err(|error| {
                ProviderError::Protocol(format!("stored context checkpoint is invalid: {error}"))
            })?;
        let mut ledger = IdempotencyLedger::default();
        for event in &events {
            if self
                .inner
                .store
                .has_sync_receipt(native.id, event.event_id, projection_version)
                .map_err(|error| ProviderError::Process(error.to_string()))?
            {
                ledger.record(ReceiptKey {
                    provider_session_id: native.id,
                    canonical_event_id: event.event_id,
                    projection_version,
                });
            }
        }
        let eligible = projection_window(
            &events,
            record.last_synced_seq,
            through_seq,
            checkpoint.as_ref(),
        )
        .map_err(|error| ProviderError::Protocol(error.to_string()))?;
        let source = eligible
            .iter()
            .rev()
            .filter_map(|event| event.origin_provider.clone())
            .find(|source| source != &native.provider)
            .unwrap_or(match native.provider {
                ProviderKind::Codex => ProviderKind::Claude,
                _ => ProviderKind::Codex,
            });
        let checkpoint_context = checkpoint.as_ref().filter(|checkpoint| {
            record.last_synced_seq < checkpoint.through_seq && checkpoint.through_seq <= through_seq
        });
        let handoff = HandoffCapsule::from_events(session.id, &source, &eligible)
            .ok()
            .and_then(|mut capsule| {
                capsule.through_seq = through_seq;
                match checkpoint_context {
                    Some(checkpoint) if capsule.checkpoint.is_none() => {
                        capsule.checkpoint = serde_json::to_string(checkpoint).ok();
                    }
                    None => capsule.checkpoint = None,
                    Some(_) => {}
                }
                if native.provider == ProviderKind::Codex {
                    capsule.retain_effects_only();
                }
                capsule
                    .has_projectable_context()
                    .then(|| capsule.render_xml())
            });
        let batch = build_sync_batch(
            ProjectionRequest {
                provider_session_id: native.id,
                last_synced_seq: record.last_synced_seq,
                through_seq,
                canonical_latest_seq: events
                    .last()
                    .map_or(record.last_synced_seq, |event| event.seq),
                projection_version,
                events: &eligible,
                handoff,
            },
            &ledger,
        )
        .map_err(|error| ProviderError::Protocol(error.to_string()))?;
        let projected = batch.events.clone();
        if projected.is_empty() {
            self.inner
                .store
                .advance_provider_cursor(native.id, through_seq)
                .map_err(|error| ProviderError::Process(error.to_string()))?;
            return Ok(());
        }
        let intents = projected
            .iter()
            .map(|event| SyncIntentEntry {
                canonical_event_id: event.event_id,
                projection_version,
                created_at: Utc::now(),
            })
            .collect::<Vec<_>>();
        self.inner
            .store
            .begin_sync_intents(native.id, &intents)
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        let receipt = provider.sync_context(native, batch).await?;
        if receipt.through_seq != through_seq || receipt.projection_version != projection_version {
            return Err(ProviderError::Protocol(
                "provider returned a mismatched sync receipt; projection remains uncertain"
                    .to_owned(),
            ));
        }
        if !sync_receipt_is_durable(receipt.native_receipt.as_deref()) {
            self.inner
                .store
                .cancel_sync_intents(native.id, &intents)
                .map_err(|error| ProviderError::Process(error.to_string()))?;
            return Ok(());
        }
        let entries = projected
            .iter()
            .map(|event| SyncReceiptEntry {
                canonical_event_id: event.event_id,
                projection_version,
                native_receipt: receipt.native_receipt.clone(),
                state: "applied".to_owned(),
                applied_at: Utc::now(),
            })
            .collect::<Vec<_>>();
        self.inner
            .store
            .finalize_sync_intents(native.id, &entries, receipt.through_seq)
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        Ok(())
    }

    fn all_events(
        &self,
        session_id: agentctl_core::UnifiedSessionId,
    ) -> Result<Vec<CanonicalEvent>> {
        let mut after = 0;
        let mut output = Vec::new();
        loop {
            let page = self
                .inner
                .store
                .list_events(session_id, after, EVENT_PAGE_SIZE)?;
            if page.is_empty() {
                break;
            }
            after = page.last().map_or(after, |event| event.seq);
            let done = page.len() < EVENT_PAGE_SIZE;
            output.extend(page);
            if done {
                break;
            }
        }
        Ok(output)
    }
}

fn deterministic_legacy_recovery_event_id(
    session_id: agentctl_core::UnifiedSessionId,
    turn_id: TurnId,
) -> EventId {
    let material = format!("legacy-turn-recovered\0{session_id}\0{turn_id}");
    let digest = Sha256::digest(material.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    EventId(Uuid::from_bytes(bytes))
}

fn sync_receipt_is_durable(native_receipt: Option<&str>) -> bool {
    !matches!(
        native_receipt,
        Some(
            "claude:should-query-false"
                | "claude:user-prompt-submit-hook"
                | "claude:next-prompt-capsule"
        )
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use agentctl_core::{
        AgentProvider, AuthMode, CanonicalEvent, EventId, EventVisibility, ProviderKind,
        ProviderSessionId, ProviderStatus, SessionStatus, SideEffectState, TurnId, TurnStatus,
        UnifiedSession, UnifiedSessionId,
    };
    use agentctl_storage::{
        NativeHandoffState, NativeLaunchState, ProviderSessionRecord, TurnRecord,
        canonical_content_hash,
    };
    use agentctl_testkit::FakeProvider;
    use agentctl_workspace::WorkspaceIdentity;
    use chrono::Utc;

    use super::{Runtime, sync_receipt_is_durable};
    use crate::{config::Config, operations, paths::AgentctlPaths};

    #[test]
    fn only_deferred_claude_projection_receipts_are_not_durable() {
        assert!(!sync_receipt_is_durable(Some("claude:should-query-false")));
        assert!(!sync_receipt_is_durable(Some(
            "claude:user-prompt-submit-hook"
        )));
        assert!(!sync_receipt_is_durable(Some("claude:next-prompt-capsule")));
        assert!(sync_receipt_is_durable(Some("codex:inject-items")));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn repeated_sync_defers_supported_should_query_until_native_user_prompt() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let paths = AgentctlPaths::resolve(Some(directory.path().join("home"))).unwrap();
        let store = operations::open_store(&paths).unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let identity = WorkspaceIdentity::discover(&workspace).unwrap();
        let now = Utc::now();
        let session = UnifiedSession {
            id: UnifiedSessionId::new(),
            name: "prompt-bound-sync".to_owned(),
            workspace_path: identity.execution_root().to_path_buf(),
            workspace_fingerprint: identity.fingerprint.clone(),
            active_provider: Some(ProviderKind::Claude),
            routing_policy: "manual".to_owned(),
            auth_mode: AuthMode::NativeLocal,
            status: SessionStatus::Idle,
            parent_session_id: None,
            created_at: now,
            updated_at: now,
            schema_version: 1,
        };
        store.create_session(&session).unwrap();
        let payload = serde_json::json!({"text": "Durable Codex handoff"});
        let canonical = CanonicalEvent {
            schema_version: 1,
            session_id: session.id,
            seq: 1,
            event_id: EventId::new(),
            turn_id: None,
            origin_provider: Some(ProviderKind::Codex),
            kind: "assistant_final".to_owned(),
            visibility: EventVisibility::Projection,
            content_hash: canonical_content_hash(
                "assistant_final",
                EventVisibility::Projection,
                &payload,
            )
            .unwrap(),
            payload,
            raw_event_id: None,
            created_at: now,
        };
        store.append_event(&canonical, None).unwrap();
        let provider_session_id = ProviderSessionId::new();
        let native_session_id = uuid::Uuid::new_v4().to_string();
        store
            .upsert_provider_session(&ProviderSessionRecord {
                id: provider_session_id,
                unified_session_id: session.id,
                provider: ProviderKind::Claude,
                native_session_id: native_session_id.clone(),
                native_version: Some("2.1.207".to_owned()),
                last_synced_seq: 0,
                status: ProviderStatus::Ready,
                reset_at: None,
                capabilities: BTreeMap::from([("should_query_false".to_owned(), true)]),
                metadata: serde_json::json!({
                    "mode": "native_interactive",
                    "native_materialized": false,
                }),
                created_at: now,
                updated_at: now,
            })
            .unwrap();

        let fake = FakeProvider::new(ProviderKind::Claude, Vec::new())
            .with_sync_receipt("claude:should-query-false");
        let mut providers: BTreeMap<ProviderKind, Arc<dyn AgentProvider>> = BTreeMap::new();
        providers.insert(ProviderKind::Claude, Arc::new(fake.clone()));
        for _ in 0..2 {
            let runtime = Runtime::new(
                store.clone(),
                &Config::default(),
                session.clone(),
                providers.clone(),
            )
            .unwrap();
            let report = runtime.sync_all().await.unwrap();
            assert_eq!(
                report["providers"][0]["delivery"],
                "next_native_user_prompt"
            );
            assert_eq!(report["providers"][0]["synced_through"], 0);
            assert_eq!(report["providers"][0]["deferred_through"], 1);
            runtime.shutdown().await.unwrap();
        }
        assert!(
            fake.syncs().await.is_empty(),
            "sync must not enqueue Claude context"
        );
        assert_eq!(fake.shutdown_count().await, 2);
        let provider = store
            .provider_session(session.id, &ProviderKind::Claude)
            .unwrap()
            .unwrap();
        assert_eq!(provider.last_synced_seq, 0);
        assert!(
            !store
                .has_sync_receipt(provider_session_id, canonical.event_id, 1)
                .unwrap()
        );

        let fake_claude = directory.path().join("fake-claude");
        std::fs::write(
            &fake_claude,
            "#!/bin/sh\nprintf '%s\\n' '2.1.207 (Claude Code)'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = Config::default();
        config.providers.claude_binary = fake_claude.to_string_lossy().into_owned();
        let prepared = crate::native_hooks::prepare_native_claude(
            &store, &paths, &config, &session, &identity,
        )
        .unwrap();
        let staged = store
            .native_handoff(prepared.launch_id)
            .unwrap()
            .expect("next native launch must journal the pending canonical delta");
        assert_eq!(staged.state, NativeHandoffState::Staged);
        assert_eq!(staged.through_seq, canonical.seq);
        assert!(staged.capsule.contains("Durable Codex handoff"));
        assert_eq!(
            store
                .provider_session(session.id, &ProviderKind::Claude)
                .unwrap()
                .unwrap()
                .last_synced_seq,
            0,
            "cursor advances only after UserPromptSubmit commits delivery"
        );
        store
            .update_native_launch(
                prepared.launch_id,
                NativeLaunchState::Failed,
                None,
                Some("test cleanup"),
                Utc::now(),
            )
            .unwrap();
        std::fs::remove_file(prepared.settings_path).unwrap();
    }

    #[test]
    fn repeated_runtime_recovery_closes_turn_once_without_growing_event_log() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AgentctlPaths::resolve(Some(directory.path().join("home"))).unwrap();
        let store = operations::open_store(&paths).unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let now = Utc::now();
        let session = UnifiedSession {
            id: UnifiedSessionId::new(),
            name: "runtime-recovery".to_owned(),
            workspace_path: workspace,
            workspace_fingerprint: "sha256:runtime-recovery".to_owned(),
            active_provider: Some(ProviderKind::Claude),
            routing_policy: "sticky-balanced".to_owned(),
            auth_mode: AuthMode::NativeLocal,
            status: SessionStatus::Active,
            parent_session_id: None,
            created_at: now,
            updated_at: now,
            schema_version: 1,
        };
        store.create_session(&session).unwrap();
        let prompt = serde_json::json!({"text": "legacy prompt"});
        store
            .append_event(
                &CanonicalEvent {
                    schema_version: 1,
                    session_id: session.id,
                    seq: 1,
                    event_id: EventId::new(),
                    turn_id: None,
                    origin_provider: Some(ProviderKind::Claude),
                    kind: "user_prompt".to_owned(),
                    visibility: EventVisibility::User,
                    content_hash: canonical_content_hash(
                        "user_prompt",
                        EventVisibility::User,
                        &prompt,
                    )
                    .unwrap(),
                    payload: prompt,
                    raw_event_id: None,
                    created_at: now,
                },
                None,
            )
            .unwrap();
        let turn_id = TurnId::new();
        store
            .create_turn(&TurnRecord {
                id: turn_id,
                session_id: session.id,
                provider: Some(ProviderKind::Claude),
                prompt_seq: 1,
                status: TurnStatus::Uncertain,
                side_effect_state: SideEffectState::Possible,
                native_turn_id: Some("legacy-native-turn".to_owned()),
                continuation: false,
                started_at: Some(now),
                completed_at: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let session_id = session.id;
        let runtime = Runtime::new(
            store.clone(),
            &Config::default(),
            session.clone(),
            BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(runtime.recover_crashed_turns().unwrap(), vec![turn_id]);
        let after_first = store.next_seq(session.id).unwrap();
        assert_eq!(
            store.get_turn(turn_id).unwrap().unwrap().status,
            TurnStatus::Failed
        );

        let reopened =
            Runtime::new(store.clone(), &Config::default(), session, BTreeMap::new()).unwrap();
        assert!(reopened.recover_crashed_turns().unwrap().is_empty());
        assert_eq!(store.next_seq(session_id).unwrap(), after_first);
        assert_eq!(
            store
                .list_events(session_id, 0, usize::MAX)
                .unwrap()
                .iter()
                .filter(|event| event.kind == "legacy_turn_recovered")
                .count(),
            1
        );
    }
}
