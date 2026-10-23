//! What a collector does to sessions on its machine: open (focus or
//! attach), start or continue one in the background, stop one.

use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::process::Command;

use crate::{
    collector::Local,
    model::{Kind, Session},
    niri, paths,
};

/// How long `claude --bg` and `claude stop` may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
/// What a launch may ask for besides the configured default.
pub const PERMISSION_MODES: &[&str] = &["plan", "acceptEdits"];
/// Longest prompt a launch takes, in characters.
pub const MAX_PROMPT: usize = 20_000;

/// Focuses the session's window, or attaches to a background session in a new
/// terminal when it has none.
pub async fn open(local: &Local, attach_command: &[String], key: &str) -> Result<String> {
    let session = local
        .resolve(key)
        .await?
        .with_context(|| format!("no session matches {key:?}"))?;
    open_session(&session, attach_command).await
}

/// [`open`] for a session already at hand.
pub async fn open_session(session: &Session, attach_command: &[String]) -> Result<String> {
    if let Some(window) = &session.window {
        niri::focus(window.id).await?;
        return Ok(format!("focused window {}", window.id));
    }
    let short_id = match (session.kind, &session.short_id) {
        (Kind::Background, Some(id)) => id.clone(),
        _ => bail!("no known window for session {}", session.id),
    };
    let argv: Vec<String> = attach_command
        .iter()
        .map(|arg| arg.replace("{id}", &short_id))
        .collect();
    let (program, args) = argv.split_first().context("empty attach command")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("running {program}"))?;
    tokio::spawn(async move { child.wait().await });
    Ok(format!("attaching to {short_id}"))
}

/// A session to start in the background, as the hub asked for it.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub cwd: String,
    pub prompt: String,
    /// A model alias or name; the configured default when None.
    pub model: Option<String>,
    /// One of [`PERMISSION_MODES`]; the configured default when None.
    pub permission_mode: Option<String>,
    /// The session to continue.
    pub resume: Option<String>,
    /// Handed to the session's hooks as `CCDASH_LAUNCH`.
    pub nonce: String,
}

impl LaunchSpec {
    /// Refuses anything the panel never sends, before a process runs.
    pub fn check(&self) -> Result<(), String> {
        if self.prompt.trim().is_empty() {
            return Err("the prompt is empty".into());
        }
        if self.prompt.chars().count() > MAX_PROMPT {
            return Err(format!("prompts are at most {MAX_PROMPT} characters"));
        }
        if let Some(model) = &self.model
            && !valid_model(model)
        {
            return Err(format!("unknown model {model:?}"));
        }
        if let Some(mode) = &self.permission_mode
            && !PERMISSION_MODES.contains(&mode.as_str())
        {
            return Err(format!("permission mode {mode:?} is not offered"));
        }
        if let Some(id) = &self.resume
            && !paths::plain_session_id(id)
        {
            return Err("unusual session id".into());
        }
        if !paths::plain_session_id(&self.nonce) {
            return Err("unusual launch id".into());
        }
        Ok(())
    }

    /// The arguments after the configured command.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec!["--bg".to_owned()];
        if let Some(id) = &self.resume {
            args.extend(["--resume".into(), id.clone()]);
        }
        if let Some(model) = &self.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(mode) = &self.permission_mode {
            args.extend(["--permission-mode".into(), mode.clone()]);
        }
        // NOTE: settings `env`, not the process environment: the background daemon spawns the session
        let settings = json!({ "env": { "CCDASH_LAUNCH": self.nonce } });
        args.extend([
            "--channels".into(),
            "plugin:ccdash@inline".into(),
            "--settings".into(),
            settings.to_string(),
            "--".into(),
            self.prompt.clone(),
        ]);
        args
    }
}

/// Aliases (`opus`, `haiku`) and full names (`claude-opus-5-5[1m]`).
pub fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 64
        && !model.starts_with('-')
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '[' | ']'))
}

/// Starts the session with `command` (the Claude Code executable and its
/// leading arguments) in its directory; the short id `claude --bg`
/// printed, or why it failed (its stderr).
pub async fn launch(command: &[String], spec: &LaunchSpec) -> Result<String, String> {
    let (program, lead) = command
        .split_first()
        .ok_or("starting sessions is off on this host")?;
    spec.check()?;
    if !std::path::Path::new(&spec.cwd).is_dir() {
        return Err(format!("{} is not a directory on this host", spec.cwd));
    }
    let output = run(Command::new(program)
        .args(lead)
        .args(spec.args())
        .current_dir(&spec.cwd))
    .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return Err(failure(&output));
    }
    launched_id(&stdout).ok_or_else(|| {
        format!(
            "claude --bg printed no session id: {}",
            stdout.lines().next().unwrap_or_default().trim()
        )
    })
}

/// The id in `backgrounded · 9c4719e5`, the first line `claude --bg` prints.
pub fn launched_id(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("backgrounded")?;
        let id = rest
            .trim_start_matches(|c: char| c.is_whitespace() || c == '·' || c == ':')
            .split_whitespace()
            .next()?;
        paths::plain_session_id(id).then(|| id.to_owned())
    })
}

/// How long a stopped session gets to exit before Stop escalates, and
/// again after it did.
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// `claude stop` and `claude rm` answer within this, so the hub's stop
/// (30 s) gets an answer and not a timeout.
const STOP_TIMEOUT: Duration = Duration::from_secs(12);

/// Ends a running session, never with SIGKILL; `Ok` says how, once it is
/// over:
/// - background: `stop_command` (`{id}` is its short id); when that fails
///   and no process runs the session any more (Claude Code still lists a
///   blocked job, sometimes for weeks), `remove_command` (`claude rm`, which
///   keeps the conversation);
/// - interactive: SIGTERM to its Claude process, then, if it is still
///   there after `EXIT_GRACE`, to its child processes as well: Claude Code
///   acts on the signal only once an editor it opened for the prompt, or a
///   running command, has finished.
///
/// # Errors
///
/// Why the session is still running.
pub async fn stop(
    stop_command: &[String],
    remove_command: &[String],
    session: &Session,
) -> Result<String, String> {
    match session.kind {
        Kind::Background => {
            let id = session
                .short_id
                .clone()
                .unwrap_or_else(|| session.id.chars().take(8).collect());
            if !paths::plain_session_id(&id) {
                return Err("unusual session id".into());
            }
            match run_with(stop_command, &id).await {
                Ok(()) => Ok("stopped".into()),
                Err(e) if !remove_command.is_empty() && !running(session.pid) => {
                    run_with(remove_command, &id).await.map_err(|removing| {
                        format!("{e}; removing it with `claude rm` failed too: {removing}")
                    })?;
                    Ok(
                        "no process ran it any more, so it was removed from Claude Code's \
                        background sessions; its conversation is kept"
                            .into(),
                    )
                }
                Err(e) => Err(e),
            }
        }
        Kind::Interactive => {
            let pid = session
                .pid
                .ok_or("the session's process is unknown; forget it instead")?;
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
                .map_err(|_| format!("process {pid} is not running"))?;
            // NOTE: a reused pid must not be signalled
            if !String::from_utf8_lossy(&cmdline).contains("claude") {
                return Err(format!("process {pid} is no longer Claude Code"));
            }
            terminate(pid).await?;
            if exited(pid).await {
                return Ok("stopped".into());
            }
            let kids = children(pid);
            for (child, _) in &kids {
                let _ = terminate(*child).await;
            }
            let waited: Vec<&str> = kids.iter().map(|(_, name)| name.as_str()).collect();
            if exited(pid).await {
                return Ok(format!(
                    "stopped once its child processes ended too ({})",
                    waited.join(", ")
                ));
            }
            let holding = children(pid)
                .into_iter()
                .map(|(child, name)| format!("{name} ({child})"))
                .collect::<Vec<_>>();
            Err(if holding.is_empty() {
                format!(
                    "Claude Code (pid {pid}) did not exit after SIGTERM; close it in its terminal"
                )
            } else {
                format!(
                    "Claude Code (pid {pid}) did not exit: it waits for {}; close that in its \
                    terminal, or stop again",
                    holding.join(", ")
                )
            })
        }
    }
}

/// Runs `command` with `{id}` replaced, within `STOP_TIMEOUT`.
async fn run_with(command: &[String], id: &str) -> Result<(), String> {
    let argv: Vec<String> = command.iter().map(|a| a.replace("{id}", id)).collect();
    let (program, args) = argv.split_first().ok_or("no command configured")?;
    let output = run_within(Command::new(program).args(args), STOP_TIMEOUT).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(failure(&output))
    }
}

async fn terminate(pid: u32) -> Result<(), String> {
    let output = run(Command::new("kill").args(["-s", "TERM", "--", &pid.to_string()])).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(failure(&output))
    }
}

/// Waits up to `EXIT_GRACE` for `pid` to end.
async fn exited(pid: u32) -> bool {
    let polls = EXIT_GRACE.as_millis() / 100;
    for _ in 0..polls {
        if !running(Some(pid)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    !running(Some(pid))
}

/// Whether `pid` is a process that has not ended (a zombie has).
fn running(pid: Option<u32>) -> bool {
    pid.and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok())
        .and_then(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
        })
        .is_some_and(|state| state != "Z" && state != "X")
}

/// The direct child processes of `pid`, with their names.
fn children(pid: u32) -> Vec<(u32, String)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|child| {
            let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).ok()?;
            let (head, rest) = stat.rsplit_once(')')?;
            let name = head.split_once('(')?.1.to_owned();
            let parent: u32 = rest.split_whitespace().nth(1)?.parse().ok()?;
            (parent == pid).then_some((child, name))
        })
        .collect()
}

async fn run(command: &mut Command) -> Result<std::process::Output, String> {
    run_within(command, RUN_TIMEOUT).await
}

async fn run_within(
    command: &mut Command,
    limit: Duration,
) -> Result<std::process::Output, String> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    match tokio::time::timeout(
        limit,
        command.stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await
    {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(format!("running {program}: {e}")),
        Err(_) => Err(format!("{program} did not finish in time")),
    }
}

/// What a failed command said: stderr, else stdout, else its status.
fn failure(output: &std::process::Output) -> String {
    let text = [&output.stderr, &output.stdout]
        .into_iter()
        .map(|bytes| String::from_utf8_lossy(bytes).trim().to_owned())
        .find(|t| !t.is_empty())
        .unwrap_or_else(|| output.status.to_string());
    crate::util::truncate(&text, 600)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_of(kind: Kind, pid: Option<u32>) -> Session {
        let mut s = Session::stub("navi", "f6c2b4cf-1b3e-4915-82a8-4be0eceab7ce", "/w");
        s.kind = kind;
        s.pid = pid;
        s.short_id = (kind == Kind::Background).then(|| "f6c2b4cf".into());
        s
    }

    #[tokio::test]
    async fn stop_ends_a_claude_waiting_for_its_editor() {
        // a shell defers its TERM trap until its foreground child (the
        // "editor") ends, as Claude Code does with an editor it opened
        let dir = std::env::temp_dir().join(format!("ccdash-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(&script, "#!/bin/sh\ntrap 'exit 0' TERM\nsleep 600\n").unwrap();
        std::fs::set_permissions(
            &script,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let mut child = std::process::Command::new(&script).spawn().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let session = session_of(Kind::Interactive, Some(child.id()));
        let how = stop(&[], &[], &session).await.unwrap();
        assert!(
            how.contains("child processes") && how.contains("sleep"),
            "{how}"
        );
        assert!(child.wait().unwrap().success());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn stop_removes_a_background_job_nothing_runs() {
        let fail = vec!["false".to_owned()];
        let ok = vec!["true".to_owned()];
        let gone = session_of(Kind::Background, None);
        let how = stop(&fail, &ok, &gone).await.unwrap();
        assert!(how.contains("removed"), "{how}");
        // a job with a live process is not removed behind its back
        let live = session_of(Kind::Background, Some(std::process::id()));
        assert!(stop(&fail, &ok, &live).await.is_err());
        // and without a remove command the stop's failure stands
        assert!(stop(&fail, &[], &gone).await.is_err());
        assert_eq!(stop(&ok, &fail, &gone).await.unwrap(), "stopped");
    }

    fn spec() -> LaunchSpec {
        LaunchSpec {
            cwd: "/w/p".into(),
            prompt: "-v fix it".into(),
            model: Some("haiku".into()),
            permission_mode: Some("plan".into()),
            resume: None,
            nonce: "0123abcd".into(),
        }
    }

    #[test]
    fn launch_arguments_end_with_the_prompt() {
        let args = spec().args();
        assert_eq!(
            &args[..5],
            ["--bg", "--model", "haiku", "--permission-mode", "plan"]
        );
        assert_eq!(&args[args.len() - 2..], ["--", "-v fix it"]);
        let settings = args.iter().position(|a| a == "--settings").unwrap();
        let env: serde_json::Value = serde_json::from_str(&args[settings + 1]).unwrap();
        assert_eq!(env["env"]["CCDASH_LAUNCH"], "0123abcd");
        let resumed = LaunchSpec {
            resume: Some("9c4719e5-9508-461b-b1f4-e7b5a4c26f9d".into()),
            model: None,
            permission_mode: None,
            ..spec()
        }
        .args();
        assert_eq!(
            &resumed[..3],
            ["--bg", "--resume", "9c4719e5-9508-461b-b1f4-e7b5a4c26f9d"]
        );
        assert!(spec().check().is_ok());
    }

    #[test]
    fn launches_refuse_what_the_panel_never_sends() {
        for bad in [
            LaunchSpec {
                prompt: " ".into(),
                ..spec()
            },
            LaunchSpec {
                model: Some("--dangerously-skip-permissions".into()),
                ..spec()
            },
            LaunchSpec {
                permission_mode: Some("bypassPermissions".into()),
                ..spec()
            },
            LaunchSpec {
                resume: Some("../x".into()),
                ..spec()
            },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        assert!(valid_model("claude-opus-5-5[1m]"));
    }

    #[test]
    fn the_printed_id_is_read() {
        let out = "backgrounded · 9c4719e5\n  claude agents             list sessions\n  claude attach 9c4719e5    open in this terminal\n";
        assert_eq!(launched_id(out).as_deref(), Some("9c4719e5"));
        assert_eq!(launched_id("claude attach 9c4719e5\n"), None);
        assert_eq!(launched_id(""), None);
    }
}
