use std::{fs, io, path::Path};

use serde_json::{Value, json};

use crate::{
    ApprovalRespondParams, CURRENT_PROTOCOL_VERSION, EnsureSessionParams, InitializeParams,
    InitializeResult, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, PluginManifest,
    ProbeResult, StartTurnParams, SyncContextParams, TurnEventNotification,
};

pub fn protocol_schema_bundle() -> Result<Value, serde_json::Error> {
    Ok(json!({
        "protocol_version": CURRENT_PROTOCOL_VERSION,
        "manifest": serde_json::to_value(schemars::schema_for!(PluginManifest))?,
        "request": serde_json::to_value(schemars::schema_for!(JsonRpcRequest))?,
        "response": serde_json::to_value(schemars::schema_for!(JsonRpcResponse))?,
        "notification": serde_json::to_value(schemars::schema_for!(JsonRpcNotification))?,
        "initialize_params": serde_json::to_value(schemars::schema_for!(InitializeParams))?,
        "initialize_result": serde_json::to_value(schemars::schema_for!(InitializeResult))?,
        "probe_result": serde_json::to_value(schemars::schema_for!(ProbeResult))?,
        "ensure_session_params": serde_json::to_value(schemars::schema_for!(EnsureSessionParams))?,
        "sync_context_params": serde_json::to_value(schemars::schema_for!(SyncContextParams))?,
        "start_turn_params": serde_json::to_value(schemars::schema_for!(StartTurnParams))?,
        "turn_event_notification": serde_json::to_value(schemars::schema_for!(TurnEventNotification))?,
        "approval_respond_params": serde_json::to_value(schemars::schema_for!(ApprovalRespondParams))?,
    }))
}

pub fn write_protocol_schema(path: impl AsRef<Path>) -> Result<(), SchemaWriteError> {
    let value = protocol_schema_bundle()?;
    let bytes = serde_json::to_vec_pretty(&value)?;
    fs::write(path, bytes).map_err(SchemaWriteError::from)
}

#[derive(Debug, thiserror::Error)]
pub enum SchemaWriteError {
    #[error("failed to generate plugin schema: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to write plugin schema: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_contains_versioned_public_contracts() {
        let schema = protocol_schema_bundle().unwrap();
        assert_eq!(schema["protocol_version"], CURRENT_PROTOCOL_VERSION);
        assert!(schema["manifest"].is_object());
        assert!(schema["turn_event_notification"].is_object());
    }
}
