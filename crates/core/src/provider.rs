use std::{collections::BTreeMap, path::PathBuf, pin::Pin};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    AgentEvent, ApprovalDecision, ApprovalId, CanonicalEvent, ProviderSessionId, SessionContext,
    TurnExecutionMode, TurnRequest, TurnStatus,
};

#[derive(
    Clone, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case", tag = "kind", content = "name")]
pub enum ProviderKind {
    Codex,
    Claude,
    Plugin(String),
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Codex => f.write_str("codex"),
            Self::Claude => f.write_str("claude"),
            Self::Plugin(name) => write!(f, "plugin:{name}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ProviderStatus {
    Unknown,
    Ready,
    Warning,
    Exhausted { resets_at: Option<DateTime<Utc>> },
    Overloaded,
    AuthError,
    Offline,
    Incompatible,
}

impl ProviderStatus {
    pub fn available(&self) -> bool {
        matches!(self, Self::Ready | Self::Warning)
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ProviderHealth {
    pub provider: ProviderKind,
    pub status: ProviderStatus,
    pub version: Option<String>,
    pub capabilities: BTreeMap<String, bool>,
    pub usage: Option<UsageSnapshot>,
    pub rate_limit: Option<RateLimitSnapshot>,
    pub checked_at: DateTime<Utc>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct UsageSnapshot {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct RateLimitSnapshot {
    pub utilization: Option<f64>,
    pub window_seconds: Option<u64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct NativeSession {
    pub id: ProviderSessionId,
    pub provider: ProviderKind,
    pub native_session_id: String,
    pub native_version: Option<String>,
    pub capabilities: BTreeMap<String, bool>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct SyncBatch {
    pub from_seq_exclusive: u64,
    pub through_seq_inclusive: u64,
    pub projection_version: u32,
    pub events: Vec<CanonicalEvent>,
    pub handoff: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct SyncReceipt {
    pub through_seq: u64,
    pub projection_version: u32,
    pub native_receipt: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeEffectStatus {
    Completed,
    Failed,
    Declined,
}

/// A provider-owned transcript read through an official native history API.
/// Private reasoning is intentionally excluded by adapters.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NativeTranscript {
    pub provider: ProviderKind,
    pub native_session_id: String,
    pub workspace_cwd: PathBuf,
    pub turns: Vec<NativeTranscriptTurn>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NativeTranscriptTurn {
    pub native_turn_id: String,
    pub status: TurnStatus,
    pub items: Vec<NativeTranscriptItem>,
}

/// Metadata for non-text input or generated artifacts. Provider adapters must
/// omit the original bytes, URL, and absolute path and retain only safe display
/// metadata plus a content-addressed digest.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NativeAttachment {
    pub kind: String,
    pub name: Option<String>,
    pub media_type: Option<String>,
    pub source_digest: String,
    pub metadata: serde_json::Value,
    pub omitted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NativeTranscriptItem {
    UserPrompt {
        native_item_id: String,
        text: String,
        #[serde(default)]
        attachments: Vec<NativeAttachment>,
        raw: serde_json::Value,
    },
    AssistantMessage {
        native_item_id: String,
        text: String,
        final_answer: bool,
        raw: serde_json::Value,
    },
    Plan {
        native_item_id: String,
        text: String,
        raw: serde_json::Value,
    },
    Command {
        native_item_id: String,
        command: String,
        cwd: Option<String>,
        exit_code: Option<i32>,
        status: NativeEffectStatus,
        raw: serde_json::Value,
    },
    FilesChanged {
        native_item_id: String,
        paths: Vec<String>,
        status: NativeEffectStatus,
        raw: serde_json::Value,
    },
    ToolCall {
        native_item_id: String,
        name: String,
        input_summary: serde_json::Value,
        status: NativeEffectStatus,
        output_digest: Option<String>,
        #[serde(default)]
        artifacts: Vec<NativeAttachment>,
        may_have_side_effects: bool,
        raw: serde_json::Value,
    },
    ContextMarker {
        native_item_id: String,
        marker_kind: String,
        summary: String,
        content_digest: Option<String>,
        raw: serde_json::Value,
    },
}

pub type ProviderEventStream =
    Pin<Box<dyn Stream<Item = Result<AgentEvent, ProviderError>> + Send>>;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider binary not found: {0}")]
    BinaryNotFound(String),
    #[error("provider is not authenticated: {0}")]
    Authentication(String),
    #[error("provider rate limit exceeded")]
    RateLimited { resets_at: Option<DateTime<Utc>> },
    #[error("provider is overloaded: {0}")]
    Overloaded(String),
    #[error("provider protocol is incompatible: {0}")]
    Incompatible(String),
    #[error("provider protocol error: {0}")]
    Protocol(String),
    #[error("provider process error: {0}")]
    Process(String),
    #[error("provider turn was interrupted")]
    Interrupted,
    #[error("provider I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait AgentProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn supports_turn_mode(&self, mode: TurnExecutionMode) -> bool {
        mode == TurnExecutionMode::ReadWrite
    }
    async fn probe(&self) -> Result<ProviderHealth, ProviderError>;
    async fn ensure_session(
        &self,
        context: &SessionContext,
    ) -> Result<NativeSession, ProviderError>;

    /// Reattaches a persisted native session after an agentctl process restart.
    /// Providers that do not require an explicit resume handshake may accept
    /// the record unchanged.
    async fn restore_session(
        &self,
        _context: &SessionContext,
        persisted: NativeSession,
    ) -> Result<NativeSession, ProviderError> {
        Ok(persisted)
    }

    /// Validates and attaches a pre-existing native provider session through
    /// the provider's supported resume interface. This does not import the
    /// native transcript into the canonical event log.
    async fn attach_existing_session(
        &self,
        _context: &SessionContext,
        _native_session_id: &str,
    ) -> Result<NativeSession, ProviderError> {
        Err(ProviderError::Incompatible(
            "provider does not support attaching existing native sessions".to_owned(),
        ))
    }

    /// Reads a provider-owned transcript through a documented native API.
    /// Providers without a stable history API must fail closed rather than
    /// inspect or edit private transcript files.
    async fn read_native_history(
        &self,
        _session: &NativeSession,
    ) -> Result<NativeTranscript, ProviderError> {
        Err(ProviderError::Incompatible(
            "provider does not expose a supported native history API".to_owned(),
        ))
    }
    async fn sync_context(
        &self,
        session: &NativeSession,
        batch: SyncBatch,
    ) -> Result<SyncReceipt, ProviderError>;
    async fn run_turn(
        &self,
        session: &NativeSession,
        request: TurnRequest,
    ) -> Result<ProviderEventStream, ProviderError>;
    async fn interrupt(
        &self,
        session: &NativeSession,
        native_turn_id: &str,
    ) -> Result<(), ProviderError>;

    /// Responds to a server-initiated approval when the provider exposes a
    /// programmatic approval channel. Providers without this capability must
    /// fail closed.
    async fn respond_approval(
        &self,
        _session: &NativeSession,
        _approval_id: ApprovalId,
        _decision: ApprovalDecision,
    ) -> Result<(), ProviderError> {
        Err(ProviderError::Incompatible(
            "provider does not expose programmatic approvals".to_owned(),
        ))
    }

    async fn shutdown(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}
