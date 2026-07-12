use std::path::PathBuf;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    ApprovalId, EventId, ProviderError, ProviderKind, RateLimitSnapshot, TurnId, TurnStatus,
    UnifiedSessionId, UsageSnapshot,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventVisibility {
    #[default]
    User,
    Projection,
    Internal,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct CanonicalEvent {
    pub schema_version: u32,
    pub session_id: UnifiedSessionId,
    pub seq: u64,
    pub event_id: EventId,
    pub turn_id: Option<TurnId>,
    pub origin_provider: Option<ProviderKind>,
    pub kind: String,
    pub visibility: EventVisibility,
    pub payload: serde_json::Value,
    pub content_hash: String,
    pub raw_event_id: Option<EventId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AgentEvent {
    SessionStarted {
        native_session_id: String,
    },
    AssistantTextDelta {
        text: String,
    },
    AssistantFinal {
        text: String,
    },
    PlanUpdated {
        steps: Vec<PlanStep>,
    },
    ToolStarted {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolCompleted {
        id: String,
        output: ToolOutput,
    },
    CommandStarted {
        id: String,
        command: String,
        cwd: PathBuf,
    },
    CommandCompleted {
        id: String,
        exit_code: Option<i32>,
        output_digest: Option<String>,
    },
    FilesChanged {
        changes: Vec<FileChange>,
    },
    ApprovalRequested {
        request: ApprovalRequest,
    },
    UsageUpdated {
        usage: UsageSnapshot,
    },
    RateLimitUpdated {
        limit: RateLimitSnapshot,
    },
    Error {
        error: NormalizedProviderError,
    },
    TurnCompleted {
        status: TurnStatus,
    },
    ProviderSpecific {
        provider: ProviderKind,
        kind: String,
        payload: serde_json::Value,
    },
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct PlanStep {
    pub text: String,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ToolOutput {
    pub summary: Option<String>,
    pub digest: Option<String>,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: FileChangeKind,
    pub digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub provider: ProviderKind,
    pub session_id: UnifiedSessionId,
    pub turn_id: TurnId,
    pub action: ApprovalAction,
    pub risk: RiskLevel,
    pub cwd: Option<PathBuf>,
    pub command: Option<String>,
    pub files: Vec<PathBuf>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ApprovalAction {
    Command,
    FileChange,
    Network { host: Option<String> },
    McpTool { server: String, tool: String },
    Permission { name: String },
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny,
    CancelTurn,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct NormalizedProviderError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl From<&ProviderError> for NormalizedProviderError {
    fn from(error: &ProviderError) -> Self {
        let retryable = matches!(
            error,
            ProviderError::RateLimited { .. }
                | ProviderError::Overloaded(_)
                | ProviderError::Process(_)
                | ProviderError::Io(_)
        );
        Self {
            code: match error {
                ProviderError::BinaryNotFound(_) => "binary_not_found",
                ProviderError::Authentication(_) => "authentication_failed",
                ProviderError::RateLimited { .. } => "rate_limit",
                ProviderError::Overloaded(_) => "overloaded",
                ProviderError::Incompatible(_) => "incompatible",
                ProviderError::Protocol(_) => "protocol",
                ProviderError::Process(_) => "process",
                ProviderError::Interrupted => "interrupted",
                ProviderError::Io(_) => "io",
            }
            .to_owned(),
            message: error.to_string(),
            retryable,
        }
    }
}
