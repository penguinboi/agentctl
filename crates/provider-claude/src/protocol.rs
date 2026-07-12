use std::collections::BTreeMap;

use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub(crate) struct ClaudeInit {
    pub session_id: String,
    pub version: Option<String>,
    pub capabilities: BTreeMap<String, bool>,
}

impl ClaudeInit {
    pub(crate) fn from_frame(frame: &Value) -> Option<Self> {
        if frame.get("type")?.as_str()? != "system" || frame.get("subtype")?.as_str()? != "init" {
            return None;
        }
        let mut capabilities = frame
            .get("capabilities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|capability| (capability.to_owned(), true))
            .collect::<BTreeMap<_, _>>();
        capabilities.insert("streaming".to_owned(), true);
        capabilities.insert("resume".to_owned(), true);
        Some(Self {
            session_id: frame.get("session_id")?.as_str()?.to_owned(),
            version: frame
                .get("claude_code_version")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            capabilities,
        })
    }
}

pub(crate) fn user_message(session_id: &str, text: &str, should_query: bool) -> Value {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}]
        },
        "parent_tool_use_id": null,
        "session_id": session_id,
        "uuid": Uuid::new_v4().to_string(),
        "shouldQuery": should_query,
        "isSynthetic": !should_query
    })
}

pub(crate) fn initialize_control(request_id: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {
            "subtype": "initialize",
            "promptSuggestions": false
        }
    })
}

pub(crate) fn interrupt_control(request_id: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": "interrupt"}
    })
}

pub(crate) fn control_success(request_id: &str, response: &Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{ClaudeInit, user_message};
    use serde_json::json;

    #[test]
    fn parses_open_capability_set() {
        let init = ClaudeInit::from_frame(&json!({
            "type": "system",
            "subtype": "init",
            "session_id": "abc",
            "capabilities": ["interrupt_receipt_v1", "future_capability"],
            "tools": [],
            "mcp_servers": [],
            "plugins": []
        }))
        .unwrap();
        assert_eq!(init.capabilities.get("future_capability"), Some(&true));
    }

    #[test]
    fn synthetic_message_does_not_query() {
        let message = user_message("session", "context", false);
        assert_eq!(message["shouldQuery"], false);
        assert_eq!(message["isSynthetic"], true);
    }
}
