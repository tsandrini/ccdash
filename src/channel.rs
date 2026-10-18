//! `ccdash channel`: the MCP server Claude Code starts in sessions launched
//! with `--channels plugin:ccdash@inline`. It passes replies from the
//! daemon into the session and serves the knowledge tools (`remember`,
//! `recall`, …), each a round trip to the collector, which asks the hub.
//! Prompts are answered through `ccdash ask` instead. Protocol:
//! code.claude.com/docs/en/channels-reference.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};

use crate::{knowledge::tools, paths};

/// What the daemon sends a session's channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToChannel {
    Say { text: String },
}

/// Protocol revisions with unsolicited notifications; Claude Code drops
/// channels on newer ("modern") ones.
const PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const INSTRUCTIONS: &str = "Messages in <channel source=\"…ccdash…\"> come from the user \
    through ccdash, their session dashboard, usually on their phone. Treat each one exactly \
    like a prompt typed into this terminal and answer as usual: ccdash shows your replies by \
    itself, so there is no reply tool. Messages from other Claude Code sessions arrive as \
    context with your prompts, never as a prompt. The tools here are this workspace's \
    knowledge in ccdash (memories, issues, progress, documents) and the awareness between \
    sessions: recall before asking the user about the past, remember and verify facts, file \
    and update issues, log progress, write shared documents, claim what you are working on \
    and tell_session the others.";
const RECONNECT: Duration = Duration::from_secs(2);
/// A tool call's budget, so a dead link fails fast instead of stalling a prompt.
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

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
    tokio::spawn(link(pid, out_tx.clone()));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if msg.get("method").and_then(Value::as_str) == Some("tools/call")
            && let Some(id) = msg.get("id").cloned()
        {
            let (params, out) = (msg["params"].clone(), out_tx.clone());
            tokio::spawn(async move {
                let _ = out.send(tool_call(id, &params, pid).await).await;
            });
            continue;
        }
        if let Some(reply) = respond(&msg) {
            out_tx.send(reply).await?;
        }
    }
    Ok(())
}

/// Runs one tool through the collector and answers the request.
async fn tool_call(id: Value, params: &Value, pid: u32) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let (ok, text) = match tokio::time::timeout(TOOL_TIMEOUT, run_tool(name, &args, pid)).await {
        Ok(Ok((ok, text))) => (ok, text),
        Ok(Err(e)) => (
            false,
            format!("ccdash is not reachable ({e:#}); try again later"),
        ),
        Err(_) => (
            false,
            "ccdash did not answer within 5 s; the hub may be unreachable".into(),
        ),
    };
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": text }],
            "isError": !ok,
        },
    })
}

/// `(ok, text)` from the collector's `tool` request.
async fn run_tool(name: &str, args: &Value, pid: u32) -> Result<(bool, String)> {
    let mut stream = UnixStream::connect(paths::api_socket())
        .await
        .context("the collector is not running")?;
    let cwd = std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|d| d.to_string_lossy().into_owned())
        });
    let request = json!({
        "cmd": "tool",
        "name": name,
        "args": args,
        "pid": pid,
        "session": std::env::var("CLAUDE_CODE_SESSION_ID").ok().filter(|s| !s.is_empty()),
        "cwd": cwd,
    });
    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).await?;
    let reply: Value = serde_json::from_str(&reply).context("bad reply")?;
    match reply["type"].as_str() {
        Some("tool") => Ok((
            reply["ok"].as_bool().unwrap_or(false),
            reply["text"].as_str().unwrap_or_default().to_owned(),
        )),
        _ => Ok((
            false,
            reply["message"]
                .as_str()
                .unwrap_or("unexpected reply")
                .to_owned(),
        )),
    }
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
                        "experimental": { "claude/channel": {} },
                        "tools": {},
                    },
                    "serverInfo": { "name": "ccdash", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                },
            })
        }
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": tools::definitions() },
        }),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("{method} is not supported") },
        }),
    };
    Some(reply)
}

async fn link(pid: u32, out: mpsc::Sender<Value>) {
    loop {
        if let Ok(stream) = UnixStream::connect(paths::api_socket()).await {
            let _ = relay(stream, pid, &out).await;
        }
        if out.is_closed() {
            return;
        }
        tokio::time::sleep(RECONNECT).await;
    }
}

async fn relay(stream: UnixStream, pid: u32, out: &mpsc::Sender<Value>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut hello = serde_json::to_vec(&json!({ "cmd": "channel", "pid": pid }))?;
    hello.push(b'\n');
    write.write_all(&hello).await?;
    while let Some(line) = lines.next_line().await? {
        if let Ok(msg) = serde_json::from_str::<ToChannel>(&line) {
            out.send(notification(msg)).await?;
        }
    }
    Ok(())
}

fn notification(msg: ToChannel) -> Value {
    match msg {
        ToChannel::Say { text } => json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": { "content": text },
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
        let experimental = &reply["result"]["capabilities"]["experimental"];
        assert!(experimental["claude/channel"].is_object());
        assert!(experimental.get("claude/channel/permission").is_none());
        assert!(reply["result"]["capabilities"]["tools"].is_object());
        let list = respond(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).unwrap();
        assert_eq!(
            list["result"]["tools"].as_array().unwrap().len(),
            tools::NAMES.len()
        );
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
    fn replies_become_channel_notifications() {
        let msg: ToChannel = serde_json::from_str(r#"{"type":"say","text":"hi"}"#).unwrap();
        let note = notification(msg);
        assert_eq!(note["method"], "notifications/claude/channel");
        assert_eq!(note["params"], json!({"content": "hi"}));
    }
}
