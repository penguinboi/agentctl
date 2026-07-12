use std::path::PathBuf;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ProviderKind, UnifiedSessionId};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    #[default]
    NativeLocal,
    ApiKey,
    EnterpriseApproved,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct SessionContext {
    pub unified_session_id: UnifiedSessionId,
    pub workspace_root: PathBuf,
    pub workspace_fingerprint: String,
    pub auth_mode: AuthMode,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct UnifiedSession {
    pub id: UnifiedSessionId,
    pub name: String,
    pub workspace_path: PathBuf,
    pub workspace_fingerprint: String,
    pub active_provider: Option<ProviderKind>,
    pub routing_policy: String,
    pub auth_mode: AuthMode,
    pub status: SessionStatus,
    pub parent_session_id: Option<UnifiedSessionId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub schema_version: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Idle,
    Recovering,
    Archived,
}
