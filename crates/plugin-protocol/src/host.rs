use std::{io, sync::Arc};

use agentctl_core::{
    AgentProvider, ApprovalDecision, ApprovalId, NormalizedProviderError, ProviderError,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
};
use tokio_util::codec::{FramedRead, LinesCodec};

use crate::{
    ApprovalRespondParams, CURRENT_PROTOCOL_VERSION, EmptyResult, EnsureSessionParams,
    EnsureSessionResult, InitializeParams, InitializeResult, InterruptTurnParams, JSONRPC_VERSION,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, METHOD_APPROVAL_RESPOND,
    METHOD_ENSURE_SESSION, METHOD_INITIALIZE, METHOD_INTERRUPT_TURN, METHOD_PROBE, METHOD_SHUTDOWN,
    METHOD_START_TURN, METHOD_SYNC_CONTEXT, METHOD_TURN_EVENT, PeerInfo, PluginCapabilities,
    ProbeResult, RequestId, RpcErrorObject, StartTurnParams, StartTurnResult, SyncContextParams,
    SyncContextResult, TurnEventNotification,
};

#[derive(Clone, Debug)]
pub struct HostOptions {
    pub provider_name: String,
    pub server: PeerInfo,
    pub protocol_versions: Vec<u32>,
    pub capabilities: PluginCapabilities,
    pub max_line_bytes: usize,
}

impl HostOptions {
    pub fn new(provider_name: impl Into<String>) -> Self {
        Self {
            provider_name: provider_name.into(),
            server: PeerInfo {
                name: "agentctl-provider-plugin".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            protocol_versions: vec![CURRENT_PROTOCOL_VERSION],
            capabilities: PluginCapabilities::default(),
            max_line_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Error)]
pub enum PluginHostError {
    #[error("plugin host transport error: {0}")]
    Transport(String),
    #[error("plugin host failed to serialize a response: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("plugin host response exceeded {limit} bytes")]
    LineTooLarge { limit: usize },
}

#[async_trait]
pub trait ApprovalResponder: Send + Sync {
    async fn respond(
        &self,
        native_session_id: &str,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), ProviderError>;
}

#[derive(Clone)]
pub struct PluginHost {
    provider: Arc<dyn AgentProvider>,
    approval_responder: Option<Arc<dyn ApprovalResponder>>,
    options: HostOptions,
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginHost")
            .field("provider_kind", &self.provider.kind())
            .field("has_approval_responder", &self.approval_responder.is_some())
            .field("options", &self.options)
            .finish()
    }
}

impl PluginHost {
    pub fn new(provider: Arc<dyn AgentProvider>, options: HostOptions) -> Self {
        Self {
            provider,
            approval_responder: None,
            options,
        }
    }

    #[must_use]
    pub fn with_approval_responder(mut self, responder: Arc<dyn ApprovalResponder>) -> Self {
        self.approval_responder = Some(responder);
        self
    }

    pub async fn serve_stdio(self) -> Result<(), PluginHostError> {
        self.serve(tokio::io::stdin(), tokio::io::stdout()).await
    }

    pub async fn serve<R, W>(self, reader: R, writer: W) -> Result<(), PluginHostError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let writer = Arc::new(Mutex::new(writer));
        let mut lines = FramedRead::new(
            reader,
            LinesCodec::new_with_max_length(self.options.max_line_bytes),
        );
        let mut initialized = false;

        while let Some(line) = lines.next().await {
            let line = line.map_err(|error| PluginHostError::Transport(error.to_string()))?;
            let request = match serde_json::from_str::<JsonRpcRequest>(&line) {
                Ok(request) if request.jsonrpc == JSONRPC_VERSION => request,
                Ok(request) => {
                    write_response(
                        &writer,
                        JsonRpcResponse::error(
                            request.id,
                            RpcErrorObject::new(
                                RpcErrorObject::INVALID_REQUEST,
                                "jsonrpc must be 2.0",
                            ),
                        ),
                        self.options.max_line_bytes,
                    )
                    .await?;
                    continue;
                }
                Err(error) => {
                    // JSON-RPC requires null id for parse errors; this protocol intentionally
                    // closes malformed peers because RequestId excludes null.
                    return Err(PluginHostError::Transport(format!(
                        "invalid JSON-RPC request: {error}"
                    )));
                }
            };

            if request.method == METHOD_INITIALIZE {
                if initialized {
                    write_rpc_error(
                        &writer,
                        request.id,
                        RpcErrorObject::INVALID_REQUEST,
                        "initialize may only be called once",
                        self.options.max_line_bytes,
                    )
                    .await?;
                    continue;
                }
                match parse_params::<InitializeParams>(request.params).and_then(|params| {
                    negotiate_version(&params.protocol_versions, &self.options.protocol_versions)
                        .map(|version| (params, version))
                }) {
                    Ok((_params, version)) => {
                        initialized = true;
                        write_result(
                            &writer,
                            request.id,
                            &InitializeResult {
                                protocol_version: version,
                                server: self.options.server.clone(),
                                provider_name: self.options.provider_name.clone(),
                                capabilities: self.options.capabilities.clone(),
                            },
                            self.options.max_line_bytes,
                        )
                        .await?;
                    }
                    Err(error) => {
                        write_response(
                            &writer,
                            JsonRpcResponse::error(request.id, error),
                            self.options.max_line_bytes,
                        )
                        .await?;
                    }
                }
                continue;
            }

            if !initialized {
                write_rpc_error(
                    &writer,
                    request.id,
                    RpcErrorObject::NOT_INITIALIZED,
                    "initialize must be called first",
                    self.options.max_line_bytes,
                )
                .await?;
                continue;
            }

            let should_shutdown = request.method == METHOD_SHUTDOWN;
            self.handle_request(request, Arc::clone(&writer)).await?;
            if should_shutdown {
                break;
            }
        }
        Ok(())
    }

    // Keeping the protocol dispatch in one match makes the supported method surface auditable.
    #[allow(clippy::too_many_lines)]
    async fn handle_request<W>(
        &self,
        request: JsonRpcRequest,
        writer: Arc<Mutex<W>>,
    ) -> Result<(), PluginHostError>
    where
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let id = request.id;
        match request.method.as_str() {
            METHOD_PROBE => match self.provider.probe().await {
                Ok(health) => {
                    write_result(
                        &writer,
                        id,
                        &ProbeResult { health },
                        self.options.max_line_bytes,
                    )
                    .await
                }
                Err(error) => {
                    write_provider_error(&writer, id, &error, self.options.max_line_bytes).await
                }
            },
            METHOD_ENSURE_SESSION => match parse_params::<EnsureSessionParams>(request.params) {
                Ok(params) => match self.provider.ensure_session(&params.context).await {
                    Ok(session) => {
                        write_result(
                            &writer,
                            id,
                            &EnsureSessionResult { session },
                            self.options.max_line_bytes,
                        )
                        .await
                    }
                    Err(error) => {
                        write_provider_error(&writer, id, &error, self.options.max_line_bytes).await
                    }
                },
                Err(error) => {
                    write_response(
                        &writer,
                        JsonRpcResponse::error(id, error),
                        self.options.max_line_bytes,
                    )
                    .await
                }
            },
            METHOD_SYNC_CONTEXT => match parse_params::<SyncContextParams>(request.params) {
                Ok(params) => match self
                    .provider
                    .sync_context(&params.session, params.batch)
                    .await
                {
                    Ok(receipt) => {
                        write_result(
                            &writer,
                            id,
                            &SyncContextResult { receipt },
                            self.options.max_line_bytes,
                        )
                        .await
                    }
                    Err(error) => {
                        write_provider_error(&writer, id, &error, self.options.max_line_bytes).await
                    }
                },
                Err(error) => {
                    write_response(
                        &writer,
                        JsonRpcResponse::error(id, error),
                        self.options.max_line_bytes,
                    )
                    .await
                }
            },
            METHOD_START_TURN => match parse_params::<StartTurnParams>(request.params) {
                Ok(params) => match self
                    .provider
                    .run_turn(&params.session, params.request.clone())
                    .await
                {
                    Ok(mut stream) => {
                        let native_session_id = params.session.native_session_id;
                        let native_turn_id = params.request.turn_id.to_string();
                        write_result(
                            &writer,
                            id,
                            &StartTurnResult {
                                native_turn_id: native_turn_id.clone(),
                            },
                            self.options.max_line_bytes,
                        )
                        .await?;
                        let max_line_bytes = self.options.max_line_bytes;
                        tokio::spawn(async move {
                            while let Some(event) = stream.next().await {
                                let event = match event {
                                    Ok(event) => event,
                                    Err(error) => agentctl_core::AgentEvent::Error {
                                        error: NormalizedProviderError::from(&error),
                                    },
                                };
                                let message = JsonRpcNotification::new(
                                    METHOD_TURN_EVENT,
                                    serde_json::to_value(TurnEventNotification {
                                        native_session_id: native_session_id.clone(),
                                        native_turn_id: native_turn_id.clone(),
                                        event,
                                    })
                                    .unwrap_or(Value::Null),
                                );
                                if write_message(&writer, &message, max_line_bytes)
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        });
                        Ok(())
                    }
                    Err(error) => {
                        write_provider_error(&writer, id, &error, self.options.max_line_bytes).await
                    }
                },
                Err(error) => {
                    write_response(
                        &writer,
                        JsonRpcResponse::error(id, error),
                        self.options.max_line_bytes,
                    )
                    .await
                }
            },
            METHOD_INTERRUPT_TURN => match parse_params::<InterruptTurnParams>(request.params) {
                Ok(params) => match self
                    .provider
                    .interrupt(&params.session, &params.native_turn_id)
                    .await
                {
                    Ok(()) => {
                        write_result(&writer, id, &EmptyResult {}, self.options.max_line_bytes)
                            .await
                    }
                    Err(error) => {
                        write_provider_error(&writer, id, &error, self.options.max_line_bytes).await
                    }
                },
                Err(error) => {
                    write_response(
                        &writer,
                        JsonRpcResponse::error(id, error),
                        self.options.max_line_bytes,
                    )
                    .await
                }
            },
            METHOD_APPROVAL_RESPOND => {
                match parse_params::<ApprovalRespondParams>(request.params) {
                    Ok(params) => {
                        if let Some(responder) = &self.approval_responder {
                            match responder
                                .respond(
                                    &params.native_session_id,
                                    params.approval_id,
                                    params.decision,
                                )
                                .await
                            {
                                Ok(()) => {
                                    write_result(
                                        &writer,
                                        id,
                                        &EmptyResult {},
                                        self.options.max_line_bytes,
                                    )
                                    .await
                                }
                                Err(error) => {
                                    write_provider_error(
                                        &writer,
                                        id,
                                        &error,
                                        self.options.max_line_bytes,
                                    )
                                    .await
                                }
                            }
                        } else {
                            write_rpc_error(
                                &writer,
                                id,
                                RpcErrorObject::METHOD_NOT_FOUND,
                                "plugin does not support approval responses",
                                self.options.max_line_bytes,
                            )
                            .await
                        }
                    }
                    Err(error) => {
                        write_response(
                            &writer,
                            JsonRpcResponse::error(id, error),
                            self.options.max_line_bytes,
                        )
                        .await
                    }
                }
            }
            METHOD_SHUTDOWN => {
                write_result(&writer, id, &EmptyResult {}, self.options.max_line_bytes).await
            }
            _ => {
                write_rpc_error(
                    &writer,
                    id,
                    RpcErrorObject::METHOD_NOT_FOUND,
                    format!("unknown method: {}", request.method),
                    self.options.max_line_bytes,
                )
                .await
            }
        }
    }
}

fn negotiate_version(client: &[u32], server: &[u32]) -> Result<u32, RpcErrorObject> {
    client
        .iter()
        .filter(|version| server.contains(version))
        .max()
        .copied()
        .ok_or_else(|| {
            RpcErrorObject::new(
                RpcErrorObject::INCOMPATIBLE,
                "no mutually supported protocol version",
            )
        })
}

fn parse_params<T: DeserializeOwned>(value: Value) -> Result<T, RpcErrorObject> {
    serde_json::from_value(value)
        .map_err(|error| RpcErrorObject::new(RpcErrorObject::INVALID_PARAMS, error.to_string()))
}

async fn write_provider_error<W: AsyncWrite + Unpin>(
    writer: &Arc<Mutex<W>>,
    id: RequestId,
    error: &ProviderError,
    max_line_bytes: usize,
) -> Result<(), PluginHostError> {
    let normalized = NormalizedProviderError::from(error);
    let rpc_error = RpcErrorObject::new(RpcErrorObject::PROVIDER_ERROR, error.to_string())
        .with_data(serde_json::to_value(normalized)?);
    write_response(
        writer,
        JsonRpcResponse::error(id, rpc_error),
        max_line_bytes,
    )
    .await
}

async fn write_rpc_error<W: AsyncWrite + Unpin>(
    writer: &Arc<Mutex<W>>,
    id: RequestId,
    code: i64,
    message: impl Into<String>,
    max_line_bytes: usize,
) -> Result<(), PluginHostError> {
    write_response(
        writer,
        JsonRpcResponse::error(id, RpcErrorObject::new(code, message)),
        max_line_bytes,
    )
    .await
}

async fn write_result<W: AsyncWrite + Unpin, T: Serialize + ?Sized>(
    writer: &Arc<Mutex<W>>,
    id: RequestId,
    result: &T,
    max_line_bytes: usize,
) -> Result<(), PluginHostError> {
    let result = serde_json::to_value(result)?;
    write_response(writer, JsonRpcResponse::success(id, result), max_line_bytes).await
}

async fn write_response<W: AsyncWrite + Unpin>(
    writer: &Arc<Mutex<W>>,
    response: JsonRpcResponse,
    max_line_bytes: usize,
) -> Result<(), PluginHostError> {
    write_message(writer, &response, max_line_bytes).await
}

async fn write_message<W: AsyncWrite + Unpin, T: Serialize + ?Sized>(
    writer: &Arc<Mutex<W>>,
    message: &T,
    max_line_bytes: usize,
) -> Result<(), PluginHostError> {
    let mut encoded = serde_json::to_vec(message)?;
    if encoded.len() > max_line_bytes {
        return Err(PluginHostError::LineTooLarge {
            limit: max_line_bytes,
        });
    }
    encoded.push(b'\n');
    writer
        .lock()
        .await
        .write_all(&encoded)
        .await
        .map_err(|error| PluginHostError::Transport(error.to_string()))
}

impl From<io::Error> for PluginHostError {
    fn from(error: io::Error) -> Self {
        Self::Transport(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use agentctl_core::{
        AgentEvent, NativeSession, ProviderEventStream, ProviderHealth, ProviderKind,
        ProviderSessionId, ProviderStatus, SessionContext, SyncBatch, SyncReceipt, TurnRequest,
        TurnStatus,
    };
    use chrono::Utc;
    use futures::stream;

    use super::*;
    use crate::{ClientOptions, PluginClient};

    #[derive(Debug)]
    struct FakeProvider;

    #[async_trait]
    impl AgentProvider for FakeProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Plugin("fake".into())
        }

        async fn probe(&self) -> Result<ProviderHealth, ProviderError> {
            Ok(ProviderHealth {
                provider: self.kind(),
                status: ProviderStatus::Ready,
                version: Some("1.0.0".into()),
                capabilities: BTreeMap::new(),
                usage: None,
                rate_limit: None,
                checked_at: Utc::now(),
                message: None,
            })
        }

        async fn ensure_session(
            &self,
            _context: &SessionContext,
        ) -> Result<NativeSession, ProviderError> {
            Ok(NativeSession {
                id: ProviderSessionId::new(),
                provider: self.kind(),
                native_session_id: "native-1".into(),
                native_version: Some("1".into()),
                capabilities: BTreeMap::new(),
            })
        }

        async fn sync_context(
            &self,
            _session: &NativeSession,
            batch: SyncBatch,
        ) -> Result<SyncReceipt, ProviderError> {
            Ok(SyncReceipt {
                through_seq: batch.through_seq_inclusive,
                projection_version: batch.projection_version,
                native_receipt: Some("ok".into()),
            })
        }

        async fn run_turn(
            &self,
            _session: &NativeSession,
            _request: TurnRequest,
        ) -> Result<ProviderEventStream, ProviderError> {
            Ok(Box::pin(stream::iter([
                Ok(AgentEvent::AssistantFinal {
                    text: "done".into(),
                }),
                Ok(AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                }),
            ])))
        }

        async fn interrupt(
            &self,
            _session: &NativeSession,
            _native_turn_id: &str,
        ) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn client_and_host_negotiate_probe_and_stream_a_turn() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (server_read, server_write) = tokio::io::split(server_io);
        let host = PluginHost::new(Arc::new(FakeProvider), HostOptions::new("fake"));
        let host_task = tokio::spawn(host.serve(server_read, server_write));

        let client =
            PluginClient::connect_io("fake", client_read, client_write, ClientOptions::default())
                .await
                .unwrap();
        assert_eq!(client.negotiated().unwrap().protocol_version, 1);
        assert!(matches!(
            client.probe().await.unwrap().status,
            ProviderStatus::Ready
        ));

        let context = SessionContext {
            unified_session_id: agentctl_core::UnifiedSessionId::new(),
            workspace_root: PathBuf::from("/repo"),
            workspace_fingerprint: "workspace".into(),
            auth_mode: agentctl_core::AuthMode::NativeLocal,
        };
        let session = client.ensure_session(&context).await.unwrap();
        let request = TurnRequest {
            session_id: context.unified_session_id,
            turn_id: agentctl_core::TurnId::new(),
            prompt: "work".into(),
            cwd: PathBuf::from("/repo"),
            continuation: false,
            execution_mode: agentctl_core::TurnExecutionMode::ReadWrite,
            metadata: Value::Null,
        };
        let mut events = client.run_turn(&session, request).await.unwrap();
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            AgentEvent::AssistantFinal { text } if text == "done"
        ));
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed
            }
        ));

        client.shutdown().await.unwrap();
        host_task.await.unwrap().unwrap();
    }
}
