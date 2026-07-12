use std::{collections::BTreeMap, str::FromStr};

use agentctl_core::{ProviderHealth, ProviderKind, ProviderStatus, RoutingDecision};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AffinityRules, CandidateScore, ProviderSignals, ScoreWeights, score_candidate};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingPolicy {
    Manual,
    ClaudeFirst,
    CodexFirst,
    Balanced,
    #[default]
    StickyBalanced,
}

impl std::fmt::Display for RoutingPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Manual => "manual",
            Self::ClaudeFirst => "claude-first",
            Self::CodexFirst => "codex-first",
            Self::Balanced => "balanced",
            Self::StickyBalanced => "sticky-balanced",
        })
    }
}

impl FromStr for RoutingPolicy {
    type Err = RoutingError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "manual" => Ok(Self::Manual),
            "claude-first" | "claude_first" => Ok(Self::ClaudeFirst),
            "codex-first" | "codex_first" => Ok(Self::CodexFirst),
            "balanced" => Ok(Self::Balanced),
            "sticky-balanced" | "sticky_balanced" | "auto" => Ok(Self::StickyBalanced),
            other => Err(RoutingError::UnknownPolicy(other.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct RouterConfig {
    #[serde(default)]
    pub policy: RoutingPolicy,
    #[serde(default = "default_switch_threshold")]
    pub switch_threshold: f64,
    #[serde(default)]
    pub weights: ScoreWeights,
    #[serde(default)]
    pub affinity: AffinityRules,
}

const fn default_switch_threshold() -> f64 {
    0.75
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            policy: RoutingPolicy::default(),
            switch_threshold: default_switch_threshold(),
            weights: ScoreWeights::default(),
            affinity: AffinityRules::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RoutingCandidate<'a> {
    pub health: &'a ProviderHealth,
    pub sync_lag: u64,
    pub recent_failures: u32,
}

#[derive(Debug)]
pub struct RoutingContext<'a> {
    pub candidates: &'a [RoutingCandidate<'a>],
    pub current_provider: Option<&'a ProviderKind>,
    pub manual_provider: Option<&'a ProviderKind>,
    pub task_category: Option<&'a str>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct RouteResult {
    pub decision: RoutingDecision,
    pub candidates: Vec<CandidateScore>,
}

#[derive(Debug, Error)]
pub enum RoutingError {
    #[error("unknown routing policy: {0}")]
    UnknownPolicy(String),
    #[error("manual routing requires a provider")]
    ManualProviderRequired,
    #[error("requested provider is not available: {0}")]
    RequestedProviderUnavailable(ProviderKind),
    #[error("no provider is currently available")]
    NoAvailableProvider,
    #[error("routing configuration contains a non-finite number")]
    InvalidConfiguration,
}

#[derive(Clone, Debug, Default)]
pub struct Router {
    config: RouterConfig,
}

impl Router {
    pub fn new(config: RouterConfig) -> Result<Self, RoutingError> {
        let numbers = [
            config.switch_threshold,
            config.weights.availability,
            config.weights.quota_headroom,
            config.weights.affinity,
            config.weights.synchronization_cost,
            config.weights.recent_failures,
        ];
        if numbers.into_iter().any(|value| !value.is_finite()) || config.switch_threshold < 0.0 {
            return Err(RoutingError::InvalidConfiguration);
        }
        Ok(Self { config })
    }

    pub fn config(&self) -> &RouterConfig {
        &self.config
    }

    pub fn route(&self, context: &RoutingContext<'_>) -> Result<RouteResult, RoutingError> {
        let scores = context
            .candidates
            .iter()
            .map(|candidate| {
                score_candidate(
                    &ProviderSignals {
                        health: candidate.health,
                        sync_lag: candidate.sync_lag,
                        recent_failures: candidate.recent_failures,
                        affinity: self
                            .config
                            .affinity
                            .score(context.task_category, &candidate.health.provider),
                    },
                    self.config.weights,
                )
            })
            .collect::<Vec<_>>();

        let selected = match self.config.policy {
            RoutingPolicy::Manual => Self::select_manual(context, &scores)?,
            RoutingPolicy::ClaudeFirst => select_preferred(&scores, &ProviderKind::Claude)
                .ok_or(RoutingError::NoAvailableProvider)?,
            RoutingPolicy::CodexFirst => select_preferred(&scores, &ProviderKind::Codex)
                .ok_or(RoutingError::NoAvailableProvider)?,
            RoutingPolicy::Balanced => best(&scores).ok_or(RoutingError::NoAvailableProvider)?,
            RoutingPolicy::StickyBalanced => self.select_sticky(context, &scores)?,
        };

        let reason = format!("{}; {}", self.config.policy, selected.reason());
        Ok(RouteResult {
            decision: RoutingDecision {
                provider: selected.provider.clone(),
                policy: self.config.policy.to_string(),
                score: selected.total,
                reason,
                replayed: false,
            },
            candidates: scores,
        })
    }

    fn select_manual<'a>(
        context: &RoutingContext<'_>,
        scores: &'a [CandidateScore],
    ) -> Result<&'a CandidateScore, RoutingError> {
        let provider = context
            .manual_provider
            .or(context.current_provider)
            .ok_or(RoutingError::ManualProviderRequired)?;
        scores
            .iter()
            .find(|candidate| candidate.provider == *provider && candidate.eligible)
            .ok_or_else(|| RoutingError::RequestedProviderUnavailable(provider.clone()))
    }

    fn select_sticky<'a>(
        &self,
        context: &RoutingContext<'_>,
        scores: &'a [CandidateScore],
    ) -> Result<&'a CandidateScore, RoutingError> {
        let best = best(scores).ok_or(RoutingError::NoAvailableProvider)?;
        let current = context.current_provider.and_then(|provider| {
            scores
                .iter()
                .find(|candidate| candidate.provider == *provider && candidate.eligible)
        });
        let current_is_healthy = context.current_provider.is_some_and(|provider| {
            context.candidates.iter().any(|candidate| {
                candidate.health.provider == *provider
                    && matches!(candidate.health.status, ProviderStatus::Ready)
            })
        });

        if let Some(current) = current
            && current_is_healthy
            && best.total - current.total <= self.config.switch_threshold
        {
            return Ok(current);
        }
        Ok(best)
    }
}

fn best(scores: &[CandidateScore]) -> Option<&CandidateScore> {
    scores
        .iter()
        .filter(|candidate| candidate.eligible)
        .max_by(|left, right| {
            left.total
                .total_cmp(&right.total)
                .then_with(|| right.provider.cmp(&left.provider))
        })
}

fn select_preferred<'a>(
    scores: &'a [CandidateScore],
    preferred: &ProviderKind,
) -> Option<&'a CandidateScore> {
    let preferred = scores
        .iter()
        .find(|candidate| candidate.provider == *preferred && candidate.eligible);
    if preferred.is_some_and(|candidate| candidate.availability >= 1.0) {
        return preferred;
    }
    scores
        .iter()
        .filter(|candidate| candidate.eligible && candidate.availability >= 1.0)
        .max_by(|left, right| left.total.total_cmp(&right.total))
        .or(preferred)
        .or_else(|| best(scores))
}

/// Converts health snapshots into the provider-keyed map commonly used by status UIs.
pub fn health_by_provider(
    health: impl IntoIterator<Item = ProviderHealth>,
) -> BTreeMap<ProviderKind, ProviderHealth> {
    health
        .into_iter()
        .map(|snapshot| (snapshot.provider.clone(), snapshot))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use proptest::prelude::*;

    use super::*;

    fn health(provider: ProviderKind, status: ProviderStatus, utilization: f64) -> ProviderHealth {
        ProviderHealth {
            provider,
            status,
            version: None,
            capabilities: BTreeMap::new(),
            usage: None,
            rate_limit: Some(agentctl_core::RateLimitSnapshot {
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
    fn first_policies_fail_over_when_preferred_provider_is_unavailable() {
        let claude = health(
            ProviderKind::Claude,
            ProviderStatus::Exhausted { resets_at: None },
            1.0,
        );
        let codex = health(ProviderKind::Codex, ProviderStatus::Ready, 0.4);
        let candidates = [
            RoutingCandidate {
                health: &claude,
                sync_lag: 0,
                recent_failures: 0,
            },
            RoutingCandidate {
                health: &codex,
                sync_lag: 2,
                recent_failures: 0,
            },
        ];
        let router = Router::new(RouterConfig {
            policy: RoutingPolicy::ClaudeFirst,
            ..RouterConfig::default()
        })
        .unwrap();
        let result = router
            .route(&RoutingContext {
                candidates: &candidates,
                current_provider: None,
                manual_provider: None,
                task_category: None,
            })
            .unwrap();
        assert_eq!(result.decision.provider, ProviderKind::Codex);
    }

    #[test]
    fn codex_first_fails_over_to_claude_when_codex_quota_is_exhausted() {
        let claude = health(ProviderKind::Claude, ProviderStatus::Ready, 0.4);
        let codex = health(
            ProviderKind::Codex,
            ProviderStatus::Exhausted { resets_at: None },
            1.0,
        );
        let candidates = [
            RoutingCandidate {
                health: &claude,
                sync_lag: 2,
                recent_failures: 0,
            },
            RoutingCandidate {
                health: &codex,
                sync_lag: 0,
                recent_failures: 0,
            },
        ];
        let result = Router::new(RouterConfig {
            policy: RoutingPolicy::CodexFirst,
            ..RouterConfig::default()
        })
        .unwrap()
        .route(&RoutingContext {
            candidates: &candidates,
            current_provider: None,
            manual_provider: None,
            task_category: None,
        })
        .unwrap();
        assert_eq!(result.decision.provider, ProviderKind::Claude);
    }

    #[test]
    fn sticky_balanced_keeps_a_healthy_current_provider_within_threshold() {
        let claude = health(ProviderKind::Claude, ProviderStatus::Ready, 0.20);
        let codex = health(ProviderKind::Codex, ProviderStatus::Ready, 0.25);
        let candidates = [
            RoutingCandidate {
                health: &claude,
                sync_lag: 0,
                recent_failures: 0,
            },
            RoutingCandidate {
                health: &codex,
                sync_lag: 0,
                recent_failures: 0,
            },
        ];
        let router = Router::default();
        let result = router
            .route(&RoutingContext {
                candidates: &candidates,
                current_provider: Some(&ProviderKind::Codex),
                manual_provider: None,
                task_category: None,
            })
            .unwrap();
        assert_eq!(result.decision.provider, ProviderKind::Codex);
    }

    #[test]
    fn warning_provider_does_not_receive_sticky_preference() {
        let claude = health(ProviderKind::Claude, ProviderStatus::Ready, 0.40);
        let codex = health(ProviderKind::Codex, ProviderStatus::Warning, 0.10);
        let candidates = [
            RoutingCandidate {
                health: &claude,
                sync_lag: 0,
                recent_failures: 0,
            },
            RoutingCandidate {
                health: &codex,
                sync_lag: 0,
                recent_failures: 0,
            },
        ];
        let result = Router::default()
            .route(&RoutingContext {
                candidates: &candidates,
                current_provider: Some(&ProviderKind::Codex),
                manual_provider: None,
                task_category: None,
            })
            .unwrap();
        assert_eq!(result.decision.provider, ProviderKind::Claude);
    }

    proptest! {
        #[test]
        fn unavailable_provider_is_never_selected(
            policy in prop_oneof![
                Just(RoutingPolicy::Balanced),
                Just(RoutingPolicy::StickyBalanced),
                Just(RoutingPolicy::ClaudeFirst),
                Just(RoutingPolicy::CodexFirst),
            ],
            usage in 0.0f64..1.0,
        ) {
            let claude = health(ProviderKind::Claude, ProviderStatus::Offline, usage);
            let codex = health(ProviderKind::Codex, ProviderStatus::Ready, usage);
            let candidates = [
                RoutingCandidate { health: &claude, sync_lag: 0, recent_failures: 0 },
                RoutingCandidate { health: &codex, sync_lag: 0, recent_failures: 0 },
            ];
            let router = Router::new(RouterConfig { policy, ..RouterConfig::default() }).unwrap();
            let result = router.route(&RoutingContext {
                candidates: &candidates,
                current_provider: Some(&ProviderKind::Claude),
                manual_provider: None,
                task_category: None,
            }).unwrap();
            prop_assert_eq!(result.decision.provider, ProviderKind::Codex);
        }
    }
}
