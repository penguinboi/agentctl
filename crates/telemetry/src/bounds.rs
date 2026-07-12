use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{Redactor, neutralize_ansi};

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
pub struct PayloadLimits {
    pub max_event_bytes: usize,
    pub max_string_bytes: usize,
    pub max_array_items: usize,
    pub max_object_entries: usize,
    pub max_depth: usize,
}

impl Default for PayloadLimits {
    fn default() -> Self {
        Self {
            max_event_bytes: 1024 * 1024,
            max_string_bytes: 256 * 1024,
            max_array_items: 10_000,
            max_object_entries: 10_000,
            max_depth: 64,
        }
    }
}

#[derive(Debug, Error)]
pub enum PayloadError {
    #[error("JSON payload is {actual} bytes, exceeding limit {limit}")]
    EventTooLarge { actual: usize, limit: usize },
    #[error("JSON string at {path} is {actual} bytes, exceeding limit {limit}")]
    StringTooLarge {
        path: String,
        actual: usize,
        limit: usize,
    },
    #[error("JSON array at {path} contains {actual} items, exceeding limit {limit}")]
    ArrayTooLarge {
        path: String,
        actual: usize,
        limit: usize,
    },
    #[error("JSON object at {path} contains {actual} entries, exceeding limit {limit}")]
    ObjectTooLarge {
        path: String,
        actual: usize,
        limit: usize,
    },
    #[error("JSON depth at {path} exceeds limit {limit}")]
    TooDeep { path: String, limit: usize },
    #[error("failed to measure JSON payload: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Clone, Debug)]
pub struct PayloadGuard {
    limits: PayloadLimits,
    redactor: Redactor,
}

impl PayloadGuard {
    pub fn new(limits: PayloadLimits, redactor: Redactor) -> Self {
        Self { limits, redactor }
    }

    pub fn limits(&self) -> PayloadLimits {
        self.limits
    }

    /// Validates limits first, then returns a recursively redacted and ANSI-safe copy.
    pub fn process_json(&self, value: &Value) -> Result<Value, PayloadError> {
        let bytes = serde_json::to_vec(value)?.len();
        if bytes > self.limits.max_event_bytes {
            return Err(PayloadError::EventTooLarge {
                actual: bytes,
                limit: self.limits.max_event_bytes,
            });
        }
        self.visit(value, 0, "$")
    }

    pub fn process_text(&self, value: &str) -> Result<String, PayloadError> {
        if value.len() > self.limits.max_string_bytes {
            return Err(PayloadError::StringTooLarge {
                path: "$".into(),
                actual: value.len(),
                limit: self.limits.max_string_bytes,
            });
        }
        Ok(neutralize_ansi(&self.redactor.redact(value)))
    }

    fn visit(&self, value: &Value, depth: usize, path: &str) -> Result<Value, PayloadError> {
        if depth > self.limits.max_depth {
            return Err(PayloadError::TooDeep {
                path: path.into(),
                limit: self.limits.max_depth,
            });
        }
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => Ok(value.clone()),
            Value::String(text) => {
                if text.len() > self.limits.max_string_bytes {
                    return Err(PayloadError::StringTooLarge {
                        path: path.into(),
                        actual: text.len(),
                        limit: self.limits.max_string_bytes,
                    });
                }
                Ok(Value::String(neutralize_ansi(&self.redactor.redact(text))))
            }
            Value::Array(items) => {
                if items.len() > self.limits.max_array_items {
                    return Err(PayloadError::ArrayTooLarge {
                        path: path.into(),
                        actual: items.len(),
                        limit: self.limits.max_array_items,
                    });
                }
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| self.visit(item, depth + 1, &format!("{path}[{index}]")))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::Array)
            }
            Value::Object(object) => {
                if object.len() > self.limits.max_object_entries {
                    return Err(PayloadError::ObjectTooLarge {
                        path: path.into(),
                        actual: object.len(),
                        limit: self.limits.max_object_entries,
                    });
                }
                object
                    .iter()
                    .map(|(key, value)| {
                        self.visit(value, depth + 1, &format!("{path}.{key}"))
                            .map(|value| (key.clone(), value))
                    })
                    .collect::<Result<serde_json::Map<_, _>, _>>()
                    .map(Value::Object)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{RedactionConfig, Redactor};

    use super::*;

    fn guard(limits: PayloadLimits) -> PayloadGuard {
        PayloadGuard::new(
            limits,
            Redactor::new(&RedactionConfig::with_secret_defaults()).unwrap(),
        )
    }

    #[test]
    fn recursively_redacts_and_neutralizes_strings() {
        let result = guard(PayloadLimits::default())
            .process_json(&serde_json::json!({
                "nested": ["token=very-secret", "\u{1b}[31mred\u{1b}[0m"]
            }))
            .unwrap();
        assert_eq!(result["nested"][0], "token=[REDACTED]");
        assert_eq!(result["nested"][1], "red");
    }

    #[test]
    fn rejects_deep_or_oversized_payloads() {
        let limits = PayloadLimits {
            max_event_bytes: 1000,
            max_string_bytes: 4,
            max_array_items: 2,
            max_object_entries: 2,
            max_depth: 1,
        };
        assert!(matches!(
            guard(limits).process_json(&serde_json::json!({"a": {"b": 1}})),
            Err(PayloadError::TooDeep { .. })
        ));
        assert!(matches!(
            guard(limits).process_json(&serde_json::json!("12345")),
            Err(PayloadError::StringTooLarge { .. })
        ));
    }

    #[test]
    fn giant_stream_delta_is_rejected_at_the_exact_string_boundary() {
        let limits = PayloadLimits {
            max_event_bytes: 512 * 1024,
            max_string_bytes: 64 * 1024,
            ..PayloadLimits::default()
        };
        let accepted = serde_json::json!({
            "type": "assistant_text_delta",
            "text": "x".repeat(limits.max_string_bytes)
        });
        assert!(guard(limits).process_json(&accepted).is_ok());

        let rejected = serde_json::json!({
            "type": "assistant_text_delta",
            "text": "x".repeat(limits.max_string_bytes + 1)
        });
        assert!(matches!(
            guard(limits).process_json(&rejected),
            Err(PayloadError::StringTooLarge { path, actual, limit })
                if path == "$.text"
                    && actual == limits.max_string_bytes + 1
                    && limit == limits.max_string_bytes
        ));
    }
}
