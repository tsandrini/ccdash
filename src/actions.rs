//! What a collector does to sessions on its machine: open (focus or
//! attach), start or continue one in the background, stop one.

use std::{
    collections::{BTreeMap, HashSet},
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::process::Command;
use tracing::{info, warn};

use crate::{
    collector::Local,
    config,
    fsx::{self, ModeExt},
    model::{Kind, Session},
    niri, paths, procs,
    sync::glob,
    util,
};

/// How long `claude --bg` and `claude stop` may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
const POWER_TIMEOUT: Duration = Duration::from_secs(15);
/// Settings files of launches older than this are pruned at the next
/// launch, unless their job is still listed: its saved options name the
/// file, and Claude Code cannot restart the job without it.
const SETTINGS_KEEP: Duration = Duration::from_secs(30 * 24 * 3600);
/// What a launch may ask for besides the configured default.
pub const PERMISSION_MODES: &[&str] = &["plan", "acceptEdits"];
/// Longest prompt a launch takes, in characters.
pub const MAX_PROMPT: usize = 20_000;

/// Opens `text` in the machine's editor (`[edit] command`, `{file}` the
/// draft's file) and waits for it to close; `opened` gets the editor's pid
/// (its process group's) so a cancel can end it. Exit status 0 gives the
/// file's content; anything else (`:cq`) is a cancel. The file lives
/// under the state dir, owner-only, and goes when the editor closes.
pub async fn edit(
    command: &[String],
    id: &str,
    text: &str,
    opened: impl FnOnce(u32),
) -> Result<String, String> {
    edit_in(&paths::state_dir().join("edit"), command, id, text, opened).await
}

/// [`edit`] with the draft's file under `dir`.
async fn edit_in(
    dir: &Path,
    command: &[String],
    id: &str,
    text: &str,
    opened: impl FnOnce(u32),
) -> Result<String, String> {
    let (program, args) = command
        .split_first()
        .ok_or("this machine does not open an editor from the panel (`[edit] command` is empty)")?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{id:?} is not an edit id"));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let _ = fsx::set_mode(dir, 0o700);
    let file = dir.join(format!("{id}.md"));
    let write = || -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&file)?;
        f.write_all(text.as_bytes())
    };
    write().map_err(|e| format!("writing {}: {e}", file.display()))?;
    let path = file.to_string_lossy();
    let args: Vec<String> = args.iter().map(|a| a.replace("{file}", &path)).collect();
    let child = match Interactive::spawn(program, &args) {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_file(&file);
            return Err(format!("running {program}: {e}"));
        }
    };
    if let Some(pid) = child.id() {
        opened(pid);
    }
    let code = child.wait().await;
    let content = std::fs::read_to_string(&file);
    let _ = std::fs::remove_file(&file);
    match code.map_err(|e| format!("waiting for {program}: {e}"))? {
        Some(0) => {}
        Some(code) => return Err(format!("cancelled: the editor exited with status {code}")),
        None => return Err("cancelled: the editor was ended by a signal".into()),
    }
    content.map_err(|e| format!("reading {} back: {e}", file.display()))
}

/// Sends SIGTERM to the process group of `pid` (a process started with its
/// own group), as a cancel of its editor; on Windows ends its tree, on a
/// blocking thread: the walk takes snapshots and a query per process.
pub fn end_group(pid: u32) {
    #[cfg(unix)]
    procs::end_group(pid, false);
    #[cfg(windows)]
    tokio::task::spawn_blocking(move || procs::end_group(pid, false));
}

/// A program the user works in (an editor, an attach), apart from the
/// collector: on Unix in a process group of its own, so a cancel ends its
/// children too; on Windows in a console of its own (the collector's may be
/// hidden), which it gets for its standard handles.
#[cfg(unix)]
struct Interactive(tokio::process::Child);

#[cfg(unix)]
impl Interactive {
    fn spawn(program: &str, args: &[String]) -> std::io::Result<Self> {
        Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .map(Self)
    }

    fn id(&self) -> Option<u32> {
        self.0.id()
    }

    /// Waits for it to end: its exit code, none when a signal ended it.
    async fn wait(mut self) -> std::io::Result<Option<i32>> {
        self.0.wait().await.map(|status| status.code())
    }

    /// Leaves it running, reaped when it ends.
    fn detach(mut self) {
        tokio::spawn(async move { self.0.wait().await });
    }
}

#[cfg(windows)]
struct Interactive(crate::win::ConsoleChild);

#[cfg(windows)]
impl Interactive {
    fn spawn(program: &str, args: &[String]) -> std::io::Result<Self> {
        crate::win::ConsoleChild::spawn(program, args).map(Self)
    }

    fn id(&self) -> Option<u32> {
        Some(self.0.pid)
    }

    /// Waits for it to end: its exit code. Polled: a blocking wait would
    /// hold the runtime's shutdown (Ctrl-C) until the editor closes.
    async fn wait(self) -> std::io::Result<Option<i32>> {
        loop {
            if let Some(code) = self.0.try_wait()? {
                return Ok(Some(code.cast_signed()));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Leaves it running.
    fn detach(self) {}
}

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
    let (program, args) = argv.split_first().context(if cfg!(windows) {
        "no terminal to attach in: set `[attach] command` (`windows\\install.ps1 -Terminal`)"
    } else {
        "no terminal to attach in: set `[attach] command` (Home Manager: `services.ccdash.terminal`)"
    })?;
    Interactive::spawn(program, args)
        .with_context(|| format!("running {program}"))?
        .detach();
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
    /// `resume` is a job of this machine and nothing about it changes: it
    /// continues on its saved options.
    pub saved: bool,
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
    /// [`settings`] wrote, `channel` adds the ccdash channel.
    pub fn args(&self, settings: &Path, channel: bool) -> Vec<String> {
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
        if channel {
            args.extend(["--channels".into(), "plugin:ccdash@inline".into()]);
        }
        args.extend([
            "--settings".into(),
            settings.to_string_lossy().into_owned(),
            "--".into(),
            self.prompt.clone(),
        ]);
        args
    }

    /// The whole command line that continues a job on its saved options,
    /// when that is what this launch does. No flags, `launch.command`'s
    /// included: with any, Claude Code starts a copy under a new id.
    fn saved_args(&self) -> Option<Vec<String>> {
        let id = self.resume.as_ref().filter(|_| self.saved)?;
        Some(
            ["--bg", "--resume", id, "--", &self.prompt]
                .map(String::from)
                .to_vec(),
        )
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
    let output = run(Command::new(util::program(program))
        .args(args)
        .current_dir(cwd))
    .await?;
    if !output.status.success() {
        return Err(format!("direnv in {cwd}: {}", failure(&output)));
    }
    direnv_vars(&String::from_utf8_lossy(&output.stdout), &cfg.env)
}

fn launch_dir() -> PathBuf {
    paths::state_dir().join("launch")
}

/// Writes the launch's settings file (owner-only) under the state dir and
/// prunes the old ones; its path.
fn write_settings(
    nonce: &str,
    settings: &Value,
    jobs: &HashSet<String>,
) -> Result<PathBuf, String> {
    let dir = launch_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let _ = fsx::set_mode(&dir, 0o700);
    prune_settings(&dir, jobs);
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

/// Notes which launch job `short` came from (`<short>.job` holds its id).
fn note_job(dir: &Path, short: &str, nonce: &str) {
    let path = dir.join(format!("{short}.job"));
    if let Err(e) = std::fs::write(&path, nonce) {
        warn!("writing {}: {e}", path.display());
    }
}

/// Whether the settings file job `id` was launched with is still there. A
/// job launched before ccdash noted this may have lost its own.
pub fn settings_kept(id: &str) -> bool {
    settings_in(&launch_dir(), id)
}

fn settings_in(dir: &Path, id: &str) -> bool {
    let short: String = id.chars().take(8).collect();
    paths::plain_session_id(&short)
        && std::fs::read_to_string(dir.join(format!("{short}.job")))
            .is_ok_and(|nonce| dir.join(format!("{}.json", nonce.trim())).is_file())
}

/// Drops old settings files and notes, except those of `jobs`, the jobs
/// Claude Code lists. Empty means no poll came yet: nothing can tell.
fn prune_settings(dir: &Path, jobs: &HashSet<String>) {
    let listed = |short: &str| jobs.iter().any(|id| id.starts_with(short));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    if jobs.is_empty() {
        return;
    }
    let entries: Vec<_> = entries.flatten().collect();
    let in_use: HashSet<String> = entries
        .iter()
        .filter_map(|e| {
            let name = e.file_name();
            let short = name.to_str()?.strip_suffix(".job")?;
            let nonce = std::fs::read_to_string(e.path()).ok()?;
            listed(short).then(|| nonce.trim().to_owned())
        })
        .collect();
    let now = SystemTime::now();
    for entry in entries {
        let name = entry.file_name();
        let kept = match name.to_string_lossy().rsplit_once('.') {
            Some((short, "job")) => listed(short),
            Some((nonce, _)) => in_use.contains(nonce),
            None => false,
        };
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age > SETTINGS_KEEP);
        if old && !kept {
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
/// it failed (its stderr). `jobs` are the jobs Claude Code lists here.
pub async fn launch(
    cfg: &config::Launch,
    spec: &LaunchSpec,
    jobs: &HashSet<String>,
) -> Result<String, String> {
    let (program, lead) = cfg
        .command
        .split_first()
        .ok_or("starting sessions is off on this host")?;
    spec.check()?;
    if !Path::new(&spec.cwd).is_dir() {
        return Err(format!("{} is not a directory on this host", spec.cwd));
    }
    if let Some(args) = spec.saved_args() {
        return started(
            run(Command::new(util::program(program))
                .args(args)
                .current_dir(&spec.cwd))
            .await?,
        );
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
    let settings = write_settings(&spec.nonce, &spec.settings(&env), jobs)?;
    let short = started(
        run(Command::new(util::program(program))
            .args(lead)
            .args(spec.args(&settings, cfg.channel))
            .current_dir(&spec.cwd))
        .await?,
    )?;
    note_job(&launch_dir(), &short, &spec.nonce);
    Ok(short)
}

/// What `claude --bg` said: the id it printed, or why it failed.
fn started(output: std::process::Output) -> Result<String, String> {
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
///   running command, has finished. On Windows its process tree ends at
///   once.
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
            let cmdline =
                procs::cmdline(pid).ok_or_else(|| format!("process {pid} is not running"))?;
            // NOTE: a reused pid must not be signalled
            if !cmdline.contains("claude") {
                return Err(format!("process {pid} is no longer Claude Code"));
            }
            end_interactive(pid).await
        }
    }
}

#[cfg(unix)]
async fn end_interactive(pid: u32) -> Result<String, String> {
    terminate(pid).await?;
    if exited(pid).await {
        return Ok("stopped".into());
    }
    let kids = procs::children(pid);
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
    let holding = procs::children(pid)
        .into_iter()
        .map(|(child, name)| format!("{name} ({child})"))
        .collect::<Vec<_>>();
    Err(if holding.is_empty() {
        format!("Claude Code (pid {pid}) did not exit after SIGTERM; close it in its terminal")
    } else {
        format!(
            "Claude Code (pid {pid}) did not exit: it waits for {}; close that in its \
            terminal, or stop again",
            holding.join(", ")
        )
    })
}

/// Windows has no SIGTERM: Claude Code and every process under it end at
/// once, without its SessionEnd hook; the collector reports the pid gone.
/// `ccdash end-tree` ends them from inside Claude's console, to undo what
/// Claude Code would have on its way out (see [`end_tree_in_console`]); if
/// that fails, they end from here and the console stays as Claude left it.
#[cfg(windows)]
async fn end_interactive(pid: u32) -> Result<String, String> {
    // NOTE: off the runtime's thread: the tree is walked with snapshots and queries
    let kids: Vec<String> = tokio::task::spawn_blocking(move || {
        procs::children(pid)
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    })
    .await
    .unwrap_or_default();
    if let Err(e) = end_in_console(pid).await {
        warn!("ending Claude Code (pid {pid}) inside its console: {e}");
    }
    if running(Some(pid)) {
        let _ = tokio::task::spawn_blocking(move || procs::end_group(pid, true)).await;
    }
    if !exited(pid).await {
        return Err(format!(
            "Claude Code (pid {pid}) did not end; close it in its terminal"
        ));
    }
    Ok(if kids.is_empty() {
        "ended at once (Windows has no SIGTERM)".into()
    } else {
        format!(
            "ended at once with its child processes ({}); Windows has no SIGTERM",
            kids.join(", ")
        )
    })
}

/// Runs `ccdash end-tree PID` with no console of its own, so that it can
/// attach to Claude's: the collector keeps its own (hidden) one, and a
/// process it starts while attached to Claude's would print in the user's
/// terminal.
#[cfg(windows)]
async fn end_in_console(pid: u32) -> Result<(), String> {
    use std::process::Stdio;
    let exe = std::env::current_exe().map_err(|e| format!("locating ccdash: {e}"))?;
    let mut command = Command::new(exe);
    command
        .args(["end-tree", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .creation_flags(windows_sys::Win32::System::Threading::DETACHED_PROCESS);
    match tokio::time::timeout(EXIT_GRACE, command.output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => Err(failure(&output)),
        Ok(Err(e)) => Err(format!("starting ccdash end-tree: {e}")),
        Err(_) => Err("ccdash end-tree did not finish in time".into()),
    }
}

/// `ccdash end-tree PID`, which the collector runs to stop an interactive
/// session on Windows: attaches to the console of `pid` (Claude Code), ends
/// it and every process under it at once, and as soon as Claude Code is
/// gone, before its shell prompts again, turns off the terminal modes it
/// had turned on and gives the console line input back. Claude Code does
/// that itself as it exits, but a process ended at once does nothing; its
/// shell would then read every mouse move as typed text.
///
/// # Errors
///
/// When the console could not be attached to or restored; the tree is
/// ended all the same.
#[cfg(windows)]
pub fn end_tree_in_console(pid: u32) -> Result<()> {
    let console = crate::win::ForeignConsole::attach(pid);
    let mut restored = None;
    procs::end_tree(
        pid,
        Some(|| {
            restored = console
                .as_ref()
                .ok()
                .map(crate::win::ForeignConsole::restore)
        }),
    );
    let attached = console.map(drop);
    match (attached, restored) {
        (Err(e), _) => Err(e).context("attaching to Claude Code's console to restore it"),
        (Ok(()), Some(result)) => result.context("restoring Claude Code's console"),
        // NOTE: Claude Code did not end; the caller says so
        (Ok(()), None) => Ok(()),
    }
}

/// Runs `command` with `{id}` replaced, within `STOP_TIMEOUT`.
/// Suspends this machine with `command` (`[power] suspend_command`), which
/// returns once the sleep is on its way.
///
/// # Errors
///
/// What the command said when it refused (an inhibitor, logind's policy),
/// or that it did not finish in time.
pub async fn suspend(command: &[String]) -> Result<(), String> {
    run_power(command, "suspend").await
}

/// Locks the user's graphical session `session` with `command` (`[power]
/// lock_command`, `{session}` replaced).
///
/// # Errors
///
/// What the command said when it failed, or that it did not finish in time.
pub async fn lock(command: &[String], session: &str) -> Result<(), String> {
    let argv: Vec<String> = command
        .iter()
        .map(|a| a.replace("{session}", session))
        .collect();
    run_power(&argv, "lock").await
}

async fn run_power(command: &[String], what: &str) -> Result<(), String> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| format!("no {what} command is configured"))?;
    let output = run_within(
        Command::new(util::program(program)).args(args),
        POWER_TIMEOUT,
    )
    .await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(failure(&output))
    }
}

async fn run_with(command: &[String], id: &str) -> Result<(), String> {
    let argv: Vec<String> = command.iter().map(|a| a.replace("{id}", id)).collect();
    let (program, args) = argv.split_first().ok_or("no command configured")?;
    let output = run_within(
        Command::new(util::program(program)).args(args),
        STOP_TIMEOUT,
    )
    .await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(failure(&output))
    }
}

#[cfg(unix)]
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
#[cfg(windows)]
fn running(pid: Option<u32>) -> bool {
    crate::util::pid_alive(pid)
}

/// Whether `pid` is a process that has not ended (a zombie has).
#[cfg(unix)]
fn running(pid: Option<u32>) -> bool {
    pid.and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok())
        .and_then(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
        })
        .is_some_and(|state| state != "Z" && state != "X")
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
    match util::output_within(command, limit).await {
        Some(Ok(output)) => Ok(output),
        Some(Err(e)) => Err(format!("running {program}: {e}")),
        None => Err(format!("{program} did not finish in time")),
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
    #[cfg(unix)]
    const fn libc_etxtbsy() -> i32 {
        26
    }

    /// Unix: a shell's TERM trap.
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_ends_a_claude_waiting_for_its_editor() {
        // a shell defers its TERM trap until its foreground child (the
        // "editor") ends, as Claude Code does with an editor it opened
        let dir = std::env::temp_dir().join(format!("ccdash-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(&script, "#!/bin/sh\ntrap 'exit 0' TERM\nsleep 600\n").unwrap();
        fsx::set_mode(&script, 0o755).unwrap();
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

    /// Unix: on Windows the editor opens a console window.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_editor_saves_cancels_or_is_ended() {
        let dir = std::env::temp_dir().join(format!("ccdash-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("editor");
        // the "editor" prepends a line, and cancels when the text says so
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$(cat \"$1\")\" in *cancel*) exit 1;; esac\n\
            { echo edited; cat \"$1\"; } > \"$1.new\" && mv \"$1.new\" \"$1\"\n",
        )
        .unwrap();
        fsx::set_mode(&script, 0o755).unwrap();
        // NOTE: through `sh`: a parallel test forking between the write and an
        // exec of the script holds it open ("text file busy")
        let cmd = vec![
            "sh".to_owned(),
            script.to_string_lossy().into_owned(),
            "{file}".to_owned(),
        ];
        let files = dir.join("drafts");
        let mut pid = None;
        let text = edit_in(&files, &cmd, "ab12", "hello\n", |p| pid = Some(p))
            .await
            .unwrap();
        assert_eq!(text, "edited\nhello\n");
        assert!(pid.is_some());
        assert!(
            std::fs::read_dir(&files).unwrap().next().is_none(),
            "file kept"
        );
        let err = edit_in(&files, &cmd, "ab13", "please cancel", |_| {})
            .await
            .unwrap_err();
        assert!(err.starts_with("cancelled"), "{err}");
        assert!(edit_in(&files, &[], "ab14", "x", |_| {}).await.is_err());
        assert!(edit_in(&files, &cmd, "../x", "x", |_| {}).await.is_err());
        // a cancel ends the editor's process group; the result is a cancel
        let slow = vec!["sh".to_owned(), "-c".to_owned(), "sleep 30".to_owned()];
        let (tx, rx) = tokio::sync::oneshot::channel();
        let waited = tokio::spawn(async move {
            edit_in(
                &std::env::temp_dir().join(format!("ccdash-edit-slow-{}", std::process::id())),
                &slow,
                "ab15",
                "x",
                |p| {
                    let _ = tx.send(p);
                },
            )
            .await
        });
        let pid = rx.await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        end_group(pid);
        let err = waited.await.unwrap().unwrap_err();
        assert!(err.starts_with("cancelled"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_stop_ends_the_whole_tree() {
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 >NUL"])
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        let pid = child.id();
        for _ in 0..50 {
            if procs::children(pid)
                .iter()
                .any(|(_, name)| name.eq_ignore_ascii_case("ping.exe"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let said = end_interactive(pid).await.unwrap();
        assert!(said.to_ascii_lowercase().contains("ping.exe"), "{said}");
        assert!(!child.wait().unwrap().success());
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
            saved: false,
        }
    }

    #[test]
    fn launch_arguments_end_with_the_prompt() {
        let file = Path::new("/state/launch/0123abcd.json");
        let args = spec().args(file, true);
        assert_eq!(
            &args[..5],
            ["--bg", "--model", "haiku", "--permission-mode", "plan"]
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--channels", "plugin:ccdash@inline"])
        );
        assert!(!spec().args(file, false).iter().any(|a| a == "--channels"));
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
        .args(file, true);
        assert_eq!(
            &resumed[..3],
            ["--bg", "--resume", "9c4719e5-9508-461b-b1f4-e7b5a4c26f9d"]
        );
        assert!(spec().check().is_ok());
    }

    #[test]
    fn a_job_of_this_machine_continues_on_its_saved_options() {
        let id = "9c4719e5-9508-461b-b1f4-e7b5a4c26f9d";
        let waking = LaunchSpec {
            resume: Some(id.into()),
            model: None,
            permission_mode: None,
            saved: true,
            ..spec()
        };
        assert_eq!(
            waking.saved_args().unwrap(),
            ["--bg", "--resume", id, "--", "-v fix it"]
        );
        assert!(spec().saved_args().is_none(), "nothing to continue");
        let elsewhere = LaunchSpec {
            saved: false,
            ..waking
        };
        assert!(elsewhere.saved_args().is_none(), "needs the channel flags");
    }

    /// Unix: `/tmp` and `echo` as a program.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_wake_runs_no_leading_flags_and_its_failure_is_the_launch_failure() {
        let waking = |command: &[&str]| {
            let cfg = config::Launch {
                command: command.iter().map(|s| (*s).to_owned()).collect(),
                ..config::Launch::default()
            };
            let spec = LaunchSpec {
                cwd: "/tmp".into(),
                resume: Some("9c4719e5".into()),
                model: None,
                permission_mode: None,
                saved: true,
                ..spec()
            };
            async move { launch(&cfg, &spec, &HashSet::new()).await }
        };
        // NOTE: `echo` prints what it was given, which then fails as no id
        let echoed = waking(&["echo", "--plugin-dir", "/p"]).await.unwrap_err();
        assert!(
            echoed.ends_with("--bg --resume 9c4719e5 -- -v fix it"),
            "{echoed}"
        );
        assert!(waking(&["false"]).await.is_err());
        assert!(waking(&["/nonexistent/claude"]).await.is_err());
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

    /// Unix: `/tmp` and `sh`.
    #[cfg(unix)]
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
        let age = |path: &Path| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(stale)
                .unwrap();
        };
        age(&old);
        let some_job = HashSet::from(["aaaabbbb-0000".to_owned()]);
        prune_settings(&launch, &HashSet::new());
        assert!(old.exists(), "no poll yet: nothing can tell");
        prune_settings(&launch, &some_job);
        assert!(!old.exists(), "an old settings file is pruned");
        let fresh = launch.join("fresh.json");
        std::fs::write(&fresh, "{}").unwrap();
        prune_settings(&launch, &some_job);
        assert!(fresh.exists());

        // its job's saved options name it: it stays as long as the job
        let settings = launch.join("nonce1.json");
        std::fs::write(&settings, "{}").unwrap();
        note_job(&launch, "aaaabbbb", "nonce1");
        let note = launch.join("aaaabbbb.job");
        age(&settings);
        age(&note);
        assert!(settings_in(&launch, "aaaabbbb-0000-4000-8000-000000000000"));
        prune_settings(&launch, &some_job);
        assert!(settings.exists() && note.exists());
        prune_settings(&launch, &HashSet::from(["ffff0000-1111".to_owned()]));
        assert!(!settings.exists() && !note.exists(), "the job is gone");
        assert!(!settings_in(
            &launch,
            "aaaabbbb-0000-4000-8000-000000000000"
        ));

        // a job with no note, or that lost its file, is not known to have it
        assert!(!settings_in(
            &launch,
            "cccc0000-0000-4000-8000-000000000000"
        ));
        note_job(&launch, "dddd0000", "nonce2");
        assert!(!settings_in(
            &launch,
            "dddd0000-0000-4000-8000-000000000000"
        ));
        assert!(!settings_in(&launch, "../../etc/passwd"));
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
        let woken = "note: woke session 9c4719e5 with its saved options (--model).\nbackgrounded · 9c4719e5 · a-name\n";
        assert_eq!(launched_id(woken).as_deref(), Some("9c4719e5"));
        assert_eq!(launched_id("claude attach 9c4719e5\n"), None);
        assert_eq!(launched_id(""), None);
    }
}
