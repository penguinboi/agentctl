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
use tokio::sync::{Mutex, RwLock};

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
            let status = if turn.status == TurnStatus::Pending && effects == SideEffectState::None {
                TurnStatus::Failed
            } else {
                TurnStatus::Uncertain
            };
            self.inner.store.update_turn_state(
                turn.id,
                status,
                effects,
                turn.native_turn_id.as_deref(),
                Utc::now(),
            )?;
            let payload = serde_json::json!({
                "previous_status": turn.status,
                "recovered_status": status,
                "side_effect_state": effects,
                "native_bridge": false,
            });
            let event = CanonicalEvent {
                schema_version: 1,
                session_id: session.id,
                seq: 0,
                event_id: EventId::new(),
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

fn sync_receipt_is_durable(native_receipt: Option<&str>) -> bool {
    !matches!(
        native_receipt,
        Some("claude:user-prompt-submit-hook" | "claude:next-prompt-capsule")
    )
}

#[cfg(test)]
mod tests {
    use super::sync_receipt_is_durable;

    #[test]
    fn only_deferred_claude_projection_receipts_are_not_durable() {
        assert!(!sync_receipt_is_durable(Some(
            "claude:user-prompt-submit-hook"
        )));
        assert!(!sync_receipt_is_durable(Some("claude:next-prompt-capsule")));
        assert!(sync_receipt_is_durable(Some("codex:inject-items")));
    }
}
