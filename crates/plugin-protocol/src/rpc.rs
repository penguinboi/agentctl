use std::collections::BTreeMap;

use agentctl_core::{
    AgentEvent, ApprovalDecision, ApprovalId, NativeSession, ProviderHealth, SessionContext,
    SyncBatch, SyncReceipt, TurnRequest,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MIN_PROTOCOL_VERSION: u32 = 1;
pub const CURRENT_PROTOCOL_VERSION: u32 = 1;
pub const JSONRPC_VERSION: &str = "2.0";

pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_PROBE: &str = "provider/probe";
pub const METHOD_ENSURE_SESSION: &str = "session/ensure";
pub const METHOD_SYNC_CONTEXT: &str = "context/sync";
pub const METHOD_START_TURN: &str = "turn/start";
pub const METHOD_TURN_EVENT: &str = "turn/event";
pub const METHOD_INTERRUPT_TURN: &str = "turn/interrupt";
pub const METHOD_APPROVAL_RESPOND: &str = "approval/respond";
pub const METHOD_SHUTDOWN: &str = "shutdown";

#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    String(String),
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl JsonRpcRequest {
    pub fn new(id: RequestId, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcErrorObject>,
}

impl JsonRpcResponse {
    pub fn success(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: RequestId, error: RpcErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct RpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcErrorObject {
    pub const PARSE_ERROR: i64 = -32_700;
    pub const INVALID_REQUEST: i64 = -32_600;
    pub const METHOD_NOT_FOUND: i64 = -32_601;
    pub const INVALID_PARAMS: i64 = -32_602;
    pub const INTERNAL_ERROR: i64 = -32_603;
    pub const NOT_INITIALIZED: i64 = -32_000;
    pub const INCOMPATIBLE: i64 = -32_001;
    pub const PROVIDER_ERROR: i64 = -32_100;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct PeerInfo {
    pub name: String,
    pub version: String,
}

// Capability negotiation is a wire-level feature bitmap, so independent booleans are explicit.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct PluginCapabilities {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub resume: bool,
    #[serde(default)]
    pub context_sync: bool,
    #[serde(default)]
    pub approvals: bool,
    #[serde(default)]
    pub rate_limits: bool,
    #[serde(default)]
    pub interruption: bool,
    #[serde(default)]
    pub extensions: BTreeMap<String, bool>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct InitializeParams {
    pub protocol_versions: Vec<u32>,
    pub client: PeerInfo,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct InitializeResult {
    pub protocol_version: u32,
    pub server: PeerInfo,
    pub provider_name: String,
    pub capabilities: PluginCapabilities,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ProbeResult {
    pub health: ProviderHealth,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct EnsureSessionParams {
    pub context: SessionContext,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct EnsureSessionResult {
    pub session: NativeSession,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct SyncContextParams {
    pub session: NativeSession,
    pub batch: SyncBatch,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct SyncContextResult {
    pub receipt: SyncReceipt,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct StartTurnParams {
    pub session: NativeSession,
    pub request: TurnRequest,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct StartTurnResult {
    pub native_turn_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct TurnEventNotification {
    pub native_session_id: String,
    pub native_turn_id: String,
    pub event: AgentEvent,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct InterruptTurnParams {
    pub session: NativeSession,
    pub native_turn_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ApprovalRespondParams {
    pub native_session_id: String,
    pub approval_id: ApprovalId,
    pub decision: ApprovalDecision,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct EmptyResult {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_envelopes_use_exact_version_and_shape() {
        let request =
            JsonRpcRequest::new(RequestId::Number(7), METHOD_PROBE, serde_json::json!({}));
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["jsonrpc"], JSONRPC_VERSION);
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], METHOD_PROBE);

        let response = JsonRpcResponse::success(RequestId::Number(7), Value::Null);
        let value = serde_json::to_value(response).unwrap();
        assert!(value.get("error").is_none());
        assert!(value.get("result").is_some());
    }
}
