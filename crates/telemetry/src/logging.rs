use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    Pretty,
    #[default]
    Compact,
    Json,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct LoggingConfig {
    #[serde(default = "default_filter")]
    pub filter: String,
    #[serde(default)]
    pub format: LogFormat,
    #[serde(default)]
    pub ansi: bool,
    #[serde(default = "default_true")]
    pub include_target: bool,
}

fn default_filter() -> String {
    "agentctl=info".into()
}

const fn default_true() -> bool {
    true
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            filter: default_filter(),
            format: LogFormat::Compact,
            ansi: false,
            include_target: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum LoggingError {
    #[error("invalid tracing filter: {0}")]
    InvalidFilter(String),
    #[error("global tracing subscriber is already initialized: {0}")]
    AlreadyInitialized(String),
}

/// Initializes diagnostics on stderr. Transcript output must use a separate writer.
pub fn init_logging(config: &LoggingConfig) -> Result<(), LoggingError> {
    let filter = EnvFilter::try_new(&config.filter)
        .map_err(|error| LoggingError::InvalidFilter(error.to_string()))?;
    let result = match config.format {
        LogFormat::Pretty => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(config.ansi)
            .with_target(config.include_target)
            .with_writer(std::io::stderr)
            .pretty()
            .try_init(),
        LogFormat::Compact => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(config.ansi)
            .with_target(config.include_target)
            .with_writer(std::io::stderr)
            .compact()
            .try_init(),
        LogFormat::Json => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_target(config.include_target)
            .with_writer(std::io::stderr)
            .json()
            .try_init(),
    };
    result.map_err(|error| LoggingError::AlreadyInitialized(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_filter_is_rejected_without_touching_global_subscriber() {
        let error = init_logging(&LoggingConfig {
            filter: "[invalid".into(),
            ..LoggingConfig::default()
        })
        .unwrap_err();
        assert!(matches!(error, LoggingError::InvalidFilter(_)));
    }
}
