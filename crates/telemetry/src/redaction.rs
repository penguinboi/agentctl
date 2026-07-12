use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct RedactionRule {
    pub name: String,
    pub pattern: String,
    #[serde(default = "default_replacement")]
    pub replacement: String,
}

fn default_replacement() -> String {
    "[REDACTED]".into()
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct RedactionConfig {
    #[serde(default)]
    pub rules: Vec<RedactionRule>,
}

impl RedactionConfig {
    /// Conservative defaults. They are intentionally overridable rather than hidden heuristics.
    pub fn with_secret_defaults() -> Self {
        Self {
            rules: vec![
                RedactionRule {
                    name: "authorization-bearer".into(),
                    pattern: r"(?i)bearer\s+[A-Za-z0-9._~+/-]{8,}".into(),
                    replacement: "Bearer [REDACTED]".into(),
                },
                RedactionRule {
                    name: "common-secret-assignment".into(),
                    pattern: r"(?i)(api[_-]?key|token|password|secret)\s*[:=]\s*[^\s,;]+".into(),
                    replacement: "$1=[REDACTED]".into(),
                },
                RedactionRule {
                    name: "http-cookie-header".into(),
                    pattern: r"(?im)\b(set-cookie|cookie)\s*:\s*[^\r\n]+".into(),
                    replacement: "$1: [REDACTED]".into(),
                },
                RedactionRule {
                    name: "private-key".into(),
                    pattern:
                        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----"
                            .into(),
                    replacement: "[REDACTED PRIVATE KEY]".into(),
                },
            ],
        }
    }
}

#[derive(Clone, Debug)]
struct CompiledRule {
    regex: Regex,
    replacement: String,
}

#[derive(Clone, Debug, Default)]
pub struct Redactor {
    rules: Vec<CompiledRule>,
}

#[derive(Debug, Error)]
pub enum RedactionError {
    #[error("invalid redaction regex for rule {name}: {source}")]
    InvalidRegex {
        name: String,
        #[source]
        source: regex::Error,
    },
    #[error("redaction rule name cannot be empty")]
    EmptyName,
}

impl Redactor {
    pub fn new(config: &RedactionConfig) -> Result<Self, RedactionError> {
        let mut rules = Vec::with_capacity(config.rules.len());
        for rule in &config.rules {
            if rule.name.trim().is_empty() {
                return Err(RedactionError::EmptyName);
            }
            let regex =
                Regex::new(&rule.pattern).map_err(|source| RedactionError::InvalidRegex {
                    name: rule.name.clone(),
                    source,
                })?;
            rules.push(CompiledRule {
                regex,
                replacement: rule.replacement.clone(),
            });
        }
        Ok(Self { rules })
    }

    pub fn redact(&self, value: &str) -> String {
        self.rules.iter().fold(value.to_owned(), |text, rule| {
            rule.regex
                .replace_all(&text, rule.replacement.as_str())
                .into_owned()
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_redact_bearer_assignment_and_private_key() {
        let redactor = Redactor::new(&RedactionConfig::with_secret_defaults()).unwrap();
        let value = "Authorization: Bearer abcdefghijklmnop token=my-secret\n-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----";
        let redacted = redactor.redact(value);
        assert!(!redacted.contains("abcdefghijklmnop"));
        assert!(!redacted.contains("my-secret"));
        assert!(!redacted.contains("\nabc\n"));
    }

    #[test]
    fn invalid_user_pattern_is_reported() {
        let result = Redactor::new(&RedactionConfig {
            rules: vec![RedactionRule {
                name: "bad".into(),
                pattern: "(".into(),
                replacement: "x".into(),
            }],
        });
        assert!(matches!(result, Err(RedactionError::InvalidRegex { .. })));
    }

    #[test]
    fn defaults_redact_cookie_headers() {
        let redactor = Redactor::new(&RedactionConfig::with_secret_defaults()).unwrap();
        let redacted = redactor.redact(
            "Cookie: sessionid=top-secret; theme=dark\nSet-Cookie: auth=also-secret; HttpOnly",
        );
        assert_eq!(redacted, "Cookie: [REDACTED]\nSet-Cookie: [REDACTED]");
    }
}
