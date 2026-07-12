use std::path::PathBuf;

use agentctl_core::{
    AgentEvent, FileChange, FileChangeKind, NormalizedProviderError, ProviderError, ProviderKind,
    RateLimitSnapshot, ToolOutput, TurnStatus, UsageSnapshot,
};
use chrono::{DateTime, Utc};
use serde_json::Value;

/// Maps one complete Claude Code stream-json frame and always retains its raw envelope.
pub(crate) fn map_message(frame: &Value) -> Vec<AgentEvent> {
    let wire_kind = wire_kind(frame);
    let mut events = vec![AgentEvent::ProviderSpecific {
        provider: ProviderKind::Claude,
        kind: format!("raw:{wire_kind}"),
        payload: frame.clone(),
    }];
    let mut normalized = match frame.get("type").and_then(Value::as_str) {
        Some("system") if frame.get("subtype").and_then(Value::as_str) == Some("init") => frame
            .get("session_id")
            .and_then(Value::as_str)
            .map(|id| {
                vec![AgentEvent::SessionStarted {
                    native_session_id: id.to_owned(),
                }]
            })
            .unwrap_or_default(),
        Some("stream_event") => map_stream_event(frame.get("event").unwrap_or(&Value::Null)),
        Some("assistant") => map_assistant(frame),
        Some("user") => map_user(frame),
        Some("result") => map_result(frame),
        Some("rate_limit_event") => parse_rate_limit(frame)
            .map(|limit| vec![AgentEvent::RateLimitUpdated { limit }])
            .unwrap_or_default(),
        Some("system")
            if frame.get("subtype").and_then(Value::as_str) == Some("permission_denied") =>
        {
            vec![AgentEvent::Error {
                error: NormalizedProviderError {
                    code: "permission_denied".to_owned(),
                    message: frame
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Claude denied a tool permission")
                        .to_owned(),
                    retryable: false,
                },
            }]
        }
        Some("system") if frame.get("subtype").and_then(Value::as_str) == Some("api_retry") => {
            vec![AgentEvent::Error {
                error: NormalizedProviderError {
                    code: frame
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("api_retry")
                        .to_owned(),
                    message: format!(
                        "Claude API retry {}/{}",
                        frame.get("attempt").and_then(Value::as_u64).unwrap_or(0),
                        frame
                            .get("max_retries")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                    ),
                    retryable: true,
                },
            }]
        }
        Some("agentctl_process_error") => vec![AgentEvent::Error {
            error: NormalizedProviderError {
                code: "process".to_owned(),
                message: frame
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Claude process failed")
                    .to_owned(),
                retryable: true,
            },
        }],
        _ => vec![AgentEvent::ProviderSpecific {
            provider: ProviderKind::Claude,
            kind: wire_kind,
            payload: frame.clone(),
        }],
    };
    events.append(&mut normalized);
    events
}

pub(crate) fn classify_result_error(frame: &Value) -> Option<ProviderError> {
    if frame.get("type").and_then(Value::as_str) != Some("result")
        || (frame.get("subtype").and_then(Value::as_str) == Some("success")
            && !frame
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false))
    {
        return None;
    }
    let message = frame
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("; ");
    let lower = message.to_ascii_lowercase();
    if lower.contains("rate limit")
        || frame.get("api_error_status").and_then(Value::as_u64) == Some(429)
    {
        Some(ProviderError::RateLimited { resets_at: None })
    } else if lower.contains("overload")
        || frame.get("api_error_status").and_then(Value::as_u64) == Some(529)
    {
        Some(ProviderError::Overloaded(message))
    } else if lower.contains("authentication")
        || lower.contains("unauthorized")
        || lower.contains("billing")
        || matches!(
            frame.get("api_error_status").and_then(Value::as_u64),
            Some(401..=403)
        )
    {
        Some(ProviderError::Authentication(message))
    } else {
        Some(ProviderError::Process(if message.is_empty() {
            "Claude turn failed".to_owned()
        } else {
            message
        }))
    }
}

fn map_stream_event(event: &Value) -> Vec<AgentEvent> {
    match event.get("type").and_then(Value::as_str) {
        Some("content_block_delta")
            if event.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta") =>
        {
            event
                .pointer("/delta/text")
                .and_then(Value::as_str)
                .map(|text| {
                    vec![AgentEvent::AssistantTextDelta {
                        text: text.to_owned(),
                    }]
                })
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

fn map_assistant(frame: &Value) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    if let Some(error) = frame.get("error").and_then(Value::as_str) {
        events.push(AgentEvent::Error {
            error: classified_error(error, error),
        });
    }
    for block in frame
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
            events.extend(map_tool_start(block));
        }
    }
    events
}

fn map_tool_start(block: &Value) -> Vec<AgentEvent> {
    let id = block
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let input = block.get("input").cloned().unwrap_or(Value::Null);
    let mut events = vec![AgentEvent::ToolStarted {
        id: id.clone(),
        name: name.clone(),
        input: input.clone(),
    }];
    if matches!(name.as_str(), "Bash" | "Shell" | "Terminal") {
        events.push(AgentEvent::CommandStarted {
            id,
            command: input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            cwd: PathBuf::from(input.get("cwd").and_then(Value::as_str).unwrap_or_default()),
        });
    }
    events
}

fn map_user(frame: &Value) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let mut completed_tool_ids = Vec::new();
    for block in frame
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let id = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        completed_tool_ids.push(id.clone());
        let summary = block.get("content").map(compact_string);
        let size = summary
            .as_ref()
            .and_then(|summary| u64::try_from(summary.len()).ok());
        events.push(AgentEvent::ToolCompleted {
            id,
            output: ToolOutput {
                summary,
                digest: None,
                size,
            },
        });
    }
    if let Some(result) = frame.get("tool_use_result")
        && let Some(path) = result
            .get("filePath")
            .or_else(|| result.get("path"))
            .and_then(Value::as_str)
    {
        events.push(AgentEvent::FilesChanged {
            changes: vec![FileChange {
                path: PathBuf::from(path),
                kind: FileChangeKind::Modified,
                digest: None,
            }],
        });
    }
    if let Some(result) = frame.get("tool_use_result") {
        let exit_code = result
            .get("exitCode")
            .or_else(|| result.get("exit_code"))
            .or_else(|| result.get("returnCode"))
            .and_then(Value::as_i64)
            .and_then(|code| i32::try_from(code).ok());
        if exit_code.is_some() {
            for id in completed_tool_ids {
                events.push(AgentEvent::CommandCompleted {
                    id,
                    exit_code,
                    output_digest: None,
                });
            }
        }
    }
    events
}

fn map_result(frame: &Value) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    if let Some(usage) = parse_usage(frame) {
        events.push(AgentEvent::UsageUpdated { usage });
    }
    if frame.get("subtype").and_then(Value::as_str) == Some("success")
        && !frame
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        if let Some(text) = frame.get("result").and_then(Value::as_str) {
            events.push(AgentEvent::AssistantFinal {
                text: text.to_owned(),
            });
        }
        events.push(AgentEvent::TurnCompleted {
            status: TurnStatus::Completed,
        });
    } else {
        let message = frame
            .get("errors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("; ");
        let error_name = detect_error_name(&message);
        events.push(AgentEvent::Error {
            error: classified_error(error_name, &message),
        });
        events.push(AgentEvent::TurnCompleted {
            status: TurnStatus::Failed,
        });
    }
    events
}

fn parse_usage(frame: &Value) -> Option<UsageSnapshot> {
    let usage = frame.get("usage")?;
    Some(UsageSnapshot {
        input_tokens: unsigned(usage.get("input_tokens")),
        output_tokens: unsigned(usage.get("output_tokens")),
        cached_input_tokens: unsigned(
            usage
                .get("cache_read_input_tokens")
                .or_else(|| usage.get("cached_input_tokens")),
        ),
        cost_usd: frame.get("total_cost_usd").and_then(Value::as_f64),
    })
}

fn parse_rate_limit(frame: &Value) -> Option<RateLimitSnapshot> {
    let info = frame.get("rate_limit_info")?;
    let rate_type = info.get("rateLimitType").and_then(Value::as_str);
    Some(RateLimitSnapshot {
        utilization: info
            .get("utilization")
            .and_then(Value::as_f64)
            .map(|value| if value > 1.0 { value / 100.0 } else { value })
            .map(|value| value.clamp(0.0, 1.0)),
        window_seconds: match rate_type {
            Some("five_hour") => Some(5 * 60 * 60),
            Some(value) if value.starts_with("seven_day") => Some(7 * 24 * 60 * 60),
            _ => None,
        },
        resets_at: parse_timestamp(info.get("resetsAt")),
        source: "claude_rate_limit_event".to_owned(),
    })
}

fn parse_timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let value = value?;
    if let Some(timestamp) = value.as_i64() {
        let seconds = if timestamp > 10_000_000_000 {
            timestamp / 1_000
        } else {
            timestamp
        };
        return DateTime::<Utc>::from_timestamp(seconds, 0);
    }
    value
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn classified_error(code: &str, message: &str) -> NormalizedProviderError {
    let retryable = matches!(
        code,
        "rate_limit" | "overloaded" | "server_error" | "unknown"
    );
    NormalizedProviderError {
        code: code.to_owned(),
        message: if message.is_empty() {
            format!("Claude provider error: {code}")
        } else {
            message.to_owned()
        },
        retryable,
    }
}

fn detect_error_name(message: &str) -> &str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("rate limit") {
        "rate_limit"
    } else if lower.contains("overload") {
        "overloaded"
    } else if lower.contains("authentication") || lower.contains("unauthorized") {
        "authentication_failed"
    } else if lower.contains("billing") {
        "billing_error"
    } else {
        "execution_error"
    }
}

fn wire_kind(frame: &Value) -> String {
    let kind = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let detail = frame
        .get("subtype")
        .and_then(Value::as_str)
        .or_else(|| frame.pointer("/event/type").and_then(Value::as_str));
    detail.map_or_else(|| kind.to_owned(), |detail| format!("{kind}:{detail}"))
}

fn compact_string(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

fn unsigned(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
    })
}

#[cfg(test)]
mod tests {
    use super::{classify_result_error, map_message};
    use agentctl_core::{AgentEvent, ProviderError, TurnStatus};

    #[test]
    fn maps_redacted_fixture_without_a_model_call() {
        let events = include_str!("../../../fixtures/claude/2.1/turn.jsonl")
            .lines()
            .flat_map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                map_message(&value)
            })
            .collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::AssistantTextDelta { text } if text == "Hello"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::CommandCompleted {
                exit_code: Some(0),
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ProviderSpecific { kind, .. } if kind.starts_with("raw:")
        )));
    }

    #[test]
    fn classifies_rate_limit_result_for_failover() {
        let error = classify_result_error(&serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "api_error_status": 429,
            "errors": ["rate limit exceeded"]
        }))
        .unwrap();
        assert!(matches!(error, ProviderError::RateLimited { .. }));
    }

    #[test]
    fn malformed_process_frame_is_normalized_and_later_result_remains_mappable() {
        let frames = [
            serde_json::json!({
                "type": "agentctl_process_error",
                "message": "malformed Claude JSONL frame at line 4"
            }),
            serde_json::json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "result": "recovered"
            }),
        ];
        let events = frames.iter().flat_map(map_message).collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error { error }
                if error.code == "process" && error.message.contains("malformed")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::AssistantFinal { text } if text == "recovered"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed
            }
        )));
    }
}
