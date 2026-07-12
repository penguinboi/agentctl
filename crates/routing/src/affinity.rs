use std::collections::BTreeMap;

use agentctl_core::ProviderKind;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A configurable provider preference for a task category.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "provider", content = "name")]
pub enum AffinityTarget {
    Claude,
    Codex,
    Plugin(String),
    #[default]
    Auto,
}

impl AffinityTarget {
    pub fn matches(&self, provider: &ProviderKind) -> bool {
        match (self, provider) {
            (Self::Claude, ProviderKind::Claude) | (Self::Codex, ProviderKind::Codex) => true,
            (Self::Plugin(expected), ProviderKind::Plugin(actual)) => expected == actual,
            _ => false,
        }
    }
}

/// Task-category affinities. Categories are application-defined and case-insensitive.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AffinityRules {
    #[serde(default)]
    rules: BTreeMap<String, AffinityTarget>,
}

impl AffinityRules {
    pub fn new(rules: impl IntoIterator<Item = (String, AffinityTarget)>) -> Self {
        Self {
            rules: rules
                .into_iter()
                .map(|(category, target)| (normalize(&category), target))
                .collect(),
        }
    }

    pub fn insert(&mut self, category: impl AsRef<str>, target: AffinityTarget) {
        self.rules.insert(normalize(category.as_ref()), target);
    }

    pub fn target(&self, category: Option<&str>) -> AffinityTarget {
        category
            .and_then(|category| self.rules.get(&normalize(category)))
            .cloned()
            .unwrap_or_default()
    }

    pub fn score(&self, category: Option<&str>, provider: &ProviderKind) -> f64 {
        let target = self.target(category);
        if matches!(target, AffinityTarget::Auto) {
            0.0
        } else if target.matches(provider) {
            1.0
        } else {
            -0.25
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &AffinityTarget)> {
        self.rules.iter().map(|(key, value)| (key.as_str(), value))
    }
}

fn normalize(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace([' ', '_'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_matching_is_normalized_and_configurable() {
        let mut rules = AffinityRules::default();
        rules.insert("Code Review", AffinityTarget::Claude);

        assert_eq!(rules.target(Some("code_review")), AffinityTarget::Claude);
        assert!(
            (rules.score(Some("CODE REVIEW"), &ProviderKind::Claude) - 1.0).abs() < f64::EPSILON
        );
        assert!(rules.score(Some("code-review"), &ProviderKind::Codex) < 0.0);
        assert!(rules.score(Some("debugging"), &ProviderKind::Codex).abs() < f64::EPSILON);
    }
}
