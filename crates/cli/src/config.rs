use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use agentctl_telemetry::{PayloadGuard, PayloadLimits, RedactionConfig, RedactionRule, Redactor};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::AgentctlPaths;

static LEGACY_AFFINITY_WARNED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    pub routing: RoutingConfig,
    pub retention_days: Option<u64>,
    pub redaction_patterns: Vec<String>,
    pub providers: ProviderConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RoutingConfig {
    pub policy: String,
    pub switch_threshold: f64,
    pub failure_window_seconds: u64,
    pub max_recent_failures: usize,
    /// Accepted only so pre-0.1 configurations keep loading. Native routing
    /// occurs before a prompt exists, so category affinity was never applied.
    #[serde(default, rename = "affinity", skip_serializing)]
    legacy_affinity: BTreeMap<String, String>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            policy: "sticky-balanced".to_owned(),
            switch_threshold: 0.25,
            failure_window_seconds: 300,
            max_recent_failures: 2,
            legacy_affinity: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProviderConfig {
    pub codex_binary: String,
    pub claude_binary: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            codex_binary: "codex".to_owned(),
            claude_binary: "claude".to_owned(),
        }
    }
}

impl Config {
    pub(crate) fn load(paths: &AgentctlPaths, workspace: &Path) -> Result<Self> {
        let mut merged = toml::Value::Table(toml::map::Map::new());
        merge_file(&mut merged, &paths.config_file)?;
        merge_project_file(&mut merged, &workspace.join(".agentctl.toml"))?;
        if merged.as_table().is_some_and(toml::map::Map::is_empty) {
            return Ok(Self::default());
        }
        let config: Self = merged
            .try_into()
            .context("agentctl configuration is invalid")?;
        config.validate()?;
        if !config.routing.legacy_affinity.is_empty()
            && !LEGACY_AFFINITY_WARNED.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                "routing.affinity is deprecated and ignored because native provider selection happens before the prompt"
            );
        }
        Ok(config)
    }

    pub(crate) fn payload_guard(&self) -> Result<PayloadGuard> {
        let mut redaction = RedactionConfig::with_secret_defaults();
        redaction
            .rules
            .extend(
                self.redaction_patterns
                    .iter()
                    .enumerate()
                    .map(|(index, pattern)| RedactionRule {
                        name: format!("user-pattern-{index}"),
                        pattern: pattern.clone(),
                        replacement: "[REDACTED]".to_owned(),
                    }),
            );
        let redactor = Redactor::new(&redaction).context("invalid redaction_patterns setting")?;
        Ok(PayloadGuard::new(PayloadLimits::default(), redactor))
    }

    fn validate(&self) -> Result<()> {
        const ROUTING_POLICIES: &[&str] = &[
            "manual",
            "claude-first",
            "codex-first",
            "balanced",
            "sticky-balanced",
        ];
        if self.retention_days == Some(0) {
            anyhow::bail!("retention_days must be at least 1");
        }
        if !ROUTING_POLICIES.contains(&self.routing.policy.as_str()) {
            anyhow::bail!(
                "routing.policy must be one of {}",
                ROUTING_POLICIES.join(", ")
            );
        }
        if !self.routing.switch_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.routing.switch_threshold)
        {
            anyhow::bail!("routing.switch_threshold must be a finite number between 0 and 1");
        }
        if self.routing.failure_window_seconds == 0 {
            anyhow::bail!("routing.failure_window_seconds must be at least 1");
        }
        if self.providers.codex_binary.trim().is_empty()
            || self.providers.claude_binary.trim().is_empty()
            || self.providers.codex_binary.contains('\0')
            || self.providers.claude_binary.contains('\0')
        {
            anyhow::bail!("provider binary names must be non-empty and contain no NUL bytes");
        }
        self.payload_guard()?;
        Ok(())
    }
}

fn merge_file(target: &mut toml::Value, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let document = fs::read_to_string(path)
        .with_context(|| format!("failed to read configuration {}", path.display()))?;
    let source: toml::Value = toml::from_str(&document)
        .with_context(|| format!("failed to parse configuration {}", path.display()))?;
    merge(target, source);
    Ok(())
}

/// Merges the repository-owned configuration after enforcing its trust
/// boundary. A checked-out repository is not allowed to select executables,
/// weaken local data handling, or change native process lifecycle. Those settings are
/// accepted only from the user-owned global configuration file.
fn merge_project_file(target: &mut toml::Value, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let document = fs::read_to_string(path)
        .with_context(|| format!("failed to read project configuration {}", path.display()))?;
    let source: toml::Value = toml::from_str(&document)
        .with_context(|| format!("failed to parse project configuration {}", path.display()))?;
    validate_project_config(&source, path)?;
    merge(target, source);
    Ok(())
}

fn validate_project_config(source: &toml::Value, path: &Path) -> Result<()> {
    const ROUTING_KEYS: &[&str] = &[
        "policy",
        "switch_threshold",
        "failure_window_seconds",
        "max_recent_failures",
        "affinity",
    ];

    let table = source.as_table().with_context(|| {
        format!(
            "project configuration {} must be a TOML document",
            path.display()
        )
    })?;
    for key in table.keys() {
        if key != "routing" {
            anyhow::bail!(
                "project configuration {} contains forbidden top-level field `{key}`; only `routing` is allowed",
                path.display()
            );
        }
    }

    if let Some(routing) = table.get("routing") {
        let routing = routing.as_table().with_context(|| {
            format!(
                "project configuration {} field `routing` must be a table",
                path.display()
            )
        })?;
        for key in routing.keys() {
            if !ROUTING_KEYS.contains(&key.as_str()) {
                anyhow::bail!(
                    "project configuration {} contains forbidden routing field `{key}`",
                    path.display()
                );
            }
        }
    }

    Ok(())
}

fn merge(target: &mut toml::Value, source: toml::Value) {
    match (target, source) {
        (toml::Value::Table(target), toml::Value::Table(source)) => {
            for (key, value) in source {
                if let Some(existing) = target.get_mut(&key) {
                    merge(existing, value);
                } else {
                    target.insert(key, value);
                }
            }
        }
        (target, source) => *target = source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_and_workspace() -> (tempfile::TempDir, AgentctlPaths, std::path::PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AgentctlPaths::resolve(Some(temporary.path().join("home"))).unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        (temporary, paths, workspace)
    }

    #[test]
    fn real_global_and_project_documents_merge_only_repo_safe_routing() {
        let (_temporary, paths, workspace) = paths_and_workspace();
        fs::write(
            &paths.config_file,
            r#"
retention_days = 45
redaction_patterns = ["GLOBAL-SECRET"]

[routing]
policy = "codex-first"
switch_threshold = 0.4
failure_window_seconds = 900
max_recent_failures = 4

[providers]
codex_binary = "/trusted/bin/codex"
claude_binary = "/trusted/bin/claude"
"#,
        )
        .unwrap();
        fs::write(
            workspace.join(".agentctl.toml"),
            r#"
[routing]
policy = "sticky-balanced"
switch_threshold = 0.8
failure_window_seconds = 60
max_recent_failures = 1

"#,
        )
        .unwrap();

        let config = Config::load(&paths, &workspace).unwrap();

        assert_eq!(config.routing.policy, "sticky-balanced");
        assert!((config.routing.switch_threshold - 0.8).abs() < f64::EPSILON);
        assert_eq!(config.routing.failure_window_seconds, 60);
        assert_eq!(config.routing.max_recent_failures, 1);
        assert_eq!(config.retention_days, Some(45));
        assert_eq!(config.redaction_patterns, ["GLOBAL-SECRET"]);
        assert_eq!(config.providers.codex_binary, "/trusted/bin/codex");
        assert_eq!(config.providers.claude_binary, "/trusted/bin/claude");
    }

    #[test]
    fn malicious_project_fields_are_rejected_before_the_global_config_is_changed() {
        let (_temporary, paths, workspace) = paths_and_workspace();
        fs::write(
            &paths.config_file,
            r#"
retention_days = 90
redaction_patterns = ["TRUSTED"]

[providers]
codex_binary = "/trusted/codex"
claude_binary = "/trusted/claude"
"#,
        )
        .unwrap();
        let malicious = workspace.join(".agentctl.toml");
        fs::write(
            &malicious,
            r#"
retention_days = 1
redaction_patterns = []

[providers]
codex_binary = "./malicious-codex"
claude_binary = "./malicious-claude"
"#,
        )
        .unwrap();

        let error = Config::load(&paths, &workspace).unwrap_err();
        assert!(error.to_string().contains("forbidden top-level field"));

        let mut global = toml::Value::Table(toml::map::Map::new());
        merge_file(&mut global, &paths.config_file).unwrap();
        let before = global.clone();
        assert!(merge_project_file(&mut global, &malicious).is_err());
        assert_eq!(global, before);
    }

    #[test]
    fn unknown_project_routing_fields_are_rejected() {
        let (_temporary, paths, workspace) = paths_and_workspace();
        fs::write(
            workspace.join(".agentctl.toml"),
            "[routing]\npolicy = 'balanced'\nprovider_binary = './owned'\n",
        )
        .unwrap();

        let error = Config::load(&paths, &workspace).unwrap_err();
        assert!(error.to_string().contains("forbidden routing field"));
    }

    #[test]
    fn legacy_affinity_setting_is_accepted_but_not_serialized() {
        let (_temporary, paths, workspace) = paths_and_workspace();
        fs::write(
            workspace.join(".agentctl.toml"),
            "[routing.affinity]\nreview = 'claude'\n",
        )
        .unwrap();

        let config = Config::load(&paths, &workspace).unwrap();
        assert_eq!(
            config.routing.legacy_affinity,
            BTreeMap::from([("review".to_owned(), "claude".to_owned())])
        );
        assert!(!toml::to_string(&config).unwrap().contains("affinity"));
    }

    #[test]
    fn checked_in_schemas_match_the_supported_routing_surface() {
        for schema in [
            include_str!("../../../schemas/config/agentctl.schema.json"),
            include_str!("../../../schemas/config/agentctl-project.schema.json"),
        ] {
            let schema: serde_json::Value = serde_json::from_str(schema).unwrap();
            let routing = &schema["properties"]["routing"]["properties"];
            assert_eq!(routing["affinity"]["deprecated"], true);
            assert_eq!(
                routing["affinity"]["additionalProperties"]["type"],
                "string"
            );
            assert_eq!(
                routing["policy"]["enum"],
                serde_json::json!([
                    "manual",
                    "claude-first",
                    "codex-first",
                    "balanced",
                    "sticky-balanced"
                ])
            );
        }
    }

    #[test]
    fn configured_redaction_patterns_are_compiled_and_applied() {
        let config = Config {
            redaction_patterns: vec![r"ACME-[0-9]{4}".to_owned()],
            ..Config::default()
        };
        let value = config
            .payload_guard()
            .unwrap()
            .process_text("reference ACME-1234")
            .unwrap();
        assert_eq!(value, "reference [REDACTED]");
    }

    #[test]
    fn invalid_security_configuration_fails_closed() {
        let config = Config {
            retention_days: Some(0),
            redaction_patterns: vec!["(".to_owned()],
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_routing_configuration_fails_closed() {
        for config in [
            Config {
                routing: RoutingConfig {
                    policy: "surprise-me".to_owned(),
                    ..RoutingConfig::default()
                },
                ..Config::default()
            },
            Config {
                routing: RoutingConfig {
                    switch_threshold: f64::NAN,
                    ..RoutingConfig::default()
                },
                ..Config::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
    }
}
