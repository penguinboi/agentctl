use agentctl_core::{ProviderHealth, ProviderKind, ProviderStatus};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ScoreWeights {
    pub availability: f64,
    pub quota_headroom: f64,
    pub affinity: f64,
    pub synchronization_cost: f64,
    pub recent_failures: f64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            availability: 5.0,
            quota_headroom: 2.0,
            affinity: 1.5,
            synchronization_cost: 1.0,
            recent_failures: 1.5,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProviderSignals<'a> {
    pub health: &'a ProviderHealth,
    pub sync_lag: u64,
    pub recent_failures: u32,
    pub affinity: f64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct CandidateScore {
    pub provider: ProviderKind,
    pub eligible: bool,
    pub total: f64,
    pub availability: f64,
    pub quota_headroom: f64,
    pub affinity: f64,
    pub synchronization_cost: f64,
    pub recent_failures: f64,
}

impl CandidateScore {
    pub fn reason(&self) -> String {
        format!(
            "availability={:.2}, quota={:.2}, affinity={:.2}, sync_cost={:.2}, failures={:.2}",
            self.availability,
            self.quota_headroom,
            self.affinity,
            self.synchronization_cost,
            self.recent_failures
        )
    }
}

pub fn score_candidate(signals: &ProviderSignals<'_>, weights: ScoreWeights) -> CandidateScore {
    let eligible = signals.health.status.available();
    let availability = availability_score(&signals.health.status);
    let quota_headroom = signals
        .health
        .rate_limit
        .as_ref()
        .and_then(|limit| limit.utilization)
        .map_or(0.5, |used| (1.0 - used).clamp(0.0, 1.0));
    let synchronization_cost = sync_cost(signals.sync_lag);
    let recent_failures = failure_penalty(signals.recent_failures);
    let total = availability.mul_add(
        weights.availability,
        quota_headroom.mul_add(
            weights.quota_headroom,
            signals.affinity.mul_add(
                weights.affinity,
                -(synchronization_cost * weights.synchronization_cost)
                    - recent_failures * weights.recent_failures,
            ),
        ),
    );

    CandidateScore {
        provider: signals.health.provider.clone(),
        eligible,
        total,
        availability,
        quota_headroom,
        affinity: signals.affinity,
        synchronization_cost,
        recent_failures,
    }
}

fn availability_score(status: &ProviderStatus) -> f64 {
    match status {
        ProviderStatus::Ready => 1.0,
        ProviderStatus::Warning => 0.45,
        ProviderStatus::Unknown => 0.10,
        ProviderStatus::Overloaded => -0.50,
        ProviderStatus::Exhausted { .. }
        | ProviderStatus::AuthError
        | ProviderStatus::Offline
        | ProviderStatus::Incompatible => -1.0,
    }
}

fn sync_cost(lag: u64) -> f64 {
    // Smoothly approaches one while still distinguishing small handoffs.
    let lag = f64::from(u32::try_from(lag).unwrap_or(u32::MAX));
    lag / (lag + 20.0)
}

fn failure_penalty(failures: u32) -> f64 {
    f64::from(failures.min(5)) / 5.0
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use proptest::prelude::*;

    use super::*;

    fn health(utilization: Option<f64>) -> ProviderHealth {
        ProviderHealth {
            provider: ProviderKind::Codex,
            status: ProviderStatus::Ready,
            version: None,
            capabilities: BTreeMap::new(),
            usage: None,
            rate_limit: utilization.map(|utilization| agentctl_core::RateLimitSnapshot {
                utilization: Some(utilization),
                window_seconds: None,
                resets_at: None,
                source: "test".into(),
            }),
            checked_at: Utc::now(),
            message: None,
        }
    }

    #[test]
    fn quota_and_sync_cost_affect_score() {
        let low_usage = health(Some(0.1));
        let high_usage = health(Some(0.9));
        let weights = ScoreWeights::default();
        let better = score_candidate(
            &ProviderSignals {
                health: &low_usage,
                sync_lag: 0,
                recent_failures: 0,
                affinity: 0.0,
            },
            weights,
        );
        let worse = score_candidate(
            &ProviderSignals {
                health: &high_usage,
                sync_lag: 100,
                recent_failures: 2,
                affinity: 0.0,
            },
            weights,
        );
        assert!(better.total > worse.total);
    }

    proptest! {
        #[test]
        fn score_is_finite_for_bounded_runtime_signals(
            utilization in -2.0f64..3.0,
            lag in 0u64..u64::MAX,
            failures in 0u32..u32::MAX,
            affinity in -1.0f64..1.0,
        ) {
            let health = health(Some(utilization));
            let score = score_candidate(
                &ProviderSignals {
                    health: &health,
                    sync_lag: lag,
                    recent_failures: failures,
                    affinity,
                },
                ScoreWeights::default(),
            );
            prop_assert!(score.total.is_finite());
        }
    }
}
