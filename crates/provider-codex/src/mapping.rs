use std::path::PathBuf;

use agentctl_core::{
    AgentEvent, FileChange, FileChangeKind, NormalizedProviderError, PlanStep, ProviderKind,
    RateLimitSnapshot, ToolOutput, TurnStatus, UsageSnapshot,
};
use chrono::{DateTime, Utc};
use serde_json::Value;

/// Maps a Codex app-server notification into provider-neutral events.
/// Unknown notifications are deliberately retained as provider-specific data.
pub fn map_notification(method: &str, params: &Value) -> Vec<AgentEvent> {
    let mut mapped = match method {
        "thread/started" => params
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(|id| {
                vec![AgentEvent::SessionStarted {
                    native_session_id: id.to_owned(),
                }]
            })
            .unwrap_or_default(),
        "item/agentMessage/delta" => params
            .get("delta")
            .and_then(Value::as_str)
            .map(|text| {
                vec![AgentEvent::AssistantTextDelta {
                    text: text.to_owned(),
                }]
            })
            .unwrap_or_default(),
        "item/started" => map_item_started(params.get("item").unwrap_or(&Value::Null)),
        "item/completed" => map_item_completed(params.get("item").unwrap_or(&Value::Null)),
        "turn/plan/updated" => vec![AgentEvent::PlanUpdated {
            steps: params
                .get("plan")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|step| {
                    Some(PlanStep {
                        text: step.get("step")?.as_str()?.to_owned(),
                        status: step
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("pending")
                            .to_owned(),
                    })
                })
                .collect(),
        }],
        "thread/tokenUsage/updated" => {
            let usage = params.pointer("/tokenUsage/total").unwrap_or(&Value::Null);
            vec![AgentEvent::UsageUpdated {
                usage: UsageSnapshot {
                    input_tokens: unsigned(usage.get("inputTokens")),
                    output_tokens: unsigned(usage.get("outputTokens")),
                    cached_input_tokens: unsigned(usage.get("cachedInputTokens")),
                    cost_usd: None,
                },
            }]
        }
        "account/rateLimits/updated" => parse_rate_limit(
            params.get("rateLimits").unwrap_or(params),
            "codex_notification",
        )
        .map(|limit| vec![AgentEvent::RateLimitUpdated { limit }])
        .unwrap_or_default(),
        "error" => vec![AgentEvent::Error {
            error: normalized_error(params),
        }],
        "turn/completed" => vec![AgentEvent::TurnCompleted {
            status: map_turn_status(
                params
                    .pointer("/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("failed"),
            ),
        }],
        _ => vec![AgentEvent::ProviderSpecific {
            provider: ProviderKind::Codex,
            kind: method.to_owned(),
            payload: params.clone(),
        }],
    };
    mapped.insert(
        0,
        AgentEvent::ProviderSpecific {
            provider: ProviderKind::Codex,
            kind: format!("raw:{method}"),
            payload: serde_json::json!({
                "method": method,
                "params": params
            }),
        },
    );
    mapped
}

fn map_item_started(item: &Value) -> Vec<AgentEvent> {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    match item.get("type").and_then(Value::as_str) {
        Some("commandExecution") => vec![AgentEvent::CommandStarted {
            id,
            command: item
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            cwd: PathBuf::from(item.get("cwd").and_then(Value::as_str).unwrap_or_default()),
        }],
        Some("mcpToolCall") => vec![AgentEvent::ToolStarted {
            id,
            name: format!(
                "mcp__{}__{}",
                item.get("server")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                item.get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ),
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
        }],
        Some("dynamicToolCall") => vec![AgentEvent::ToolStarted {
            id,
            name: item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
        }],
        _ => Vec::new(),
    }
}

fn map_item_completed(item: &Value) -> Vec<AgentEvent> {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    match item.get("type").and_then(Value::as_str) {
        Some("agentMessage") => item
            .get("text")
            .and_then(Value::as_str)
            .map(|text| {
                vec![AgentEvent::AssistantFinal {
                    text: text.to_owned(),
                }]
            })
            .unwrap_or_default(),
        Some("plan") => item
            .get("text")
            .and_then(Value::as_str)
            .map(|text| {
                vec![AgentEvent::PlanUpdated {
                    steps: vec![PlanStep {
                        text: text.to_owned(),
                        status: "completed".to_owned(),
                    }],
                }]
            })
            .unwrap_or_default(),
        Some("commandExecution") => {
            let mut events = Vec::new();
            if let Some(summary) = item
                .get("aggregatedOutput")
                .or_else(|| item.get("output"))
                .or_else(|| item.get("stdout"))
                .map(stringify_compact)
            {
                events.push(AgentEvent::ToolCompleted {
                    id: id.clone(),
                    output: ToolOutput {
                        size: u64::try_from(summary.len()).ok(),
                        summary: Some(summary),
                        digest: None,
                    },
                });
            }
            events.push(AgentEvent::CommandCompleted {
                id,
                exit_code: item
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .and_then(|code| i32::try_from(code).ok()),
                output_digest: None,
            });
            events
        }
        Some("fileChange") => {
            let changes = item
                .get("changes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(map_file_change)
                .collect::<Vec<_>>();
            vec![AgentEvent::FilesChanged { changes }]
        }
        Some("mcpToolCall" | "dynamicToolCall") => {
            let output = item
                .get("result")
                .or_else(|| item.get("contentItems"))
                .or_else(|| item.get("error"));
            let summary = output.map(stringify_compact);
            let size = summary
                .as_ref()
                .and_then(|text| u64::try_from(text.len()).ok());
            vec![AgentEvent::ToolCompleted {
                id,
                output: ToolOutput {
                    summary,
                    digest: None,
                    size,
                },
            }]
        }
        _ => Vec::new(),
    }
}

fn map_file_change(value: &Value) -> Option<FileChange> {
    let kind = value
        .pointer("/kind/type")
        .or_else(|| value.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("update");
    Some(FileChange {
        path: PathBuf::from(value.get("path")?.as_str()?),
        kind: match kind {
            "add" => FileChangeKind::Added,
            "delete" => FileChangeKind::Deleted,
            "move" | "rename" => FileChangeKind::Renamed,
            _ => FileChangeKind::Modified,
        },
        digest: None,
    })
}

pub fn parse_rate_limit(value: &Value, source: &str) -> Option<RateLimitSnapshot> {
    let window = value
        .get("primary")
        .filter(|window| !window.is_null())
        .or_else(|| value.get("secondary").filter(|window| !window.is_null()))?;
    let used_percent = window.get("usedPercent").and_then(Value::as_f64);
    let utilization = used_percent.map(|percent| (percent / 100.0).clamp(0.0, 1.0));
    let window_seconds = window
        .get("windowDurationMins")
        .and_then(Value::as_u64)
        .and_then(|minutes| minutes.checked_mul(60));
    let resets_at = window
        .get("resetsAt")
        .and_then(Value::as_i64)
        .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0));
    Some(RateLimitSnapshot {
        utilization,
        window_seconds,
        resets_at,
        source: source.to_owned(),
    })
}

fn normalized_error(params: &Value) -> NormalizedProviderError {
    let error = params.get("error").unwrap_or(params);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Codex turn failed")
        .to_owned();
    let info = error
        .get("codexErrorInfo")
        .map(Value::to_string)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let (code, retryable) = if info.contains("usagelimitexceeded") {
        ("rate_limit", true)
    } else if info.contains("serveroverloaded") {
        ("overloaded", true)
    } else if info.contains("unauthorized") {
        ("authentication_failed", false)
    } else {
        (
            "provider_error",
            params
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )
    };
    NormalizedProviderError {
        code: code.to_owned(),
        message,
        retryable,
    }
}

fn map_turn_status(status: &str) -> TurnStatus {
    match status {
        "completed" => TurnStatus::Completed,
        "interrupted" => TurnStatus::Interrupted,
        "inProgress" => TurnStatus::Running,
        _ => TurnStatus::Failed,
    }
}

fn unsigned(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
    })
}

fn stringify_compact(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{map_notification, parse_rate_limit};
    use agentctl_core::{AgentEvent, FileChangeKind, TurnStatus};
    use serde_json::json;

    #[test]
    fn maps_fixture_sequence() {
        let fixture = include_str!("../tests/fixtures/turn.jsonl");
        let events = fixture
            .lines()
            .flat_map(|line| {
                let frame: serde_json::Value = serde_json::from_str(line).unwrap();
                map_notification(
                    frame["method"].as_str().unwrap(),
                    frame.get("params").unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::FilesChanged { changes }
                if changes[0].kind == FileChangeKind::Modified
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed
            }
        )));
    }

    #[test]
    fn duplicate_and_out_of_order_item_frames_remain_explicit_for_canonical_ordering() {
        let completed = json!({
            "item": {
                "type": "commandExecution",
                "id": "cmd-1",
                "exitCode": 0
            }
        });
        let started = json!({
            "item": {
                "type": "commandExecution",
                "id": "cmd-1",
                "command": "cargo test",
                "cwd": "/repo"
            }
        });
        let events = [
            ("item/completed", &completed),
            ("item/completed", &completed),
            ("item/started", &started),
        ]
        .into_iter()
        .flat_map(|(method, params)| map_notification(method, params))
        .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::CommandCompleted { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::CommandStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    AgentEvent::ProviderSpecific { kind, .. } if kind.starts_with("raw:")
                ))
                .count(),
            3
        );
    }

    #[test]
    fn normalizes_percent_and_epoch_reset() {
        let limit = parse_rate_limit(
            &json!({
                "primary": {
                    "usedPercent": 63,
                    "windowDurationMins": 300,
                    "resetsAt": 1_800_000_000
                }
            }),
            "test",
        )
        .unwrap();
        assert_eq!(limit.utilization, Some(0.63));
        assert_eq!(limit.window_seconds, Some(18_000));
        assert!(limit.resets_at.is_some());
    }
}
