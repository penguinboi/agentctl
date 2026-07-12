use agentctl_core::{ApprovalAction, ApprovalDecision, ApprovalRequest, ProviderKind, RiskLevel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Command,
    FileChange,
    Network,
    McpTool,
    Permission,
}

impl From<&ApprovalAction> for ActionKind {
    fn from(value: &ApprovalAction) -> Self {
        match value {
            ApprovalAction::Command => Self::Command,
            ApprovalAction::FileChange => Self::FileChange,
            ApprovalAction::Network { .. } => Self::Network,
            ApprovalAction::McpTool { .. } => Self::McpTool,
            ApprovalAction::Permission { .. } => Self::Permission,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleDecision {
    Prompt,
    AllowOnce,
    Deny,
    CancelTurn,
}

impl RuleDecision {
    pub fn approval_decision(self) -> Option<ApprovalDecision> {
        match self {
            Self::Prompt => None,
            Self::AllowOnce => Some(ApprovalDecision::AllowOnce),
            Self::Deny => Some(ApprovalDecision::Deny),
            Self::CancelTurn => Some(ApprovalDecision::CancelTurn),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ApprovalRule {
    pub provider: Option<ProviderKind>,
    pub action: Option<ActionKind>,
    pub maximum_risk: RiskLevel,
    pub decision: RuleDecision,
}

impl ApprovalRule {
    fn matches(&self, request: &ApprovalRequest, effective_risk: RiskLevel) -> bool {
        self.provider
            .as_ref()
            .is_none_or(|provider| provider == &request.provider)
            && self
                .action
                .is_none_or(|action| action == ActionKind::from(&request.action))
            && effective_risk <= self.maximum_risk
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ApprovalPolicy {
    /// Deliberately unsupported in the unified gateway. Kept explicit so unsafe config fails closed.
    #[serde(default)]
    pub bypass_permissions: bool,
    #[serde(default = "default_true")]
    pub allow_session_grants: bool,
    #[serde(default)]
    pub rules: Vec<ApprovalRule>,
    #[serde(default = "default_decision")]
    pub default: RuleDecision,
}

const fn default_true() -> bool {
    true
}

const fn default_decision() -> RuleDecision {
    RuleDecision::Prompt
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            bypass_permissions: false,
            allow_session_grants: true,
            rules: Vec::new(),
            default: RuleDecision::Prompt,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyOutcome {
    Prompt,
    Decide(ApprovalDecision),
}

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("bypass_permissions is not supported by the unified approval gateway")]
    BypassNotSupported,
    #[error("critical actions cannot be automatically approved")]
    CriticalAutoApproval,
}

impl ApprovalPolicy {
    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.bypass_permissions {
            return Err(PolicyError::BypassNotSupported);
        }
        if self.rules.iter().any(|rule| {
            rule.maximum_risk == RiskLevel::Critical && rule.decision == RuleDecision::AllowOnce
        }) {
            return Err(PolicyError::CriticalAutoApproval);
        }
        Ok(())
    }

    pub fn evaluate(&self, request: &ApprovalRequest, effective_risk: RiskLevel) -> PolicyOutcome {
        if request
            .command
            .as_deref()
            .is_some_and(is_agent_managed_worktree_command)
        {
            return PolicyOutcome::Decide(ApprovalDecision::Deny);
        }
        let decision = self
            .rules
            .iter()
            .find(|rule| rule.matches(request, effective_risk))
            .map_or(self.default, |rule| rule.decision);

        // A malformed/unvalidated policy still fails safely at runtime.
        if effective_risk == RiskLevel::Critical && decision == RuleDecision::AllowOnce {
            PolicyOutcome::Prompt
        } else {
            decision
                .approval_decision()
                .map_or(PolicyOutcome::Prompt, PolicyOutcome::Decide)
        }
    }
}

/// Worktree lifecycle belongs to agentctl so both native providers remain on
/// the same physical workspace. Provider-initiated worktree mutations are
/// denied even if a broader session grant exists.
pub fn is_agent_managed_worktree_command(command: &str) -> bool {
    let normalized = command.to_ascii_lowercase();
    let words = normalized
        .split(|character: char| character.is_whitespace() || matches!(character, ';' | '&' | '|'))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    words
        .iter()
        .enumerate()
        .filter(|(_, word)| word.ends_with("git"))
        .any(|(index, _)| {
            let tail = words.iter().skip(index + 1).take(8).collect::<Vec<_>>();
            let Some(worktree) = tail.iter().position(|word| **word == "worktree") else {
                return false;
            };
            tail.iter().skip(worktree + 1).take(4).any(|word| {
                matches!(
                    **word,
                    "add" | "move" | "remove" | "prune" | "lock" | "unlock" | "repair"
                )
            })
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agentctl_core::{ApprovalId, TurnId, UnifiedSessionId};

    use super::*;

    fn request(risk: RiskLevel) -> ApprovalRequest {
        ApprovalRequest {
            id: ApprovalId::new(),
            provider: ProviderKind::Codex,
            session_id: UnifiedSessionId::new(),
            turn_id: TurnId::new(),
            action: ApprovalAction::Command,
            risk,
            cwd: Some(PathBuf::from("/repo")),
            command: Some("cargo test".into()),
            files: Vec::new(),
            reason: None,
        }
    }

    #[test]
    fn default_policy_prompts_and_never_bypasses() {
        let policy = ApprovalPolicy::default();
        assert!(!policy.bypass_permissions);
        assert!(matches!(
            policy.evaluate(&request(RiskLevel::Low), RiskLevel::Low),
            PolicyOutcome::Prompt
        ));
    }

    #[test]
    fn critical_auto_approval_is_rejected_and_fails_closed() {
        let policy = ApprovalPolicy {
            rules: vec![ApprovalRule {
                provider: None,
                action: None,
                maximum_risk: RiskLevel::Critical,
                decision: RuleDecision::AllowOnce,
            }],
            ..ApprovalPolicy::default()
        };
        assert!(matches!(
            policy.validate(),
            Err(PolicyError::CriticalAutoApproval)
        ));
        assert!(matches!(
            policy.evaluate(&request(RiskLevel::Critical), RiskLevel::Critical),
            PolicyOutcome::Prompt
        ));
    }

    #[test]
    fn provider_initiated_worktree_lifecycle_is_always_denied() {
        let policy = ApprovalPolicy::default();
        let mut request = request(RiskLevel::Low);
        request.command = Some("git worktree add ../review HEAD".to_owned());
        assert!(matches!(
            policy.evaluate(&request, RiskLevel::Low),
            PolicyOutcome::Decide(ApprovalDecision::Deny)
        ));
        assert!(is_agent_managed_worktree_command(
            "/usr/bin/git worktree remove ../review"
        ));
        assert!(is_agent_managed_worktree_command(
            "git -C /repo worktree add ../review"
        ));
        assert!(!is_agent_managed_worktree_command("git worktree list"));
    }
}
