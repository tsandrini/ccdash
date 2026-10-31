//! What a collector does to sessions on its machine: open (focus or
//! attach), start or continue one in the background, stop one.

use std::{
    collections::BTreeMap,
    fs::{OpenOptions, Permissions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::process::Command;
use tracing::info;

use crate::{
    collector::Local,
    config,
    model::{Kind, Session},
    niri, paths,
    sync::glob,
};

/// How long `claude --bg` and `claude stop` may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
/// Settings files of launches older than this are pruned at the next launch.
const SETTINGS_KEEP: Duration = Duration::from_secs(24 * 3600);
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

    /// The arguments after the configured command; `settings` is the file
    /// [`settings`] wrote.
    pub fn args(&self, settings: &Path) -> Vec<String> {
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
        args.extend([
            "--channels".into(),
            "plugin:ccdash@inline".into(),
            "--settings".into(),
            settings.to_string_lossy().into_owned(),
            "--".into(),
            self.prompt.clone(),
        ]);
        args
    }

    /// The session's settings: the launch id and the direnv variables as
    /// `env`. Settings, not the process environment: the background daemon
    /// spawns the session with its own.
    pub fn settings(&self, env: &BTreeMap<String, String>) -> Value {
        let mut all = env.clone();
        all.insert("CCDASH_LAUNCH".into(), self.nonce.clone());
        json!({ "env": all })
    }
}

/// The variables direnv exported (`direnv export json` output) that match
/// one of `globs`; unset ones (null) and the rest are left out. Empty
/// output means nothing to export.
pub fn direnv_vars(output: &str, globs: &[String]) -> Result<BTreeMap<String, String>, String> {
    if output.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let exported: BTreeMap<String, Value> =
        serde_json::from_str(output).map_err(|e| format!("direnv printed no JSON object: {e}"))?;
    Ok(exported
        .into_iter()
        .filter_map(|(name, value)| Some((name, value.as_str()?.to_owned())))
        .filter(|(name, _)| globs.iter().any(|g| glob(g, name)))
        .collect())
}

/// The direnv environment of `cwd` the session gets, per the config; empty
/// when direnv is off.
async fn direnv_env(cfg: &config::Launch, cwd: &str) -> Result<BTreeMap<String, String>, String> {
    let Some((program, args)) = cfg.direnv.split_first() else {
        return Ok(BTreeMap::new());
    };
    let output = run(Command::new(program).args(args).current_dir(cwd)).await?;
    if !output.status.success() {
        return Err(format!("direnv in {cwd}: {}", failure(&output)));
    }
    direnv_vars(&String::from_utf8_lossy(&output.stdout), &cfg.env)
}

/// Writes the launch's settings file (owner-only) under the state dir and
/// prunes the old ones; its path.
fn write_settings(nonce: &str, settings: &Value) -> Result<PathBuf, String> {
    let dir = paths::state_dir().join("launch");
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let _ = std::fs::set_permissions(&dir, Permissions::from_mode(0o700));
    prune_settings(&dir);
    let path = dir.join(format!("{nonce}.json"));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    file.write_all(settings.to_string().as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path)
}

fn prune_settings(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age > SETTINGS_KEEP);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
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

/// Starts the session with `cfg.command` (the Claude Code executable and
/// its leading arguments) in its directory, with the directory's direnv
/// variables when configured; the short id `claude --bg` printed, or why
/// it failed (its stderr).
pub async fn launch(cfg: &config::Launch, spec: &LaunchSpec) -> Result<String, String> {
    let (program, lead) = cfg
        .command
        .split_first()
        .ok_or("starting sessions is off on this host")?;
    spec.check()?;
    if !Path::new(&spec.cwd).is_dir() {
        return Err(format!("{} is not a directory on this host", spec.cwd));
    }
    let env = direnv_env(cfg, &spec.cwd).await?;
    if !cfg.direnv.is_empty() {
        let names: Vec<&str> = env.keys().map(String::as_str).collect();
        info!(
            "launch {}: direnv in {} gives {} variable(s): {}",
            spec.nonce,
            spec.cwd,
            names.len(),
            names.join(", ")
        );
    }
    let settings = write_settings(&spec.nonce, &spec.settings(&env))?;
    let output = run(Command::new(program)
        .args(lead)
        .args(spec.args(&settings))
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

    /// `ETXTBSY` on Linux.
    const fn libc_etxtbsy() -> i32 {
        26
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
        // NOTE: a parallel test forking between the write and the exec holds the
        // script open ("text file busy"); spawning is retried
        let mut child = loop {
            match std::process::Command::new(&script).spawn() {
                Ok(child) => break child,
                Err(e) if e.raw_os_error() == Some(libc_etxtbsy()) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("spawning the editor script: {e}"),
            }
        };
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
        let file = Path::new("/state/launch/0123abcd.json");
        let args = spec().args(file);
        assert_eq!(
            &args[..5],
            ["--bg", "--model", "haiku", "--permission-mode", "plan"]
        );
        assert_eq!(&args[args.len() - 2..], ["--", "-v fix it"]);
        let settings = args.iter().position(|a| a == "--settings").unwrap();
        assert_eq!(args[settings + 1], "/state/launch/0123abcd.json");
        let env = spec().settings(&BTreeMap::from([(
            "CLAUDE_META_X".to_owned(),
            "1".to_owned(),
        )]));
        assert_eq!(env["env"]["CCDASH_LAUNCH"], "0123abcd");
        assert_eq!(env["env"]["CLAUDE_META_X"], "1");
        let resumed = LaunchSpec {
            resume: Some("9c4719e5-9508-461b-b1f4-e7b5a4c26f9d".into()),
            model: None,
            permission_mode: None,
            ..spec()
        }
        .args(file);
        assert_eq!(
            &resumed[..3],
            ["--bg", "--resume", "9c4719e5-9508-461b-b1f4-e7b5a4c26f9d"]
        );
        assert!(spec().check().is_ok());
    }

    #[test]
    fn direnv_variables_pass_by_glob() {
        let out = r#"{"CLAUDE_META_TOKEN":"s3cret","CLAUDE_META_URL":"https://x","PATH":"/nix","DIRENV_DIR":"-/w","GONE":null}"#;
        let globs = ["CLAUDE_META_*".to_owned()];
        let env = direnv_vars(out, &globs).unwrap();
        assert_eq!(
            env.keys().collect::<Vec<_>>(),
            ["CLAUDE_META_TOKEN", "CLAUDE_META_URL"]
        );
        assert_eq!(env["CLAUDE_META_URL"], "https://x");
        assert!(
            direnv_vars("", &globs).unwrap().is_empty(),
            "nothing to export"
        );
        assert!(
            direnv_vars(out, &[]).unwrap().is_empty(),
            "no globs, nothing passes"
        );
        assert!(direnv_vars("direnv: error", &globs).is_err());
    }

    #[tokio::test]
    async fn direnv_runs_in_the_launch_directory_and_its_failure_is_the_launch_failure() {
        let cfg = |direnv: &[&str]| config::Launch {
            direnv: direnv.iter().map(|s| (*s).to_owned()).collect(),
            env: vec!["CLAUDE_META_*".into()],
            ..config::Launch::default()
        };
        assert!(direnv_env(&cfg(&[]), "/").await.unwrap().is_empty(), "off");
        let env = direnv_env(
            &cfg(&[
                "sh",
                "-c",
                r#"printf '{"CLAUDE_META_X":"%s","PATH":"p"}' "$PWD""#,
            ]),
            "/tmp",
        )
        .await
        .unwrap();
        assert_eq!(env.get("CLAUDE_META_X").map(String::as_str), Some("/tmp"));
        assert!(!env.contains_key("PATH"));
        let refused = direnv_env(
            &cfg(&[
                "sh",
                "-c",
                "echo 'direnv: error .envrc is blocked' >&2; exit 1",
            ]),
            "/tmp",
        )
        .await
        .unwrap_err();
        assert!(refused.contains("is blocked"), "{refused}");
        assert!(
            direnv_env(&cfg(&["/nonexistent/direnv"]), "/tmp")
                .await
                .is_err()
        );
    }

    #[test]
    fn settings_files_are_private_and_pruned() {
        let dir = std::env::temp_dir().join(format!("ccdash-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let launch = dir.join("launch");
        std::fs::create_dir_all(&launch).unwrap();
        let old = launch.join("old.json");
        std::fs::write(&old, "{}").unwrap();
        let stale = SystemTime::now() - SETTINGS_KEEP - Duration::from_secs(60);
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(stale)
            .unwrap();
        prune_settings(&launch);
        assert!(!old.exists(), "a day-old settings file is pruned");
        let fresh = launch.join("fresh.json");
        std::fs::write(&fresh, "{}").unwrap();
        prune_settings(&launch);
        assert!(fresh.exists());
        let _ = std::fs::remove_dir_all(&dir);
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
