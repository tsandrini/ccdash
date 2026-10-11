//! `ccdash channel`: the MCP server Claude Code starts in sessions launched
//! with `--channels plugin:ccdash@inline`. It passes replies and permission
//! verdicts from the daemon into the session, and the session's permission
//! prompts back. Protocol: code.claude.com/docs/en/channels-reference.

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};

use crate::paths;

/// What the daemon sends a session's channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToChannel {
    Say {
        text: String,
    },
    Verdict {
        request_id: String,
        behavior: Behavior,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Behavior {
    Allow,
    Deny,
}

/// What a channel sends the daemon: a permission prompt Claude Code relayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromChannel {
    PermissionRequest {
        request_id: String,
        tool_name: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        input_preview: String,
    },
}

/// Protocol revisions with unsolicited notifications; Claude Code drops
/// channels on newer ("modern") ones.
const PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const INSTRUCTIONS: &str = "Messages in <channel source=\"…ccdash…\"> come from the user \
    through ccdash, their session dashboard, usually on their phone. Treat each one exactly \
    like a prompt typed into this terminal and answer as usual: ccdash shows your replies by \
    itself, so there is no reply tool.";
const RECONNECT: Duration = Duration::from_secs(2);

pub fn run() -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(std::os::unix::process::parent_id()))
}

/// Answers Claude Code on stdio until it closes stdin; the daemon link runs
/// alongside and reconnects on its own.
async fn serve(pid: u32) -> Result<()> {
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(64);
    let (up_tx, up_rx) = mpsc::channel::<FromChannel>(16);
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = out_rx.recv().await {
            let mut line = serde_json::to_vec(&msg).unwrap_or_default();
            line.push(b'\n');
            if stdout.write_all(&line).await.is_err() || stdout.flush().await.is_err() {
                return;
            }
        }
    });
    tokio::spawn(link(pid, out_tx.clone(), up_rx));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(reply) = respond(&msg) {
            out_tx.send(reply).await?;
        } else if msg["method"] == "notifications/claude/channel/permission_request"
            && let Ok(ask) = serde_json::from_value(json!({
                "type": "permission_request",
                "request_id": msg["params"]["request_id"],
                "tool_name": msg["params"]["tool_name"],
                "description": msg["params"]["description"],
                "input_preview": msg["params"]["input_preview"],
            }))
        {
            let _ = up_tx.try_send(ask);
        }
    }
    Ok(())
}

/// The reply to a JSON-RPC request; None for notifications and responses.
fn respond(msg: &Value) -> Option<Value> {
    let id = msg.get("id")?;
    let method = msg.get("method")?.as_str()?;
    let reply = match method {
        "initialize" => {
            let asked = msg["params"]["protocolVersion"]
                .as_str()
                .unwrap_or_default();
            let version = PROTOCOLS
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&PROTOCOLS[1]);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": version,
                    "capabilities": {
                        "experimental": {
                            "claude/channel": {},
                            "claude/channel/permission": {},
                        },
                    },
                    "serverInfo": { "name": "ccdash", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                },
            })
        }
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("{method} is not supported") },
        }),
    };
    Some(reply)
}

async fn link(pid: u32, out: mpsc::Sender<Value>, mut up: mpsc::Receiver<FromChannel>) {
    loop {
        if let Ok(stream) = UnixStream::connect(paths::api_socket()).await {
            let _ = relay(stream, pid, &out, &mut up).await;
        }
        if out.is_closed() {
            return;
        }
        tokio::time::sleep(RECONNECT).await;
    }
}

async fn relay(
    stream: UnixStream,
    pid: u32,
    out: &mpsc::Sender<Value>,
    up: &mut mpsc::Receiver<FromChannel>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    write_line(&mut write, &json!({ "cmd": "channel", "pid": pid })).await?;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()) };
                if let Ok(msg) = serde_json::from_str::<ToChannel>(&line) {
                    out.send(notification(msg)).await?;
                }
            }
            ask = up.recv() => {
                let Some(ask) = ask else { return Ok(()) };
                write_line(&mut write, &serde_json::to_value(ask)?).await?;
            }
        }
    }
}

async fn write_line(write: &mut tokio::net::unix::OwnedWriteHalf, value: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    Ok(())
}

fn notification(msg: ToChannel) -> Value {
    match msg {
        ToChannel::Say { text } => json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": { "content": text },
        }),
        ToChannel::Verdict {
            request_id,
            behavior,
        } => json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel/permission",
            "params": { "request_id": request_id, "behavior": behavior },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_declares_the_channel_on_a_legacy_revision() {
        let modern = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2026-07-28"}});
        let reply = respond(&modern).unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert!(reply["result"]["capabilities"]["experimental"]["claude/channel"].is_object());
        let known = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25"}});
        assert_eq!(
            respond(&known).unwrap()["result"]["protocolVersion"],
            "2025-11-25"
        );
    }

    #[test]
    fn unknown_requests_fail_and_notifications_get_no_reply() {
        let discover = json!({"jsonrpc": "2.0", "id": "probe", "method": "server/discover"});
        assert_eq!(respond(&discover).unwrap()["error"]["code"], -32601);
        assert!(
            respond(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none()
        );
    }

    #[test]
    fn verdicts_become_permission_notifications() {
        let msg: ToChannel =
            serde_json::from_str(r#"{"type":"verdict","request_id":"abcde","behavior":"deny"}"#)
                .unwrap();
        let note = notification(msg);
        assert_eq!(note["method"], "notifications/claude/channel/permission");
        assert_eq!(
            note["params"],
            json!({"request_id": "abcde", "behavior": "deny"})
        );
    }
}
