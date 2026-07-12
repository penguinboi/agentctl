use std::collections::BTreeSet;

use agentctl_core::{AgentEvent, SideEffectState};
use serde::{Deserialize, Serialize};

use crate::GitSnapshot;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SideEffectTracker {
    state: SideEffectState,
    started_commands: BTreeSet<String>,
    completed_commands: BTreeSet<String>,
    changed_files: BTreeSet<String>,
    external_tools: BTreeSet<String>,
}

impl SideEffectTracker {
    pub fn state(&self) -> SideEffectState {
        self.state
    }

    pub fn observe_event(&mut self, event: &AgentEvent) -> SideEffectState {
        let observation = match event {
            AgentEvent::CommandStarted { id, .. } => {
                self.started_commands.insert(id.clone());
                SideEffectState::Possible
            }
            AgentEvent::CommandCompleted { id, .. } => {
                self.completed_commands.insert(id.clone());
                SideEffectState::Possible
            }
            AgentEvent::FilesChanged { changes } => {
                self.changed_files.extend(
                    changes
                        .iter()
                        .map(|change| change.path.to_string_lossy().into_owned()),
                );
                SideEffectState::Confirmed
            }
            AgentEvent::ToolStarted { id, name, .. } => {
                self.external_tools.insert(format!("{name}:{id}"));
                SideEffectState::Possible
            }
            AgentEvent::ToolCompleted { .. } => SideEffectState::Possible,
            AgentEvent::ProviderSpecific { kind, .. }
                if kind.contains("file")
                    || kind.contains("patch")
                    || kind.contains("write")
                    || kind.contains("migration") =>
            {
                SideEffectState::Possible
            }
            _ => SideEffectState::None,
        };
        self.state = self.state.observe(observation);
        self.state
    }

    pub fn observe_workspace(
        &mut self,
        before: &GitSnapshot,
        after: &GitSnapshot,
    ) -> SideEffectState {
        if after.changed_since(before) {
            self.state = self.state.observe(SideEffectState::Confirmed);
        } else if !before.coverage_complete || !after.coverage_complete {
            // An ignored/untracked path (or a non-Git workspace) can change
            // without appearing in Git's patch material. Never replay a
            // failed turn merely because that partial snapshot is unchanged.
            self.state = self.state.observe(SideEffectState::Possible);
        }
        self.state
    }

    pub fn replay_is_safe(&self) -> bool {
        self.state == SideEffectState::None
    }

    pub fn uncertain_commands(&self) -> Vec<&str> {
        self.started_commands
            .difference(&self.completed_commands)
            .map(String::as_str)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn command_makes_replay_uncertain_and_file_change_confirms_effect() {
        let mut tracker = SideEffectTracker::default();
        tracker.observe_event(&AgentEvent::CommandStarted {
            id: "cmd".to_owned(),
            command: "cargo test".to_owned(),
            cwd: PathBuf::from("/repo"),
        });
        assert_eq!(tracker.state(), SideEffectState::Possible);
        assert!(!tracker.replay_is_safe());
        tracker.observe_event(&AgentEvent::FilesChanged {
            changes: Vec::new(),
        });
        assert_eq!(tracker.state(), SideEffectState::Confirmed);
    }

    #[test]
    fn incomplete_workspace_coverage_forbids_replay() {
        let snapshot = GitSnapshot {
            root: PathBuf::from("/repo"),
            head: None,
            branch: None,
            changed_paths: Vec::new(),
            dirty: false,
            diff_digest: "sha256:same".to_owned(),
            coverage_complete: false,
            captured_at: Utc::now(),
        };
        let mut tracker = SideEffectTracker::default();
        tracker.observe_workspace(&snapshot, &snapshot);
        assert_eq!(tracker.state(), SideEffectState::Possible);
        assert!(!tracker.replay_is_safe());
    }
}
