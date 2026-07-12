use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agentctl_core::{ProviderError, ProviderKind};
use agentctl_workspace::{ProcessTree, configure_tokio_process_group};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, broadcast, mpsc, oneshot, watch},
    time::timeout,
};
use tokio_util::codec::{FramedRead, LinesCodec};
use uuid::Uuid;

use crate::{
    hooks::{HANDOFF_POLICY, HookRuntime},
    protocol::{ClaudeInit, initialize_control, interrupt_control},
};

const MAX_PROTOCOL_LINE_BYTES: usize = 8 * 1024 * 1024;
type PendingSender = oneshot::Sender<Result<Value, ProviderError>>;
type PendingControls = Arc<Mutex<HashMap<String, PendingSender>>>;

#[derive(Debug)]
struct Outbound(Value);

/// One long-lived Claude Code SDK-mode subprocess.
#[derive(Debug)]
pub(crate) struct ClaudeProcess {
    instance_id: Uuid,
    outbound: mpsc::Sender<Outbound>,
    inbound: broadcast::Sender<Value>,
    pending_control: PendingControls,
    init: watch::Receiver<Option<ClaudeInit>>,
    child: Mutex<Child>,
    process_tree: ProcessTree,
    hook_runtime: HookRuntime,
    control_timeout: Duration,
    closed: Arc<AtomicBool>,
}

pub(crate) struct SpawnOptions<'a> {
    pub binary: &'a Path,
    pub session_id: &'a str,
    pub cwd: &'a Path,
    pub resume: bool,
    pub channel_capacity: usize,
    pub control_timeout: Duration,
    pub runtime_root: Option<&'a Path>,
    pub features: ProcessFeatures,
    pub policy_file: Option<&'a Path>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcessFeatures(u8);

impl ProcessFeatures {
    const HOOK: u8 = 1 << 0;
    const PARTIAL_MESSAGES: u8 = 1 << 1;
    const REPLAY_USER_MESSAGES: u8 = 1 << 2;
    const HOOK_EVENTS: u8 = 1 << 3;

    pub(crate) fn empty() -> Self {
        Self(0)
    }

    pub(crate) fn with_hook(self, enabled: bool) -> Self {
        self.with(Self::HOOK, enabled)
    }

    pub(crate) fn with_partial_messages(self, enabled: bool) -> Self {
        self.with(Self::PARTIAL_MESSAGES, enabled)
    }

    pub(crate) fn with_replay_user_messages(self, enabled: bool) -> Self {
        self.with(Self::REPLAY_USER_MESSAGES, enabled)
    }

    pub(crate) fn with_hook_events(self, enabled: bool) -> Self {
        self.with(Self::HOOK_EVENTS, enabled)
    }

    fn with(mut self, feature: u8, enabled: bool) -> Self {
        if enabled {
            self.0 |= feature;
        }
        self
    }

    fn contains(self, feature: u8) -> bool {
        self.0 & feature != 0
    }
}

impl ClaudeProcess {
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn spawn(options: SpawnOptions<'_>) -> Result<Arc<Self>, ProviderError> {
        let hook_runtime = HookRuntime::create(
            options.runtime_root,
            options.session_id,
            options.features.contains(ProcessFeatures::HOOK),
        )
        .await?;
        let mut command = Command::new(options.binary);
        command
            .args([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
            ])
            .current_dir(options.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if options.features.contains(ProcessFeatures::PARTIAL_MESSAGES) {
            command.arg("--include-partial-messages");
        }
        if options
            .features
            .contains(ProcessFeatures::REPLAY_USER_MESSAGES)
        {
            command.arg("--replay-user-messages");
        }
        if options.features.contains(ProcessFeatures::HOOK_EVENTS) {
            command.arg("--include-hook-events");
        }
        if options.resume {
            command.args(["--resume", options.session_id]);
        } else {
            command.args(["--session-id", options.session_id]);
        }
        if let Some(policy_file) = options.policy_file {
            command.arg("--append-system-prompt-file").arg(policy_file);
        } else {
            command.arg("--append-system-prompt").arg(HANDOFF_POLICY);
        }
        if let Some(settings) = hook_runtime.settings() {
            command.arg("--settings").arg(settings);
        }

        configure_tokio_process_group(&mut command);

        let mut child = command.spawn().map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                ProviderError::BinaryNotFound(options.binary.display().to_string())
            }
            _ => ProviderError::Io(error),
        })?;
        let process_tree = match ProcessTree::attach(&child) {
            Ok(process_tree) => process_tree,
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(ProviderError::Process(error.to_string()));
            }
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProviderError::Process("Claude stdin was not piped".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::Process("Claude stdout was not piped".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProviderError::Process("Claude stderr was not piped".to_owned()))?;
        let (outbound, outbound_rx) = mpsc::channel(options.channel_capacity.max(1));
        let (inbound, _) = broadcast::channel(options.channel_capacity.max(16));
        let pending_control = Arc::new(Mutex::new(HashMap::new()));
        let (init_sender, init) = watch::channel(None);
        let closed = Arc::new(AtomicBool::new(false));

        tokio::spawn(write_loop(stdin, outbound_rx, Arc::clone(&closed)));
        tokio::spawn(read_loop(
            stdout,
            Arc::clone(&pending_control),
            inbound.clone(),
            init_sender,
            Arc::clone(&closed),
        ));
        tokio::spawn(async move {
            let mut lines = FramedRead::new(
                BufReader::new(stderr),
                LinesCodec::new_with_max_length(MAX_PROTOCOL_LINE_BYTES),
            );
            while let Some(line) = lines.next().await {
                match line {
                    Ok(line) => tracing::debug!(
                        provider = %ProviderKind::Claude,
                        bytes = line.len(),
                        "provider stderr line suppressed"
                    ),
                    Err(error) => {
                        tracing::warn!(provider = %ProviderKind::Claude, %error, "failed to read provider stderr");
                        break;
                    }
                }
            }
        });

        let process = Arc::new(Self {
            instance_id: Uuid::new_v4(),
            outbound,
            inbound,
            pending_control,
            init,
            child: Mutex::new(child),
            process_tree,
            hook_runtime,
            control_timeout: options.control_timeout,
            closed,
        });
        process
            .send_control(initialize_control(&Uuid::new_v4().to_string()))
            .await?;
        Ok(process)
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.inbound.subscribe()
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) async fn send(&self, frame: Value) -> Result<(), ProviderError> {
        self.outbound
            .send(Outbound(frame))
            .await
            .map_err(|_| ProviderError::Process("Claude stdin writer stopped".to_owned()))
    }

    pub(crate) async fn send_control(&self, frame: Value) -> Result<Value, ProviderError> {
        let request_id = frame
            .get("request_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProviderError::Protocol("control request omitted request_id".to_owned())
            })?
            .to_owned();
        let (sender, receiver) = oneshot::channel();
        self.pending_control
            .lock()
            .await
            .insert(request_id.clone(), sender);
        if let Err(error) = self.send(frame).await {
            self.pending_control.lock().await.remove(&request_id);
            return Err(error);
        }
        match timeout(self.control_timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ProviderError::Process(
                "Claude control response channel closed".to_owned(),
            )),
            Err(_) => {
                self.pending_control.lock().await.remove(&request_id);
                Err(ProviderError::Process(format!(
                    "Claude control request {request_id} timed out"
                )))
            }
        }
    }

    pub(crate) async fn respond_control(&self, frame: Value) -> Result<(), ProviderError> {
        self.send(frame).await
    }

    pub(crate) async fn interrupt(&self) -> Result<(), ProviderError> {
        self.send_control(interrupt_control(&Uuid::new_v4().to_string()))
            .await?;
        Ok(())
    }

    pub(crate) async fn wait_init(&self, duration: Duration) -> Option<ClaudeInit> {
        let mut receiver = self.init.clone();
        if let Some(init) = receiver.borrow().clone() {
            return Some(init);
        }
        timeout(duration, async move {
            loop {
                if receiver.changed().await.is_err() {
                    return None;
                }
                if let Some(init) = receiver.borrow().clone() {
                    return Some(init);
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    pub(crate) async fn stage_hook_context(&self, handoff: &str) -> Result<(), ProviderError> {
        self.hook_runtime.stage_context(handoff).await
    }

    pub(crate) async fn shutdown(&self) -> Result<(), ProviderError> {
        let mut child = self.child.lock().await;
        let root_running = child.try_wait()?.is_none();
        let terminated = self
            .process_tree
            .terminate()
            .map_err(|error| ProviderError::Process(error.to_string()));
        if root_running {
            child.wait().await?;
        }
        self.closed.store(true, Ordering::Release);
        terminated
    }

    pub(crate) fn instance_id(&self) -> Uuid {
        self.instance_id
    }
}

async fn write_loop(
    mut stdin: ChildStdin,
    mut receiver: mpsc::Receiver<Outbound>,
    closed: Arc<AtomicBool>,
) {
    while let Some(Outbound(frame)) = receiver.recv().await {
        let encoded = match serde_json::to_vec(&frame) {
            Ok(encoded) => encoded,
            Err(error) => {
                tracing::error!(provider = %ProviderKind::Claude, %error, "failed to encode frame");
                continue;
            }
        };
        if stdin.write_all(&encoded).await.is_err()
            || stdin.write_all(b"\n").await.is_err()
            || stdin.flush().await.is_err()
        {
            closed.store(true, Ordering::Release);
            return;
        }
    }
    closed.store(true, Ordering::Release);
}

async fn read_loop(
    stdout: tokio::process::ChildStdout,
    pending: PendingControls,
    inbound: broadcast::Sender<Value>,
    init_sender: watch::Sender<Option<ClaudeInit>>,
    closed: Arc<AtomicBool>,
) {
    let mut lines = FramedRead::new(
        BufReader::new(stdout),
        LinesCodec::new_with_max_length(MAX_PROTOCOL_LINE_BYTES),
    );
    while let Some(line) = lines.next().await {
        let frame = match line {
            Ok(line) => match serde_json::from_str::<Value>(&line) {
                Ok(frame) => frame,
                Err(error) => json!({
                    "type": "agentctl_process_error",
                    "message": format!("malformed Claude JSONL frame: {error}"),
                    "raw": line
                }),
            },
            Err(error) => {
                let _ = inbound.send(json!({
                    "type": "agentctl_process_error",
                    "message": format!("Claude protocol line exceeded bounds: {error}")
                }));
                break;
            }
        };
        if let Some(init) = ClaudeInit::from_frame(&frame) {
            let _ = init_sender.send(Some(init));
        }
        if frame.get("type").and_then(Value::as_str) == Some("control_response") {
            let response = frame.get("response").cloned().unwrap_or(Value::Null);
            if let Some(request_id) = response.get("request_id").and_then(Value::as_str)
                && let Some(sender) = pending.lock().await.remove(request_id)
            {
                let result = if response.get("subtype").and_then(Value::as_str) == Some("error") {
                    Err(ProviderError::Protocol(
                        response
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("Claude control request failed")
                            .to_owned(),
                    ))
                } else {
                    Ok(response.get("response").cloned().unwrap_or(Value::Null))
                };
                let _ = sender.send(result);
            }
        }
        let _ = inbound.send(frame);
    }
    closed.store(true, Ordering::Release);
    let senders = std::mem::take(&mut *pending.lock().await);
    for sender in senders.into_values() {
        let _ = sender.send(Err(ProviderError::Process(
            "Claude process exited".to_owned(),
        )));
    }
    let _ = inbound.send(json!({
        "type": "agentctl_process_error",
        "message": "Claude process exited"
    }));
}
