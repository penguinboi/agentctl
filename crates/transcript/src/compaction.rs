use std::collections::{BTreeMap, BTreeSet};

use agentctl_core::{CanonicalEvent, EventId, EventVisibility, TurnId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Result, TranscriptError};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Decision {
    pub text: String,
    pub source_seq: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextCheckpoint {
    pub through_seq: u64,
    pub objective: String,
    pub decisions: Vec<Decision>,
    pub changed_files: Vec<String>,
    pub validated_behaviors: Vec<String>,
    pub open_tasks: Vec<String>,
    pub known_failures: Vec<String>,
    #[serde(default)]
    pub artifact_digests: Vec<String>,
    pub previous_checkpoint_digest: Option<String>,
    /// Canonical events from the recent-turn window that should accompany this
    /// checkpoint when rebuilding a provider projection.
    #[serde(default)]
    pub retained_event_ids: Vec<EventId>,
}

impl ContextCheckpoint {
    pub fn content_hash(&self) -> Result<String> {
        Ok(format!(
            "sha256:{}",
            hex::encode(Sha256::digest(serde_json::to_vec(self)?))
        ))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct CompactionPolicy {
    pub recent_turns: usize,
    pub max_decisions: usize,
    pub max_changed_files: usize,
    pub max_validations: usize,
    pub max_open_tasks: usize,
    pub max_failures: usize,
    pub max_artifacts: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            recent_turns: 5,
            max_decisions: 100,
            max_changed_files: 500,
            max_validations: 100,
            max_open_tasks: 100,
            max_failures: 100,
            max_artifacts: 100,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompactionResult {
    pub checkpoint: ContextCheckpoint,
    pub retained_events: Vec<CanonicalEvent>,
    pub omitted_digests: Vec<String>,
}

struct CompactionState {
    checkpoint: ContextCheckpoint,
    decisions: BTreeMap<String, u64>,
    changed_files: BTreeSet<String>,
    validated: BTreeSet<String>,
    open_tasks: BTreeSet<String>,
    failures: BTreeSet<String>,
    artifacts: BTreeSet<String>,
    started_commands: BTreeMap<(Option<TurnId>, String), String>,
}

pub fn compact(
    previous: Option<&ContextCheckpoint>,
    events: &[CanonicalEvent],
    policy: CompactionPolicy,
) -> Result<CompactionResult> {
    validate_order(events)?;
    let mut state = CompactionState::new(previous)?;
    for event in events
        .iter()
        .filter(|event| event.visibility != EventVisibility::Internal)
    {
        state.observe(event);
    }
    let mut checkpoint = state.finish(policy);

    let recent_turn_ids = recent_turn_ids(events, policy.recent_turns);
    let retained_events: Vec<_> = events
        .iter()
        .filter(|event| {
            event.visibility != EventVisibility::Internal
                && event
                    .turn_id
                    .is_some_and(|turn_id| recent_turn_ids.contains(&turn_id))
        })
        .cloned()
        .collect();
    checkpoint.retained_event_ids = retained_events.iter().map(|event| event.event_id).collect();
    let retained_ids: BTreeSet<_> = retained_events.iter().map(|event| event.event_id).collect();
    let mut omitted_digests: Vec<_> = events
        .iter()
        .filter(|event| !retained_ids.contains(&event.event_id))
        .map(|event| event.content_hash.clone())
        .collect();
    omitted_digests.sort();
    omitted_digests.dedup();
    Ok(CompactionResult {
        checkpoint,
        retained_events,
        omitted_digests,
    })
}

impl CompactionState {
    fn new(previous: Option<&ContextCheckpoint>) -> Result<Self> {
        let mut checkpoint = previous.cloned().unwrap_or_default();
        checkpoint.previous_checkpoint_digest =
            previous.map(ContextCheckpoint::content_hash).transpose()?;
        Ok(Self {
            decisions: checkpoint
                .decisions
                .iter()
                .map(|decision| (decision.text.clone(), decision.source_seq))
                .collect(),
            changed_files: checkpoint.changed_files.iter().cloned().collect(),
            validated: checkpoint.validated_behaviors.iter().cloned().collect(),
            open_tasks: checkpoint.open_tasks.iter().cloned().collect(),
            failures: checkpoint.known_failures.iter().cloned().collect(),
            artifacts: checkpoint.artifact_digests.iter().cloned().collect(),
            checkpoint,
            started_commands: BTreeMap::new(),
        })
    }

    fn observe(&mut self, event: &CanonicalEvent) {
        self.checkpoint.through_seq = event.seq;
        match event.kind.as_str() {
            "user_prompt" | "user_message" => {
                if let Some(text) = extract_text(&event.payload) {
                    self.checkpoint.objective = text;
                }
                self.record_attachment_digests(&event.payload, "/attachments");
            }
            "decision" => {
                if let Some(text) = extract_text(&event.payload) {
                    self.decisions.insert(text, event.seq);
                }
                extend_string_map(&mut self.decisions, &event.payload, "decisions", event.seq);
            }
            "assistant_final" => {
                extend_string_map(&mut self.decisions, &event.payload, "decisions", event.seq);
                extend_set(&mut self.open_tasks, &event.payload, "open_tasks");
            }
            "files_changed" => self.record_changed_files(&event.payload),
            "command_started" => self.remember_command(event),
            "command_completed" => self.complete_command(event),
            "tool_completed" => {
                if let Some(digest) = event
                    .payload
                    .pointer("/output/digest")
                    .and_then(serde_json::Value::as_str)
                {
                    self.artifacts.insert(digest.to_owned());
                }
                self.record_attachment_digests(&event.payload, "/output/artifacts");
            }
            "workspace_snapshot" => self.record_workspace(&event.payload),
            "plan_updated" => update_tasks(&event.payload, &mut self.open_tasks),
            "error" => {
                if let Some(text) = extract_text(&event.payload) {
                    self.failures.insert(text);
                }
            }
            _ => {
                extend_set(&mut self.validated, &event.payload, "validated_behaviors");
                extend_set(&mut self.open_tasks, &event.payload, "open_tasks");
                extend_set(&mut self.failures, &event.payload, "known_failures");
            }
        }
    }

    fn record_changed_files(&mut self, payload: &serde_json::Value) {
        extend_set(&mut self.changed_files, payload, "files");
        if let Some(changes) = payload.get("changes").and_then(serde_json::Value::as_array) {
            self.changed_files
                .extend(changes.iter().filter_map(extract_changed_path));
        }
    }

    fn record_attachment_digests(&mut self, payload: &serde_json::Value, pointer: &str) {
        let Some(attachments) = payload
            .pointer(pointer)
            .and_then(serde_json::Value::as_array)
        else {
            return;
        };
        self.artifacts
            .extend(attachments.iter().filter_map(|attachment| {
                attachment
                    .get("source_digest")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
            }));
    }

    fn remember_command(&mut self, event: &CanonicalEvent) {
        if let (Some(id), Some(command)) = (
            event.payload.get("id").and_then(serde_json::Value::as_str),
            event
                .payload
                .get("command")
                .and_then(serde_json::Value::as_str),
        ) {
            self.started_commands
                .insert((event.turn_id, id.to_owned()), command.to_owned());
        }
    }

    fn complete_command(&mut self, event: &CanonicalEvent) {
        let id = event
            .payload
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let command = event
            .payload
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                self.started_commands
                    .get(&(event.turn_id, id.to_owned()))
                    .cloned()
            });
        classify_command(
            &event.payload,
            command.as_deref(),
            &mut self.validated,
            &mut self.failures,
        );
        collect_digest(&event.payload, "output_digest", &mut self.artifacts);
    }

    fn record_workspace(&mut self, payload: &serde_json::Value) {
        let snapshot = payload.get("snapshot").unwrap_or(payload);
        if let Some(changes) = snapshot
            .get("changed_paths")
            .and_then(serde_json::Value::as_array)
        {
            self.changed_files
                .extend(changes.iter().filter_map(extract_changed_path));
        }
        if let Some(digest) = payload
            .pointer("/diff_blob/digest")
            .and_then(serde_json::Value::as_str)
        {
            self.artifacts.insert(digest.to_owned());
        }
    }

    fn finish(mut self, policy: CompactionPolicy) -> ContextCheckpoint {
        self.checkpoint.decisions = self
            .decisions
            .into_iter()
            .map(|(text, source_seq)| Decision { text, source_seq })
            .collect();
        self.checkpoint
            .decisions
            .sort_by_key(|decision| decision.source_seq);
        keep_last(&mut self.checkpoint.decisions, policy.max_decisions);
        self.checkpoint.changed_files = capped(self.changed_files, policy.max_changed_files);
        self.checkpoint.validated_behaviors = capped(self.validated, policy.max_validations);
        self.checkpoint.open_tasks = capped(self.open_tasks, policy.max_open_tasks);
        self.checkpoint.known_failures = capped(self.failures, policy.max_failures);
        self.checkpoint.artifact_digests = capped(self.artifacts, policy.max_artifacts);
        self.checkpoint
    }
}

fn extract_changed_path(change: &serde_json::Value) -> Option<String> {
    change
        .get("path")
        .and_then(serde_json::Value::as_str)
        .or_else(|| change.as_str())
        .map(ToOwned::to_owned)
}

fn validate_order(events: &[CanonicalEvent]) -> Result<()> {
    let mut previous = 0;
    for event in events {
        if event.seq <= previous {
            return Err(TranscriptError::NonMonotonic(event.seq));
        }
        previous = event.seq;
    }
    Ok(())
}

fn recent_turn_ids(events: &[CanonicalEvent], count: usize) -> BTreeSet<TurnId> {
    let mut ordered = Vec::new();
    for turn_id in events.iter().filter_map(|event| event.turn_id) {
        if ordered.last() != Some(&turn_id) {
            ordered.push(turn_id);
        }
    }
    ordered
        .into_iter()
        .rev()
        .take(count)
        .collect::<BTreeSet<_>>()
}

fn classify_command(
    value: &serde_json::Value,
    command: Option<&str>,
    validated: &mut BTreeSet<String>,
    failures: &mut BTreeSet<String>,
) {
    let Some(command) = command else {
        return;
    };
    match value.get("exit_code").and_then(serde_json::Value::as_i64) {
        Some(0) => {
            validated.insert(command.to_owned());
        }
        Some(code) => {
            failures.insert(format!("{command}: exit {code}"));
        }
        None => {
            failures.insert(format!("{command}: result unknown"));
        }
    }
}

fn collect_digest(value: &serde_json::Value, key: &str, target: &mut BTreeSet<String>) {
    if let Some(digest) = value.get(key).and_then(serde_json::Value::as_str) {
        target.insert(digest.to_owned());
    }
}

fn update_tasks(value: &serde_json::Value, tasks: &mut BTreeSet<String>) {
    if let Some(steps) = value.get("steps").and_then(serde_json::Value::as_array) {
        for step in steps {
            let Some(text) = step.get("text").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let status = step
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("pending");
            if matches!(status, "completed" | "done") {
                tasks.remove(text);
            } else {
                tasks.insert(text.to_owned());
            }
        }
    }
}

fn extract_text(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("text").and_then(serde_json::Value::as_str))
        .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
        .or_else(|| {
            value
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
        })
        .map(ToOwned::to_owned)
}

fn extend_set(target: &mut BTreeSet<String>, value: &serde_json::Value, key: &str) {
    if let Some(items) = value.get(key).and_then(serde_json::Value::as_array) {
        target.extend(
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToOwned::to_owned),
        );
    }
}

fn extend_string_map(
    target: &mut BTreeMap<String, u64>,
    value: &serde_json::Value,
    key: &str,
    seq: u64,
) {
    if let Some(items) = value.get(key).and_then(serde_json::Value::as_array) {
        for item in items.iter().filter_map(serde_json::Value::as_str) {
            target.insert(item.to_owned(), seq);
        }
    }
}

fn capped<T: Ord>(items: BTreeSet<T>, max: usize) -> Vec<T> {
    items.into_iter().take(max).collect()
}

fn keep_last<T>(items: &mut Vec<T>, max: usize) {
    if items.len() > max {
        items.drain(..items.len() - max);
    }
}

#[cfg(test)]
mod tests {
    use agentctl_core::{EventId, ProviderKind, UnifiedSessionId};
    use chrono::Utc;

    use super::*;

    fn event(seq: u64, turn: TurnId, kind: &str, payload: serde_json::Value) -> CanonicalEvent {
        CanonicalEvent {
            schema_version: 1,
            session_id: UnifiedSessionId::new(),
            seq,
            event_id: EventId::new(),
            turn_id: Some(turn),
            origin_provider: Some(ProviderKind::Codex),
            kind: kind.to_owned(),
            visibility: EventVisibility::User,
            payload,
            content_hash: format!("sha256:{seq}"),
            raw_event_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn compaction_is_deterministic_and_keeps_recent_turns() {
        let first_turn = TurnId::new();
        let second_turn = TurnId::new();
        let events = vec![
            event(
                1,
                first_turn,
                "user_prompt",
                serde_json::json!({"text": "fix race"}),
            ),
            event(
                2,
                first_turn,
                "decision",
                serde_json::json!({"text": "lock by worktree"}),
            ),
            event(
                3,
                second_turn,
                "user_prompt",
                serde_json::json!({"text": "run tests"}),
            ),
            event(
                4,
                second_turn,
                "command_completed",
                serde_json::json!({"command": "cargo test", "exit_code": 0}),
            ),
        ];
        let policy = CompactionPolicy {
            recent_turns: 1,
            ..CompactionPolicy::default()
        };
        let first = compact(None, &events, policy).unwrap();
        let second = compact(None, &events, policy).unwrap();
        assert_eq!(first.checkpoint, second.checkpoint);
        assert_eq!(first.retained_events.len(), 2);
        assert_eq!(first.checkpoint.objective, "run tests");
        assert_eq!(first.checkpoint.validated_behaviors, vec!["cargo test"]);
        assert_eq!(first.checkpoint.retained_event_ids.len(), 2);
    }

    #[test]
    fn compaction_correlates_normalized_command_events_and_artifacts() {
        let turn = TurnId::new();
        let events = vec![
            event(
                1,
                turn,
                "command_started",
                serde_json::json!({
                    "type": "command_started",
                    "id": "cmd-1",
                    "command": "cargo clippy",
                    "cwd": "/repo"
                }),
            ),
            event(
                2,
                turn,
                "command_completed",
                serde_json::json!({
                    "type": "command_completed",
                    "id": "cmd-1",
                    "exit_code": 0,
                    "output_digest": "sha256:output"
                }),
            ),
            event(
                3,
                turn,
                "workspace_snapshot",
                serde_json::json!({
                    "snapshot": {"changed_paths": [{"path": "src/lib.rs"}]},
                    "diff_blob": {"digest": "sha256:patch"}
                }),
            ),
        ];
        let result = compact(None, &events, CompactionPolicy::default()).unwrap();
        assert_eq!(result.checkpoint.validated_behaviors, vec!["cargo clippy"]);
        assert_eq!(result.checkpoint.changed_files, vec!["src/lib.rs"]);
        assert_eq!(
            result.checkpoint.artifact_digests,
            vec!["sha256:output", "sha256:patch"]
        );
    }
}
