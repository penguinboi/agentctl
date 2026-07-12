//! Integration primitives for interactive, provider-owned Claude Code sessions.
//!
//! This module does not spawn Claude in headless (`-p`) mode. It builds a
//! temporary `--settings` document for the native interactive CLI and parses
//! the documented command-hook payloads emitted by that process.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

const MAX_HOOK_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONTEXT_CHARS: usize = 10_000;
const MAX_ID_BYTES: usize = 1_024;
const MAX_PATH_BYTES: usize = 32 * 1024;
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ARGS: usize = 128;
const MAX_ARG_BYTES: usize = 32 * 1024;

pub const DEFAULT_NATIVE_HOOK_TIMEOUT_SECONDS: u64 = 10;

/// Hook events used to mirror the lifecycle of an interactive Claude session.
pub const INTERACTIVE_HOOK_EVENTS: [&str; 8] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

/// A command hook which Claude Code must execute directly, without a shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHookCommand {
    executable: PathBuf,
    args: Vec<String>,
    timeout_seconds: u64,
}

impl NativeHookCommand {
    /// Creates a validated exec-form hook command.
    pub fn new(
        executable: impl Into<PathBuf>,
        args: Vec<String>,
        timeout_seconds: u64,
    ) -> Result<Self, NativeHookError> {
        let executable = executable.into();
        validate_executable(&executable)?;
        if args.len() > MAX_ARGS {
            return Err(NativeHookError::InvalidSettings(format!(
                "hook command has more than {MAX_ARGS} arguments"
            )));
        }
        for argument in &args {
            validate_bounded_string(argument, "hook argument", MAX_ARG_BYTES, true)
                .map_err(NativeHookError::InvalidSettings)?;
        }
        if !(1..=60).contains(&timeout_seconds) {
            return Err(NativeHookError::InvalidSettings(
                "hook timeout must be between 1 and 60 seconds".to_owned(),
            ));
        }
        Ok(Self {
            executable,
            args,
            timeout_seconds,
        })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds
    }

    fn handler_json(&self) -> Result<Value, NativeHookError> {
        let command = self.executable.to_str().ok_or_else(|| {
            NativeHookError::InvalidSettings("hook executable path must be valid UTF-8".to_owned())
        })?;
        Ok(json!({
            "type": "command",
            "command": command,
            // Presence of args selects Claude Code's exec form, including when
            // the argument vector is empty.
            "args": self.args,
            "timeout": self.timeout_seconds,
        }))
    }
}

/// Merges agentctl lifecycle hooks into an existing Claude settings value.
///
/// Existing settings and hook groups are preserved byte-for-byte at the JSON
/// value level. Calling this function repeatedly is idempotent. A malformed
/// `hooks` object is rejected instead of overwritten.
pub fn merge_interactive_hook_settings(
    existing: Option<&Value>,
    command: &NativeHookCommand,
) -> Result<Value, NativeHookError> {
    let mut settings = existing.cloned().unwrap_or_else(|| json!({}));
    let root = settings.as_object_mut().ok_or_else(|| {
        NativeHookError::InvalidSettings("Claude settings root must be an object".to_owned())
    })?;
    let hooks = object_field_or_insert(root, "hooks")?;
    let handler = command.handler_json()?;

    for event_name in INTERACTIVE_HOOK_EVENTS {
        let groups = array_field_or_insert(hooks, event_name)?;
        if !contains_unmatched_handler(groups, &handler) {
            groups.push(json!({ "hooks": [handler.clone()] }));
        }
    }
    Ok(settings)
}

/// Convenience builder for callers forwarding their current executable and
/// argument vector to the native hook handler.
pub fn build_native_hook_settings(
    existing: Option<&Value>,
    executable: &Path,
    command_args: &[OsString],
) -> Result<Value, NativeHookError> {
    let args = command_args
        .iter()
        .map(|argument| {
            argument.to_str().map(ToOwned::to_owned).ok_or_else(|| {
                NativeHookError::InvalidSettings("hook arguments must be valid UTF-8".to_owned())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let command = NativeHookCommand::new(executable, args, DEFAULT_NATIVE_HOOK_TIMEOUT_SECONDS)?;
    merge_interactive_hook_settings(existing, &command)
}

/// Parses, merges, and pretty-prints a Claude settings document.
pub fn merge_interactive_hook_settings_json(
    existing: Option<&[u8]>,
    command: &NativeHookCommand,
) -> Result<Vec<u8>, NativeHookError> {
    let parsed = match existing {
        Some(bytes) if !bytes.is_empty() => Some(serde_json::from_slice(bytes)?),
        _ => None,
    };
    let merged = merge_interactive_hook_settings(parsed.as_ref(), command)?;
    Ok(serde_json::to_vec_pretty(&merged)?)
}

/// Fields common to all supported native Claude hook events.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NativeHookCommon {
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub cwd: PathBuf,
    pub prompt_id: Option<String>,
    pub permission_mode: Option<PermissionMode>,
}

/// Claude Code permission modes documented for hook payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PermissionMode {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "manual")]
    Manual,
    #[serde(rename = "plan")]
    Plan,
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "dontAsk")]
    DontAsk,
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStartSource {
    Startup,
    Resume,
    Clear,
    Compact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndReason {
    Clear,
    Resume,
    Logout,
    PromptInputExit,
    BypassPermissionsDisabled,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopFailureKind {
    RateLimit,
    Overloaded,
    AuthenticationFailed,
    OauthOrgNotAllowed,
    BillingError,
    InvalidRequest,
    ModelNotFound,
    ServerError,
    MaxOutputTokens,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionStartHook {
    pub common: NativeHookCommon,
    pub source: SessionStartSource,
    pub model: Option<String>,
    pub agent_type: Option<String>,
    pub session_title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UserPromptSubmitHook {
    pub common: NativeHookCommon,
    pub prompt: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PreToolUseHook {
    pub common: NativeHookCommon,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PostToolUseHook {
    pub common: NativeHookCommon,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_response: Value,
    pub tool_use_id: String,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PostToolUseFailureHook {
    pub common: NativeHookCommon,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: String,
    pub error: String,
    pub is_interrupt: Option<bool>,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StopHook {
    pub common: NativeHookCommon,
    pub stop_hook_active: bool,
    pub last_assistant_message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StopFailureHook {
    pub common: NativeHookCommon,
    pub error: StopFailureKind,
    pub error_details: Option<String>,
    pub last_assistant_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionEndHook {
    pub common: NativeHookCommon,
    pub reason: SessionEndReason,
}

/// Normalized lifecycle event from a provider-owned interactive Claude session.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
pub enum NativeHookEvent {
    SessionStart(SessionStartHook),
    UserPromptSubmit(UserPromptSubmitHook),
    PreToolUse(PreToolUseHook),
    PostToolUse(PostToolUseHook),
    PostToolUseFailure(PostToolUseFailureHook),
    Stop(StopHook),
    StopFailure(StopFailureHook),
    SessionEnd(SessionEndHook),
}

impl NativeHookEvent {
    pub fn common(&self) -> &NativeHookCommon {
        match self {
            Self::SessionStart(event) => &event.common,
            Self::UserPromptSubmit(event) => &event.common,
            Self::PreToolUse(event) => &event.common,
            Self::PostToolUse(event) => &event.common,
            Self::PostToolUseFailure(event) => &event.common,
            Self::Stop(event) => &event.common,
            Self::StopFailure(event) => &event.common,
            Self::SessionEnd(event) => &event.common,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.common().session_id
    }

    pub fn cwd(&self) -> &Path {
        &self.common().cwd
    }

    /// Validates that an event belongs to the provider session agentctl launched.
    pub fn validate_expected_session(
        &self,
        expected_session_id: &str,
    ) -> Result<(), NativeHookError> {
        validate_bounded_string(
            expected_session_id,
            "expected session_id",
            MAX_ID_BYTES,
            false,
        )
        .map_err(NativeHookError::InvalidPayload)?;
        if self.session_id() != expected_session_id {
            return Err(NativeHookError::SessionMismatch {
                expected: expected_session_id.to_owned(),
                actual: self.session_id().to_owned(),
            });
        }
        Ok(())
    }
}

/// Parses one documented Claude Code command-hook payload.
pub fn parse_hook_payload(input: &[u8]) -> Result<NativeHookEvent, NativeHookError> {
    if input.is_empty() {
        return Err(NativeHookError::InvalidPayload(
            "hook payload is empty".to_owned(),
        ));
    }
    if input.len() > MAX_HOOK_PAYLOAD_BYTES {
        return Err(NativeHookError::PayloadTooLarge {
            actual: input.len(),
            limit: MAX_HOOK_PAYLOAD_BYTES,
        });
    }
    let value: Value = serde_json::from_slice(input)?;
    let object = value.as_object().ok_or_else(|| {
        NativeHookError::InvalidPayload("hook payload must be an object".to_owned())
    })?;
    let event_name = required_string(object, "hook_event_name", MAX_ID_BYTES)?;
    match event_name.as_str() {
        "SessionStart" => parse_session_start(value),
        "UserPromptSubmit" => parse_user_prompt_submit(value),
        "PreToolUse" => parse_pre_tool_use(value),
        "PostToolUse" => parse_post_tool_use(value),
        "PostToolUseFailure" => parse_post_tool_use_failure(value),
        "Stop" => parse_stop(value),
        "StopFailure" => parse_stop_failure(value),
        "SessionEnd" => parse_session_end(value),
        unsupported => Err(NativeHookError::UnsupportedEvent(unsupported.to_owned())),
    }
}

/// Parses a payload and rejects events from a different native Claude session.
pub fn parse_hook_payload_for_session(
    input: &[u8],
    expected_session_id: &str,
) -> Result<NativeHookEvent, NativeHookError> {
    let event = parse_hook_payload(input)?;
    event.validate_expected_session(expected_session_id)?;
    Ok(event)
}

/// Builds the structured stdout expected from a `SessionStart` command hook.
pub fn session_start_additional_context(context: &str) -> Result<Value, NativeHookError> {
    additional_context_for_event("SessionStart", context)
}

/// Builds the structured stdout expected from a `UserPromptSubmit` command
/// hook. This is the safe handoff boundary: Claude adds the context to the
/// prompt that is about to query the model, while an idle native launch does
/// not consume it.
pub fn user_prompt_submit_additional_context(context: &str) -> Result<Value, NativeHookError> {
    additional_context_for_event("UserPromptSubmit", context)
}

fn additional_context_for_event(
    hook_event_name: &'static str,
    context: &str,
) -> Result<Value, NativeHookError> {
    validate_bounded_string(context, "additionalContext", MAX_TEXT_BYTES, false)
        .map_err(NativeHookError::InvalidOutput)?;
    let char_count = context.chars().count();
    if char_count > MAX_CONTEXT_CHARS {
        return Err(NativeHookError::InvalidOutput(format!(
            "additionalContext exceeds Claude Code's {MAX_CONTEXT_CHARS}-character limit"
        )));
    }
    Ok(json!({
        "hookSpecificOutput": {
            "hookEventName": hook_event_name,
            "additionalContext": context,
        }
    }))
}

/// Short alias used by native-hook command handlers when writing stdout.
pub fn additional_context_output(context: &str) -> Result<Value, NativeHookError> {
    session_start_additional_context(context)
}

/// Serializes structured `SessionStart` hook stdout as a single JSON document.
pub fn session_start_additional_context_json(context: &str) -> Result<Vec<u8>, NativeHookError> {
    Ok(serde_json::to_vec(&session_start_additional_context(
        context,
    )?)?)
}

#[derive(Debug, Error)]
pub enum NativeHookError {
    #[error("invalid Claude settings: {0}")]
    InvalidSettings(String),
    #[error("invalid Claude hook payload: {0}")]
    InvalidPayload(String),
    #[error("invalid Claude hook output: {0}")]
    InvalidOutput(String),
    #[error("unsupported Claude hook event: {0}")]
    UnsupportedEvent(String),
    #[error("Claude hook payload is {actual} bytes; limit is {limit} bytes")]
    PayloadTooLarge { actual: usize, limit: usize },
    #[error("Claude hook session mismatch: expected {expected}, received {actual}")]
    SessionMismatch { expected: String, actual: String },
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Deserialize)]
struct CommonWire {
    session_id: String,
    transcript_path: String,
    cwd: String,
    prompt_id: Option<String>,
    permission_mode: Option<PermissionMode>,
}

impl TryFrom<CommonWire> for NativeHookCommon {
    type Error = NativeHookError;

    fn try_from(wire: CommonWire) -> Result<Self, Self::Error> {
        validate_bounded_string(&wire.session_id, "session_id", MAX_ID_BYTES, false)
            .map_err(NativeHookError::InvalidPayload)?;
        if let Some(prompt_id) = &wire.prompt_id {
            validate_bounded_string(prompt_id, "prompt_id", MAX_ID_BYTES, false)
                .map_err(NativeHookError::InvalidPayload)?;
            uuid::Uuid::parse_str(prompt_id).map_err(|_| {
                NativeHookError::InvalidPayload("prompt_id must be a UUID".to_owned())
            })?;
        }
        let transcript_path = validated_absolute_path(&wire.transcript_path, "transcript_path")?;
        let cwd = validated_absolute_path(&wire.cwd, "cwd")?;
        Ok(Self {
            session_id: wire.session_id,
            transcript_path,
            cwd,
            prompt_id: wire.prompt_id,
            permission_mode: wire.permission_mode,
        })
    }
}

#[derive(Debug, Deserialize)]
struct SessionStartWire {
    #[serde(flatten)]
    common: CommonWire,
    source: SessionStartSource,
    model: Option<String>,
    agent_type: Option<String>,
    session_title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserPromptSubmitWire {
    #[serde(flatten)]
    common: CommonWire,
    prompt: String,
}

#[derive(Debug, Deserialize)]
struct PreToolUseWire {
    #[serde(flatten)]
    common: CommonWire,
    tool_name: String,
    tool_input: Value,
    tool_use_id: String,
}

#[derive(Debug, Deserialize)]
struct PostToolUseWire {
    #[serde(flatten)]
    common: CommonWire,
    tool_name: String,
    tool_input: Value,
    tool_response: Value,
    tool_use_id: String,
    duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PostToolUseFailureWire {
    #[serde(flatten)]
    common: CommonWire,
    tool_name: String,
    tool_input: Value,
    tool_use_id: String,
    error: String,
    is_interrupt: Option<bool>,
    duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct StopWire {
    #[serde(flatten)]
    common: CommonWire,
    stop_hook_active: bool,
    last_assistant_message: String,
}

#[derive(Debug, Deserialize)]
struct StopFailureWire {
    #[serde(flatten)]
    common: CommonWire,
    error: StopFailureKind,
    error_details: Option<String>,
    last_assistant_message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionEndWire {
    #[serde(flatten)]
    common: CommonWire,
    reason: SessionEndReason,
}

fn parse_session_start(value: Value) -> Result<NativeHookEvent, NativeHookError> {
    let wire: SessionStartWire = serde_json::from_value(value)?;
    validate_optional_text(wire.model.as_deref(), "model", MAX_ID_BYTES)?;
    validate_optional_text(wire.agent_type.as_deref(), "agent_type", MAX_ID_BYTES)?;
    validate_optional_text(wire.session_title.as_deref(), "session_title", MAX_ID_BYTES)?;
    Ok(NativeHookEvent::SessionStart(SessionStartHook {
        common: wire.common.try_into()?,
        source: wire.source,
        model: wire.model,
        agent_type: wire.agent_type,
        session_title: wire.session_title,
    }))
}

fn parse_user_prompt_submit(value: Value) -> Result<NativeHookEvent, NativeHookError> {
    let wire: UserPromptSubmitWire = serde_json::from_value(value)?;
    validate_bounded_string(&wire.prompt, "prompt", MAX_TEXT_BYTES, false)
        .map_err(NativeHookError::InvalidPayload)?;
    Ok(NativeHookEvent::UserPromptSubmit(UserPromptSubmitHook {
        common: wire.common.try_into()?,
        prompt: wire.prompt,
    }))
}

fn parse_pre_tool_use(value: Value) -> Result<NativeHookEvent, NativeHookError> {
    let wire: PreToolUseWire = serde_json::from_value(value)?;
    validate_tool_fields(&wire.tool_name, &wire.tool_input, &wire.tool_use_id)?;
    Ok(NativeHookEvent::PreToolUse(PreToolUseHook {
        common: wire.common.try_into()?,
        tool_name: wire.tool_name,
        tool_input: wire.tool_input,
        tool_use_id: wire.tool_use_id,
    }))
}

fn parse_post_tool_use(value: Value) -> Result<NativeHookEvent, NativeHookError> {
    let wire: PostToolUseWire = serde_json::from_value(value)?;
    validate_tool_fields(&wire.tool_name, &wire.tool_input, &wire.tool_use_id)?;
    Ok(NativeHookEvent::PostToolUse(PostToolUseHook {
        common: wire.common.try_into()?,
        tool_name: wire.tool_name,
        tool_input: wire.tool_input,
        tool_response: wire.tool_response,
        tool_use_id: wire.tool_use_id,
        duration_ms: wire.duration_ms,
    }))
}

fn parse_post_tool_use_failure(value: Value) -> Result<NativeHookEvent, NativeHookError> {
    let wire: PostToolUseFailureWire = serde_json::from_value(value)?;
    validate_tool_fields(&wire.tool_name, &wire.tool_input, &wire.tool_use_id)?;
    validate_bounded_string(&wire.error, "error", MAX_TEXT_BYTES, false)
        .map_err(NativeHookError::InvalidPayload)?;
    Ok(NativeHookEvent::PostToolUseFailure(
        PostToolUseFailureHook {
            common: wire.common.try_into()?,
            tool_name: wire.tool_name,
            tool_input: wire.tool_input,
            tool_use_id: wire.tool_use_id,
            error: wire.error,
            is_interrupt: wire.is_interrupt,
            duration_ms: wire.duration_ms,
        },
    ))
}

fn parse_stop(value: Value) -> Result<NativeHookEvent, NativeHookError> {
    let wire: StopWire = serde_json::from_value(value)?;
    validate_bounded_string(
        &wire.last_assistant_message,
        "last_assistant_message",
        MAX_TEXT_BYTES,
        true,
    )
    .map_err(NativeHookError::InvalidPayload)?;
    Ok(NativeHookEvent::Stop(StopHook {
        common: wire.common.try_into()?,
        stop_hook_active: wire.stop_hook_active,
        last_assistant_message: wire.last_assistant_message,
    }))
}

fn parse_stop_failure(value: Value) -> Result<NativeHookEvent, NativeHookError> {
    let wire: StopFailureWire = serde_json::from_value(value)?;
    validate_optional_text(
        wire.error_details.as_deref(),
        "error_details",
        MAX_TEXT_BYTES,
    )?;
    validate_optional_text(
        wire.last_assistant_message.as_deref(),
        "last_assistant_message",
        MAX_TEXT_BYTES,
    )?;
    Ok(NativeHookEvent::StopFailure(StopFailureHook {
        common: wire.common.try_into()?,
        error: wire.error,
        error_details: wire.error_details,
        last_assistant_message: wire.last_assistant_message,
    }))
}

fn parse_session_end(value: Value) -> Result<NativeHookEvent, NativeHookError> {
    let wire: SessionEndWire = serde_json::from_value(value)?;
    Ok(NativeHookEvent::SessionEnd(SessionEndHook {
        common: wire.common.try_into()?,
        reason: wire.reason,
    }))
}

fn validate_tool_fields(
    tool_name: &str,
    tool_input: &Value,
    tool_use_id: &str,
) -> Result<(), NativeHookError> {
    validate_bounded_string(tool_name, "tool_name", MAX_ID_BYTES, false)
        .map_err(NativeHookError::InvalidPayload)?;
    validate_bounded_string(tool_use_id, "tool_use_id", MAX_ID_BYTES, false)
        .map_err(NativeHookError::InvalidPayload)?;
    if !tool_input.is_object() {
        return Err(NativeHookError::InvalidPayload(
            "tool_input must be an object".to_owned(),
        ));
    }
    Ok(())
}

fn validate_optional_text(
    value: Option<&str>,
    name: &str,
    limit: usize,
) -> Result<(), NativeHookError> {
    if let Some(value) = value {
        validate_bounded_string(value, name, limit, true)
            .map_err(NativeHookError::InvalidPayload)?;
    }
    Ok(())
}

fn validated_absolute_path(value: &str, name: &str) -> Result<PathBuf, NativeHookError> {
    validate_bounded_string(value, name, MAX_PATH_BYTES, false)
        .map_err(NativeHookError::InvalidPayload)?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(NativeHookError::InvalidPayload(format!(
            "{name} must be absolute"
        )));
    }
    Ok(path)
}

fn validate_executable(path: &Path) -> Result<(), NativeHookError> {
    if !path.is_absolute() {
        return Err(NativeHookError::InvalidSettings(
            "hook executable path must be absolute".to_owned(),
        ));
    }
    let encoded = path.to_str().ok_or_else(|| {
        NativeHookError::InvalidSettings("hook executable path must be valid UTF-8".to_owned())
    })?;
    validate_bounded_string(encoded, "hook executable", MAX_PATH_BYTES, false)
        .map_err(NativeHookError::InvalidSettings)
}

fn validate_bounded_string(
    value: &str,
    name: &str,
    limit: usize,
    allow_empty: bool,
) -> Result<(), String> {
    if !allow_empty && value.is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    if value.len() > limit {
        return Err(format!("{name} exceeds {limit} bytes"));
    }
    if value.contains('\0') {
        return Err(format!("{name} contains a NUL byte"));
    }
    Ok(())
}

fn object_field_or_insert<'a>(
    object: &'a mut Map<String, Value>,
    field: &str,
) -> Result<&'a mut Map<String, Value>, NativeHookError> {
    let value = object.entry(field.to_owned()).or_insert_with(|| json!({}));
    value.as_object_mut().ok_or_else(|| {
        NativeHookError::InvalidSettings(format!("Claude settings `{field}` must be an object"))
    })
}

fn array_field_or_insert<'a>(
    object: &'a mut Map<String, Value>,
    field: &str,
) -> Result<&'a mut Vec<Value>, NativeHookError> {
    let value = object.entry(field.to_owned()).or_insert_with(|| json!([]));
    value.as_array_mut().ok_or_else(|| {
        NativeHookError::InvalidSettings(format!("Claude settings hook `{field}` must be an array"))
    })
}

fn contains_unmatched_handler(groups: &[Value], handler: &Value) -> bool {
    groups.iter().any(|group| {
        let Some(group) = group.as_object() else {
            return false;
        };
        if group.contains_key("matcher") {
            return false;
        }
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|handlers| handlers.contains(handler))
    })
}

fn required_string(
    object: &Map<String, Value>,
    field: &str,
    limit: usize,
) -> Result<String, NativeHookError> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| NativeHookError::InvalidPayload(format!("{field} must be a string")))?;
    validate_bounded_string(value, field, limit, false).map_err(NativeHookError::InvalidPayload)?;
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::{Value, json};

    use super::{
        INTERACTIVE_HOOK_EVENTS, NativeHookCommand, NativeHookError, NativeHookEvent,
        PermissionMode, SessionEndReason, SessionStartSource, StopFailureKind,
        merge_interactive_hook_settings, merge_interactive_hook_settings_json, parse_hook_payload,
        parse_hook_payload_for_session, session_start_additional_context,
        session_start_additional_context_json, user_prompt_submit_additional_context,
    };

    fn common(event: &str) -> Value {
        json!({
            "session_id": "claude-session",
            "prompt_id": "550e8400-e29b-41d4-a716-446655440000",
            "transcript_path": "/tmp/claude-session.jsonl",
            "cwd": "/repo",
            "permission_mode": "default",
            "hook_event_name": event,
        })
    }

    fn with_fields(event: &str, fields: Value) -> Vec<u8> {
        let mut payload = common(event);
        let Value::Object(fields) = fields else {
            panic!("test fields must be an object");
        };
        payload.as_object_mut().unwrap().extend(fields);
        serde_json::to_vec(&payload).unwrap()
    }

    fn command() -> NativeHookCommand {
        NativeHookCommand::new(
            Path::new("/usr/local/bin/agentctl"),
            vec!["native-hook".to_owned(), "ingest".to_owned()],
            10,
        )
        .unwrap()
    }

    #[test]
    fn merges_all_hooks_without_overwriting_user_settings() {
        let existing = json!({
            "model": "opus",
            "permissions": {"allow": ["Bash(cargo test:*)"]},
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write",
                    "hooks": [{"type": "command", "command": "/opt/format", "args": []}]
                }],
                "Notification": [{"hooks": [{"type": "command", "command": "/opt/ping"}]}]
            }
        });
        let merged = merge_interactive_hook_settings(Some(&existing), &command()).unwrap();

        assert_eq!(merged["model"], "opus");
        assert_eq!(merged["permissions"], existing["permissions"]);
        assert_eq!(
            merged["hooks"]["Notification"],
            existing["hooks"]["Notification"]
        );
        assert_eq!(
            merged["hooks"]["PostToolUse"][0],
            existing["hooks"]["PostToolUse"][0]
        );
        for event in INTERACTIVE_HOOK_EVENTS {
            let groups = merged["hooks"][event].as_array().unwrap();
            let handler = groups.last().unwrap()["hooks"][0].as_object().unwrap();
            assert_eq!(handler["type"], "command");
            assert_eq!(handler["command"], "/usr/local/bin/agentctl");
            assert_eq!(handler["args"], json!(["native-hook", "ingest"]));
            assert_eq!(handler["timeout"], 10);
            assert!(handler.get("shell").is_none());
        }
    }

    #[test]
    fn settings_merge_is_idempotent_and_json_round_trips() {
        let first = merge_interactive_hook_settings(None, &command()).unwrap();
        let second = merge_interactive_hook_settings(Some(&first), &command()).unwrap();
        assert_eq!(first, second);

        let encoded =
            merge_interactive_hook_settings_json(Some(br#"{"theme":"dark"}"#), &command()).unwrap();
        let decoded: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded["theme"], "dark");
    }

    #[test]
    fn settings_fail_closed_for_invalid_shapes_and_commands() {
        let error =
            merge_interactive_hook_settings(Some(&json!({"hooks": []})), &command()).unwrap_err();
        assert!(matches!(error, NativeHookError::InvalidSettings(_)));
        let error = NativeHookCommand::new("agentctl", Vec::new(), 10).unwrap_err();
        assert!(matches!(error, NativeHookError::InvalidSettings(_)));
        let error = NativeHookCommand::new("/bin/agentctl", Vec::new(), 0).unwrap_err();
        assert!(matches!(error, NativeHookError::InvalidSettings(_)));
    }

    #[test]
    fn parses_session_start() {
        let event = parse_hook_payload(&with_fields(
            "SessionStart",
            json!({
                "source": "resume",
                "permission_mode": "manual",
                "model": "claude-opus-4",
                "agent_type": "reviewer",
                "session_title": "native chat"
            }),
        ))
        .unwrap();
        let NativeHookEvent::SessionStart(event) = event else {
            panic!("wrong event")
        };
        assert_eq!(event.source, SessionStartSource::Resume);
        assert_eq!(event.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(event.common.permission_mode, Some(PermissionMode::Manual));
        assert_eq!(event.common.cwd, Path::new("/repo"));
    }

    #[test]
    fn parses_user_prompt_submit() {
        let event = parse_hook_payload(&with_fields(
            "UserPromptSubmit",
            json!({"prompt": "continue the migration"}),
        ))
        .unwrap();
        let NativeHookEvent::UserPromptSubmit(event) = event else {
            panic!("wrong event")
        };
        assert_eq!(event.prompt, "continue the migration");
    }

    #[test]
    fn parses_pre_tool_use() {
        let event = parse_hook_payload(&with_fields(
            "PreToolUse",
            json!({
                "tool_name": "Bash",
                "tool_input": {"command": "cargo test"},
                "tool_use_id": "toolu_pre_123"
            }),
        ))
        .unwrap();
        let NativeHookEvent::PreToolUse(event) = event else {
            panic!("wrong event")
        };
        assert_eq!(event.tool_name, "Bash");
        assert_eq!(event.tool_input["command"], "cargo test");
    }

    #[test]
    fn parses_successful_tool_use_without_assuming_response_shape() {
        let event = parse_hook_payload(&with_fields(
            "PostToolUse",
            json!({
                "tool_name": "Write",
                "tool_input": {"file_path": "/repo/src/lib.rs", "content": "safe"},
                "tool_response": ["provider", "specific", "response"],
                "tool_use_id": "toolu_123",
                "duration_ms": 12
            }),
        ))
        .unwrap();
        let NativeHookEvent::PostToolUse(event) = event else {
            panic!("wrong event")
        };
        assert_eq!(event.tool_name, "Write");
        assert_eq!(event.tool_input["file_path"], "/repo/src/lib.rs");
        assert_eq!(event.tool_response[0], "provider");
        assert_eq!(event.duration_ms, Some(12));
    }

    #[test]
    fn parses_failed_tool_use() {
        let event = parse_hook_payload(&with_fields(
            "PostToolUseFailure",
            json!({
                "tool_name": "Bash",
                "tool_input": {"command": "cargo test"},
                "tool_use_id": "toolu_456",
                "error": "exit status 1",
                "is_interrupt": false,
                "duration_ms": 4187
            }),
        ))
        .unwrap();
        let NativeHookEvent::PostToolUseFailure(event) = event else {
            panic!("wrong event")
        };
        assert_eq!(event.error, "exit status 1");
        assert_eq!(event.is_interrupt, Some(false));
        assert_eq!(event.tool_input["command"], "cargo test");
    }

    #[test]
    fn parses_stop_and_stop_failure() {
        let stop = parse_hook_payload(&with_fields(
            "Stop",
            json!({
                "stop_hook_active": false,
                "last_assistant_message": "Migration completed."
            }),
        ))
        .unwrap();
        let NativeHookEvent::Stop(stop) = stop else {
            panic!("wrong event")
        };
        assert!(!stop.stop_hook_active);
        assert_eq!(stop.last_assistant_message, "Migration completed.");

        let failure = parse_hook_payload(&with_fields(
            "StopFailure",
            json!({
                "error": "rate_limit",
                "error_details": "429 Too Many Requests",
                "last_assistant_message": "API Error: Rate limit reached"
            }),
        ))
        .unwrap();
        let NativeHookEvent::StopFailure(failure) = failure else {
            panic!("wrong event")
        };
        assert_eq!(failure.error, StopFailureKind::RateLimit);
        assert_eq!(
            failure.error_details.as_deref(),
            Some("429 Too Many Requests")
        );
    }

    #[test]
    fn parses_session_end() {
        let event = parse_hook_payload(&with_fields(
            "SessionEnd",
            json!({"reason": "prompt_input_exit"}),
        ))
        .unwrap();
        let NativeHookEvent::SessionEnd(event) = event else {
            panic!("wrong event")
        };
        assert_eq!(event.reason, SessionEndReason::PromptInputExit);
    }

    #[test]
    fn validates_expected_native_session() {
        let payload = with_fields("UserPromptSubmit", json!({"prompt": "hello"}));
        parse_hook_payload_for_session(&payload, "claude-session").unwrap();
        let error = parse_hook_payload_for_session(&payload, "different-session").unwrap_err();
        assert!(matches!(error, NativeHookError::SessionMismatch { .. }));
    }

    #[test]
    fn rejects_unknown_events_missing_fields_and_wrong_types() {
        let unknown = serde_json::to_vec(&common("FutureEvent")).unwrap();
        assert!(matches!(
            parse_hook_payload(&unknown),
            Err(NativeHookError::UnsupportedEvent(_))
        ));
        let missing_prompt = serde_json::to_vec(&common("UserPromptSubmit")).unwrap();
        assert!(matches!(
            parse_hook_payload(&missing_prompt),
            Err(NativeHookError::Json(_))
        ));
        let wrong_tool_input = with_fields(
            "PostToolUse",
            json!({
                "tool_name": "Bash",
                "tool_input": "cargo test",
                "tool_response": {},
                "tool_use_id": "toolu_1"
            }),
        );
        assert!(matches!(
            parse_hook_payload(&wrong_tool_input),
            Err(NativeHookError::InvalidPayload(_))
        ));
    }

    #[test]
    fn rejects_relative_paths_invalid_enums_and_invalid_prompt_ids() {
        let mut relative = common("SessionEnd");
        relative["cwd"] = json!("relative/path");
        relative["reason"] = json!("other");
        assert!(matches!(
            parse_hook_payload(&serde_json::to_vec(&relative).unwrap()),
            Err(NativeHookError::InvalidPayload(_))
        ));

        let invalid_reason = with_fields("SessionEnd", json!({"reason": "future_reason"}));
        assert!(matches!(
            parse_hook_payload(&invalid_reason),
            Err(NativeHookError::Json(_))
        ));

        let mut invalid_prompt_id = common("SessionEnd");
        invalid_prompt_id["prompt_id"] = json!("not-a-uuid");
        invalid_prompt_id["reason"] = json!("other");
        assert!(matches!(
            parse_hook_payload(&serde_json::to_vec(&invalid_prompt_id).unwrap()),
            Err(NativeHookError::InvalidPayload(_))
        ));
    }

    #[test]
    fn builds_bounded_structured_session_start_output() {
        let value = session_start_additional_context("handoff from Codex").unwrap();
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(
            value["hookSpecificOutput"]["additionalContext"],
            "handoff from Codex"
        );
        let bytes = session_start_additional_context_json("context").unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["hookSpecificOutput"]["additionalContext"],
            "context"
        );
        assert!(matches!(
            session_start_additional_context(""),
            Err(NativeHookError::InvalidOutput(_))
        ));
        assert!(matches!(
            session_start_additional_context(&"x".repeat(10_001)),
            Err(NativeHookError::InvalidOutput(_))
        ));
    }

    #[test]
    fn builds_user_prompt_submit_additional_context_output() {
        let value = user_prompt_submit_additional_context("handoff from Codex").unwrap();
        assert_eq!(
            value["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert_eq!(
            value["hookSpecificOutput"]["additionalContext"],
            "handoff from Codex"
        );
        assert!(matches!(
            user_prompt_submit_additional_context(""),
            Err(NativeHookError::InvalidOutput(_))
        ));
    }
}
