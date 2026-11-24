//! `ccdash win tray`: the collector under an icon in the taskbar's notification
//! area (docs/windows.md). It starts the collector and starts it again
//! when it ends, shows how it and its sessions are doing, opens the panel,
//! the configuration and the log, and keeps the build in step with the hub
//! ([`update`]). No window and no console: what it has to say goes to
//! `tray.log` next to the collector's log.

pub mod icon;
mod shell;
pub mod update;

use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::windows::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use windows_sys::Win32::{
    Storage::FileSystem::FILE_SHARE_READ,
    System::Threading::{CREATE_NO_WINDOW, DETACHED_PROCESS},
};

use self::{
    icon::Badge,
    shell::{Item, Notice, Remote, View},
    update::Found,
};
use crate::{
    config::{self, CollectorConfig},
    fsx, ipc,
    model::{Activity, Snapshot},
    paths, protocol,
    sync::collector::base_url,
    win,
};

/// How often the worker looks at the collector's process.
const TICK: Duration = Duration::from_secs(1);
/// Every this many ticks it asks the collector how things are.
const POLL_TICKS: u64 = 3;
/// How long the collector's answer is waited for.
const ANSWER: Duration = Duration::from_secs(3);
/// The wait before a collector that ended is started again: doubled from
/// the first to the last while it keeps ending, back to the first once it
/// ran for `SETTLED`.
const RESTART: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(60));
const SETTLED: Duration = Duration::from_secs(60);
/// Ends in a row after which the icon says so.
const ENDS_TO_TELL: u32 = 3;
/// The first look at the releases after a start, and the wait between
/// later ones: long while the hub is content, short while it speaks
/// another protocol (the release it wants may still be on its way).
const FIRST_CHECK: Duration = Duration::from_secs(30);
const CHECK_EVERY: Duration = Duration::from_secs(4 * 3600);
const CHECK_EVERY_REFUSED: Duration = Duration::from_secs(120);
/// The wait after a look that failed: at logon and after sleep the first
/// one often comes before the network.
const CHECK_AGAIN: Duration = Duration::from_secs(600);
/// How long `--replace` waits for the tray it replaces, and a start from a
/// console for the tray it started to stay up.
const REPLACE_WAIT: Duration = Duration::from_secs(30);
const DETACH_SETTLE: Duration = Duration::from_millis(1500);
/// How much of the collector's log is read for what its link says.
const LOG_TAIL: u64 = 32 << 10;

/// The collector's process, as the worker sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Collector {
    #[default]
    Starting,
    Running,
    /// Not running, and why.
    Down(String),
}

/// The collector's link to the hub.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Link {
    /// The collector has not said yet.
    #[default]
    Unknown,
    Up,
    /// Down, with the collector's last complaint about it when it has one.
    Down(Option<String>),
    /// Refused: the hub speaks this protocol, not this build's.
    Speaks(u32),
}

/// This machine's sessions that still run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Sessions {
    total: usize,
    working: usize,
    /// Waiting for the user: a permission prompt, or their turn.
    waiting: usize,
    /// Of those, at a permission prompt, which a hook holds open for the
    /// panel's answer.
    asking: usize,
}

impl Sessions {
    /// Whether a restart of the collector would cost something now: the
    /// prompts it holds for the panel, or the events of a turn under way.
    fn busy(self) -> bool {
        self.working > 0 || self.asking > 0
    }
}

/// What is known about updates.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Update {
    #[default]
    Quiet,
    /// The build of this commit is being put in place.
    Installing(String),
    /// The releases hold no build for this protocol.
    Nothing(u32),
    Failed(String),
}

#[derive(Default)]
struct State {
    collector: Collector,
    link: Link,
    sessions: Sessions,
    update: Update,
    notice: Option<Notice>,
}

impl State {
    fn badge(&self) -> Badge {
        if matches!(self.update, Update::Installing(_)) {
            return Badge::Busy;
        }
        match (&self.collector, &self.link) {
            (Collector::Down(_), _) | (Collector::Running, Link::Speaks(_)) => Badge::Trouble,
            (Collector::Running, Link::Up) if self.sessions.waiting > 0 => Badge::Waiting,
            (Collector::Running, Link::Up) => Badge::None,
            _ => Badge::Offline,
        }
    }

    /// One line on how things are, for the menu and the icon's tip.
    fn summary(&self) -> String {
        if let Update::Installing(rev) = &self.update {
            return format!("Updating to {}", update::short(rev));
        }
        match (&self.collector, &self.link) {
            (Collector::Down(why), _) => format!("The collector is not running: {why}"),
            (Collector::Starting, _) | (Collector::Running, Link::Unknown) => {
                "The collector is starting".to_owned()
            }
            (Collector::Running, Link::Speaks(theirs)) => format!(
                "The hub speaks protocol {theirs}, this build {}",
                protocol::VERSION
            ),
            (Collector::Running, Link::Down(None)) => "The hub is out of reach".to_owned(),
            (Collector::Running, Link::Down(Some(why))) => {
                format!("The hub is out of reach: {why}")
            }
            (Collector::Running, Link::Up) => {
                let Sessions {
                    total,
                    working,
                    waiting,
                    ..
                } = self.sessions;
                match (total, waiting) {
                    (0, _) => "Connected, no sessions".to_owned(),
                    (_, 0) => format!("Connected: {total} sessions, {working} working"),
                    _ => format!("Connected: {waiting} waiting for you, {working} working"),
                }
            }
        }
    }

    /// What the menu adds about updates, when there is something to add.
    fn update_line(&self) -> Option<String> {
        match &self.update {
            Update::Quiet | Update::Installing(_) => None,
            Update::Nothing(protocol) => Some(format!(
                "No build for protocol {protocol} in the releases yet"
            )),
            Update::Failed(why) => Some(format!("The update failed: {why}")),
        }
    }
}

/// What the worker is asked to do.
enum Cmd {
    Restart,
    /// Look at the releases now, and say what came of it.
    Check,
    /// A new build is in place at this path: hand over to it.
    Installed(PathBuf),
    Quit,
}

/// What the icon's thread, the worker and an update share.
struct Shared {
    state: Mutex<State>,
    remote: Remote,
    worker: mpsc::Sender<Cmd>,
    /// `--config`, for the collector and for Open the configuration.
    config: Option<PathBuf>,
    /// An update looks at the releases or installs right now.
    updating: AtomicBool,
    /// The last look failed: the next one comes sooner.
    look_again: AtomicBool,
    /// The file (its SHA-256) of the build that could not be installed: it
    /// is not fetched again by itself while the release holds it.
    refused_build: Mutex<Option<String>>,
    /// Windows is logging the user off or shutting down: the collector it
    /// ends is not started again.
    ending: AtomicBool,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes the state and has the icon show it.
    fn set(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state());
        self.remote.refresh();
    }

    fn tell(&self, title: &str, text: impl Into<String>, warning: bool) {
        let notice = Notice {
            title: title.to_owned(),
            text: text.into(),
            warning,
        };
        self.set(|state| state.notice = Some(notice));
    }

    fn ask(&self, cmd: Cmd) {
        let _ = self.worker.send(cmd);
    }

    fn config_file(&self) -> PathBuf {
        self.config.clone().unwrap_or_else(paths::config_file)
    }

    fn open_panel(&self) {
        let opened = config::load::<CollectorConfig>(self.config.clone()).and_then(|cfg| {
            let url = base_url(&cfg.hub.url);
            if url.is_empty() {
                bail!("hub.url is not set in {}", self.config_file().display());
            }
            win::shell_open(&url, None).with_context(|| format!("opening {url}"))
        });
        if let Err(e) = opened {
            self.tell("ccdash", format!("{e:#}"), true);
        }
    }

    /// Opens a text file in the program Windows has for it, else Notepad.
    fn open_file(&self, path: &Path) {
        let path = path.to_string_lossy();
        let opened = win::shell_open(&path, None)
            .or_else(|_| win::shell_open(&path, Some("notepad.exe")))
            .with_context(|| format!("opening {path}"));
        if let Err(e) = opened {
            self.tell("ccdash", format!("{e:#}"), true);
        }
    }
}

const OPEN_PANEL: u32 = 1;
const OPEN_CONFIG: u32 = 2;
const OPEN_LOG: u32 = 3;
const RESTART_COLLECTOR: u32 = 4;
const CHECK_UPDATES: u32 = 5;
const QUIT: u32 = 6;

impl shell::App for Shared {
    fn view(&self) -> View {
        let state = self.state();
        View {
            badge: state.badge(),
            tip: format!("ccdash: {}", state.summary()),
        }
    }

    fn notice(&self) -> Option<Notice> {
        self.state().notice.take()
    }

    fn menu(&self) -> Vec<Item> {
        let action = |id, text: &str| Item::Action {
            id,
            text: text.to_owned(),
        };
        let state = self.state();
        let mut items = vec![
            Item::Label(update::this_build()),
            Item::Label(state.summary()),
        ];
        items.extend(state.update_line().map(Item::Label));
        items.extend([
            Item::Separator,
            Item::Default {
                id: OPEN_PANEL,
                text: "Open the panel".to_owned(),
            },
            action(OPEN_CONFIG, "Open the configuration"),
            action(OPEN_LOG, "Show the log"),
            Item::Separator,
            action(RESTART_COLLECTOR, "Restart the collector"),
        ]);
        if update::enabled() {
            items.push(action(CHECK_UPDATES, "Check for updates"));
        }
        items.extend([Item::Separator, action(QUIT, "Quit")]);
        items
    }

    fn chosen(&self, id: u32) {
        match id {
            OPEN_PANEL => self.open_panel(),
            OPEN_CONFIG => self.open_file(&self.config_file()),
            OPEN_LOG => self.open_file(&collector_log()),
            RESTART_COLLECTOR => self.ask(Cmd::Restart),
            CHECK_UPDATES => self.ask(Cmd::Check),
            QUIT => self.ask(Cmd::Quit),
            _ => {}
        }
    }

    fn clicked(&self) {
        self.open_panel();
    }

    fn session_ending(&self, ending: bool) {
        // NOTE: said twice when the session ends as asked: once is enough for the log
        if self.ending.swap(ending, Ordering::SeqCst) != ending {
            info!(
                "{}",
                if ending {
                    "Windows is ending the session"
                } else {
                    "Windows goes on with the session"
                }
            );
        }
    }
}

pub fn collector_log() -> PathBuf {
    paths::state_dir().join("collector.log")
}

/// Starts `path` anew, the file of the run before kept as `<path>.old`. A
/// run that wrote nothing is not one to keep: it would push out the last
/// that did.
fn fresh_log(path: &Path) -> std::io::Result<File> {
    if std::fs::metadata(path).is_ok_and(|held| held.len() > 0) {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        let _ = std::fs::rename(path, old);
    }
    File::create(path)
}

/// How long a process that was ended is waited for.
const GONE: Duration = Duration::from_secs(10);

/// Ends the collector that serves the API pipe of this runtime directory,
/// whoever started it, and waits for it to be gone; its process id when
/// there was one. Found by its pipe, not by its name: a second collector
/// with a runtime directory of its own (`CCDASH_RUNTIME_DIR`) is another
/// one, and stays.
pub fn end_collector() -> Option<u32> {
    let pid = ipc::Stream::connect_within(&paths::api_socket(), ANSWER)
        .ok()?
        .server_pid()
        .filter(|pid| *pid != std::process::id())?;
    win::terminate(pid).ok()?;
    win::wait_gone(pid, GONE);
    Some(pid)
}

/// Ends the tray that holds the lock of this state directory, and waits
/// for it to be gone; its process id when there was one.
pub fn end_tray() -> Option<u32> {
    let dir = paths::state_dir();
    if lock(&dir, 1).is_some() {
        return None;
    }
    let pid = std::fs::read_to_string(dir.join(LOCK))
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|pid| *pid != std::process::id())?;
    win::terminate(pid).ok()?;
    win::wait_gone(pid, GONE);
    Some(pid)
}

/// Starts `exe win tray` with no console, for good: it does not end with this
/// process, and holds none of its handles.
///
/// # Errors
///
/// The process cannot be started.
pub fn start_detached(exe: &Path, config: Option<&Path>, replace: Option<u32>) -> Result<Child> {
    let mut command = Command::new(exe);
    command.args(["win", "tray"]);
    if let Some(config) = config {
        command.arg("--config").arg(config);
    }
    if let Some(pid) = replace {
        command.arg("--replace").arg(pid.to_string());
    }
    if let Some(dir) = exe.parent() {
        // NOTE: the hooks of the sessions the collector starts run `ccdash` by its name, and
        // the PATH setup has just changed in the registry is not this process's yet
        let path = std::env::var_os("PATH").unwrap_or_default();
        let entries = || std::env::split_paths(&path);
        if !entries().any(|entry| entry == dir)
            && let Ok(path) = std::env::join_paths(std::iter::once(dir.to_owned()).chain(entries()))
        {
            command.env("PATH", path);
        }
        // NOTE: not where setup was run, a directory the tray would hold for as long as it runs
        command.current_dir(dir);
    }
    win::keep_std_handles();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .with_context(|| format!("starting {} win tray", exe.display()))
}

/// The collector's process and when to start it.
struct Supervisor {
    config: Option<PathBuf>,
    child: Option<Child>,
    started: Instant,
    /// When a collector that ended may be started again, and the wait
    /// after the next end.
    again: Instant,
    wait: Duration,
    /// Ends in a row, none of them after a settled run.
    ends: u32,
}

impl Supervisor {
    fn new(config: Option<PathBuf>) -> Self {
        Self {
            config,
            child: None,
            started: Instant::now(),
            again: Instant::now(),
            wait: RESTART.0,
            ends: 0,
        }
    }

    fn spawn(&mut self) -> Result<()> {
        let log = fresh_log(&collector_log()).context("creating the collector's log")?;
        let mut command = Command::new(std::env::current_exe()?);
        command.arg("collector");
        if let Some(config) = &self.config {
            command.arg("--config").arg(config);
        }
        // NOTE: a console without a window, which the programs the collector runs share: with
        // none at all, each of them would open a window of its own
        let child = command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .context("starting the collector")?;
        info!("collector started (pid {})", child.id());
        self.child = Some(child);
        self.started = Instant::now();
        Ok(())
    }

    /// Looks at the collector: starts it when none runs, it is time and
    /// `may_start`. Returns its state, and whether it just ended for the
    /// `ENDS_TO_TELL`th time in a row.
    fn tick(&mut self, may_start: bool) -> (Collector, bool) {
        let mut tell = false;
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(None) => return (Collector::Running, false),
                Ok(Some(status)) => {
                    warn!("the collector ended ({status})");
                    if self.started.elapsed() >= SETTLED {
                        (self.wait, self.ends) = (RESTART.0, 0);
                    }
                    self.ends += 1;
                    // NOTE: ended by Windows with the session is not one more failure
                    tell = may_start && self.ends == ENDS_TO_TELL;
                    self.again = Instant::now() + self.wait;
                    self.wait = (self.wait * 2).min(RESTART.1);
                }
                Err(e) => warn!("the collector's process cannot be looked at: {e}"),
            }
            self.child = None;
        }
        // NOTE: a collector somebody else started (a terminal, the old Run key) is left to run
        if ipc::serving(&paths::api_socket()) {
            return (Collector::Running, tell);
        }
        if !may_start {
            // NOTE: a collector started into a session that ends would be ended at once, and
            // its empty log would stand where the last run's is
            return (
                Collector::Down("Windows is ending the session".to_owned()),
                false,
            );
        }
        if Instant::now() < self.again {
            return (Collector::Down("it ended; see the log".to_owned()), tell);
        }
        match self.spawn() {
            Ok(()) => (Collector::Starting, tell),
            Err(e) => {
                warn!("{e:#}");
                self.again = Instant::now() + self.wait;
                self.wait = (self.wait * 2).min(RESTART.1);
                (Collector::Down(format!("{e:#}")), tell)
            }
        }
    }

    /// Ends the collector: the one started here, and with `all` one that
    /// was started elsewhere and serves this tray's pipe.
    fn stop(&mut self, all: bool) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            info!("collector stopped");
        }
        if all && let Some(pid) = end_collector() {
            info!("collector stopped (pid {pid}, started elsewhere)");
        }
        (self.again, self.wait, self.ends) = (Instant::now(), RESTART.0, 0);
    }
}

/// Asks the collector for its snapshot; none when it does not answer.
pub fn snapshot() -> Option<Snapshot> {
    let mut stream = ipc::Stream::connect_within(&paths::api_socket(), ANSWER).ok()?;
    stream.set_read_timeout(Some(ANSWER)).ok()?;
    stream.set_write_timeout(Some(ANSWER)).ok()?;
    writeln!(stream, r#"{{"cmd":"snapshot"}}"#).ok()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn sessions(snapshot: &Snapshot) -> Sessions {
    let mut out = Sessions::default();
    for session in &snapshot.sessions {
        match session.activity {
            Activity::Gone => continue,
            Activity::Working => out.working += 1,
            Activity::Permission => (out.waiting, out.asking) = (out.waiting + 1, out.asking + 1),
            Activity::YourTurn => out.waiting += 1,
            Activity::Idle | Activity::Parked => {}
        }
        out.total += 1;
    }
    out
}

/// The end of the collector's log.
fn log_tail() -> String {
    let read = || -> std::io::Result<String> {
        let mut file = File::open(collector_log())?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL)))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    read().unwrap_or_default()
}

/// What the collector's log says about a link that is down: its last
/// complaint since it was last connected, and the hub's protocol when the
/// complaint is that the hub speaks another one (the hub's refusal in
/// `hub/collect.rs`, the collector's own check in `collector/link.rs`).
fn link_down(log: &str) -> Link {
    const LINK: &str = "ccdash::collector::link: ";
    const SPEAKS: &str = "the hub speaks protocol ";
    for line in log.lines().rev() {
        let Some((head, said)) = line.split_once(LINK) else {
            continue;
        };
        if said.starts_with("connected to the hub") {
            break;
        }
        if !head.contains("WARN") {
            continue;
        }
        if let Some((_, rest)) = said.split_once(SPEAKS) {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(theirs) = digits.parse() {
                return Link::Speaks(theirs);
            }
        }
        let said = said.strip_prefix("hub link: ").unwrap_or(said);
        return Link::Down(Some(crate::util::truncate(said, 120)));
    }
    Link::Down(None)
}

/// When to look at the releases, and for which protocol.
struct Checks {
    next: Instant,
    /// The hub's protocol at the last look, when it refused this build's.
    refused: Option<u32>,
}

impl Checks {
    /// The protocol to look for when it is time to look. A refusal not
    /// seen before is looked into at once. A newer build of the protocol
    /// in use waits while the sessions are `busy`: taking it restarts the
    /// collector, and the prompts it holds for the panel go with it. A
    /// refused collector holds none, and has nothing to wait for.
    fn due(&mut self, link: &Link, busy: bool) -> Option<u32> {
        let refused = match link {
            Link::Speaks(theirs) => Some(*theirs),
            _ => None,
        };
        let now = Instant::now();
        if refused == self.refused && now < self.next {
            return None;
        }
        if refused.is_none() && busy {
            return None;
        }
        self.refused = refused;
        self.next = now
            + if refused.is_some() {
                CHECK_EVERY_REFUSED
            } else {
                CHECK_EVERY
            };
        Some(refused.unwrap_or(protocol::VERSION))
    }

    /// The next look comes within `wait`.
    fn within(&mut self, wait: Duration) {
        self.next = self.next.min(Instant::now() + wait);
    }
}

/// What came of a look at the releases.
enum Looked {
    /// Another build is in place, and the worker hands over to it.
    Installed,
    /// Nothing to install: what the menu says of it, and the answer for
    /// who asked.
    Nothing(Update, String),
    /// The build there is the one that could not be installed before.
    Refused,
}

/// Looks at the release for `protocol` and installs its build when it is
/// another one, on a thread of its own; `asked` says what came of it even
/// when that is nothing. A failure is in the menu and the log, and a
/// notification only for who asked: the icon says by itself when the hub
/// refuses this build.
fn check_update(shared: &Arc<Shared>, protocol: u32, asked: bool) {
    if !update::enabled() || shared.updating.swap(true, Ordering::SeqCst) {
        return;
    }
    let shared = Arc::clone(shared);
    std::thread::spawn(move || {
        let refused = || {
            shared
                .refused_build
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };
        let outcome = update::check(protocol).and_then(|found| match found {
            // NOTE: not the same 16 MB at every look, to fail on them the same way
            Found::Other(manifest) if !asked && *refused() == Some(manifest.sha256.clone()) => {
                Ok(Looked::Refused)
            }
            Found::Other(manifest) => {
                info!("installing {} (protocol {protocol})", manifest.rev);
                shared.set(|state| state.update = Update::Installing(manifest.rev.clone()));
                // NOTE: only a build that came and cannot be used: one whose download failed
                // is fetched again at the next look
                let exe = update::install(&manifest).inspect_err(|e| {
                    if e.downcast_ref::<update::Unusable>().is_some() {
                        *refused() = Some(manifest.sha256.clone());
                    }
                })?;
                shared.ask(Cmd::Installed(exe));
                Ok(Looked::Installed)
            }
            Found::Current => Ok(Looked::Nothing(
                Update::Quiet,
                "This is the newest build".to_owned(),
            )),
            Found::Nothing => Ok(Looked::Nothing(
                Update::Nothing(protocol),
                format!("The releases hold no build for protocol {protocol} yet"),
            )),
        });
        match outcome {
            // NOTE: `updating` stays set: the worker hands over to the new build
            Ok(Looked::Installed) => return,
            Ok(Looked::Nothing(update, text)) => {
                shared.set(|state| state.update = update);
                if asked {
                    shared.tell("ccdash", text, false);
                }
            }
            Ok(Looked::Refused) => {}
            Err(e) => {
                let why = format!("{e:#}");
                warn!("update: {why}");
                shared.set(|state| state.update = Update::Failed(why.clone()));
                if asked {
                    shared.tell("ccdash could not update", why, true);
                }
                shared.look_again.store(true, Ordering::SeqCst);
            }
        }
        shared.updating.store(false, Ordering::SeqCst);
    });
}

/// A new build is in place at `exe`: it sets itself up (its plugin, its
/// hooks), then its tray takes over from this one.
fn hand_over(shared: &Shared, supervisor: &mut Supervisor, exe: &Path) -> Result<()> {
    let setup = Command::new(exe)
        .args(["win", "setup", "--refresh"])
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match setup {
        Ok(output) => {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                info!("setup: {line}");
            }
            if !output.status.success() {
                // NOTE: the new build still runs with what the old one set up
                warn!("setup: {}", String::from_utf8_lossy(&output.stderr).trim());
            }
        }
        Err(e) => warn!("setup: {e}"),
    }
    supervisor.stop(true);
    start_detached(exe, shared.config.as_deref(), Some(std::process::id()))?;
    Ok(())
}

fn worker(shared: &Arc<Shared>, commands: &mpsc::Receiver<Cmd>) {
    let mut supervisor = Supervisor::new(shared.config.clone());
    let mut checks = Checks {
        next: Instant::now() + FIRST_CHECK,
        refused: None,
    };
    let mut tick = 0u64;
    loop {
        match commands.recv_timeout(TICK) {
            Ok(Cmd::Restart) => {
                info!("restarting the collector");
                supervisor.stop(true);
                shared.set(|state| (state.collector, state.link) = Default::default());
            }
            Ok(Cmd::Check) => {
                let protocol = match shared.state().link {
                    Link::Speaks(theirs) => theirs,
                    _ => protocol::VERSION,
                };
                check_update(shared, protocol, true);
            }
            Ok(Cmd::Installed(exe)) => match hand_over(shared, &mut supervisor, &exe) {
                Ok(()) => break,
                Err(e) => {
                    warn!("{e:#}");
                    shared.updating.store(false, Ordering::SeqCst);
                    shared.set(|state| state.update = Update::Failed(format!("{e:#}")));
                }
            },
            Ok(Cmd::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                supervisor.stop(false);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let (collector, keeps_ending) = supervisor.tick(!shared.ending.load(Ordering::SeqCst));
        if keeps_ending {
            shared.tell(
                "ccdash",
                "The collector keeps ending. Its log says why: Show the log in this icon's menu.",
                true,
            );
            // NOTE: a build whose collector cannot run may have a successor that can
            checks.within(Duration::ZERO);
        }
        let polled =
            (collector == Collector::Running && tick.is_multiple_of(POLL_TICKS)).then(|| {
                let snapshot = snapshot();
                let link = match &snapshot {
                    Some(s) if s.hosts.first().is_some_and(|host| host.connected) => Link::Up,
                    Some(_) => link_down(&log_tail()),
                    None => Link::Unknown,
                };
                (link, snapshot.as_ref().map(sessions).unwrap_or_default())
            });
        tick += 1;
        let changed = {
            let mut state = shared.state();
            let before = (state.collector.clone(), state.link.clone(), state.sessions);
            if collector != Collector::Running {
                (state.link, state.sessions) = Default::default();
            } else if let Some((link, sessions)) = polled {
                // NOTE: the collector logs a refusal once. When it is no longer in the part of
                // the log that is read, it stands until the link is up or complains otherwise
                if !matches!((&state.link, &link), (Link::Speaks(_), Link::Down(None))) {
                    state.link = link;
                }
                state.sessions = sessions;
            }
            state.collector = collector;
            before != (state.collector.clone(), state.link.clone(), state.sessions)
        };
        if changed {
            info!("{}", shared.state().summary());
            shared.remote.refresh();
        }
        if shared.look_again.swap(false, Ordering::SeqCst) {
            checks.within(CHECK_AGAIN);
        }
        // NOTE: whatever the link says, and when the collector does not run at all: a build
        // that cannot start is the one that most needs its successor
        let (link, busy) = {
            let state = shared.state();
            (state.link.clone(), state.sessions.busy())
        };
        if let Some(protocol) = checks.due(&link, busy) {
            check_update(shared, protocol, false);
        }
    }
    shared.remote.quit();
}

/// The file whose open handle is the lock of the one tray of a state
/// directory; it holds the tray's process id.
const LOCK: &str = "tray.lock";

/// Takes the tray's lock, trying `tries` times half a second apart; none
/// while another process holds it. Others may read the file (the id of
/// the process to end, for `end_tray`), not write it.
fn lock(dir: &Path, tries: u32) -> Option<File> {
    for attempt in 0..tries {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(500));
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(FILE_SHARE_READ)
            .open(dir.join(LOCK));
        if let Ok(mut file) = file {
            let _ = file.set_len(0);
            let _ = write!(file, "{}", std::process::id());
            return Some(file);
        }
    }
    None
}

fn logging(path: &Path) -> Result<()> {
    let file = fresh_log(path).with_context(|| format!("creating {}", path.display()))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("CCDASH_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .init();
    Ok(())
}

/// Started from a console: the tray starts again without one, and this
/// process returns to the shell.
fn detach(dir: &Path, config: Option<&Path>, replace: Option<u32>) -> Result<()> {
    if replace.is_none() && lock(dir, 1).is_none() {
        println!("ccdash is in the tray already");
        return Ok(());
    }
    let mut child = start_detached(&std::env::current_exe()?, config, replace)?;
    // NOTE: a console opened for this start alone (Explorer's, on a Windows that still gives
    // one) goes now, not to stand through the settling
    win::leave_own_console();
    std::thread::sleep(DETACH_SETTLE);
    if let Some(status) = child.try_wait()? {
        bail!(
            "the tray ended at once ({status}); see {}",
            dir.join("tray.log").display()
        );
    }
    let _ = writeln!(
        std::io::stdout(),
        "ccdash is in the tray (pid {}); its log is {}",
        child.id(),
        dir.join("tray.log").display()
    );
    Ok(())
}

/// `ccdash win update`: looks at the release for the hub's protocol (this
/// build's, unless the hub refuses it) and puts its build in place; the
/// new build then sets itself up and starts. `only_check` just says what
/// the releases hold.
///
/// # Errors
///
/// The releases cannot be read, or the build cannot be put in place or
/// set up.
pub fn update_now(only_check: bool) -> Result<()> {
    let connected = |s: &Snapshot| s.hosts.first().is_some_and(|host| host.connected);
    let protocol = match snapshot() {
        Some(s) if !connected(&s) => match link_down(&log_tail()) {
            Link::Speaks(theirs) => theirs,
            _ => protocol::VERSION,
        },
        _ => protocol::VERSION,
    };
    let this = update::this_build();
    match update::check(protocol)? {
        Found::Current => println!("{this} is the newest build for protocol {protocol}"),
        Found::Nothing => println!("the releases hold no build for protocol {protocol} yet"),
        Found::Other(manifest) if only_check => println!(
            "the releases hold {} for protocol {protocol}; this is {this}",
            update::short(&manifest.rev)
        ),
        Found::Other(manifest) => {
            let exe = update::install(&manifest)?;
            println!(
                "{} for protocol {protocol} is in place",
                update::short(&manifest.rev)
            );
            // NOTE: setup starts the tray for good. It gets this process's output to write to,
            // and must not get the caller's pipe a second time, as a handle it knows nothing
            // of and would pass on: whoever reads that pipe would wait for the tray to end
            win::keep_std_handles();
            let status = Command::new(&exe)
                .args(["win", "setup"])
                .status()
                .with_context(|| format!("starting {} win setup", exe.display()))?;
            if !status.success() {
                bail!("its setup failed ({status})");
            }
        }
    }
    Ok(())
}

/// `ccdash win tray`.
///
/// # Errors
///
/// From a console: the tray could not be started without one, or ended at
/// once. Without one: the state directory or the icon cannot be made.
pub fn run(config: Option<PathBuf>, replace: Option<u32>) -> Result<()> {
    // NOTE: the tray that stays does not run where this one was started, nor does its
    // collector: a `--config` given from there has to mean the same file to them
    let config = config.map(|path| std::path::absolute(&path).unwrap_or(path));
    let dir = paths::state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fsx::set_mode(&dir, 0o700)?;
    if win::has_console() {
        return detach(&dir, config.as_deref(), replace);
    }
    if let Some(pid) = replace {
        win::wait_gone(pid, REPLACE_WAIT);
    }
    // NOTE: a second tray has nothing to do, and nobody to tell
    let Some(_lock) = lock(&dir, if replace.is_some() { 20 } else { 1 }) else {
        return Ok(());
    };
    logging(&dir.join("tray.log"))?;
    info!("{}", update::this_build());
    if let Ok(exe) = std::env::current_exe() {
        update::sweep(&exe);
    }
    let (worker_tx, worker_rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        state: Mutex::default(),
        remote: Remote::default(),
        worker: worker_tx,
        config,
        updating: AtomicBool::new(false),
        look_again: AtomicBool::new(false),
        refused_build: Mutex::default(),
        ending: AtomicBool::new(false),
    });
    if replace.is_some() {
        shared.state().notice = Some(Notice {
            title: "ccdash was updated".to_owned(),
            text: format!("Now {}.", update::this_build()),
            warning: false,
        });
    }
    let thread = std::thread::spawn({
        let shared = Arc::clone(&shared);
        move || worker(&shared, &worker_rx)
    });
    let served = shell::run(Arc::clone(&shared) as Arc<dyn shell::App>, &shared.remote);
    // NOTE: the icon's loop also ends when something closes its window (`taskkill`)
    shared.ask(Cmd::Quit);
    let _ = thread.join();
    info!("the tray ends");
    served.context("the tray icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(collector: Collector, link: Link, waiting: usize) -> State {
        State {
            collector,
            link,
            sessions: Sessions {
                total: 3,
                working: 1,
                waiting,
                asking: 0,
            },
            ..State::default()
        }
    }

    #[test]
    fn the_icon_says_what_most_needs_a_look() {
        let s = state(Collector::Running, Link::Up, 0);
        assert_eq!(s.badge(), Badge::None);
        assert_eq!(s.summary(), "Connected: 3 sessions, 1 working");
        let s = state(Collector::Running, Link::Up, 2);
        assert_eq!(s.badge(), Badge::Waiting);
        assert_eq!(s.summary(), "Connected: 2 waiting for you, 1 working");
        let s = state(Collector::Running, Link::Down(None), 2);
        assert_eq!(
            s.badge(),
            Badge::Offline,
            "nothing waits where no answer goes"
        );
        let s = state(Collector::Running, Link::Speaks(20), 0);
        assert_eq!(s.badge(), Badge::Trouble);
        assert_eq!(
            s.summary(),
            format!(
                "The hub speaks protocol 20, this build {}",
                protocol::VERSION
            )
        );
        let s = state(Collector::Down("it ended; see the log".into()), Link::Up, 2);
        assert_eq!(s.badge(), Badge::Trouble);
        let mut s = state(Collector::Running, Link::Speaks(20), 0);
        s.update = Update::Installing("1a2b3c4d5e".into());
        assert_eq!(s.badge(), Badge::Busy);
        assert_eq!(s.summary(), "Updating to 1a2b3c4");
        assert_eq!(State::default().badge(), Badge::Offline);
        assert_eq!(State::default().summary(), "The collector is starting");
    }

    #[test]
    fn sessions_count_by_what_they_wait_for_and_gone_ones_not_at_all() {
        let mut snapshot: Snapshot = serde_json::from_value(serde_json::json!({
            "host": "winbox", "now": 0, "workspaces": [], "rate_limits": null,
            "sessions": [],
        }))
        .unwrap();
        assert_eq!(sessions(&snapshot), Sessions::default());
        snapshot.sessions = [
            Activity::Working,
            Activity::Permission,
            Activity::YourTurn,
            Activity::Parked,
            Activity::Idle,
            Activity::Gone,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, activity)| {
            let mut session = crate::model::Session::stub("winbox", &i.to_string(), r"C:\w");
            session.activity = activity;
            session
        })
        .collect();
        assert_eq!(
            sessions(&snapshot),
            Sessions {
                total: 5,
                working: 1,
                waiting: 2,
                asking: 1
            }
        );
        assert!(sessions(&snapshot).busy(), "a turn runs, a prompt is held");
        snapshot.sessions.retain(|session| {
            !matches!(session.activity, Activity::Working | Activity::Permission)
        });
        assert!(
            !sessions(&snapshot).busy(),
            "a session at its turn loses nothing"
        );
    }

    #[test]
    fn a_log_starts_anew_and_keeps_the_last_run_that_wrote() {
        let dir = std::env::temp_dir().join(format!("ccdash-tray-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (log, old) = (dir.join("collector.log"), dir.join("collector.log.old"));
        writeln!(fresh_log(&log).unwrap(), "the first run").unwrap();
        drop(fresh_log(&log).unwrap());
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "the first run\n");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
        // NOTE: a run ended as it started (at a log-off) wrote nothing: the first run's stays
        drop(fresh_log(&log).unwrap());
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "the first run\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The collector's log lines, as `tracing` writes them to a file.
    fn log(lines: &[(&str, &str)]) -> String {
        lines
            .iter()
            .map(|(level, said)| {
                format!("2026-11-18T18:50:11.607345Z {level:>5} ccdash::collector::link: {said}\n")
            })
            .collect()
    }

    #[test]
    fn the_log_names_the_hubs_protocol_when_it_refuses_this_one() {
        // NOTE: as the hub refuses (hub/collect.rs) and as the collector itself notices
        // (collector/link.rs)
        for said in [
            "hub link: refused: the hub speaks protocol 16, not 19",
            "hub link: the hub speaks protocol 16, this collector 19",
        ] {
            assert_eq!(
                link_down(&log(&[("WARN", said)])),
                Link::Speaks(16),
                "{said}"
            );
        }
    }

    #[test]
    fn only_what_the_log_says_since_the_last_connection_counts() {
        let refused = (
            "WARN",
            "hub link: refused: the hub speaks protocol 16, not 19",
        );
        let connected = (
            "INFO",
            "connected to the hub at wss://hub.example.org/collect",
        );
        let replayed = ("INFO", "replayed 124 queued events");
        let closed = ("INFO", "hub closed the link");
        let unreachable = (
            "WARN",
            "hub wss://hub.example.org/collect: no route to host",
        );
        assert_eq!(
            link_down(&log(&[refused, connected, replayed])),
            Link::Down(None)
        );
        assert_eq!(
            link_down(&log(&[refused, connected, closed])),
            Link::Down(None)
        );
        assert_eq!(
            link_down(&log(&[refused, connected, closed, unreachable])),
            Link::Down(Some(
                "hub wss://hub.example.org/collect: no route to host".into()
            ))
        );
        assert_eq!(
            link_down(&log(&[connected, unreachable, refused])),
            Link::Speaks(16)
        );
        assert_eq!(link_down(""), Link::Down(None));
    }

    #[test]
    fn a_refusal_is_looked_into_at_once_and_then_now_and_again() {
        let mut checks = Checks {
            next: Instant::now() + FIRST_CHECK,
            refused: None,
        };
        assert_eq!(
            checks.due(&Link::Up, false),
            None,
            "not before the first look is due"
        );
        assert_eq!(checks.due(&Link::Speaks(20), false), Some(20));
        assert_eq!(
            checks.due(&Link::Speaks(20), false),
            None,
            "looked a moment ago"
        );
        assert_eq!(
            checks.due(&Link::Speaks(21), true),
            Some(21),
            "another refusal, and no session's work is a reason to wait"
        );
        assert_eq!(
            checks.due(&Link::Up, false),
            Some(protocol::VERSION),
            "content again: the newest build of this protocol"
        );
        assert_eq!(checks.due(&Link::Down(None), false), None);
        checks.next = Instant::now();
        assert_eq!(
            checks.due(&Link::Down(None), false),
            Some(protocol::VERSION)
        );
    }

    #[test]
    fn a_look_is_due_whatever_the_link_says_and_waits_for_a_quiet_moment() {
        let mut checks = Checks {
            next: Instant::now(),
            refused: None,
        };
        // NOTE: a collector that does not run or answer: its build may have a successor
        assert_eq!(checks.due(&Link::Unknown, false), Some(protocol::VERSION));
        checks.next = Instant::now();
        assert_eq!(checks.due(&Link::Up, true), None, "a prompt is held");
        assert_eq!(checks.due(&Link::Up, true), None);
        assert_eq!(
            checks.due(&Link::Up, false),
            Some(protocol::VERSION),
            "as soon as nothing is"
        );
        // NOTE: a look that failed is repeated sooner, a later one not put off
        checks.within(CHECK_AGAIN);
        assert!(checks.next <= Instant::now() + CHECK_AGAIN);
        checks.within(CHECK_EVERY);
        assert!(checks.next <= Instant::now() + CHECK_AGAIN);
    }
}
