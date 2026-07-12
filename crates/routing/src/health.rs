use std::collections::VecDeque;

use agentctl_core::{ProviderError, ProviderStatus};
use chrono::{DateTime, Duration, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    BinaryMissing,
    Authentication,
    Quota,
    Overload,
    Incompatible,
    Protocol,
    Process,
    Interrupted,
    Io,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ErrorClassification {
    pub class: FailureClass,
    pub status: ProviderStatus,
    pub retryable: bool,
    pub counts_as_failure: bool,
}

pub fn classify_error(error: &ProviderError) -> ErrorClassification {
    match error {
        ProviderError::BinaryNotFound(_) => classification(
            FailureClass::BinaryMissing,
            ProviderStatus::Offline,
            false,
            true,
        ),
        ProviderError::Authentication(_) => classification(
            FailureClass::Authentication,
            ProviderStatus::AuthError,
            false,
            true,
        ),
        ProviderError::RateLimited { resets_at } => classification(
            FailureClass::Quota,
            ProviderStatus::Exhausted {
                resets_at: *resets_at,
            },
            resets_at.is_some_and(|reset| reset <= Utc::now()),
            true,
        ),
        ProviderError::Overloaded(_) => classification(
            FailureClass::Overload,
            ProviderStatus::Overloaded,
            true,
            true,
        ),
        ProviderError::Incompatible(_) => classification(
            FailureClass::Incompatible,
            ProviderStatus::Incompatible,
            false,
            true,
        ),
        ProviderError::Protocol(_) => {
            classification(FailureClass::Protocol, ProviderStatus::Warning, false, true)
        }
        ProviderError::Process(_) => {
            classification(FailureClass::Process, ProviderStatus::Offline, true, true)
        }
        ProviderError::Interrupted => classification(
            FailureClass::Interrupted,
            ProviderStatus::Ready,
            false,
            false,
        ),
        ProviderError::Io(_) => {
            classification(FailureClass::Io, ProviderStatus::Offline, true, true)
        }
    }
}

fn classification(
    class: FailureClass,
    status: ProviderStatus,
    retryable: bool,
    counts_as_failure: bool,
) -> ErrorClassification {
    ErrorClassification {
        class,
        status,
        retryable,
        counts_as_failure,
    }
}

/// Rolling failure tracker used by routing penalties and health degradation.
#[derive(Clone, Debug)]
pub struct HealthStateMachine {
    status: ProviderStatus,
    failures: VecDeque<DateTime<Utc>>,
    failure_window: Duration,
    warning_threshold: usize,
}

impl HealthStateMachine {
    pub fn new(failure_window: Duration, warning_threshold: usize) -> Self {
        Self {
            status: ProviderStatus::Unknown,
            failures: VecDeque::new(),
            failure_window,
            warning_threshold: warning_threshold.max(1),
        }
    }

    pub fn status(&self) -> &ProviderStatus {
        &self.status
    }

    pub fn recent_failures(&mut self, now: DateTime<Utc>) -> u32 {
        self.prune(now);
        u32::try_from(self.failures.len()).unwrap_or(u32::MAX)
    }

    pub fn record_success(&mut self, now: DateTime<Utc>) {
        self.prune(now);
        self.status = if self.failures.len() >= self.warning_threshold {
            ProviderStatus::Warning
        } else {
            ProviderStatus::Ready
        };
    }

    pub fn record_error(&mut self, error: &ProviderError, now: DateTime<Utc>) {
        self.prune(now);
        let classification = classify_error(error);
        if classification.counts_as_failure {
            self.failures.push_back(now);
        }
        self.status = classification.status;
    }

    pub fn observe(&mut self, status: ProviderStatus, now: DateTime<Utc>) {
        self.prune(now);
        self.status = status;
    }

    fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - self.failure_window;
        while self
            .failures
            .front()
            .is_some_and(|failure| *failure < cutoff)
        {
            self.failures.pop_front();
        }
    }
}

impl Default for HealthStateMachine {
    fn default() -> Self {
        Self::new(Duration::minutes(5), 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_provider_errors_without_parsing_human_output() {
        let reset = Utc::now() + Duration::minutes(10);
        let result = classify_error(&ProviderError::RateLimited {
            resets_at: Some(reset),
        });
        assert_eq!(result.class, FailureClass::Quota);
        assert!(matches!(
            result.status,
            ProviderStatus::Exhausted { resets_at: Some(value) } if value == reset
        ));
        assert!(!result.retryable);
    }

    #[test]
    fn old_failures_expire_and_success_recovers_health() {
        let now = Utc::now();
        let mut machine = HealthStateMachine::new(Duration::seconds(30), 2);
        machine.record_error(&ProviderError::Process("crash".into()), now);
        machine.record_error(&ProviderError::Overloaded("busy".into()), now);
        machine.record_success(now + Duration::seconds(10));
        assert!(matches!(machine.status(), ProviderStatus::Warning));

        machine.record_success(now + Duration::seconds(31));
        assert!(matches!(machine.status(), ProviderStatus::Ready));
        assert_eq!(machine.recent_failures(now + Duration::seconds(31)), 0);
    }

    #[test]
    fn user_interrupt_is_not_a_provider_failure() {
        let now = Utc::now();
        let mut machine = HealthStateMachine::default();
        machine.record_error(&ProviderError::Interrupted, now);
        assert!(matches!(machine.status(), ProviderStatus::Ready));
        assert_eq!(machine.recent_failures(now), 0);
    }
}
