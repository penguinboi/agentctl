//! Deterministic provider doubles and protocol replay helpers.

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use agentctl_core::{
    AgentEvent, AgentProvider, NativeSession, ProviderError, ProviderEventStream, ProviderHealth,
    ProviderKind, ProviderSessionId, ProviderStatus, SessionContext, SyncBatch, SyncReceipt,
    TurnExecutionMode, TurnRequest, TurnStatus,
};
use async_trait::async_trait;
use chrono::Utc;
use futures::stream;
use tokio::sync::{Mutex, Notify};

/// Deterministic gate for exercising cancellation while a provider probe is in flight.
#[derive(Clone, Debug, Default)]
pub struct FakeProbeGate {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl FakeProbeGate {
    pub async fn wait_started(&self) {
        self.started.notified().await;
    }

    pub fn release(&self) {
        self.release.notify_waiters();
    }

    async fn block(&self) {
        self.started.notify_one();
        self.release.notified().await;
    }
}

#[derive(Clone, Debug)]
pub enum FakeAction {
    Event(AgentEvent),
    Error(FakeError),
    Delay(Duration),
    WriteFile { path: PathBuf, contents: String },
}

#[derive(Clone, Debug)]
pub enum FakeError {
    RateLimited,
    Overloaded,
    Interrupted,
    Process(String),
    Protocol(String),
}

impl FakeError {
    fn into_provider(self) -> ProviderError {
        match self {
            Self::RateLimited => ProviderError::RateLimited { resets_at: None },
            Self::Overloaded => ProviderError::Overloaded("fake overload".to_owned()),
            Self::Interrupted => ProviderError::Interrupted,
            Self::Process(message) => ProviderError::Process(message),
            Self::Protocol(message) => ProviderError::Protocol(message),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FakeProvider {
    kind: ProviderKind,
    turns: Arc<Mutex<VecDeque<Vec<FakeAction>>>>,
    syncs: Arc<Mutex<Vec<SyncBatch>>>,
    interrupted: Arc<Mutex<Vec<String>>>,
    restored: Arc<Mutex<Vec<NativeSession>>>,
    shutdowns: Arc<Mutex<u64>>,
    health: Arc<Mutex<ProviderStatus>>,
    requests: Arc<Mutex<Vec<TurnRequest>>>,
    read_only_support: bool,
    fixed_sync_receipt: Option<String>,
    run_turn_error: Option<FakeError>,
    probe_gate: Option<FakeProbeGate>,
    ensure_session_gate: Option<FakeProbeGate>,
    sync_gate: Option<FakeProbeGate>,
    interrupt_gate: Option<FakeProbeGate>,
    shutdown_gate: Option<FakeProbeGate>,
    shutdown_error: Option<FakeError>,
}

impl FakeProvider {
    pub fn new(kind: ProviderKind, actions: Vec<FakeAction>) -> Self {
        Self::scripted(kind, vec![actions])
    }

    pub fn scripted(kind: ProviderKind, turns: Vec<Vec<FakeAction>>) -> Self {
        Self {
            kind,
            turns: Arc::new(Mutex::new(turns.into())),
            syncs: Arc::new(Mutex::new(Vec::new())),
            interrupted: Arc::new(Mutex::new(Vec::new())),
            restored: Arc::new(Mutex::new(Vec::new())),
            shutdowns: Arc::new(Mutex::new(0)),
            health: Arc::new(Mutex::new(ProviderStatus::Ready)),
            requests: Arc::new(Mutex::new(Vec::new())),
            read_only_support: false,
            fixed_sync_receipt: None,
            run_turn_error: None,
            probe_gate: None,
            ensure_session_gate: None,
            sync_gate: None,
            interrupt_gate: None,
            shutdown_gate: None,
            shutdown_error: None,
        }
    }

    #[must_use]
    pub fn with_read_only_support(mut self) -> Self {
        self.read_only_support = true;
        self
    }

    #[must_use]
    pub fn with_status(mut self, status: ProviderStatus) -> Self {
        self.health = Arc::new(Mutex::new(status));
        self
    }

    #[must_use]
    pub fn with_sync_receipt(mut self, receipt: impl Into<String>) -> Self {
        self.fixed_sync_receipt = Some(receipt.into());
        self
    }

    #[must_use]
    pub fn with_run_turn_error(mut self, error: FakeError) -> Self {
        self.run_turn_error = Some(error);
        self
    }

    #[must_use]
    pub fn with_probe_gate(mut self, gate: FakeProbeGate) -> Self {
        self.probe_gate = Some(gate);
        self
    }

    #[must_use]
    pub fn with_ensure_session_gate(mut self, gate: FakeProbeGate) -> Self {
        self.ensure_session_gate = Some(gate);
        self
    }

    #[must_use]
    pub fn with_sync_gate(mut self, gate: FakeProbeGate) -> Self {
        self.sync_gate = Some(gate);
        self
    }

    #[must_use]
    pub fn with_interrupt_gate(mut self, gate: FakeProbeGate) -> Self {
        self.interrupt_gate = Some(gate);
        self
    }

    #[must_use]
    pub fn with_shutdown_gate(mut self, gate: FakeProbeGate) -> Self {
        self.shutdown_gate = Some(gate);
        self
    }

    #[must_use]
    pub fn with_shutdown_error(mut self, error: FakeError) -> Self {
        self.shutdown_error = Some(error);
        self
    }

    pub async fn set_status(&self, status: ProviderStatus) {
        *self.health.lock().await = status;
    }

    pub async fn syncs(&self) -> Vec<SyncBatch> {
        self.syncs.lock().await.clone()
    }

    pub async fn interruptions(&self) -> Vec<String> {
        self.interrupted.lock().await.clone()
    }

    pub async fn restores(&self) -> Vec<NativeSession> {
        self.restored.lock().await.clone()
    }

    pub async fn shutdown_count(&self) -> u64 {
        *self.shutdowns.lock().await
    }

    pub async fn requests(&self) -> Vec<TurnRequest> {
        self.requests.lock().await.clone()
    }
}

#[async_trait]
impl AgentProvider for FakeProvider {
    fn kind(&self) -> ProviderKind {
        self.kind.clone()
    }

    fn supports_turn_mode(&self, mode: TurnExecutionMode) -> bool {
        mode == TurnExecutionMode::ReadWrite || self.read_only_support
    }

    async fn probe(&self) -> Result<ProviderHealth, ProviderError> {
        if let Some(gate) = &self.probe_gate {
            gate.block().await;
        }
        Ok(ProviderHealth {
            provider: self.kind(),
            status: self.health.lock().await.clone(),
            version: Some("fake-1".to_owned()),
            capabilities: BTreeMap::from([
                ("streaming".to_owned(), true),
                ("resume".to_owned(), true),
                ("context_sync".to_owned(), true),
            ]),
            usage: None,
            rate_limit: None,
            checked_at: Utc::now(),
            message: None,
        })
    }

    async fn ensure_session(
        &self,
        context: &SessionContext,
    ) -> Result<NativeSession, ProviderError> {
        if let Some(gate) = &self.ensure_session_gate {
            gate.block().await;
        }
        Ok(NativeSession {
            id: ProviderSessionId::new(),
            provider: self.kind(),
            native_session_id: format!("fake-{}-{}", self.kind, context.unified_session_id),
            native_version: Some("fake-1".to_owned()),
            capabilities: BTreeMap::new(),
        })
    }

    async fn sync_context(
        &self,
        _session: &NativeSession,
        batch: SyncBatch,
    ) -> Result<SyncReceipt, ProviderError> {
        if let Some(gate) = &self.sync_gate {
            gate.block().await;
        }
        let receipt = SyncReceipt {
            through_seq: batch.through_seq_inclusive,
            projection_version: batch.projection_version,
            native_receipt: Some(
                self.fixed_sync_receipt
                    .clone()
                    .unwrap_or_else(|| format!("fake:{}", batch.through_seq_inclusive)),
            ),
        };
        self.syncs.lock().await.push(batch);
        Ok(receipt)
    }

    async fn restore_session(
        &self,
        _context: &SessionContext,
        persisted: NativeSession,
    ) -> Result<NativeSession, ProviderError> {
        self.restored.lock().await.push(persisted.clone());
        Ok(persisted)
    }

    async fn run_turn(
        &self,
        _session: &NativeSession,
        request: TurnRequest,
    ) -> Result<ProviderEventStream, ProviderError> {
        self.requests.lock().await.push(request.clone());
        if let Some(error) = self.run_turn_error.clone() {
            return Err(error.into_provider());
        }
        let actions = self.turns.lock().await.pop_front().unwrap_or_default();
        let output = stream::unfold(
            (actions.into_iter(), request.cwd),
            |(mut actions, cwd)| async move {
                loop {
                    match actions.next()? {
                        FakeAction::Event(event) => return Some((Ok(event), (actions, cwd))),
                        FakeAction::Error(error) => {
                            return Some((Err(error.into_provider()), (actions, cwd)));
                        }
                        FakeAction::Delay(delay) => tokio::time::sleep(delay).await,
                        FakeAction::WriteFile { path, contents } => {
                            if path.is_absolute()
                                || path.components().any(|component| {
                                    matches!(component, std::path::Component::ParentDir)
                                })
                            {
                                return Some((
                                    Err(ProviderError::Protocol(
                                        "fake write path escaped the workspace".to_owned(),
                                    )),
                                    (actions, cwd),
                                ));
                            }
                            let destination = cwd.join(&path);
                            if let Some(parent) = destination.parent()
                                && let Err(error) = std::fs::create_dir_all(parent)
                            {
                                return Some((Err(ProviderError::Io(error)), (actions, cwd)));
                            }
                            if let Err(error) = std::fs::write(&destination, contents) {
                                return Some((Err(ProviderError::Io(error)), (actions, cwd)));
                            }
                            return Some((
                                Ok(AgentEvent::FilesChanged {
                                    changes: vec![agentctl_core::FileChange {
                                        path,
                                        kind: agentctl_core::FileChangeKind::Modified,
                                        digest: None,
                                    }],
                                }),
                                (actions, cwd),
                            ));
                        }
                    }
                }
            },
        );
        Ok(Box::pin(output))
    }

    async fn interrupt(
        &self,
        _session: &NativeSession,
        native_turn_id: &str,
    ) -> Result<(), ProviderError> {
        self.interrupted
            .lock()
            .await
            .push(native_turn_id.to_owned());
        if let Some(gate) = &self.interrupt_gate {
            gate.block().await;
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), ProviderError> {
        if let Some(gate) = &self.shutdown_gate {
            gate.block().await;
        }
        let mut shutdowns = self.shutdowns.lock().await;
        *shutdowns = shutdowns.saturating_add(1);
        if let Some(error) = self.shutdown_error.clone() {
            return Err(error.into_provider());
        }
        Ok(())
    }
}

pub fn successful_turn(text: impl Into<String>) -> Vec<FakeAction> {
    vec![
        FakeAction::Event(AgentEvent::AssistantTextDelta { text: text.into() }),
        FakeAction::Event(AgentEvent::TurnCompleted {
            status: TurnStatus::Completed,
        }),
    ]
}

pub fn replay_jsonl<T: serde::de::DeserializeOwned>(
    input: &str,
) -> Result<Vec<T>, serde_json::Error> {
    input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect()
}

#[derive(Clone, Debug)]
pub enum ReplayItem<T> {
    Event { line: usize, value: T },
    Malformed { line: usize, error: String },
}

/// Replays every non-empty frame without aborting at malformed JSON. This is
/// useful for parser chaos tests where later frames must remain observable.
pub fn replay_jsonl_lossy<T: serde::de::DeserializeOwned>(input: &str) -> Vec<ReplayItem<T>> {
    input
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| match serde_json::from_str(line) {
            Ok(value) => ReplayItem::Event {
                line: index + 1,
                value,
            },
            Err(error) => ReplayItem::Malformed {
                line: index + 1,
                error: error.to_string(),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentctl_core::{AuthMode, UnifiedSessionId};
    use futures::StreamExt;
    use std::path::PathBuf;

    #[tokio::test]
    async fn records_idempotency_inputs_for_sync_tests() {
        let provider = FakeProvider::new(ProviderKind::Codex, successful_turn("ok"));
        let context = SessionContext {
            unified_session_id: UnifiedSessionId::new(),
            workspace_root: PathBuf::from("/tmp/test"),
            workspace_fingerprint: "workspace".to_owned(),
            auth_mode: AuthMode::NativeLocal,
        };
        let session = provider.ensure_session(&context).await.unwrap();
        let stream = provider
            .run_turn(
                &session,
                TurnRequest {
                    session_id: context.unified_session_id,
                    turn_id: agentctl_core::TurnId::new(),
                    prompt: "test".to_owned(),
                    cwd: context.workspace_root,
                    continuation: false,
                    execution_mode: TurnExecutionMode::ReadWrite,
                    metadata: serde_json::Value::Null,
                },
            )
            .await
            .unwrap();
        assert_eq!(stream.collect::<Vec<_>>().await.len(), 2);
    }

    #[test]
    fn versioned_root_fixtures_replay_without_provider_quota() {
        let codex = include_str!("../../../fixtures/codex/0.144/events.jsonl");
        let claude = include_str!("../../../fixtures/claude/2.1/events.jsonl");
        assert_eq!(replay_jsonl::<serde_json::Value>(codex).unwrap().len(), 3);
        assert_eq!(replay_jsonl::<serde_json::Value>(claude).unwrap().len(), 3);
    }

    #[test]
    fn lossy_replay_keeps_frames_after_malformed_json() {
        let frames =
            replay_jsonl_lossy::<serde_json::Value>("{\"seq\":1}\nnot-json\n{\"seq\":3}\n");
        assert!(matches!(frames[0], ReplayItem::Event { line: 1, .. }));
        assert!(matches!(frames[1], ReplayItem::Malformed { line: 2, .. }));
        assert!(matches!(frames[2], ReplayItem::Event { line: 3, .. }));
    }

    #[test]
    fn chaos_fixture_preserves_out_of_order_duplicates_and_recovers_after_malformed_frame() {
        let frames = replay_jsonl_lossy::<serde_json::Value>(include_str!(
            "../../../fixtures/chaos/provider-frames.jsonl"
        ));
        assert_eq!(frames.len(), 5);
        let observed = frames
            .iter()
            .filter_map(|item| match item {
                ReplayItem::Event { value, .. } => {
                    value.get("seq").and_then(serde_json::Value::as_u64)
                }
                ReplayItem::Malformed { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, vec![2, 1, 1, 3]);
        assert!(matches!(frames[2], ReplayItem::Malformed { line: 3, .. }));
        assert!(matches!(frames[4], ReplayItem::Event { line: 5, .. }));
    }

    #[tokio::test]
    async fn fake_provider_can_emit_duplicate_out_of_order_frames_and_continue_after_error() {
        let frame = |seq| AgentEvent::ProviderSpecific {
            provider: ProviderKind::Codex,
            kind: "chaos_frame".to_owned(),
            payload: serde_json::json!({"seq": seq}),
        };
        let provider = FakeProvider::new(
            ProviderKind::Codex,
            vec![
                FakeAction::Event(frame(2)),
                FakeAction::Event(frame(1)),
                FakeAction::Event(frame(1)),
                FakeAction::Error(FakeError::Protocol("malformed JSON".to_owned())),
                FakeAction::Event(frame(3)),
            ],
        );
        let context = SessionContext {
            unified_session_id: UnifiedSessionId::new(),
            workspace_root: PathBuf::from("/tmp/test"),
            workspace_fingerprint: "workspace".to_owned(),
            auth_mode: AuthMode::NativeLocal,
        };
        let session = provider.ensure_session(&context).await.unwrap();
        let stream = provider
            .run_turn(
                &session,
                TurnRequest {
                    session_id: context.unified_session_id,
                    turn_id: agentctl_core::TurnId::new(),
                    prompt: "chaos".to_owned(),
                    cwd: context.workspace_root,
                    continuation: false,
                    metadata: serde_json::Value::Null,
                    execution_mode: agentctl_core::TurnExecutionMode::ReadWrite,
                },
            )
            .await
            .unwrap();
        let items = stream.collect::<Vec<_>>().await;
        assert_eq!(items.len(), 5);
        assert!(matches!(items[3], Err(ProviderError::Protocol(_))));
        assert!(matches!(items[4], Ok(AgentEvent::ProviderSpecific { .. })));
    }
}
