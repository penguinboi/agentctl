use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    io,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use agentctl_core::{
    AgentEvent, AgentProvider, NativeSession, ProviderError, ProviderEventStream, ProviderHealth,
    ProviderKind, SessionContext, SyncBatch, SyncReceipt, TurnExecutionMode, TurnRequest,
};
use agentctl_workspace::{ProcessTree, configure_tokio_process_group};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    process::{Child, Command},
    sync::{Mutex, broadcast, mpsc, oneshot},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::codec::{FramedRead, LinesCodec};

use crate::{
    ApprovalRespondParams, CURRENT_PROTOCOL_VERSION, EmptyResult, EnsureSessionParams,
    EnsureSessionResult, InitializeParams, InitializeResult, InterruptTurnParams, JSONRPC_VERSION,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, METHOD_APPROVAL_RESPOND,
    METHOD_ENSURE_SESSION, METHOD_INITIALIZE, METHOD_INTERRUPT_TURN, METHOD_PROBE, METHOD_SHUTDOWN,
    METHOD_START_TURN, METHOD_SYNC_CONTEXT, METHOD_TURN_EVENT, PeerInfo, PluginManifest,
    ProbeResult, RequestId, RpcErrorObject, StartTurnParams, StartTurnResult, SyncContextParams,
    SyncContextResult, TurnEventNotification,
};

/// Environment variables inherited by plugin subprocesses. Provider credentials,
/// agentctl state paths, CI secrets, and arbitrary parent variables are deliberately
/// excluded. Manifest-declared values are added after this allowlist and may override it.
const INHERITED_ENV_ALLOWLIST: &[&str] = &[
    "COMSPEC",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LOGNAME",
    "PATH",
    "PATHEXT",
    "SHELL",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "TMPDIR",
    "TZ",
    "USER",
    "WINDIR",
];

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub request_timeout: Duration,
    pub turn_event_timeout: Duration,
    pub max_line_bytes: usize,
    pub protocol_versions: Vec<u32>,
    pub client: PeerInfo,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            turn_event_timeout: Duration::from_secs(300),
            max_line_bytes: 4 * 1024 * 1024,
            protocol_versions: vec![CURRENT_PROTOCOL_VERSION],
            client: PeerInfo {
                name: "agentctl".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        }
    }
}

#[derive(Debug, Error)]
pub enum PluginClientError {
    #[error("invalid plugin manifest: {0}")]
    Manifest(#[from] crate::ManifestError),
    #[error("failed to start plugin process: {0}")]
    Spawn(#[source] io::Error),
    #[error("failed to contain plugin process tree: {0}")]
    ProcessTree(String),
    #[error("plugin process did not expose piped {0}")]
    MissingPipe(&'static str),
    #[error(transparent)]
    Rpc(#[from] RpcCallError),
    #[error("plugin selected unsupported protocol version {0}")]
    UnsupportedProtocol(u32),
    #[error("plugin identity mismatch: expected {expected}, got {actual}")]
    IdentityMismatch { expected: String, actual: String },
}

#[derive(Debug, Error)]
pub enum RpcCallError {
    #[error("plugin transport is closed")]
    Closed,
    #[error("plugin transport error: {0}")]
    Transport(String),
    #[error("plugin request timed out: {0}")]
    Timeout(String),
    #[error("plugin returned JSON-RPC error {code}: {message}")]
    Remote {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("failed to encode plugin request: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("failed to decode plugin response: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("plugin response channel closed")]
    ResponseDropped,
    #[error("plugin response exceeded {limit} bytes")]
    LineTooLarge { limit: usize },
}

impl From<RpcCallError> for ProviderError {
    fn from(error: RpcCallError) -> Self {
        match error {
            RpcCallError::Remote { code, message, .. } if code == RpcErrorObject::INCOMPATIBLE => {
                Self::Incompatible(message)
            }
            RpcCallError::Remote { message, .. } => Self::Protocol(message),
            RpcCallError::Decode(error) => Self::Protocol(error.to_string()),
            other => Self::Process(other.to_string()),
        }
    }
}

type PendingResponse = oneshot::Sender<Result<Value, RpcCallError>>;
type DynWriter = Box<dyn AsyncWrite + Send + Unpin>;

struct ClientInner {
    name: String,
    writer: Mutex<DynWriter>,
    pending: Mutex<HashMap<RequestId, PendingResponse>>,
    notifications: broadcast::Sender<JsonRpcNotification>,
    server_requests: broadcast::Sender<JsonRpcRequest>,
    next_id: AtomicU64,
    closed: AtomicBool,
    options: ClientOptions,
    negotiated: OnceLock<InitializeResult>,
    child: Mutex<Option<Child>>,
    process_tree: Mutex<Option<ProcessTree>>,
}

#[derive(Clone)]
pub struct PluginClient {
    inner: Arc<ClientInner>,
}

impl std::fmt::Debug for PluginClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginClient")
            .field("name", &self.inner.name)
            .field("closed", &self.inner.closed.load(Ordering::Relaxed))
            .field("negotiated", &self.inner.negotiated.get())
            .finish_non_exhaustive()
    }
}

impl PluginClient {
    pub async fn connect(
        manifest: &PluginManifest,
        options: ClientOptions,
    ) -> Result<Self, PluginClientError> {
        manifest.validate()?;
        let mut command = plugin_command(manifest);
        let mut child = command.spawn().map_err(PluginClientError::Spawn)?;
        let process_tree = match ProcessTree::attach(&child) {
            Ok(process_tree) => process_tree,
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(PluginClientError::ProcessTree(error.to_string()));
            }
        };
        let stdin = child
            .stdin
            .take()
            .ok_or(PluginClientError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(PluginClientError::MissingPipe("stdout"))?;
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            });
        }

        let client = Self::from_transport(&manifest.name, stdout, stdin, options);
        *client.inner.child.lock().await = Some(child);
        *client.inner.process_tree.lock().await = Some(process_tree);
        if let Err(error) = client.handshake().await {
            client.force_stop().await;
            return Err(error);
        }
        Ok(client)
    }

    /// Establishes a client over arbitrary async I/O, useful for daemon embedding and tests.
    pub async fn connect_io<R, W>(
        plugin_name: impl Into<String>,
        reader: R,
        writer: W,
        options: ClientOptions,
    ) -> Result<Self, PluginClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let client = Self::from_transport(plugin_name.into(), reader, writer, options);
        if let Err(error) = client.handshake().await {
            client.force_stop().await;
            return Err(error);
        }
        Ok(client)
    }

    fn from_transport<R, W>(
        plugin_name: impl Into<String>,
        reader: R,
        writer: W,
        options: ClientOptions,
    ) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (notifications, _) = broadcast::channel(256);
        let (server_requests, _) = broadcast::channel(32);
        let inner = Arc::new(ClientInner {
            name: plugin_name.into(),
            writer: Mutex::new(Box::new(writer)),
            pending: Mutex::new(HashMap::new()),
            notifications,
            server_requests,
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            options,
            negotiated: OnceLock::new(),
            child: Mutex::new(None),
            process_tree: Mutex::new(None),
        });
        spawn_reader(Arc::clone(&inner), reader);
        Self { inner }
    }

    async fn handshake(&self) -> Result<(), PluginClientError> {
        let params = InitializeParams {
            protocol_versions: self.inner.options.protocol_versions.clone(),
            client: self.inner.options.client.clone(),
        };
        let result: InitializeResult = self.call(METHOD_INITIALIZE, &params).await?;
        if !self
            .inner
            .options
            .protocol_versions
            .contains(&result.protocol_version)
        {
            return Err(PluginClientError::UnsupportedProtocol(
                result.protocol_version,
            ));
        }
        if result.provider_name != self.inner.name {
            return Err(PluginClientError::IdentityMismatch {
                expected: self.inner.name.clone(),
                actual: result.provider_name,
            });
        }
        let _ = self.inner.negotiated.set(result);
        Ok(())
    }

    pub fn negotiated(&self) -> Option<&InitializeResult> {
        self.inner.negotiated.get()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JsonRpcNotification> {
        self.inner.notifications.subscribe()
    }

    pub fn subscribe_server_requests(&self) -> broadcast::Receiver<JsonRpcRequest> {
        self.inner.server_requests.subscribe()
    }

    pub async fn call<P, R>(&self, method: &str, params: &P) -> Result<R, RpcCallError>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let value = serde_json::to_value(params).map_err(RpcCallError::Encode)?;
        let result = self.call_value(method, value).await?;
        serde_json::from_value(result).map_err(RpcCallError::Decode)
    }

    pub async fn call_value(&self, method: &str, params: Value) -> Result<Value, RpcCallError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(RpcCallError::Closed);
        }
        let id = RequestId::Number(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let request = JsonRpcRequest::new(id.clone(), method, params);
        let mut encoded = serde_json::to_vec(&request).map_err(RpcCallError::Encode)?;
        if encoded.len() > self.inner.options.max_line_bytes {
            return Err(RpcCallError::LineTooLarge {
                limit: self.inner.options.max_line_bytes,
            });
        }
        encoded.push(b'\n');

        let (sender, receiver) = oneshot::channel();
        self.inner.pending.lock().await.insert(id.clone(), sender);
        if let Err(error) = self.inner.writer.lock().await.write_all(&encoded).await {
            self.inner.pending.lock().await.remove(&id);
            return Err(RpcCallError::Transport(error.to_string()));
        }
        match tokio::time::timeout(self.inner.options.request_timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcCallError::ResponseDropped),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(RpcCallError::Timeout(method.to_owned()))
            }
        }
    }

    pub async fn notify<P: Serialize + ?Sized>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<(), RpcCallError> {
        let params = serde_json::to_value(params).map_err(RpcCallError::Encode)?;
        self.write_message(&JsonRpcNotification::new(method, params))
            .await
    }

    pub async fn respond_success<R: Serialize + ?Sized>(
        &self,
        id: RequestId,
        result: &R,
    ) -> Result<(), RpcCallError> {
        let result = serde_json::to_value(result).map_err(RpcCallError::Encode)?;
        self.write_message(&JsonRpcResponse::success(id, result))
            .await
    }

    pub async fn respond_error(
        &self,
        id: RequestId,
        error: RpcErrorObject,
    ) -> Result<(), RpcCallError> {
        self.write_message(&JsonRpcResponse::error(id, error)).await
    }

    async fn write_message<T: Serialize + ?Sized>(&self, value: &T) -> Result<(), RpcCallError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(RpcCallError::Closed);
        }
        let mut encoded = serde_json::to_vec(value).map_err(RpcCallError::Encode)?;
        if encoded.len() > self.inner.options.max_line_bytes {
            return Err(RpcCallError::LineTooLarge {
                limit: self.inner.options.max_line_bytes,
            });
        }
        encoded.push(b'\n');
        self.inner
            .writer
            .lock()
            .await
            .write_all(&encoded)
            .await
            .map_err(|error| RpcCallError::Transport(error.to_string()))
    }

    pub async fn respond_approval(
        &self,
        params: &ApprovalRespondParams,
    ) -> Result<(), RpcCallError> {
        let _: EmptyResult = self.call(METHOD_APPROVAL_RESPOND, params).await?;
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<(), RpcCallError> {
        let graceful = if self.inner.closed.load(Ordering::Acquire) {
            Ok(())
        } else {
            match tokio::time::timeout(
                self.inner
                    .options
                    .request_timeout
                    .min(Duration::from_secs(5)),
                self.call::<Value, EmptyResult>(METHOD_SHUTDOWN, &Value::Null),
            )
            .await
            {
                Ok(result) => result.map(|_| ()),
                Err(_) => Err(RpcCallError::Timeout(METHOD_SHUTDOWN.to_owned())),
            }
        };
        self.force_stop().await;
        graceful
    }

    async fn force_stop(&self) {
        self.inner.closed.store(true, Ordering::Release);
        let _ = self.inner.writer.lock().await.shutdown().await;
        if let Some(process_tree) = self.inner.process_tree.lock().await.take() {
            let _ = process_tree.terminate();
        }
        if let Some(mut child) = self.inner.child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        fail_pending(&self.inner, "plugin stopped").await;
    }

    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::Plugin(self.inner.name.clone())
    }
}

fn plugin_command(manifest: &PluginManifest) -> Command {
    let mut command = Command::new(&manifest.executable);
    command
        .args(&manifest.args)
        .env_clear()
        .envs(plugin_environment(manifest, std::env::vars_os()))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    configure_tokio_process_group(&mut command);
    command
}

fn plugin_environment(
    manifest: &PluginManifest,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> BTreeMap<OsString, OsString> {
    let mut environment = inherited
        .into_iter()
        .filter(|(key, _)| {
            key.to_str().is_some_and(|key| {
                INHERITED_ENV_ALLOWLIST
                    .iter()
                    .any(|allowed| key.eq_ignore_ascii_case(allowed))
            })
        })
        .collect::<BTreeMap<_, _>>();
    environment.extend(
        manifest
            .env
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    );
    environment
}

fn spawn_reader<R>(inner: Arc<ClientInner>, reader: R)
where
    R: AsyncRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let mut lines = FramedRead::new(
            reader,
            LinesCodec::new_with_max_length(inner.options.max_line_bytes),
        );
        while let Some(line) = lines.next().await {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    fail_pending(&inner, &error.to_string()).await;
                    inner.closed.store(true, Ordering::Release);
                    return;
                }
            };
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(error) => {
                    fail_pending(&inner, &format!("invalid JSON: {error}")).await;
                    inner.closed.store(true, Ordering::Release);
                    return;
                }
            };
            if value.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
                fail_pending(&inner, "invalid JSON-RPC version").await;
                inner.closed.store(true, Ordering::Release);
                return;
            }

            if value.get("method").is_some() && value.get("id").is_some() {
                if let Ok(request) = serde_json::from_value::<JsonRpcRequest>(value) {
                    let _ = inner.server_requests.send(request);
                }
            } else if value.get("method").is_some() {
                if let Ok(notification) = serde_json::from_value::<JsonRpcNotification>(value) {
                    let _ = inner.notifications.send(notification);
                }
            } else if let Ok(response) = serde_json::from_value::<JsonRpcResponse>(value)
                && let Some(sender) = inner.pending.lock().await.remove(&response.id)
            {
                let result = match (response.result, response.error) {
                    (Some(result), None) => Ok(result),
                    (None, Some(error)) => Err(RpcCallError::Remote {
                        code: error.code,
                        message: error.message,
                        data: error.data,
                    }),
                    _ => Err(RpcCallError::Transport(
                        "malformed JSON-RPC response".into(),
                    )),
                };
                let _ = sender.send(result);
            }
        }
        inner.closed.store(true, Ordering::Release);
        fail_pending(&inner, "plugin closed stdout").await;
    });
}

async fn fail_pending(inner: &ClientInner, message: &str) {
    let pending = std::mem::take(&mut *inner.pending.lock().await);
    for (_, sender) in pending {
        let _ = sender.send(Err(RpcCallError::Transport(message.to_owned())));
    }
}

#[async_trait]
impl AgentProvider for PluginClient {
    fn kind(&self) -> ProviderKind {
        self.provider_kind()
    }

    fn supports_turn_mode(&self, mode: TurnExecutionMode) -> bool {
        mode == TurnExecutionMode::ReadWrite
            || self.negotiated().is_some_and(|negotiated| {
                negotiated
                    .capabilities
                    .extensions
                    .get("read_only_review")
                    .copied()
                    .unwrap_or(false)
            })
    }

    async fn probe(&self) -> Result<ProviderHealth, ProviderError> {
        let mut result: ProbeResult = self.call(METHOD_PROBE, &Value::Null).await?;
        result.health.provider = self.provider_kind();
        Ok(result.health)
    }

    async fn ensure_session(
        &self,
        context: &SessionContext,
    ) -> Result<NativeSession, ProviderError> {
        let mut result: EnsureSessionResult = self
            .call(
                METHOD_ENSURE_SESSION,
                &EnsureSessionParams {
                    context: context.clone(),
                },
            )
            .await?;
        result.session.provider = self.provider_kind();
        Ok(result.session)
    }

    async fn sync_context(
        &self,
        session: &NativeSession,
        batch: SyncBatch,
    ) -> Result<SyncReceipt, ProviderError> {
        let result: SyncContextResult = self
            .call(
                METHOD_SYNC_CONTEXT,
                &SyncContextParams {
                    session: session.clone(),
                    batch,
                },
            )
            .await?;
        Ok(result.receipt)
    }

    async fn run_turn(
        &self,
        session: &NativeSession,
        request: TurnRequest,
    ) -> Result<ProviderEventStream, ProviderError> {
        let mut notifications = self.subscribe();
        let result: StartTurnResult = self
            .call(
                METHOD_START_TURN,
                &StartTurnParams {
                    session: session.clone(),
                    request,
                },
            )
            .await?;
        let expected_session = session.native_session_id.clone();
        let expected_turn = result.native_turn_id;
        let timeout = self.inner.options.turn_event_timeout;
        let (sender, receiver) = mpsc::channel(128);

        tokio::spawn(async move {
            loop {
                let notification = match tokio::time::timeout(timeout, notifications.recv()).await {
                    Ok(Ok(notification)) => notification,
                    Ok(Err(broadcast::error::RecvError::Lagged(count))) => {
                        let _ = sender
                            .send(Err(ProviderError::Protocol(format!(
                                "plugin event stream lagged by {count} messages"
                            ))))
                            .await;
                        break;
                    }
                    Ok(Err(broadcast::error::RecvError::Closed)) => {
                        let _ = sender
                            .send(Err(ProviderError::Process(
                                "plugin event stream closed".into(),
                            )))
                            .await;
                        break;
                    }
                    Err(_) => {
                        let _ = sender
                            .send(Err(ProviderError::Process(
                                "timed out waiting for plugin turn event".into(),
                            )))
                            .await;
                        break;
                    }
                };
                if notification.method != METHOD_TURN_EVENT {
                    continue;
                }
                let event: TurnEventNotification = match serde_json::from_value(notification.params)
                {
                    Ok(event) => event,
                    Err(error) => {
                        let _ = sender
                            .send(Err(ProviderError::Protocol(error.to_string())))
                            .await;
                        break;
                    }
                };
                if event.native_session_id != expected_session
                    || event.native_turn_id != expected_turn
                {
                    continue;
                }
                let completed = matches!(event.event, AgentEvent::TurnCompleted { .. });
                if sender.send(Ok(event.event)).await.is_err() || completed {
                    break;
                }
            }
        });
        Ok(Box::pin(ReceiverStream::new(receiver)))
    }

    async fn interrupt(
        &self,
        session: &NativeSession,
        native_turn_id: &str,
    ) -> Result<(), ProviderError> {
        let _: EmptyResult = self
            .call(
                METHOD_INTERRUPT_TURN,
                &InterruptTurnParams {
                    session: session.clone(),
                    native_turn_id: native_turn_id.into(),
                },
            )
            .await?;
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), ProviderError> {
        PluginClient::shutdown(self).await.map_err(Into::into)
    }
}

// Keep the public stream alias visible in generated docs without exposing implementation details.
pub type PluginEventStream =
    Pin<Box<dyn futures::Stream<Item = Result<AgentEvent, ProviderError>> + Send>>;

#[cfg(test)]
mod environment_tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use super::*;
    use crate::{CURRENT_PROTOCOL_VERSION, PluginCapabilities};
    #[cfg(unix)]
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn manifest() -> PluginManifest {
        PluginManifest {
            manifest_version: crate::MANIFEST_VERSION,
            name: "example".into(),
            version: "1.0.0".into(),
            protocol_versions: vec![CURRENT_PROTOCOL_VERSION],
            executable: PathBuf::from("example-plugin"),
            args: Vec::new(),
            env: BTreeMap::from([
                ("PATH".into(), "/plugin/bin".into()),
                ("PLUGIN_MODE".into(), "safe".into()),
            ]),
            capabilities: PluginCapabilities::default(),
        }
    }

    #[test]
    fn plugin_environment_drops_parent_secrets_and_keeps_only_allowlisted_values() {
        let inherited = [
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from("HOME"), OsString::from("/home/user")),
            (
                OsString::from("CLAUDE_CODE_OAUTH_TOKEN"),
                OsString::from("must-not-leak"),
            ),
            (
                OsString::from("GITHUB_TOKEN"),
                OsString::from("must-not-leak"),
            ),
        ];

        let environment = plugin_environment(&manifest(), inherited);

        assert_eq!(
            environment.get(&OsString::from("HOME")),
            Some(&OsString::from("/home/user"))
        );
        assert_eq!(
            environment.get(&OsString::from("PATH")),
            Some(&OsString::from("/plugin/bin"))
        );
        assert_eq!(
            environment.get(&OsString::from("PLUGIN_MODE")),
            Some(&OsString::from("safe"))
        );
        assert!(!environment.contains_key(&OsString::from("CLAUDE_CODE_OAUTH_TOKEN")));
        assert!(!environment.contains_key(&OsString::from("GITHUB_TOKEN")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawned_plugin_receives_the_sanitized_environment() {
        let excluded_key = std::env::vars_os()
            .map(|(key, _)| key)
            .find(|key| {
                key.to_str().is_some_and(|key| {
                    !INHERITED_ENV_ALLOWLIST
                        .iter()
                        .any(|allowed| key.eq_ignore_ascii_case(allowed))
                        && key != "PLUGIN_MODE"
                })
            })
            .expect("test process should have at least one non-allowlisted variable");
        let mut manifest = manifest();
        manifest.executable = PathBuf::from("/usr/bin/env");

        let output = plugin_command(&manifest).output().await.unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let excluded_prefix = format!("{}=", excluded_key.to_string_lossy());

        assert!(stdout.lines().any(|line| line == "PLUGIN_MODE=safe"));
        assert!(stdout.lines().any(|line| line == "PATH=/plugin/bin"));
        assert!(
            !stdout
                .lines()
                .any(|line| line.starts_with(&excluded_prefix))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plugin_teardown_kills_its_process_group_descendants() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let mut manifest = manifest();
        manifest.executable = PathBuf::from("/bin/sh");
        manifest.args = vec![
            "-c".to_owned(),
            "echo $$; sleep 30 & echo $!; wait".to_owned(),
        ];
        manifest
            .env
            .insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
        let mut child = plugin_command(&manifest).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let parent = lines
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let descendant = lines
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .parse::<i32>()
            .unwrap();
        assert_eq!(child.id(), u32::try_from(parent).ok());

        let process_tree = ProcessTree::attach(&child).unwrap();
        process_tree.terminate().unwrap();
        let _ = child.kill().await;
        let _ = child.wait().await;
        let mut gone = false;
        for _ in 0..50 {
            if matches!(kill(Pid::from_raw(descendant), None), Err(Errno::ESRCH)) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gone, "plugin descendant survived process-group teardown");
    }
}
