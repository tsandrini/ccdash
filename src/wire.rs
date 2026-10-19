//! Datagram format between `ccdash emit`/`ccdash tee` and the daemon.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::util::truncate;

pub const VERSION: u8 = 1;
/// Well under the default unix datagram send buffer.
pub const MAX_DATAGRAM: usize = 60 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Hook,
    Statusline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u8,
    pub source: Source,
    /// Unix milliseconds at the time the emitter ran.
    pub ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Remote Control session id (`CLAUDE_CODE_BRIDGE_SESSION_ID`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
    /// A queued title this hook handed to Claude Code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_title: Option<String>,
    pub payload: Value,
}

/// Tools whose `tool_response` the hub reads: TaskCreate returns the task
/// id; Bash's output is kept (reduced to its text, see [`bash_output`]).
const KEPT_RESPONSES: &[&str] = &["TaskCreate", "Bash"];
const LONG_KEYS: &[(&str, usize)] = &[
    ("last_assistant_message", 32 * 1024),
    ("prompt", 8 * 1024),
    ("compact_summary", 8 * 1024),
    ("delta", 16 * 1024),
    ("stdout", 8 * 1024),
    ("stderr", 8 * 1024),
    ("error", 8 * 1024),
];
const DEFAULT_LIMIT: usize = 2048;

/// Serializes `env`, shrinking the payload until it fits in one datagram.
pub fn encode(mut env: Envelope) -> Option<Vec<u8>> {
    if let Value::Object(map) = &mut env.payload {
        let tool = map
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        map.remove("permission_suggestions");
        if tool == "Bash" {
            if let Some(response) = map.get_mut("tool_response") {
                *response = bash_output(response);
            }
        } else if !KEPT_RESPONSES.contains(&tool.as_str()) {
            map.remove("tool_response");
        }
    }
    for divisor in [1, 4, 16] {
        clamp(&mut env.payload, DEFAULT_LIMIT / divisor, divisor);
        let bytes = serde_json::to_vec(&env).ok()?;
        if bytes.len() <= MAX_DATAGRAM {
            return Some(bytes);
        }
    }
    None
}

/// What the hub stores of a Bash result: its text, and whether the command
/// was interrupted. `bashEditDiff` and anything else is dropped.
fn bash_output(response: &Value) -> Value {
    let mut out = serde_json::Map::new();
    match response {
        Value::Object(map) => {
            for key in ["stdout", "stderr", "interrupted"] {
                if let Some(v) = map.get(key).filter(|v| !v.is_null()) {
                    out.insert(key.to_owned(), v.clone());
                }
            }
        }
        Value::String(s) => {
            out.insert("stdout".to_owned(), Value::String(s.clone()));
        }
        _ => {}
    }
    Value::Object(out)
}

fn clamp(value: &mut Value, limit: usize, divisor: usize) {
    match value {
        Value::String(s) if s.len() > limit => *s = truncate(s, limit),
        Value::Array(items) => items.iter_mut().for_each(|v| clamp(v, limit, divisor)),
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let limit = LONG_KEYS
                    .iter()
                    .find(|(k, _)| k == key)
                    .map_or(limit, |(_, l)| l / divisor);
                clamp(child, limit, divisor);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(payload: Value) -> Envelope {
        Envelope {
            v: VERSION,
            source: Source::Hook,
            ts: 0,
            pid: None,
            remote_id: None,
            applied_title: None,
            payload,
        }
    }

    #[test]
    fn drops_tool_response_and_clamps_strings() {
        let bytes = encode(envelope(json!({
            "tool_response": {"stdout": "x".repeat(100)},
            "tool_input": {"command": "y".repeat(10_000)},
            "last_assistant_message": "z".repeat(10_000),
        })))
        .unwrap();
        let back: Envelope = serde_json::from_slice(&bytes).unwrap();
        assert!(back.payload.get("tool_response").is_none());
        assert!(
            back.payload["tool_input"]["command"]
                .as_str()
                .unwrap()
                .len()
                <= DEFAULT_LIMIT
        );
        assert_eq!(
            back.payload["last_assistant_message"]
                .as_str()
                .unwrap()
                .len(),
            10_000
        );
    }

    #[test]
    fn keeps_bash_output_text_only() {
        let bytes = encode(envelope(json!({
            "tool_name": "Bash",
            "tool_response": {"stdout": "x".repeat(20_000), "stderr": "", "interrupted": false,
                "isImage": false, "bashEditDiff": {"big": "y".repeat(50_000)}},
        })))
        .unwrap();
        let back: Envelope = serde_json::from_slice(&bytes).unwrap();
        let response = &back.payload["tool_response"];
        assert_eq!(response["stdout"].as_str().unwrap().len(), 8 * 1024);
        assert_eq!(response["interrupted"], false);
        assert!(response.get("bashEditDiff").is_none());
        assert!(response.get("isImage").is_none());

        let bytes = encode(envelope(json!({
            "tool_name": "Bash",
            "hook_event_name": "PostToolUseFailure",
            "error": "e".repeat(20_000),
        })))
        .unwrap();
        let back: Envelope = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.payload["error"].as_str().unwrap().len(), 8 * 1024);
    }

    #[test]
    fn keeps_task_create_response() {
        let bytes = encode(envelope(json!({
            "tool_name": "TaskCreate",
            "tool_response": {"task": {"id": "3", "subject": "Ship it"}},
        })))
        .unwrap();
        let back: Envelope = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.payload["tool_response"]["task"]["id"], "3");
    }

    #[test]
    fn oversized_payloads_still_fit() {
        let many: Vec<String> = (0..200).map(|_| "w".repeat(4096)).collect();
        let bytes = encode(envelope(json!({ "list": many }))).unwrap();
        assert!(bytes.len() <= MAX_DATAGRAM);
    }
}
