use std::path::PathBuf;

use agentctl_core::{
    ApprovalAction, ApprovalDecision, ApprovalId, ApprovalRequest, ProviderKind, RiskLevel, TurnId,
    UnifiedSessionId,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ApprovalError, ApprovalGateway};

pub const MCP_PERMISSION_PROTOCOL_VERSION: u32 = 1;

/// Stable wire request used by a Claude permission-prompt MCP tool or another provider bridge.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct McpPermissionRequest {
    pub protocol_version: u32,
    pub request_id: String,
    pub provider: ProviderKind,
    pub session_id: UnifiedSessionId,
    pub turn_id: TurnId,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub cwd: Option<PathBuf>,
    pub command: Option<String>,
    #[serde(default)]
    pub files: Vec<PathBuf>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct McpPermissionResponse {
    pub protocol_version: u32,
    pub request_id: String,
    pub decision: ApprovalDecision,
    pub message: Option<String>,
}

#[derive(Debug, Error)]
pub enum McpBridgeError {
    #[error("unsupported MCP permission protocol version: {0}")]
    UnsupportedVersion(u32),
    #[error(transparent)]
    Approval(#[from] ApprovalError),
}

#[async_trait]
pub trait PermissionPromptBridge: Send + Sync {
    async fn handle(
        &self,
        request: McpPermissionRequest,
    ) -> Result<McpPermissionResponse, McpBridgeError>;
}

#[derive(Clone, Debug)]
pub struct GatewayMcpBridge {
    gateway: ApprovalGateway,
}

impl GatewayMcpBridge {
    pub fn new(gateway: ApprovalGateway) -> Self {
        Self { gateway }
    }
}

#[async_trait]
impl PermissionPromptBridge for GatewayMcpBridge {
    async fn handle(
        &self,
        request: McpPermissionRequest,
    ) -> Result<McpPermissionResponse, McpBridgeError> {
        if request.protocol_version != MCP_PERMISSION_PROTOCOL_VERSION {
            return Err(McpBridgeError::UnsupportedVersion(request.protocol_version));
        }
        let action = classify_tool_action(&request);
        let approval = ApprovalRequest {
            id: parse_or_create_id(&request.request_id),
            provider: request.provider.clone(),
            session_id: request.session_id,
            turn_id: request.turn_id,
            action,
            risk: RiskLevel::Low,
            cwd: request.cwd.clone(),
            command: request.command.clone(),
            files: request.files.clone(),
            reason: request.reason.clone(),
        };
        let resolution = self.gateway.request(approval).await?;
        Ok(McpPermissionResponse {
            protocol_version: MCP_PERMISSION_PROTOCOL_VERSION,
            request_id: request.request_id,
            decision: resolution.decision,
            message: Some(format!("resolved by {:?}", resolution.source)),
        })
    }
}

fn classify_tool_action(request: &McpPermissionRequest) -> ApprovalAction {
    if request.command.is_some() {
        ApprovalAction::Command
    } else if !request.files.is_empty() {
        ApprovalAction::FileChange
    } else {
        let (server, tool) = request
            .tool_name
            .split_once("__")
            .unwrap_or(("mcp", request.tool_name.as_str()));
        ApprovalAction::McpTool {
            server: server.to_owned(),
            tool: tool.to_owned(),
        }
    }
}

fn parse_or_create_id(value: &str) -> ApprovalId {
    value.parse().unwrap_or_else(|_| ApprovalId::new())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::ApprovalPolicy;

    use super::*;

    #[tokio::test]
    async fn bridge_rejects_unknown_protocol_before_prompting() {
        let (gateway, _receiver) =
            ApprovalGateway::new(ApprovalPolicy::default(), 1, Duration::from_secs(1)).unwrap();
        let bridge = GatewayMcpBridge::new(gateway);
        let result = bridge
            .handle(McpPermissionRequest {
                protocol_version: 999,
                request_id: "request".into(),
                provider: ProviderKind::Claude,
                session_id: UnifiedSessionId::new(),
                turn_id: TurnId::new(),
                tool_name: "shell".into(),
                tool_input: serde_json::json!({}),
                cwd: None,
                command: None,
                files: Vec::new(),
                reason: None,
            })
            .await;
        assert!(matches!(
            result,
            Err(McpBridgeError::UnsupportedVersion(999))
        ));
    }
}
