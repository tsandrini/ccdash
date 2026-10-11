//! Client side of the ingest socket, run by Claude Code as a hook or statusline.
//!
//! Must stay fast and silent: no async runtime, never fails the caller, and
//! no output except a queued title (see [`apply_pending_title`]).

use std::{
    io::{Read, Write},
    os::unix::net::UnixDatagram,
    process::{Command, Stdio},
};

use serde_json::{Value, json};

use crate::{
    paths,
    util::now_ms,
    wire::{self, Envelope, Source},
};

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
    let env = Envelope {
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
    };
    let Some(bytes) = wire::encode(env) else {
        return;
    };
    if let Ok(socket) = UnixDatagram::unbound() {
        let _ = socket.set_nonblocking(true);
        let _ = socket.send_to(&bytes, paths::ingest_socket());
    }
}

/// Hands a title queued by `ccdash rename` to Claude Code: UserPromptSubmit
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
    let output = json!({
        "hookSpecificOutput": { "hookEventName": event, "sessionTitle": title }
    });
    println!("{output}");
    Some(title.to_owned())
}
