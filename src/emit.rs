//! Client side of the ingest socket, run by Claude Code as a hook or statusline.
//!
//! Must stay fast and silent: no async runtime, never fails the caller, and
//! no output except what a hook may answer with: a queued title (see
//! [`apply_pending_title`]); on SessionStart, UserPromptSubmit and edits,
//! the context the collector injects (see `knowledge::tools::injection_of`);
//! the one-machine guard: a prompt in a session open on another machine is
//! blocked, a resume of one warned about; and the stop of a turn the owner
//! interrupted from the panel, which the main thread's tool hooks ask the
//! collector about (see [`tool_hook`]).

use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::{
    ipc,
    knowledge::tools,
    paths,
    protocol::Injection,
    util::now_ms,
    wire::{self, Envelope, Source},
};

/// Hooks run with a 5 s timeout; the collector answers from the cache
/// before these run out. Counted from the hook's start, so the ingest send
/// and the connect come out of them.
const START_WAIT: Duration = Duration::from_millis(3500);
const HOOK_WAIT: Duration = Duration::from_millis(1800);
/// Why a turn ended, as the terminal shows it and Claude reads it at its
/// next turn ("<hook> hook stopped continuation: …").
pub const INTERRUPT_REASON: &str =
    "Interrupted from ccdash: the user stopped this turn from their dashboard";

/// Forwards stdin to the daemon. With a `command`, also runs it on the same
/// stdin and returns its exit code (statusline tee).
pub fn run(source: Source, command: &[String]) -> i32 {
    let mut input = Vec::new();
    let _ = std::io::stdin().take(4 << 20).read_to_end(&mut input);
    send(source, &input);

    let Some((program, args)) = command.split_first() else {
        return 0;
    };
    let Ok(mut child) = Command::new(crate::util::program(program))
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
    let started = Instant::now();
    let env = envelope(source, payload, applied_title);
    let Some(bytes) = wire::encode(env.clone()) else {
        return;
    };
    ipc::send(&paths::ingest_socket(), &bytes);
    if source == Source::Hook {
        let answer = inject(&env, started).unwrap_or_default();
        if let Some(output) = hook_output(&env.payload, env.applied_title.as_deref(), &answer) {
            println!("{output}");
        }
    }
}

/// What the collector answered a hook with.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Answer {
    /// Context for Claude.
    pub context: Option<String>,
    /// Why the prompt must not run here (the guard); shown to the owner.
    pub block: Option<String>,
    /// A warning shown to the owner (the guard on a resume).
    pub warn: Option<String>,
    /// Ends the turn with this reason (an interrupt from the panel).
    pub stop: Option<String>,
}

/// A main thread tool hook: one that may have to stop an interrupted turn.
pub fn tool_hook(payload: &Value) -> bool {
    matches!(
        payload.get("hook_event_name").and_then(Value::as_str),
        Some("PreToolUse" | "PostToolUse" | "PostToolUseFailure")
    ) && payload.get("agent_id").is_none_or(Value::is_null)
}

/// Asks the collector what to inject for this hook, within the hook's time
/// (from `started`).
fn inject(env: &Envelope, started: Instant) -> Option<Answer> {
    let what = tools::injection_of(&env.payload);
    if what.is_none() && !tool_hook(&env.payload) {
        return None;
    }
    let wait = match what {
        Some(Injection::SessionStart) => START_WAIT,
        _ => HOOK_WAIT,
    };
    let left = || Some(wait.saturating_sub(started.elapsed())).filter(|left| !left.is_zero());
    let mut stream = ipc::Stream::connect_within(&paths::api_socket(), left()?).ok()?;
    stream
        .set_write_timeout(Some(left()?.min(Duration::from_millis(500))))
        .ok()?;
    let mut line = serde_json::to_vec(&json!({ "cmd": "inject", "envelope": env })).ok()?;
    line.push(b'\n');
    stream.write_all(&line).ok()?;
    stream.set_read_timeout(Some(left()?)).ok()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).ok()?;
    let reply: Value = serde_json::from_str(&reply).ok()?;
    let text = || {
        reply["text"]
            .as_str()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_owned)
    };
    Some(match reply["type"].as_str() {
        Some("block") => Answer {
            block: text(),
            ..Answer::default()
        },
        Some("stop") => Answer {
            stop: text(),
            ..Answer::default()
        },
        Some("context") => Answer {
            context: text(),
            warn: reply["warn"].as_str().map(str::to_owned),
            ..Answer::default()
        },
        _ => Answer {
            warn: reply["warn"].as_str().map(str::to_owned),
            ..Answer::default()
        },
    })
}

/// The hook's JSON answer: the title it adopts, the context it reads, the
/// guard's block or warning, and the stop of an interrupted turn.
pub fn hook_output(payload: &Value, title: Option<&str>, answer: &Answer) -> Option<Value> {
    let event = payload.get("hook_event_name")?.as_str()?;
    let mut top = json!({});
    let mut out = json!({ "hookEventName": event });
    if let Some(reason) = &answer.block {
        top["decision"] = json!("block");
        top["reason"] = json!(reason);
    }
    if let Some(warning) = &answer.warn {
        top["systemMessage"] = json!(warning);
    }
    if let Some(reason) = &answer.stop {
        top["continue"] = json!(false);
        top["stopReason"] = json!(reason);
        // NOTE: `continue: false` alone lets a PreToolUse's tool run first (2.1.312)
        if event == "PreToolUse" {
            out["permissionDecision"] = json!("deny");
            out["permissionDecisionReason"] = json!(reason);
        }
    }
    if let Some(title) = title {
        out["sessionTitle"] = json!(title);
    }
    if let Some(context) = &answer.context {
        out["additionalContext"] = json!(context);
    }
    if out.as_object().is_some_and(|o| o.len() > 1) {
        top["hookSpecificOutput"] = out;
    }
    (top.as_object().is_some_and(|o| !o.is_empty())).then_some(top)
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
            .or_else(crate::procs::parent_id),
        remote_id: std::env::var("CLAUDE_CODE_BRIDGE_SESSION_ID")
            .ok()
            .filter(|id| !id.is_empty()),
        applied_title,
        launch: std::env::var("CCDASH_LAUNCH")
            .ok()
            .filter(|id| paths::plain_session_id(id)),
        project: std::env::var("CCDASH_PROJECT")
            .ok()
            .filter(|p| !p.trim().is_empty()),
        branch: None,
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

    fn context(text: &str) -> Answer {
        Answer {
            context: Some(text.into()),
            ..Answer::default()
        }
    }

    #[test]
    fn hook_output_merges_title_and_context() {
        let payload = json!({"hook_event_name": "UserPromptSubmit", "session_id": "s"});
        assert!(hook_output(&payload, None, &Answer::default()).is_none());
        let out = hook_output(&payload, Some("T"), &context("ctx")).unwrap();
        assert_eq!(
            out,
            json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "sessionTitle": "T", "additionalContext": "ctx"}})
        );
        let blocked = Answer {
            block: Some("open on navi".into()),
            ..Answer::default()
        };
        assert_eq!(
            hook_output(&payload, None, &blocked).unwrap(),
            json!({"decision": "block", "reason": "open on navi"})
        );
        let start = json!({"hook_event_name": "SessionStart", "source": "resume"});
        let warned = Answer {
            context: Some("ctx".into()),
            warn: Some("open on navi".into()),
            ..Answer::default()
        };
        assert_eq!(
            hook_output(&start, None, &warned).unwrap(),
            json!({"systemMessage": "open on navi", "hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": "ctx"}})
        );
        let edit = json!({"hook_event_name": "PreToolUse"});
        let out = hook_output(&edit, None, &context("c")).unwrap();
        assert!(out["hookSpecificOutput"].get("sessionTitle").is_none());
        assert!(
            out["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none()
        );
    }

    #[test]
    fn an_interrupt_stops_the_turn_and_denies_the_tool_before_it_runs() {
        let stop = Answer {
            stop: Some(INTERRUPT_REASON.into()),
            ..Answer::default()
        };
        let pre = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash"});
        let out = hook_output(&pre, None, &stop).unwrap();
        assert_eq!(out["continue"], false);
        assert_eq!(out["stopReason"], INTERRUPT_REASON);
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            out["hookSpecificOutput"]["permissionDecisionReason"],
            INTERRUPT_REASON
        );
        let post = json!({"hook_event_name": "PostToolUse", "tool_name": "Bash"});
        let out = hook_output(&post, None, &stop).unwrap();
        assert_eq!(out["continue"], false);
        assert!(out.get("hookSpecificOutput").is_none());
        assert!(tool_hook(&pre) && tool_hook(&post));
        assert!(!tool_hook(
            &json!({"hook_event_name": "PreToolUse", "agent_id": "a1"})
        ));
        assert!(!tool_hook(&json!({"hook_event_name": "Stop"})));
    }
}
