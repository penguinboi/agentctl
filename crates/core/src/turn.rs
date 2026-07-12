use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ProviderKind, TurnId, UnifiedSessionId};

/// Provider-neutral execution contract for a turn. Read-only review is
/// capability-gated and must fail closed when an adapter cannot enforce it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnExecutionMode {
    #[default]
    ReadWrite,
    ReviewReadOnly,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct TurnRequest {
    pub session_id: UnifiedSessionId,
    pub turn_id: TurnId,
    pub prompt: String,
    pub cwd: PathBuf,
    pub continuation: bool,
    #[serde(default)]
    pub execution_mode: TurnExecutionMode,
    pub metadata: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    #[default]
    Pending,
    Running,
    WaitingOnApproval,
    Completed,
    Interrupted,
    Failed,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectState {
    #[default]
    None,
    Possible,
    Confirmed,
}

impl SideEffectState {
    #[must_use]
    pub fn observe(self, next: Self) -> Self {
        std::cmp::max(self, next)
    }
}

impl Ord for SideEffectState {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (*self as u8).cmp(&(*other as u8))
    }
}

impl PartialOrd for SideEffectState {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct RoutingDecision {
    pub provider: ProviderKind,
    pub policy: String,
    pub score: f64,
    pub reason: String,
    pub replayed: bool,
}
