//! Client side of the ingest socket, run by Claude Code as a hook or statusline.
//!
//! Must stay fast and silent: no async runtime, never fails the caller, and
//! no output except what a hook may answer with: a queued title (see
//! [`apply_pending_title`]) and, on SessionStart, UserPromptSubmit and
//! edits, the context the collector injects (see `knowledge::tools::injection_of`).

use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::{UnixDatagram, UnixStream},
    process::{Command, Stdio},
    time::Duration,
};

use serde_json::{Value, json};

use crate::{
    knowledge::tools,
    paths,
    protocol::Injection,
    util::now_ms,
    wire::{self, Envelope, Source},
};

/// Hooks run with a 5 s timeout; the collector answers from the cache
/// before these run out.
const START_WAIT: Duration = Duration::from_millis(3500);
const HOOK_WAIT: Duration = Duration::from_millis(1800);

/// Forwards stdin to the daemon. With a `command`, also runs it on the same
/// stdin and returns its exit code (statusline tee).
pub fn run(source: Source, command: &[String]) -> i32 {
    let mut input = Vec::new();
    let _ = std::io::stdin().take(4 << 20).read_to_end(&mut input);
    send(source, &input);

    let Some((program, args)) = command.split_first() else {
        return 0;
    };
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
    else {
        return 127;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&input);
    }
    child.wait().map_or(1, |s| s.code().unwrap_or(1))
}

fn send(source: Source, input: &[u8]) {
    let Ok(payload) = serde_json::from_slice::<Value>(input) else {
        return;
    };
    let applied_title = match source {
        Source::Hook => apply_pending_title(&payload),
        Source::Statusline => None,
    };
    let env = envelope(source, payload, applied_title);
    let Some(bytes) = wire::encode(env.clone()) else {
        return;
    };
    if let Ok(socket) = UnixDatagram::unbound() {
        let _ = socket.set_nonblocking(true);
        let _ = socket.send_to(&bytes, paths::ingest_socket());
    }
    if source == Source::Hook {
        let context = inject(&env);
        if let Some(output) = hook_output(
            &env.payload,
            env.applied_title.as_deref(),
            context.as_deref(),
        ) {
            println!("{output}");
        }
    }
}

/// Asks the collector what to inject for this hook, within the hook's time.
fn inject(env: &Envelope) -> Option<String> {
    let what = tools::injection_of(&env.payload)?;
    let wait = match what {
        Injection::SessionStart => START_WAIT,
        _ => HOOK_WAIT,
    };
    let mut stream = UnixStream::connect(paths::api_socket()).ok()?;
    stream.set_read_timeout(Some(wait)).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    let mut line = serde_json::to_vec(&json!({ "cmd": "inject", "envelope": env })).ok()?;
    line.push(b'\n');
    stream.write_all(&line).ok()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).ok()?;
    let reply: Value = serde_json::from_str(&reply).ok()?;
    (reply["type"] == "context")
        .then(|| reply["text"].as_str().map(str::to_owned))
        .flatten()
        .filter(|t| !t.trim().is_empty())
}

/// The hook's JSON answer: the title it adopts and the context it reads.
pub fn hook_output(payload: &Value, title: Option<&str>, context: Option<&str>) -> Option<Value> {
    if title.is_none() && context.is_none() {
        return None;
    }
    let event = payload.get("hook_event_name")?.as_str()?;
    let mut out = json!({ "hookEventName": event });
    if let Some(title) = title {
        out["sessionTitle"] = json!(title);
    }
    if let Some(context) = context {
        out["additionalContext"] = json!(context);
    }
    Some(json!({ "hookSpecificOutput": out }))
}

/// Wraps a hook or statusline payload with what Claude Code's environment
/// tells about the session.
pub fn envelope(source: Source, payload: Value, applied_title: Option<String>) -> Envelope {
    Envelope {
        v: wire::VERSION,
        source,
        ts: now_ms(),
        pid: std::env::var("CLAUDE_PID")
            .ok()
            .and_then(|p| p.parse().ok())
            .or_else(|| Some(std::os::unix::process::parent_id())),
        remote_id: std::env::var("CLAUDE_CODE_BRIDGE_SESSION_ID")
            .ok()
            .filter(|id| !id.is_empty()),
        applied_title,
        payload,
    }
}

/// Takes a title queued by `ccdash rename` for Claude Code: UserPromptSubmit
/// and SessionStart hooks may answer with `hookSpecificOutput.sessionTitle`.
fn apply_pending_title(payload: &Value) -> Option<String> {
    let event = payload.get("hook_event_name")?.as_str()?;
    if !matches!(event, "UserPromptSubmit" | "SessionStart") {
        return None;
    }
    let path = paths::pending_title(payload.get("session_id")?.as_str()?)?;
    let title = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    Some(title.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_output_merges_title_and_context() {
        let payload = json!({"hook_event_name": "UserPromptSubmit", "session_id": "s"});
        assert!(hook_output(&payload, None, None).is_none());
        let out = hook_output(&payload, Some("T"), Some("ctx")).unwrap();
        assert_eq!(
            out,
            json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "sessionTitle": "T", "additionalContext": "ctx"}})
        );
        let edit = json!({"hook_event_name": "PreToolUse"});
        let out = hook_output(&edit, None, Some("c")).unwrap();
        assert!(out["hookSpecificOutput"].get("sessionTitle").is_none());
        assert!(
            out["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none()
        );
    }
}
