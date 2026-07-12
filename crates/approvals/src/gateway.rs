use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};

use agentctl_core::{
    ApprovalAction, ApprovalDecision, ApprovalRequest, ProviderKind, UnifiedSessionId,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::{ApprovalPolicy, PolicyError, PolicyOutcome, RiskClassifier};

#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum GrantScope {
    Command {
        command: String,
        cwd: Option<PathBuf>,
    },
    FileChange {
        files: Vec<PathBuf>,
    },
    Network {
        host: Option<String>,
    },
    McpTool {
        server: String,
        tool: String,
    },
    Permission {
        name: String,
    },
}

impl GrantScope {
    fn from_request(request: &ApprovalRequest) -> Self {
        match &request.action {
            ApprovalAction::Command => Self::Command {
                command: request.command.clone().unwrap_or_default(),
                cwd: request.cwd.clone(),
            },
            ApprovalAction::FileChange => {
                let mut files = request.files.clone();
                files.sort();
                files.dedup();
                Self::FileChange { files }
            }
            ApprovalAction::Network { host } => Self::Network { host: host.clone() },
            ApprovalAction::McpTool { server, tool } => Self::McpTool {
                server: server.clone(),
                tool: tool.clone(),
            },
            ApprovalAction::Permission { name } => Self::Permission { name: name.clone() },
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SessionGrant {
    session_id: UnifiedSessionId,
    provider: ProviderKind,
    scope: GrantScope,
}

impl SessionGrant {
    fn from_request(request: &ApprovalRequest) -> Self {
        Self {
            session_id: request.session_id,
            provider: request.provider.clone(),
            scope: GrantScope::from_request(request),
        }
    }
}

#[derive(Debug)]
pub struct ApprovalPrompt {
    pub request: ApprovalRequest,
    response: oneshot::Sender<ApprovalDecision>,
}

impl ApprovalPrompt {
    pub fn respond(self, decision: ApprovalDecision) -> Result<(), ApprovalDecision> {
        self.response.send(decision)
    }

    pub fn is_closed(&self) -> bool {
        self.response.is_closed()
    }
}

#[derive(Debug)]
pub struct ApprovalReceiver {
    receiver: mpsc::Receiver<ApprovalPrompt>,
}

impl ApprovalReceiver {
    pub async fn recv(&mut self) -> Option<ApprovalPrompt> {
        self.receiver.recv().await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionSource {
    Policy,
    SessionGrant,
    User,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApprovalResolution {
    pub decision: ApprovalDecision,
    pub source: ResolutionSource,
}

#[derive(Debug, Error)]
pub enum ApprovalError {
    #[error(transparent)]
    InvalidPolicy(#[from] PolicyError),
    #[error("approval handler is not running")]
    HandlerUnavailable,
    #[error("approval response timed out")]
    TimedOut,
    #[error("approval handler dropped the request")]
    ResponseDropped,
}

#[derive(Clone, Debug)]
pub struct ApprovalGateway {
    inner: Arc<GatewayInner>,
}

#[derive(Debug)]
struct GatewayInner {
    policy: ApprovalPolicy,
    classifier: RiskClassifier,
    sender: mpsc::Sender<ApprovalPrompt>,
    grants: Mutex<HashSet<SessionGrant>>,
    timeout: Duration,
}

impl ApprovalGateway {
    pub fn new(
        policy: ApprovalPolicy,
        capacity: usize,
        timeout: Duration,
    ) -> Result<(Self, ApprovalReceiver), ApprovalError> {
        policy.validate()?;
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        Ok((
            Self {
                inner: Arc::new(GatewayInner {
                    policy,
                    classifier: RiskClassifier,
                    sender,
                    grants: Mutex::new(HashSet::new()),
                    timeout,
                }),
            },
            ApprovalReceiver { receiver },
        ))
    }

    pub async fn request(
        &self,
        mut request: ApprovalRequest,
    ) -> Result<ApprovalResolution, ApprovalError> {
        request.risk = self.inner.classifier.classify(&request);
        let grant = SessionGrant::from_request(&request);
        if self.inner.grants.lock().await.contains(&grant) {
            return Ok(ApprovalResolution {
                decision: ApprovalDecision::AllowOnce,
                source: ResolutionSource::SessionGrant,
            });
        }

        if let PolicyOutcome::Decide(decision) = self.inner.policy.evaluate(&request, request.risk)
        {
            return Ok(ApprovalResolution {
                decision,
                source: ResolutionSource::Policy,
            });
        }

        let (response, receive_response) = oneshot::channel();
        self.inner
            .sender
            .send(ApprovalPrompt { request, response })
            .await
            .map_err(|_| ApprovalError::HandlerUnavailable)?;
        let decision = tokio::time::timeout(self.inner.timeout, receive_response)
            .await
            .map_err(|_| ApprovalError::TimedOut)?
            .map_err(|_| ApprovalError::ResponseDropped)?;

        if decision == ApprovalDecision::AllowSession && self.inner.policy.allow_session_grants {
            self.inner.grants.lock().await.insert(grant);
        }
        Ok(ApprovalResolution {
            decision,
            source: ResolutionSource::User,
        })
    }

    pub async fn revoke_session(&self, session_id: UnifiedSessionId) -> usize {
        let mut grants = self.inner.grants.lock().await;
        let before = grants.len();
        grants.retain(|grant| grant.session_id != session_id);
        before - grants.len()
    }

    pub async fn session_grant_count(&self) -> usize {
        self.inner.grants.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agentctl_core::{ApprovalAction, ApprovalId, RiskLevel, TurnId, UnifiedSessionId};

    use super::*;

    fn request(session_id: UnifiedSessionId, command: &str) -> ApprovalRequest {
        ApprovalRequest {
            id: ApprovalId::new(),
            provider: ProviderKind::Claude,
            session_id,
            turn_id: TurnId::new(),
            action: ApprovalAction::Command,
            risk: RiskLevel::Low,
            cwd: Some(PathBuf::from("/repo")),
            command: Some(command.into()),
            files: Vec::new(),
            reason: None,
        }
    }

    #[tokio::test]
    async fn session_grant_is_exact_and_can_be_revoked() {
        let session = UnifiedSessionId::new();
        let (gateway, mut receiver) =
            ApprovalGateway::new(ApprovalPolicy::default(), 8, Duration::from_secs(1)).unwrap();
        let responder = tokio::spawn(async move {
            receiver
                .recv()
                .await
                .unwrap()
                .respond(ApprovalDecision::AllowSession)
                .unwrap();
        });

        let first = gateway
            .request(request(session, "cargo test"))
            .await
            .unwrap();
        responder.await.unwrap();
        assert_eq!(first.source, ResolutionSource::User);
        assert_eq!(gateway.session_grant_count().await, 1);

        let repeated = gateway
            .request(request(session, "cargo test"))
            .await
            .unwrap();
        assert_eq!(repeated.source, ResolutionSource::SessionGrant);
        assert_eq!(repeated.decision, ApprovalDecision::AllowOnce);

        assert_eq!(gateway.revoke_session(session).await, 1);
        assert_eq!(gateway.session_grant_count().await, 0);
    }

    #[tokio::test]
    async fn missing_handler_fails_closed() {
        let (gateway, receiver) =
            ApprovalGateway::new(ApprovalPolicy::default(), 1, Duration::from_millis(10)).unwrap();
        drop(receiver);
        let error = gateway
            .request(request(UnifiedSessionId::new(), "cargo test"))
            .await
            .unwrap_err();
        assert!(matches!(error, ApprovalError::HandlerUnavailable));
    }

    #[tokio::test]
    async fn timeout_and_dropped_response_fail_closed_without_creating_grants() {
        let session = UnifiedSessionId::new();
        let (timeout_gateway, _receiver) =
            ApprovalGateway::new(ApprovalPolicy::default(), 1, Duration::from_millis(10)).unwrap();
        let timeout = timeout_gateway
            .request(request(session, "cargo test"))
            .await
            .unwrap_err();
        assert!(matches!(timeout, ApprovalError::TimedOut));
        assert_eq!(timeout_gateway.session_grant_count().await, 0);

        let (dropped_gateway, mut receiver) =
            ApprovalGateway::new(ApprovalPolicy::default(), 1, Duration::from_secs(1)).unwrap();
        let discard_task = tokio::spawn(async move {
            drop(receiver.recv().await.unwrap());
        });
        let response_error = dropped_gateway
            .request(request(session, "cargo check"))
            .await
            .unwrap_err();
        discard_task.await.unwrap();
        assert!(matches!(response_error, ApprovalError::ResponseDropped));
        assert_eq!(dropped_gateway.session_grant_count().await, 0);
    }

    #[tokio::test]
    async fn cancel_turn_round_trips_as_interruption_and_never_becomes_a_session_grant() {
        let session = UnifiedSessionId::new();
        let (gateway, mut receiver) =
            ApprovalGateway::new(ApprovalPolicy::default(), 1, Duration::from_secs(1)).unwrap();
        let responder = tokio::spawn(async move {
            receiver
                .recv()
                .await
                .unwrap()
                .respond(ApprovalDecision::CancelTurn)
                .unwrap();
        });
        let resolution = gateway
            .request(request(session, "cargo test"))
            .await
            .unwrap();
        responder.await.unwrap();
        assert_eq!(resolution.decision, ApprovalDecision::CancelTurn);
        assert_eq!(resolution.source, ResolutionSource::User);
        assert_eq!(gateway.session_grant_count().await, 0);
    }
}
