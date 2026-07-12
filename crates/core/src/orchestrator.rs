//! Pure turn-coordination invariants used by the I/O orchestration layer.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{SideEffectState, TurnStatus};

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct TurnExecutionState {
    pub prompt_seq: u64,
    pub status: TurnStatus,
    pub side_effects: SideEffectState,
    pub last_confirmed_event_seq: Option<u64>,
}

impl TurnExecutionState {
    pub fn new(prompt_seq: u64) -> Self {
        Self {
            prompt_seq,
            status: TurnStatus::Pending,
            side_effects: SideEffectState::None,
            last_confirmed_event_seq: None,
        }
    }

    /// The current prompt is sent as a real turn and must never be projected.
    pub fn sync_through_seq(&self) -> u64 {
        self.prompt_seq.saturating_sub(1)
    }

    pub fn observe_side_effect(&mut self, state: SideEffectState) {
        self.side_effects = self.side_effects.observe(state);
    }

    pub fn failover_strategy(&self) -> FailoverStrategy {
        match self.side_effects {
            SideEffectState::None => FailoverStrategy::ReplayOriginalPrompt,
            SideEffectState::Possible | SideEffectState::Confirmed => {
                FailoverStrategy::ContinueFromWorkspace
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailoverStrategy {
    ReplayOriginalPrompt,
    ContinueFromWorkspace,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_stops_before_current_prompt() {
        assert_eq!(TurnExecutionState::new(42).sync_through_seq(), 41);
        assert_eq!(TurnExecutionState::new(0).sync_through_seq(), 0);
    }

    #[test]
    fn possible_side_effect_permanently_disables_replay() {
        let mut state = TurnExecutionState::new(2);
        assert_eq!(
            state.failover_strategy(),
            FailoverStrategy::ReplayOriginalPrompt
        );
        state.observe_side_effect(SideEffectState::Possible);
        state.observe_side_effect(SideEffectState::None);
        assert_eq!(
            state.failover_strategy(),
            FailoverStrategy::ContinueFromWorkspace
        );
        state.observe_side_effect(SideEffectState::Confirmed);
        assert_eq!(
            state.failover_strategy(),
            FailoverStrategy::ContinueFromWorkspace
        );
    }
}
