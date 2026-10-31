//! The collector's side of the workspace sync: a scheduler thread that
//! owns the units (what the hub's setup says travels, what exists here),
//! settles inotify bursts (250 ms, 2 s at most), and hands passes to a
//! pool of four workers, one pass per unit at a time. A pass snapshots,
//! pushes, fetches and applies the other hosts' snapshots (one at a time,
//! re-snapshotting in between), checks held conflicts for markers, and
//! reports the unit's state; the scheduler sends the changed states to the
//! hub. Safety nets: a sweep at start, after an overflow and every 30
//! minutes; a refs refresh every 5 minutes. The setup is cached on disk
//! for an offline start. See `meta/plans/ccdash-hub/h9.md`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tracing::{debug, info, warn};

use super::{
    ConflictReport, Diverged, HELD_REFS, HeldAction, HeldReport, IgnoredDir, Rules, SYNC_REFS,
    SeenUnit, Side, TIPS_REFS, UnitKind, UnitState, UnitStatus, WorkspaceConfig, WorkspaceSetup,
    WorkspaceStatus, apply,
    git::{Git, Opts, Repo},
    snapshot::{self, Local, Outcome as Snap},
    units::{self, Layout},
    watch::{self, Event, Registry},
};
use crate::{protocol::ToHub, util::now_ms};

/// A burst of events settles for this long before a pass…
const QUIET: Duration = Duration::from_millis(250);
/// …but waits no longer than this while writes keep coming.
const BURST_MAX: Duration = Duration::from_secs(2);
/// Everything is snapshotted this often, in case an event went missing.
const SWEEP_EVERY: Duration = Duration::from_secs(1800);
/// Every unit's remote refs are fetched this often, against missed pushes.
const REFRESH_EVERY: Duration = Duration::from_secs(300);
/// Changed unit states go up at most this often.
const REPORT_EVERY: Duration = Duration::from_secs(1);
/// Every unit's state goes up again this often.
const FULL_EVERY: Duration = Duration::from_secs(300);
/// A unit that could not be applied (a lock, a live tool call) is tried
/// again this soon.
const SOON: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(120);
/// A quiescent directory a process holds is looked at again this often.
const LAG_RETRY: Duration = Duration::from_secs(15);
/// A tool call older than this no longer defers applies (its hook went
/// missing).
const TOOL_STALE_MS: i64 = 600_000;
const WORKERS: usize = 4;

/// What the collector, the hub and the watcher ask of the scheduler.
pub enum Cmd {
    /// The hub's (or the cache's) configuration and unit list.
    Setup(WorkspaceSetup),
    /// A push by `host` landed on the hub for `unit`.
    Changed { unit: String, host: String },
    /// Fetch and snapshot everything (or one unit) now.
    Resync(Option<String>),
    /// Write one side of a conflict over the file; answered to the hub.
    Resolve {
        id: i64,
        unit: String,
        path: String,
        blob: String,
    },
    /// Apply or drop a held snapshot (`commit`: only that one).
    Held {
        unit: String,
        action: HeldAction,
        commit: Option<String>,
    },
    /// Take the first snapshot of units that exist here and on the hub
    /// (one unit, or every one "to adopt"); `prefer` makes one side win
    /// outright instead of merging.
    Adopt {
        unit: Option<String>,
        prefer: Option<Side>,
    },
    /// Create every unit the hub has and this host lacks, now.
    Init,
    /// The state of every unit, for the CLI.
    Status(oneshot::Sender<Value>),
    /// The hub link came up or went down.
    Connected(bool),
}

/// What reaches the scheduler thread: commands, watcher events, finished
/// jobs.
enum Msg {
    Cmd(Cmd),
    Watch(Event),
    Done(Box<Done>),
}

/// Tool calls in flight on this host (PreToolUse without PostToolUse):
/// session → its directory and when the call started. Applies to units
/// under (or over) that directory wait.
#[derive(Clone, Default)]
pub struct Busy(Arc<Mutex<HashMap<String, (String, i64)>>>);

impl Busy {
    pub fn start(&self, session: &str, cwd: &str, at: i64) {
        if let Ok(mut map) = self.0.lock() {
            map.insert(session.to_owned(), (cwd.to_owned(), at));
        }
    }

    pub fn end(&self, session: &str) {
        if let Ok(mut map) = self.0.lock() {
            map.remove(session);
        }
    }

    /// Drops every call of a session (and its subagents) that ended.
    pub fn end_session(&self, session: &str) {
        if let Ok(mut map) = self.0.lock() {
            map.retain(|k, _| k != session && !k.starts_with(&format!("{session}:")));
        }
    }

    /// Whether a live tool call touches `dir`: the session's label.
    pub fn covers(&self, dir: &Path, now: i64) -> Option<String> {
        let map = self.0.lock().ok()?;
        map.iter()
            .filter(|(_, (_, since))| now - since < TOOL_STALE_MS)
            .find(|(_, (cwd, _))| {
                let cwd = Path::new(cwd);
                dir.starts_with(cwd) || cwd.starts_with(dir)
            })
            .map(|(session, _)| session.chars().take(8).collect())
    }
}

/// The collector's handle on the scheduler.
#[derive(Clone)]
pub struct Handle {
    tx: Sender<Msg>,
    pub busy: Busy,
}

impl Handle {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(Msg::Cmd(cmd));
    }

    /// The state of every unit, as JSON.
    pub async fn status(&self) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Status(tx));
        rx.await.context("the workspace thread stopped")
    }
}

/// How the thread runs.
pub struct Settings {
    pub host: String,
    pub root: PathBuf,
    /// `$XDG_STATE_HOME/ccdash/workspace`.
    pub state: PathBuf,
    /// The hub's git base, `https://host/git/`.
    pub hub_base: String,
    pub token_file: PathBuf,
    /// The starting configuration, until the hub's arrives.
    pub starting: WorkspaceConfig,
}

/// Starts the scheduler and its workers; unit states go to the hub
/// through `hub`.
///
/// # Errors
///
/// The state directory or database cannot be opened.
pub fn spawn(settings: Settings, hub: UnboundedSender<ToHub>) -> Result<Handle> {
    fs::create_dir_all(&settings.state)?;
    let git = Git::prepare(
        &settings.state,
        &settings.host,
        &settings.starting.always_exclude,
        Some(settings.hub_base.clone()),
        Some(settings.token_file.clone()),
    )?;
    let db = Db::open(&settings.state.join("workspace.db"))?;
    let (tx, rx) = mpsc::channel();
    let busy = Busy::default();
    let registry = match Registry::spawn(&settings.root, settings.starting.depth, {
        let tx = tx.clone();
        let (wtx, wrx) = mpsc::channel::<Event>();
        thread::Builder::new()
            .name("workspace-events".into())
            .spawn(move || {
                while let Ok(event) = wrx.recv() {
                    if tx.send(Msg::Watch(event)).is_err() {
                        return;
                    }
                }
            })?;
        wtx
    }) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!(
                "workspace: cannot watch {} ({e}); sweeping every {:?} instead",
                settings.root.display(),
                SWEEP_EVERY
            );
            None
        }
    };
    let shared = Arc::new(Shared {
        git,
        layout: Layout {
            root: settings.root.clone(),
            state: settings.state.clone(),
        },
        host: settings.host.clone(),
        hub_base: settings.hub_base.clone(),
        registry,
        busy: busy.clone(),
    });
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let job_rx = Arc::new(Mutex::new(job_rx));
    for n in 0..WORKERS {
        let (shared, rx, tx) = (shared.clone(), job_rx.clone(), tx.clone());
        thread::Builder::new()
            .name(format!("workspace-{n}"))
            .spawn(move || worker(&shared, &rx, &tx))?;
    }
    let cached = read_setup(&settings.state);
    let scheduler = Scheduler {
        shared,
        hub,
        db,
        setup: cached.unwrap_or_else(|| WorkspaceSetup {
            version: 0,
            config: settings.starting.clone(),
            units: BTreeMap::new(),
        }),
        setup_known: false,
        units: BTreeMap::new(),
        seen: BTreeMap::new(),
        jobs: job_tx,
        in_flight: 0,
        connected: false,
        changed: BTreeSet::new(),
        last_report: Instant::now(),
        last_full: Instant::now(),
        last_sweep: Instant::now(),
        last_refresh: Instant::now(),
        sweep_due: true,
        discover_due: true,
        started: now_ms(),
        error: None,
        pending_sizes: 0,
        seen_dirty: false,
        starting: settings.starting.clone(),
    };
    thread::Builder::new()
        .name("workspace".into())
        .spawn(move || scheduler.run(&rx))?;
    Ok(Handle { tx, busy })
}

fn read_setup(state: &Path) -> Option<WorkspaceSetup> {
    let bytes = fs::read(state.join("setup.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_setup(state: &Path, setup: &WorkspaceSetup) {
    let tmp = state.join("setup.json.tmp");
    let result = serde_json::to_vec(setup)
        .map_err(std::io::Error::other)
        .and_then(|bytes| fs::write(&tmp, bytes))
        .and_then(|()| fs::rename(&tmp, state.join("setup.json")));
    if let Err(e) = result {
        warn!("workspace: caching the setup: {e}");
    }
}

/// What every worker shares.
struct Shared {
    git: Git,
    layout: Layout,
    host: String,
    hub_base: String,
    registry: Option<Registry>,
    busy: Busy,
}

impl Shared {
    fn url(&self, unit: &str) -> String {
        format!("{}{unit}.git", self.hub_base)
    }

    /// The unit's excludes file when its rules add to the defaults.
    fn excludes(&self, unit: &str, rules: &Rules) -> Option<PathBuf> {
        let defaults: Vec<String> = fs::read_to_string(&self.git.excludes_file)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        if rules.always_exclude == defaults {
            return None;
        }
        let scratch = self.layout.scratch(unit);
        fs::create_dir_all(&scratch).ok()?;
        self.git.excludes_for(&scratch, &rules.always_exclude).ok()
    }
}

/// Work for a worker.
enum Job {
    Pass(Box<PassJob>),
    /// Register the unit's watches (or those of new directories).
    Watch {
        unit: String,
        kind: UnitKind,
        rules: Rules,
        /// Directories that appeared, relative to the unit; empty: the
        /// whole unit.
        new_dirs: Vec<String>,
    },
    /// Working tree plus `.git`, the always excluded left out.
    Size {
        unit: String,
        kind: UnitKind,
        always_exclude: Vec<String>,
        selected: bool,
    },
    /// The ignored directories present, with their size.
    Ignored {
        unit: String,
        kind: UnitKind,
        rules: Rules,
    },
    /// A unit the hub has and this host lacks.
    Create {
        unit: String,
        kind: UnitKind,
        rules: Rules,
    },
    Resolve {
        id: i64,
        unit: String,
        kind: UnitKind,
        path: String,
        blob: String,
    },
    Held {
        unit: String,
        kind: UnitKind,
        rules: Rules,
        action: HeldAction,
        local: Box<Option<Local>>,
    },
}

struct PassJob {
    unit: String,
    kind: UnitKind,
    rules: Rules,
    local: Option<Local>,
    /// Host → the snapshot of theirs this host last incorporated.
    merged: BTreeMap<String, String>,
    /// This host's commit as the hub last took it.
    pushed: Option<String>,
    open_conflicts: Vec<ConflictReport>,
    /// Nested units, relative to this one.
    children: Vec<String>,
    fetch: bool,
    apply: bool,
    /// May take the first snapshot although the hub holds others'.
    /// The owner said adopt (not merely: a snapshot was taken before).
    adopt_said: bool,
    /// An adoption where one side wins outright.
    prefer: Option<Side>,
    /// Why applies wait this time (a live tool call).
    defer_apply: Option<String>,
    /// Quiescent directories to write once free.
    lagging: Vec<String>,
}

#[derive(Default)]
struct PassResult {
    local: Option<Local>,
    pushed: Option<String>,
    merged: BTreeMap<String, String>,
    /// Hosts with a snapshot on the hub and their commits.
    remotes: BTreeMap<String, String>,
    state: Option<UnitState>,
    detail: Option<String>,
    conflicts: Vec<ConflictReport>,
    closed: Vec<String>,
    held: Option<HeldReport>,
    diverged: Vec<Diverged>,
    waiting: Vec<(String, String)>,
    nested: Vec<String>,
    oversize: Vec<String>,
    error: Option<String>,
    retry: Option<Duration>,
    unadopted: bool,
    /// The hub has no repository for the unit yet.
    seeding: bool,
    /// The unit's directory is gone from this host.
    missing: bool,
    /// Lagging directories written this pass.
    caught_up: Vec<String>,
    /// Directories that lagged behind an apply this pass.
    lagged: Vec<String>,
}

/// What creating a unit comes to.
enum Made {
    /// Fast-forwarded from the hub: the snapshot and the remotes merged.
    Done(Box<(Local, BTreeMap<String, String>)>),
    /// The directory held files already (a hand-copied unit): a repository
    /// was initialised, the owner adopts it.
    Adoptable,
}

/// What a created unit comes to, or why not.
type Created = Result<Made, String>;

// NOTE: the big variants are boxed; `Watch` still dwarfs `Resolve`
#[allow(clippy::large_enum_variant)]
enum Done {
    Pass {
        unit: String,
        result: Box<PassResult>,
    },
    Watch {
        unit: String,
        nested: Vec<String>,
        watched: usize,
    },
    Size {
        unit: String,
        kind: UnitKind,
        size: u64,
        selected: bool,
    },
    Ignored {
        unit: String,
        dirs: Vec<IgnoredDir>,
    },
    Create {
        unit: String,
        result: Box<Created>,
    },
    Resolve {
        id: i64,
        unit: String,
        path: String,
        error: Option<String>,
    },
    Held {
        unit: String,
        result: Result<Option<Local>, String>,
    },
}

/// The scheduler's picture of one unit.
struct Entry {
    kind: UnitKind,
    rules: Rules,
    excludes: Option<PathBuf>,
    present: bool,
    /// The hub lists it as travelling.
    included: bool,
    /// This host took a snapshot of it before.
    adopted: bool,
    /// The owner said adopt (the first snapshot although the hub holds others').
    adopt: bool,
    /// … and which side wins outright, if any.
    adopt_prefer: Option<Side>,
    local: Option<Local>,
    merged: BTreeMap<String, String>,
    remotes: BTreeMap<String, String>,
    pushed: Option<String>,
    conflicts: Vec<ConflictReport>,
    held: Option<HeldReport>,
    diverged: Vec<Diverged>,
    waiting: Vec<(String, String)>,
    ignored: Vec<IgnoredDir>,
    state: UnitState,
    detail: Option<String>,
    last_pass: Option<i64>,
    failing_since: Option<i64>,
    files: u64,
    /// First and last event of the current burst.
    burst: Option<(Instant, Instant)>,
    due_fetch: bool,
    due_apply: bool,
    running: bool,
    retry: Option<(Instant, Duration)>,
    /// The current failure's wait, doubling up to `MAX_BACKOFF`.
    backoff: Option<Duration>,
    /// The current failure was logged at warn level already.
    warned: bool,
    watched: bool,
    creating: bool,
    /// New directories waiting for the watch job.
    new_dirs: Vec<String>,
    /// What the owner chose for the held snapshot, run once the unit is idle.
    held_action: Option<HeldAction>,
    /// Quiescent directories whose working tree lags behind the snapshot
    /// (a process held them during an apply); written once free.
    lagging: BTreeSet<String>,
}

impl Entry {
    fn new(kind: UnitKind, rules: Rules, excludes: Option<PathBuf>) -> Self {
        Self {
            kind,
            rules,
            excludes,
            present: true,
            included: true,
            adopted: false,
            adopt: false,
            adopt_prefer: None,
            backoff: None,
            warned: false,
            local: None,
            merged: BTreeMap::new(),
            remotes: BTreeMap::new(),
            pushed: None,
            conflicts: Vec::new(),
            held: None,
            diverged: Vec::new(),
            waiting: Vec::new(),
            ignored: Vec::new(),
            state: UnitState::Seeding,
            detail: None,
            last_pass: None,
            failing_since: None,
            files: 0,
            burst: None,
            due_fetch: true,
            due_apply: true,
            running: false,
            retry: None,
            watched: false,
            creating: false,
            new_dirs: Vec::new(),
            held_action: None,
            lagging: BTreeSet::new(),
        }
    }

    fn status(&self, unit: &str) -> UnitStatus {
        UnitStatus {
            unit: unit.to_owned(),
            kind: self.kind,
            state: self.state,
            detail: self.detail.clone(),
            snapshot: self.local.as_ref().map(|l| l.commit.clone()),
            files: self.files,
            last_pass: self.last_pass,
            failing_since: self.failing_since,
            conflicts: self.conflicts.clone(),
            held: self.held.clone(),
            diverged: self.diverged.clone(),
            ignored: self.ignored.clone(),
        }
    }

    /// Whether a pass is due now, and when the next check is.
    fn due(&self, now: Instant) -> (bool, Option<Instant>) {
        if self.running || !self.present || !self.included || self.rules.paused || self.creating {
            return (false, None);
        }
        if self.held_action.is_some() {
            return (true, None);
        }
        if let Some((at, _)) = self.retry {
            return if now >= at {
                (true, None)
            } else {
                (false, Some(at))
            };
        }
        if let Some((first, last)) = self.burst {
            let settled =
                now.duration_since(last) >= QUIET || now.duration_since(first) >= BURST_MAX;
            return if settled {
                (true, None)
            } else {
                (false, Some((last + QUIET).min(first + BURST_MAX)))
            };
        }
        (self.due_fetch || self.due_apply, None)
    }
}

struct Scheduler {
    shared: Arc<Shared>,
    hub: UnboundedSender<ToHub>,
    db: Db,
    setup: WorkspaceSetup,
    /// The hub (or the cache) handed a setup over.
    setup_known: bool,
    units: BTreeMap<String, Entry>,
    /// Directories at unit depth found here, with their size once known.
    seen: BTreeMap<String, (UnitKind, Option<u64>, bool)>,
    jobs: Sender<Job>,
    in_flight: usize,
    connected: bool,
    changed: BTreeSet<String>,
    last_report: Instant,
    last_full: Instant,
    last_sweep: Instant,
    last_refresh: Instant,
    sweep_due: bool,
    discover_due: bool,
    started: i64,
    error: Option<String>,
    pending_sizes: usize,
    seen_dirty: bool,
    /// The collector config's starting configuration, offered to a hub
    /// without one.
    starting: WorkspaceConfig,
}

impl Scheduler {
    fn run(mut self, rx: &Receiver<Msg>) {
        if self.setup_known || !self.setup.units.is_empty() {
            self.setup_known = true;
        }
        loop {
            self.tick();
            let wait = self.next_wait();
            match rx.recv_timeout(wait) {
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(msg) = rx.try_recv() {
                self.handle(msg);
            }
        }
    }

    fn next_wait(&self) -> Duration {
        let now = Instant::now();
        let mut wait = Duration::from_secs(5);
        for e in self.units.values() {
            if let (_, Some(at)) = e.due(now) {
                wait = wait.min(at.saturating_duration_since(now));
            }
        }
        if !self.changed.is_empty() {
            wait = wait.min(REPORT_EVERY.saturating_sub(now.duration_since(self.last_report)));
        }
        wait.max(Duration::from_millis(10))
    }

    /// Timers, dispatch, reports.
    fn tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_sweep) >= SWEEP_EVERY {
            self.sweep_due = true;
            self.discover_due = true;
        }
        if now.duration_since(self.last_refresh) >= REFRESH_EVERY {
            self.last_refresh = now;
            for e in self.units.values_mut() {
                e.due_fetch = true;
            }
        }
        if self.discover_due {
            self.discover_due = false;
            self.discover();
        }
        if self.sweep_due && self.setup_known {
            self.sweep_due = false;
            self.last_sweep = now;
            for (id, e) in &mut self.units {
                e.due_fetch = true;
                e.due_apply = true;
                if e.burst.is_none() {
                    e.burst = Some((now - BURST_MAX, now - BURST_MAX));
                }
                if e.present && e.included {
                    let _ = self.jobs.send(Job::Ignored {
                        unit: id.clone(),
                        kind: e.kind,
                        rules: e.rules.clone(),
                    });
                }
            }
        }
        self.dispatch();
        if !self.changed.is_empty() && now.duration_since(self.last_report) >= REPORT_EVERY {
            self.report(false);
        }
        if now.duration_since(self.last_full) >= FULL_EVERY {
            self.report(true);
        }
        // NOTE: what is seen is decided by the hub's configuration: not before it has one
        let configured = self.setup.version > 0 || self.starting == WorkspaceConfig::default();
        if self.seen_dirty && self.pending_sizes == 0 && configured {
            self.seen_dirty = false;
            self.report_seen();
        }
    }

    fn dispatch(&mut self) {
        // NOTE: every pass talks to the hub; while the link is down 226 units
        // retrying made 2,000 DNS lookups a minute and pihole refused the host.
        // Due units wait, the reconnect marks them all due again.
        if !self.connected {
            return;
        }
        let now = Instant::now();
        let busy_now = now_ms();
        let due: Vec<String> = self
            .units
            .iter()
            .filter(|(_, e)| e.due(now).0)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            if self.in_flight >= WORKERS * 2 {
                break;
            }
            if let Some(e) = self.units.get_mut(&id)
                && let Some(action) = e.held_action.take()
            {
                info!("workspace {id}: {action:?} of the held snapshot");
                e.running = true;
                self.in_flight += 1;
                let _ = self.jobs.send(Job::Held {
                    unit: id.clone(),
                    kind: e.kind,
                    rules: e.rules.clone(),
                    action,
                    local: Box::new(e.local.clone()),
                });
                continue;
            }
            let children: Vec<String> = self
                .units
                .keys()
                .filter_map(|other| other.strip_prefix(&format!("{id}/")).map(str::to_owned))
                .collect();
            let Some(e) = self.units.get_mut(&id) else {
                continue;
            };
            let dir = self.shared.layout.unit_dir(&id);
            let defer_apply = self
                .shared
                .busy
                .covers(&dir, busy_now)
                .map(|s| format!("a tool call of session {s} is in flight"));
            let job = PassJob {
                unit: id.clone(),
                kind: e.kind,
                rules: e.rules.clone(),
                local: e.local.clone(),
                merged: e.merged.clone(),
                pushed: e.pushed.clone(),
                open_conflicts: e.conflicts.clone(),
                children,
                fetch: e.due_fetch || e.due_apply,
                apply: true,
                adopt_said: e.adopt,
                prefer: if e.adopt { e.adopt_prefer } else { None },
                defer_apply,
                lagging: e.lagging.iter().cloned().collect(),
            };
            e.running = true;
            e.burst = None;
            e.retry = None;
            e.due_fetch = false;
            e.due_apply = false;
            self.in_flight += 1;
            let _ = self.jobs.send(Job::Pass(Box::new(job)));
        }
    }

    fn handle(&mut self, msg: Msg) {
        let cmd = match msg {
            Msg::Cmd(cmd) => cmd,
            Msg::Watch(event) => return self.watched(event),
            Msg::Done(done) => return self.done(*done),
        };
        match cmd {
            Cmd::Setup(setup) => self.take_setup(setup),
            Cmd::Changed { unit, host } => {
                debug!("workspace: {host} pushed {unit}");
                match self.units.get_mut(&unit) {
                    Some(e) => {
                        e.due_fetch = true;
                        e.due_apply = true;
                        if !e.present && e.included && !e.creating {
                            e.retry = None;
                            self.discover_due = true;
                        }
                    }
                    None => self.discover_due = true,
                }
            }
            Cmd::Resync(unit) => match unit {
                Some(id) => {
                    if let Some(e) = self.units.get_mut(&id) {
                        e.due_fetch = true;
                        e.due_apply = true;
                        e.burst = Some((Instant::now() - BURST_MAX, Instant::now() - BURST_MAX));
                    }
                }
                None => {
                    self.sweep_due = true;
                    self.discover_due = true;
                }
            },
            Cmd::Resolve {
                id,
                unit,
                path,
                blob,
            } => match self.units.get(&unit) {
                Some(e) if e.present => {
                    let _ = self.jobs.send(Job::Resolve {
                        id,
                        unit,
                        kind: e.kind,
                        path,
                        blob,
                    });
                }
                _ => {
                    let _ = self.hub.send(ToHub::WorkspaceResolved {
                        id,
                        error: Some(format!("{unit} is not synced on this host")),
                    });
                }
            },
            Cmd::Held {
                unit,
                action,
                commit,
            } => match self.units.get_mut(&unit) {
                Some(e) if e.held.is_some() => {
                    let current = e.held.as_ref().map(|h| h.commit.clone());
                    if commit.is_some() && commit != current {
                        warn!(
                            "workspace {unit}: the held snapshot moved on since the panel showed it; choose again"
                        );
                    } else {
                        // NOTE: run between passes, never under one
                        e.held_action = Some(action);
                    }
                }
                Some(_) => warn!("workspace {unit}: nothing is held here"),
                None => warn!("workspace {unit}: not a unit here"),
            },
            Cmd::Adopt { unit, prefer } => {
                for (id, e) in &mut self.units {
                    if unit.as_ref().is_none_or(|u| u == id) && e.state == UnitState::Unadopted {
                        e.adopt = true;
                        e.adopt_prefer = prefer;
                        // NOTE: a restart in the middle (a deploy) must not lose the request
                        if let Err(err) = self.db.save(id, e) {
                            warn!("workspace db: {err:#}");
                        }
                        e.due_fetch = true;
                        e.due_apply = true;
                        e.retry = None;
                    }
                }
            }
            Cmd::Init => {
                for e in self.units.values_mut() {
                    e.retry = None;
                }
                self.discover_due = true;
            }
            Cmd::Status(reply) => {
                let _ = reply.send(self.status_json());
            }
            Cmd::Connected(up) => {
                self.connected = up;
                if up {
                    self.report(true);
                    self.seen_dirty = true;
                    for e in self.units.values_mut() {
                        e.due_fetch = true;
                        e.due_apply = true;
                    }
                }
            }
        }
    }

    fn watched(&mut self, event: Event) {
        match event {
            Event::Touched(unit) => {
                if let Some(e) = self.units.get_mut(&unit) {
                    let now = Instant::now();
                    e.burst = Some(match e.burst {
                        Some((first, _)) => (first, now),
                        None => (now, now),
                    });
                }
            }
            Event::NewDir { unit, rel } => {
                if let Some(e) = self.units.get_mut(&unit) {
                    e.new_dirs.push(rel);
                    if !e.running {
                        let dirs = std::mem::take(&mut e.new_dirs);
                        let _ = self.jobs.send(Job::Watch {
                            unit,
                            kind: e.kind,
                            rules: e.rules.clone(),
                            new_dirs: dirs,
                        });
                    }
                }
            }
            Event::Layout => self.discover_due = true,
            Event::Overflow => {
                warn!("workspace: inotify queue overflowed; sweeping");
                self.sweep_due = true;
            }
            Event::Stopped => {
                self.error = Some("the file watcher stopped; sweeping every 30 min".into());
                self.sweep_due = true;
            }
        }
    }

    fn take_setup(&mut self, setup: WorkspaceSetup) {
        if setup.version == 0 && self.starting != WorkspaceConfig::default() {
            info!("workspace: the hub has no configuration yet; offering this machine's");
            let _ = self.hub.send(ToHub::WorkspaceConfig {
                config: self.starting.clone(),
            });
        }
        let fresh = setup.version != self.setup.version || !self.setup_known;
        self.setup = setup;
        self.setup_known = true;
        write_setup(&self.shared.layout.state, &self.setup);
        if let Err(e) = Git::prepare(
            &self.shared.layout.state,
            &self.shared.host,
            &self.setup.config.always_exclude,
            Some(self.shared.hub_base.clone()),
            Some(self.shared.git.token_file.clone().unwrap_or_default()),
        ) {
            warn!("workspace: rewriting the excludes file: {e:#}");
        }
        // units that left the setup
        let gone: Vec<String> = self
            .units
            .keys()
            .filter(|id| !self.setup.units.contains_key(*id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(reg) = &self.shared.registry {
                reg.forget(&id);
            }
            self.units.remove(&id);
            self.changed.insert(id);
        }
        for (id, e) in &mut self.units {
            let rules = self.setup.config.rules(id);
            if rules != e.rules {
                e.excludes = self.shared.excludes(id, &rules);
                e.rules = rules;
                e.due_fetch = true;
                e.due_apply = true;
                e.burst = Some((Instant::now() - BURST_MAX, Instant::now() - BURST_MAX));
                if e.rules.paused {
                    e.state = UnitState::Paused;
                    e.detail = None;
                    self.changed.insert(id.clone());
                }
            }
        }
        if fresh {
            info!(
                "workspace: setup v{} with {} units",
                self.setup.version,
                self.setup.units.len()
            );
            self.discover_due = true;
        }
    }

    /// Lists the directories at unit depth, starts the units the setup
    /// includes, measures the new ones.
    fn discover(&mut self) {
        let found = units::discover(&self.shared.layout.root, self.setup.config.depth);
        let mut present: BTreeSet<String> = BTreeSet::new();
        for f in &found {
            present.insert(f.id.clone());
            let selected = self.setup.config.selects(&f.id);
            let entry = self
                .seen
                .entry(f.id.clone())
                .or_insert((f.kind, None, selected));
            entry.0 = f.kind;
            entry.2 = selected;
            if self.setup_known && !self.setup.units.contains_key(&f.id) && entry.1.is_none() {
                self.pending_sizes += 1;
                self.seen_dirty = true;
                let _ = self.jobs.send(Job::Size {
                    unit: f.id.clone(),
                    kind: f.kind,
                    always_exclude: self.setup.config.rules(&f.id).always_exclude,
                    selected,
                });
            }
        }
        // nested units the snapshots found live in `seen` too
        for id in self.seen.keys().cloned().collect::<Vec<_>>() {
            if self.shared.layout.unit_dir(&id).join(".git").exists() {
                present.insert(id);
            }
        }
        if !self.setup_known {
            return;
        }
        let included: Vec<(String, UnitKind)> = self
            .setup
            .units
            .iter()
            .map(|(id, k)| (id.clone(), *k))
            .collect();
        for (id, kind) in included {
            // NOTE: a nested unit is below unit depth: ask the filesystem
            let dir = self.shared.layout.unit_dir(&id);
            let is_here = present.contains(&id)
                || dir.join(".git").exists()
                || (kind == UnitKind::Plain && dir.is_dir());
            let local_kind = self.seen.get(&id).map_or(kind, |(k, _, _)| *k);
            let kind = if is_here { local_kind } else { kind };
            if !self.units.contains_key(&id) {
                let rules = self.setup.config.rules(&id);
                let excludes = self.shared.excludes(&id, &rules);
                let mut e = Entry::new(kind, rules, excludes);
                if let Ok(saved) = self.db.load(&id) {
                    e.adopted = saved.adopted;
                    e.merged = saved.merged;
                    e.pushed = saved.pushed;
                    e.conflicts = saved.conflicts;
                    e.held = saved.held;
                    e.files = saved.tracked;
                    e.lagging = saved.lagging;
                    if let Some(prefer) = saved.adopt {
                        e.adopt = true;
                        e.adopt_prefer = prefer;
                    }
                }
                e.present = is_here;
                if e.rules.paused {
                    e.state = UnitState::Paused;
                } else if !is_here {
                    e.state = UnitState::Creating;
                }
                self.units.insert(id.clone(), e);
                self.changed.insert(id.clone());
            }
            let Some(e) = self.units.get_mut(&id) else {
                continue;
            };
            e.included = true;
            if e.present != is_here {
                e.present = is_here;
                e.state = if is_here {
                    UnitState::Seeding
                } else {
                    UnitState::Missing
                };
                e.detail = None;
                self.changed.insert(id.clone());
            }
            if is_here && !e.watched && !e.rules.paused {
                e.watched = true;
                let _ = self.jobs.send(Job::Watch {
                    unit: id.clone(),
                    kind: e.kind,
                    rules: e.rules.clone(),
                    new_dirs: Vec::new(),
                });
            }
            let retry_passed = e.retry.is_none_or(|(at, _)| Instant::now() >= at);
            if !is_here && !e.creating && !e.rules.paused && retry_passed {
                let (kind, rules) = (e.kind, e.rules.clone());
                if self.may_create(&id) {
                    if let Some(e) = self.units.get_mut(&id) {
                        e.creating = true;
                    }
                    let _ = self.jobs.send(Job::Create {
                        unit: id.clone(),
                        kind,
                        rules,
                    });
                }
            }
        }
    }

    fn done(&mut self, done: Done) {
        match done {
            Done::Pass { unit, result } => {
                let result = *result;
                self.in_flight = self.in_flight.saturating_sub(1);
                let Some(e) = self.units.get_mut(&unit) else {
                    return;
                };
                e.running = false;
                e.last_pass = Some(now_ms());
                if result.missing {
                    e.present = false;
                    e.watched = false;
                    e.state = UnitState::Missing;
                    e.detail = result.detail;
                    self.discover_due = true;
                    self.changed.insert(unit);
                    return;
                }
                if let Some(local) = result.local {
                    e.files = local.tracked;
                    e.local = Some(local);
                    e.adopted = true;
                    if result.error.is_none() {
                        e.adopt = false;
                        e.adopt_prefer = None;
                    }
                }
                if let Some(pushed) = result.pushed {
                    e.pushed = Some(pushed);
                }
                e.merged.extend(result.merged);
                e.remotes = result.remotes;
                for path in &result.closed {
                    e.conflicts.retain(|c| &c.path != path);
                }
                for c in result.conflicts {
                    e.conflicts.retain(|o| o.path != c.path);
                    e.conflicts.push(c);
                }
                for d in result.diverged {
                    e.diverged
                        .retain(|o| o.branch != d.branch || o.host != d.host);
                    e.diverged.push(d);
                }
                e.waiting = result.waiting;
                for p in &result.caught_up {
                    e.lagging.remove(p);
                }
                e.lagging.extend(result.lagged.iter().cloned());
                e.held = result.held.clone().or(e.held.take());
                for rel in &result.nested {
                    self.note_nested(format!("{unit}/{rel}"));
                }
                if let Some(e2) = self.units.get_mut(&unit) {
                    match &result.error {
                        Some(error) => {
                            let changed = e2.detail.as_deref() != Some(error.as_str());
                            e2.state = UnitState::Failed;
                            e2.detail = Some(error.clone());
                            e2.failing_since.get_or_insert(now_ms());
                            // NOTE: `retry` is taken at dispatch; the wait lives here
                            let backoff = e2
                                .backoff
                                .map_or(Duration::from_secs(2), |b| (b * 2).min(MAX_BACKOFF));
                            e2.backoff = Some(backoff);
                            e2.retry = Some((Instant::now() + backoff, backoff));
                            let lasting = e2.failing_since.is_some_and(|s| now_ms() - s > 60_000);
                            if lasting && (changed || !e2.warned) {
                                warn!("workspace {unit}: {error}");
                                e2.warned = true;
                            } else {
                                debug!("workspace {unit}: {error}");
                            }
                        }
                        None => {
                            e2.failing_since = None;
                            e2.backoff = None;
                            e2.warned = false;
                            e2.state = result.state.unwrap_or(UnitState::InStep);
                            e2.detail = result.detail.clone();
                            e2.retry = result.retry.map(|d| (Instant::now() + d, d));
                            if result.unadopted {
                                e2.state = UnitState::Unadopted;
                            }
                            if !e2.conflicts.is_empty() && e2.state == UnitState::InStep {
                                e2.state = UnitState::Conflict;
                            }
                            if !e2.lagging.is_empty() && e2.retry.is_none() {
                                e2.retry = Some((Instant::now() + LAG_RETRY, LAG_RETRY));
                            }
                            if !e2.waiting.is_empty() && e2.state == UnitState::InStep {
                                e2.state = UnitState::Waiting;
                                e2.detail = Some(
                                    e2.waiting
                                        .iter()
                                        .map(|(p, proc_)| format!("{proc_} holds {p} open"))
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                );
                            }
                            if e2.held.is_some() {
                                e2.state = UnitState::Held;
                            }
                            if !result.oversize.is_empty() {
                                let list = result.oversize.join(", ");
                                e2.detail = Some(match e2.detail.take() {
                                    Some(d) => format!("{d}; oversize left out: {list}"),
                                    None => format!("oversize left out: {list}"),
                                });
                            }
                        }
                    }
                    if !e2.new_dirs.is_empty() {
                        let dirs = std::mem::take(&mut e2.new_dirs);
                        let _ = self.jobs.send(Job::Watch {
                            unit: unit.clone(),
                            kind: e2.kind,
                            rules: e2.rules.clone(),
                            new_dirs: dirs,
                        });
                    }
                    if let Err(err) = self.db.save(&unit, e2) {
                        warn!("workspace db: {err:#}");
                    }
                }
                self.changed.insert(unit);
            }
            Done::Watch {
                unit,
                nested,
                watched,
            } => {
                debug!("workspace: {unit}: {watched} directories watched");
                for rel in nested {
                    self.note_nested(format!("{unit}/{rel}"));
                }
            }
            Done::Size {
                unit,
                kind,
                size,
                selected,
            } => {
                self.pending_sizes = self.pending_sizes.saturating_sub(1);
                self.seen.insert(unit, (kind, Some(size), selected));
                self.seen_dirty = true;
            }
            Done::Ignored { unit, dirs } => {
                if let Some(e) = self.units.get_mut(&unit)
                    && e.ignored != dirs
                {
                    e.ignored = dirs;
                    self.changed.insert(unit);
                }
            }
            Done::Create { unit, result } => {
                let Some(e) = self.units.get_mut(&unit) else {
                    return;
                };
                e.creating = false;
                match *result {
                    Ok(Made::Done(done)) => {
                        let (local, merged) = *done;
                        info!("workspace: created {unit} from the hub");
                        // NOTE: units nested under this one may be created now
                        self.discover_due = true;
                        e.present = true;
                        e.adopted = true;
                        e.files = local.tracked;
                        e.local = Some(local);
                        e.merged = merged;
                        e.pushed = None;
                        e.state = UnitState::Behind;
                        e.detail = None;
                        e.due_fetch = true;
                        e.due_apply = true;
                        e.watched = true;
                        let _ = self.jobs.send(Job::Watch {
                            unit: unit.clone(),
                            kind: e.kind,
                            rules: e.rules.clone(),
                            new_dirs: Vec::new(),
                        });
                        if let Err(err) = self.db.save(&unit, e) {
                            warn!("workspace db: {err:#}");
                        }
                    }
                    Ok(Made::Adoptable) => {
                        info!("workspace: {unit} exists here as a copy: adopt it");
                        self.discover_due = true;
                        e.present = true;
                        e.state = UnitState::Unadopted;
                        e.detail = Some(
                            "exists here as a copy without this sync's history: run `ccdash workspace adopt`"
                                .into(),
                        );
                        e.due_fetch = true;
                        e.due_apply = true;
                    }
                    Err(error) => {
                        // the seeding host's first push has not landed yet
                        if error.contains("no such unit") || error.contains("not found") {
                            debug!("workspace: creating {unit}: {error}");
                        } else {
                            warn!("workspace: creating {unit}: {error}");
                        }
                        e.state = UnitState::Missing;
                        e.detail = Some(error);
                        e.retry = Some((
                            Instant::now() + Duration::from_secs(30),
                            Duration::from_secs(30),
                        ));
                    }
                }
                self.changed.insert(unit);
            }
            Done::Resolve {
                id,
                unit,
                path,
                error,
            } => {
                if error.is_none()
                    && let Some(e) = self.units.get_mut(&unit)
                {
                    // NOTE: a binary conflict has no markers to notice; the choice closes it
                    e.conflicts.retain(|c| c.path != path);
                    if e.conflicts.is_empty() && e.state == UnitState::Conflict {
                        e.state = UnitState::InStep;
                    }
                    e.burst = Some((Instant::now(), Instant::now()));
                    let _ = self.db.save(&unit, e);
                    self.changed.insert(unit.clone());
                }
                let _ = self.hub.send(ToHub::WorkspaceResolved { id, error });
            }
            Done::Held { unit, result } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                let Some(e) = self.units.get_mut(&unit) else {
                    return;
                };
                e.running = false;
                match &result {
                    Ok(_) => info!("workspace {unit}: the held snapshot is settled"),
                    Err(error) => warn!("workspace {unit}: held snapshot: {error}"),
                }
                match result {
                    Ok(local) => {
                        e.held = None;
                        if let Some(local) = local {
                            e.files = local.tracked;
                            e.local = Some(local);
                            e.pushed = None;
                        }
                        e.state = UnitState::Ahead;
                        e.detail = None;
                        e.due_fetch = true;
                        e.due_apply = true;
                        let _ = self.db.save(&unit, e);
                    }
                    Err(error) => {
                        e.detail = Some(error);
                    }
                }
                self.changed.insert(unit);
            }
        }
    }

    /// Whether a unit the hub has and this host lacks may be created now:
    /// not while a unit above it is being created (the parent's checkout
    /// and the child's creation would race for the path).
    fn may_create(&self, id: &str) -> bool {
        !self
            .units
            .iter()
            .any(|(parent, e)| id.starts_with(&format!("{parent}/")) && e.creating)
    }

    /// A nested repository a snapshot or a watch found: a candidate unit,
    /// measured and reported like one found at unit depth.
    fn note_nested(&mut self, id: String) {
        if let std::collections::btree_map::Entry::Vacant(v) = self.seen.entry(id.clone()) {
            let selected = self.setup.config.selects(&id);
            v.insert((UnitKind::Git, None, selected));
            self.discover_due = true;
            if self.setup_known && !self.setup.units.contains_key(&id) {
                self.pending_sizes += 1;
                self.seen_dirty = true;
                let _ = self.jobs.send(Job::Size {
                    always_exclude: self.setup.config.rules(&id).always_exclude,
                    unit: id,
                    kind: UnitKind::Git,
                    selected,
                });
            }
        }
    }

    fn status_json(&self) -> Value {
        let units: Vec<UnitStatus> = self.units.iter().map(|(id, e)| e.status(id)).collect();
        json!({
            "host": self.shared.host,
            "root": self.shared.layout.root,
            "setup_version": self.setup.version,
            "connected": self.connected,
            "status": self.overall(),
            "units": units,
            "new": self.seen.iter().filter(|(id, _)| !self.units.contains_key(*id)).map(|(id, (kind, size, selected))| json!({"unit": id, "kind": kind, "size": size, "selected": selected})).collect::<Vec<_>>(),
        })
    }

    fn overall(&self) -> WorkspaceStatus {
        let in_step = self
            .units
            .values()
            .filter(|e| e.state == UnitState::InStep)
            .count() as u64;
        let pending = self
            .units
            .values()
            .filter(|e| {
                matches!(
                    e.state,
                    UnitState::Ahead | UnitState::Behind | UnitState::Merging
                )
            })
            .count() as u64;
        let total = self.units.values().filter(|e| e.included).count() as u64;
        let busy = self
            .units
            .values()
            .filter(|e| matches!(e.state, UnitState::Seeding | UnitState::Creating) || e.creating)
            .count() as u64;
        WorkspaceStatus {
            root: self.shared.layout.root.to_string_lossy().into_owned(),
            setup_version: self.setup.version,
            watching: self.shared.registry.is_some(),
            units: total,
            in_step,
            pending,
            error: self.error.clone(),
            progress: (busy > 0 && now_ms() - self.started < 3_600_000)
                .then(|| (total - busy, total)),
            last_pass: self.units.values().filter_map(|e| e.last_pass).max(),
        }
    }

    fn report(&mut self, full: bool) {
        let ids: Vec<String> = if full {
            self.units.keys().cloned().collect()
        } else {
            self.changed.iter().cloned().collect()
        };
        let units: Vec<UnitStatus> = ids
            .iter()
            .filter_map(|id| self.units.get(id).map(|e| e.status(id)))
            .collect();
        let _ = self.hub.send(ToHub::Workspace {
            status: self.overall(),
            units,
            full,
        });
        self.changed.clear();
        self.last_report = Instant::now();
        if full {
            self.last_full = Instant::now();
        }
    }

    fn report_seen(&self) {
        let units: Vec<SeenUnit> = self
            .seen
            .iter()
            .map(|(id, (kind, size, selected))| SeenUnit {
                unit: id.clone(),
                kind: *kind,
                size: size.unwrap_or(0),
                selected: *selected,
            })
            .collect();
        let _ = self.hub.send(ToHub::WorkspaceUnits { units });
    }
}

/// Runs jobs until the queue closes.
fn worker(shared: &Shared, rx: &Arc<Mutex<Receiver<Job>>>, tx: &Sender<Msg>) {
    loop {
        let job = {
            let Ok(guard) = rx.lock() else {
                return;
            };
            guard.recv()
        };
        let Ok(job) = job else {
            return;
        };
        let done = match job {
            Job::Pass(job) => {
                let unit = job.unit.clone();
                let result = Box::new(pass(shared, &job));
                Done::Pass { unit, result }
            }
            Job::Watch {
                unit,
                kind,
                rules,
                new_dirs,
            } => {
                let (nested, watched) = watch_unit(shared, &unit, kind, &rules, &new_dirs);
                Done::Watch {
                    unit,
                    nested,
                    watched,
                }
            }
            Job::Size {
                unit,
                kind,
                always_exclude,
                selected,
            } => {
                // NOTE: a directory being copied in is measured once it stops growing
                let dir = shared.layout.unit_dir(&unit);
                let mut bytes = units::size_of(&dir, &always_exclude).1;
                for _ in 0..10 {
                    thread::sleep(Duration::from_secs(2));
                    let again = units::size_of(&dir, &always_exclude).1;
                    if again == bytes {
                        break;
                    }
                    bytes = again;
                }
                Done::Size {
                    unit,
                    kind,
                    size: bytes,
                    selected,
                }
            }
            Job::Ignored { unit, kind, rules } => Done::Ignored {
                dirs: ignored_dirs(shared, &unit, kind, &rules),
                unit,
            },
            Job::Create { unit, kind, rules } => {
                let result =
                    Box::new(create(shared, &unit, kind, &rules).map_err(|e| format!("{e:#}")));
                Done::Create { unit, result }
            }
            Job::Resolve {
                id,
                unit,
                kind,
                path,
                blob,
            } => {
                let error = shared
                    .layout
                    .repo(&unit, kind)
                    .and_then(|repo| apply::resolve(&shared.git, &repo, &path, &blob))
                    .err()
                    .map(|e| format!("{e:#}"));
                Done::Resolve {
                    id,
                    unit,
                    path,
                    error,
                }
            }
            Job::Held {
                unit,
                kind,
                rules,
                action,
                local,
            } => {
                let result = held(shared, &unit, kind, &rules, action, (*local).as_ref())
                    .map_err(|e| format!("{e:#}"));
                Done::Held { unit, result }
            }
        };
        if tx.send(Msg::Done(Box::new(done))).is_err() {
            return;
        }
    }
}

/// Hosts with a snapshot in the repository and their commits.
fn remotes(shared: &Shared, repo: &Repo) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (name, sha) in shared.git.refs(repo, &format!("{SYNC_REFS}/"))? {
        let Some(host) = name.strip_prefix(&format!("{SYNC_REFS}/")) else {
            continue;
        };
        if host == shared.host || host.contains('/') {
            continue;
        }
        out.insert(host.to_owned(), sha);
    }
    Ok(out)
}

fn fetch_refspecs(shared: &Shared) -> Vec<String> {
    vec![
        format!("+{SYNC_REFS}/*:{SYNC_REFS}/*"),
        format!("^{SYNC_REFS}/{}", shared.host),
        format!("^{SYNC_REFS}/diverged/*"),
        format!("^{SYNC_REFS}/held/*"),
        format!("+{TIPS_REFS}/*:{TIPS_REFS}/*"),
        format!("^{TIPS_REFS}/{}/*", shared.host),
    ]
}

fn push_refspecs(shared: &Shared, local: &Local) -> Vec<String> {
    let me = &shared.host;
    let mut specs = vec![
        format!("+{SYNC_REFS}/{me}:{SYNC_REFS}/{me}"),
        format!("+refs/heads/*:{TIPS_REFS}/{me}/heads/*"),
        format!("+refs/tags/*:{TIPS_REFS}/{me}/tags/*"),
    ];
    for (n, sha) in &local.meta.stash {
        specs.push(format!("+{sha}:{TIPS_REFS}/{me}/stash/{n}"));
    }
    specs
}

/// Whether a fetch failed because the hub has no repository for the unit.
fn no_repo(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    text.contains("not found") || text.contains("404") || text.contains("no such unit")
}

/// Processes holding files open, for the quiescent rule; read once per
/// pass when needed.
struct OpenFiles(Vec<(PathBuf, String)>);

impl OpenFiles {
    fn scan() -> Self {
        let me = std::process::id();
        let mut out = Vec::new();
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if pid == me {
                continue;
            }
            let comm = fs::read_to_string(entry.path().join("comm"))
                .map(|c| c.trim().to_owned())
                .unwrap_or_default();
            for fd in fs::read_dir(entry.path().join("fd"))
                .into_iter()
                .flatten()
                .flatten()
            {
                if let Ok(target) = fs::read_link(fd.path())
                    && target.is_absolute()
                {
                    out.push((target, comm.clone()));
                }
            }
        }
        Self(out)
    }

    fn holder(&self, dir: &Path) -> Option<String> {
        self.0
            .iter()
            .find(|(p, _)| p.starts_with(dir))
            .map(|(_, comm)| comm.clone())
    }
}

/// One pass over a unit.
fn pass(shared: &Shared, job: &PassJob) -> PassResult {
    let mut res = PassResult {
        merged: job.merged.clone(),
        pushed: job.pushed.clone(),
        ..PassResult::default()
    };
    match run_pass(shared, job, &mut res) {
        Ok(()) => {}
        Err(e) => {
            res.error = Some(format!("{e:#}"));
        }
    }
    res
}

fn run_pass(shared: &Shared, job: &PassJob, res: &mut PassResult) -> Result<()> {
    if !shared.layout.unit_dir(&job.unit).is_dir() {
        res.missing = true;
        res.state = Some(UnitState::Missing);
        res.detail = Some("the directory is not here".into());
        return Ok(());
    }
    let repo = match job.kind {
        UnitKind::Plain => shared.layout.ensure_hidden(&shared.git, &job.unit)?,
        UnitKind::Git => shared.layout.repo(&job.unit, job.kind)?,
    };
    let scratch = shared.layout.scratch(&job.unit);
    fs::create_dir_all(&scratch)?;
    let excludes = shared.excludes(&job.unit, &job.rules);
    let url = shared.url(&job.unit);
    let mut local = match &job.local {
        Some(l) => Some(l.clone()),
        None => apply::own_snapshot(&shared.git, &repo, &shared.host)?,
    };
    let needs_open_files = job
        .rules
        .sync_ignored
        .iter()
        .any(|e| e.quiescent && !snapshot::expand(&repo.work_tree, &e.pattern).is_empty());
    let open = needs_open_files.then(OpenFiles::scan);
    let busy = |dir: &Path| open.as_ref().and_then(|o| o.holder(dir));

    if job.fetch {
        match shared.git.fetch(&repo, &url, &fetch_refspecs(shared)) {
            Ok(()) => {}
            Err(e) if no_repo(&e) => res.seeding = true,
            Err(e) => {
                res.retry = Some(Duration::from_secs(5));
                return Err(e).context("fetching from the hub");
            }
        }
    }
    let mut remotes_now = remotes(shared, &repo)?;
    let mut from_hub = false;
    if !job.adopt_said && !remotes_now.is_empty() {
        // a copy the hub never saw: no snapshot yet, or a first snapshot
        // (one not built on another snapshot) whose push never landed
        let unseen = match &local {
            None => true,
            // NOTE: a unit created from the hub holds the hub's own commit before
            // its first push: that one is known there, not an unseen copy
            Some(l) => {
                let mut unseen = job.pushed.is_none();
                if unseen {
                    // known there when it is a tip or behind one
                    for sha in remotes_now.values() {
                        if sha == &l.commit
                            || shared.git.merge_base(&repo, &l.commit, sha)?.as_deref()
                                == Some(l.commit.as_str())
                        {
                            unseen = false;
                            break;
                        }
                    }
                }
                unseen && first_snapshot(&shared.git, &repo, &l.commit)?
            }
        };
        if !unseen {
            // the hub knows this copy
        } else if local.is_none() && !shared.layout.has_files(&job.unit) {
            // NOTE: nothing here to adopt (an empty directory or repository):
            // the hub's snapshot becomes the first content, as on a new machine
            from_hub = true;
        } else {
            res.unadopted = true;
            res.local = local.clone();
            res.remotes = remotes_now;
            res.detail = Some(format!(
                "exists here and on the hub ({}): run `ccdash workspace adopt`",
                res.remotes.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
            return Ok(());
        }
    }

    if !job.lagging.is_empty()
        && let Some(l) = local.as_ref()
    {
        match apply::catch_up(&shared.git, &repo, &scratch, l, &job.lagging, &busy) {
            Ok(done) => {
                if !done.is_empty() {
                    info!("workspace {}: {} caught up", job.unit, done.join(", "));
                }
                res.caught_up = done;
            }
            Err(e) => debug!("workspace {}: {e:#}", job.unit),
        }
    }
    let mut children = job.children.clone();
    let mut pushed = res.pushed.clone();
    // NOTE: an adoption with a preference pushes its merge first, never the
    // raw first snapshot: another host would merge that one its own way
    let mut hold_push = job.prefer.is_some() && !remotes_now.is_empty();
    for round in 0..6 {
        if from_hub && local.is_none() {
            // straight to the apply: the hub's snapshot is written as the first content
        } else {
            let report = snapshot::take(&snapshot::Take {
                git: &shared.git,
                repo: &repo,
                scratch: &scratch,
                excludes: excludes.as_deref(),
                rules: &job.rules,
                host: &shared.host,
                children: &children,
                previous: local.as_ref(),
                busy: &busy,
            })?;
            res.waiting = report.waiting.clone();
            res.oversize = report.oversize.clone();
            for n in &report.nested {
                if !children.contains(n) {
                    children.push(n.clone());
                    res.nested.push(n.clone());
                }
            }
            match report.outcome {
                Snap::Skipped(reason) => {
                    res.state = Some(UnitState::Skipped);
                    res.detail = Some(reason);
                    res.retry = Some(SOON);
                    break;
                }
                Snap::Held(h) => {
                    res.held = Some(h);
                    res.state = Some(UnitState::Held);
                    break;
                }
                Snap::Taken(l) => local = Some(l),
                Snap::Unchanged => {}
            }
        }
        if let Some(l) = local.as_ref()
            && pushed.as_deref() != Some(l.commit.as_str())
            && !hold_push
        {
            let mut attempt = shared
                .git
                .push(&repo, &url, &push_refspecs(shared, l), true);
            // NOTE: a shallow clone's first push to a fresh repository loses its branch
            // tips with "remote failed to report status"; the second push lands them
            if attempt
                .as_ref()
                .is_err_and(|e| format!("{e:#}").contains("remote failure"))
            {
                attempt = shared
                    .git
                    .push(&repo, &url, &push_refspecs(shared, l), true);
            }
            match attempt {
                Ok(Ok(())) => {
                    pushed = Some(l.commit.clone());
                    res.seeding = false;
                }
                Ok(Err(refused)) => {
                    // NOTE: the hub's compare-and-swap: another host moved; fetch and merge again
                    debug!("workspace {}: {}", job.unit, refused.message);
                    if round == 5 {
                        bail!("the hub kept refusing the merge: {}", refused.message);
                    }
                    shared.git.fetch(&repo, &url, &fetch_refspecs(shared))?;
                    remotes_now = remotes(shared, &repo)?;
                    for (host, sha) in &remotes_now {
                        if let Some(tip) = refused.tips.get(host)
                            && tip != sha
                        {
                            debug!("workspace {}: {host} is at {tip} now", job.unit);
                        }
                    }
                    // NOTE: a merge commit naming a tip that moved on is refused for
                    // ever; when it added nothing to our side, our side alone goes
                    if let Some(first) = merge_of_moved_tips(&shared.git, &repo, l, &remotes_now)? {
                        debug!(
                            "workspace {}: dropping the merge {}, back to {}",
                            job.unit,
                            &l.commit[..8],
                            &first[..8]
                        );
                        shared.git.update_ref(
                            &repo,
                            &format!("{SYNC_REFS}/{}", shared.host),
                            &first,
                        )?;
                        local = apply::own_snapshot(&shared.git, &repo, &shared.host)?;
                        pushed = None;
                    }
                }
                Err(e) => {
                    res.state = Some(UnitState::Ahead);
                    res.retry = Some(Duration::from_secs(5));
                    res.local = local;
                    return Err(e).context("pushing to the hub");
                }
            }
        }
        if !job.apply {
            break;
        }
        let mut applied = false;
        for (host, sha) in &remotes_now {
            if res.merged.get(host) == Some(sha) {
                continue;
            }
            if let Some(reason) = &job.defer_apply {
                res.state = Some(UnitState::Skipped);
                res.detail = Some(reason.clone());
                res.retry = Some(SOON);
                break;
            }
            let report = apply::apply(&apply::Apply {
                git: &shared.git,
                repo: &repo,
                scratch: &scratch,
                excludes: excludes.as_deref(),
                rules: &job.rules,
                host: &shared.host,
                local: local.as_ref(),
                other: host,
                remote: sha,
                busy: &busy,
                prefer: job.prefer,
            })?;
            res.waiting = report.waiting.clone();
            hold_push = false;
            match report.outcome {
                apply::Outcome::Ahead => {
                    res.merged.insert(host.clone(), sha.clone());
                }
                apply::Outcome::FastForward { commit } | apply::Outcome::Merged { commit } => {
                    info!(
                        "workspace {}: applied {} from {host} ({} files{})",
                        job.unit,
                        &sha[..8.min(sha.len())],
                        report.files,
                        if report.conflicts.is_empty() {
                            String::new()
                        } else {
                            format!(", {} conflicts", report.conflicts.len())
                        }
                    );
                    res.merged.insert(host.clone(), sha.clone());
                    res.conflicts.extend(report.conflicts);
                    res.diverged.extend(report.diverged);
                    res.lagged
                        .extend(report.waiting.iter().map(|(p, _)| p.clone()));
                    for note in report.notes {
                        debug!("workspace {}: {note}", job.unit);
                    }
                    local = apply::own_snapshot(&shared.git, &repo, &shared.host)?
                        .or_else(|| local.clone());
                    if let Some(l) = &mut local {
                        l.commit = commit;
                    }
                    pushed = None;
                    applied = true;
                    break;
                }
                apply::Outcome::Retry(reason) => {
                    res.state = Some(UnitState::Skipped);
                    res.detail = Some(reason);
                    res.retry = Some(SOON);
                    break;
                }
            }
        }
        if !applied {
            break;
        }
    }
    res.local = local.clone();
    res.pushed = pushed.clone();
    res.remotes = remotes_now.clone();

    // conflicts whose markers are gone are over; a binary one once the file
    // is no longer the kept version
    for c in &job.open_conflicts {
        let over = if c.kind == "binary" {
            let now = shared.git.run(
                Some(&repo),
                &["hash-object", "--", &c.path],
                &Opts::default(),
            )?;
            !now.ok || now.text() != c.ours
        } else {
            !apply::has_markers(&repo.work_tree.join(&c.path))
        };
        if over {
            res.closed.push(c.path.clone());
        }
    }
    if res.state.is_none() {
        let behind = remotes_now
            .iter()
            .any(|(h, sha)| res.merged.get(h) != Some(sha));
        res.state = Some(if res.seeding {
            UnitState::Seeding
        } else if local
            .as_ref()
            .is_some_and(|l| pushed.as_deref() != Some(l.commit.as_str()))
        {
            UnitState::Ahead
        } else if behind {
            UnitState::Behind
        } else {
            UnitState::InStep
        });
        if res.seeding {
            res.detail = Some("the hub has no repository for it yet".into());
            res.retry = Some(Duration::from_secs(60));
        }
    }
    Ok(())
}

/// For a merge commit whose extra parents are no host's current tip and
/// whose tree is its first parent's (the merge added nothing of ours):
/// that first parent, to fall back to.
fn merge_of_moved_tips(
    git: &Git,
    repo: &Repo,
    local: &Local,
    remotes: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    let line = git
        .ok(
            Some(repo),
            &["rev-list", "--parents", "-n", "1", &local.commit],
            &Opts::default(),
        )?
        .text();
    let parents: Vec<&str> = line.split_whitespace().skip(1).collect();
    if parents.len() < 2 {
        return Ok(None);
    }
    let moved = parents[1..]
        .iter()
        .any(|p| !remotes.values().any(|tip| tip == p));
    if !moved {
        return Ok(None);
    }
    let first = parents[0];
    if git.tree_of(repo, first)? != local.tree {
        return Ok(None);
    }
    Ok(Some(first.to_owned()))
}

/// Whether `commit` is a first snapshot: none of its parents is a
/// snapshot commit (the hub has seen nothing of this chain).
fn first_snapshot(git: &Git, repo: &Repo, commit: &str) -> Result<bool> {
    let parents = git
        .ok(
            Some(repo),
            &["rev-list", "--parents", "-n", "1", commit],
            &Opts::default(),
        )?
        .text();
    for parent in parents.split_whitespace().skip(1) {
        let raw = git.ok(
            Some(repo),
            &["cat-file", "commit", parent],
            &Opts::default(),
        )?;
        let text = String::from_utf8_lossy(&raw.stdout);
        let message = text.split_once("\n\n").map_or("", |(_, m)| m);
        if snapshot::Meta::host_of(message).is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Registers the unit's watches: every directory `ls-files` knows, the
/// git dir with its refs, the opted-in ignored directories; or, for new
/// directories, those that `check-ignore` lets through.
fn watch_unit(
    shared: &Shared,
    unit: &str,
    kind: UnitKind,
    rules: &Rules,
    new_dirs: &[String],
) -> (Vec<String>, usize) {
    let Some(reg) = &shared.registry else {
        return (Vec::new(), 0);
    };
    let Ok(repo) = shared.layout.repo(unit, kind) else {
        return (Vec::new(), 0);
    };
    let excludes = shared.excludes(unit, rules);
    let unit_dir = shared.layout.unit_dir(unit);
    let mut nested = Vec::new();
    if !new_dirs.is_empty() {
        let ignored = shared
            .git
            .ignored(&repo, excludes.as_deref(), new_dirs)
            .unwrap_or_default();
        for rel in new_dirs {
            let opted_in = rules
                .sync_ignored
                .iter()
                .any(|e| rel == &e.pattern || rel.starts_with(&format!("{}/", e.pattern)));
            if ignored.contains(rel) && !opted_in {
                continue;
            }
            // a new directory in the git dir (a branch namespace): the
            // hidden git dir of a plain unit is not under the unit
            if let Some(inner) = rel.strip_prefix(".git/") {
                let dir = repo.git_dir.join(inner);
                if let Err(e) = reg.watch(unit, rel, &dir) {
                    debug!("workspace: cannot watch {}: {e}", dir.display());
                }
                continue;
            }
            if unit_dir.join(rel).join(".git").exists() {
                nested.push(rel.clone());
                continue;
            }
            nested.extend(reg.watch_tree(unit, &unit_dir, rel, &rules.always_exclude));
        }
        return (nested, reg.count(unit));
    }
    let listed = shared.git.run(
        Some(&repo),
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        &Opts {
            excludes: excludes.as_deref(),
            ..Opts::default()
        },
    );
    let paths: Vec<String> = listed.map(|o| o.records()).unwrap_or_default();
    for rel in watch::parent_dirs(&paths) {
        let dir = if rel.is_empty() {
            unit_dir.clone()
        } else {
            unit_dir.join(&rel)
        };
        if dir.join(".git").exists() && !rel.is_empty() {
            nested.push(rel);
            continue;
        }
        if let Err(e) = reg.watch(unit, &rel, &dir) {
            debug!("workspace: cannot watch {}: {e}", dir.display());
        }
    }
    for dir in watch::git_dirs(&repo.git_dir) {
        let rel = format!(
            ".git/{}",
            dir.strip_prefix(&repo.git_dir)
                .unwrap_or(&dir)
                .to_string_lossy()
        );
        if let Err(e) = reg.watch(unit, rel.trim_end_matches('/'), &dir) {
            debug!("workspace: cannot watch {}: {e}", dir.display());
        }
    }
    for entry in &rules.sync_ignored {
        for rel in snapshot::expand(&unit_dir, &entry.pattern) {
            if unit_dir.join(&rel).is_dir() {
                nested.extend(reg.watch_tree(unit, &unit_dir, &rel, &rules.always_exclude));
            }
        }
    }
    (nested, reg.count(unit))
}

/// The ignored directories present in a unit with their size, for the
/// panel's switches.
fn ignored_dirs(shared: &Shared, unit: &str, kind: UnitKind, rules: &Rules) -> Vec<IgnoredDir> {
    let Ok(repo) = shared.layout.repo(unit, kind) else {
        return Vec::new();
    };
    let excludes = shared.excludes(unit, rules);
    let Ok(out) = shared.git.run(
        Some(&repo),
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--directory",
            "--exclude-standard",
        ],
        &Opts {
            excludes: excludes.as_deref(),
            ..Opts::default()
        },
    ) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for record in out.records() {
        let Some(path) = record.strip_suffix('/') else {
            continue;
        };
        if path.starts_with(".git") {
            continue;
        }
        let (files, bytes) = units::size_of(&repo.work_tree.join(path), &[]);
        if files == 0 {
            continue;
        }
        // a rule naming the directory, or one under it
        let carried = rules.sync_ignored.iter().find(|e| {
            snapshot::expand(&repo.work_tree, &e.pattern)
                .iter()
                .any(|p| p == path || p.starts_with(&format!("{path}/")))
        });
        dirs.push(IgnoredDir {
            path: path.to_owned(),
            files,
            bytes,
            carried: carried.is_some(),
            quiescent: carried.is_some_and(|e| e.quiescent),
        });
        if dirs.len() >= 50 {
            break;
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.bytes));
    dirs
}

/// Creates a unit the hub has: an empty repository fast-forwarded to the
/// newest snapshot, then pushed as this host's.
fn create(shared: &Shared, unit: &str, kind: UnitKind, rules: &Rules) -> Result<Made> {
    let dir = shared.layout.unit_dir(unit);
    let fresh = !dir.exists();
    let result = create_in(shared, unit, kind, rules);
    // NOTE: a half-made repository would be snapshotted as the unit's state next
    if result.is_err() && fresh && !shared.layout.has_files(unit) {
        let _ = fs::remove_dir_all(&dir);
        if kind == UnitKind::Plain {
            let _ = fs::remove_dir_all(shared.layout.hidden_git(unit));
        }
    }
    result
}

fn create_in(shared: &Shared, unit: &str, kind: UnitKind, rules: &Rules) -> Result<Made> {
    let (repo, had_files) = match kind {
        UnitKind::Plain => {
            let had = shared.layout.has_files(unit);
            (shared.layout.ensure_hidden(&shared.git, unit)?, had)
        }
        UnitKind::Git => shared.layout.create_git(&shared.git, unit)?,
    };
    if had_files {
        return Ok(Made::Adoptable);
    }
    let url = shared.url(unit);
    shared
        .git
        .fetch(&repo, &url, &fetch_refspecs(shared))
        .context("fetching from the hub")?;
    let remotes = remotes(shared, &repo)?;
    if remotes.is_empty() {
        bail!("nothing on the hub yet");
    }
    let mut newest: Option<(i64, String, String)> = None;
    for (host, sha) in &remotes {
        let at: i64 = shared
            .git
            .ok(
                Some(&repo),
                &["log", "-1", "--format=%ct", sha],
                &Opts::default(),
            )?
            .text()
            .parse()
            .unwrap_or(0);
        if newest.as_ref().is_none_or(|(t, _, _)| at > *t) {
            newest = Some((at, host.clone(), sha.clone()));
        }
    }
    let (_, host, sha) = newest.expect("a remote");
    let scratch = shared.layout.scratch(unit);
    let report = apply::create(
        &shared.git,
        &repo,
        &scratch,
        rules,
        &shared.host,
        &host,
        &sha,
    )?;
    if let apply::Outcome::Retry(reason) = report.outcome {
        bail!("{reason}");
    }
    let local = apply::own_snapshot(&shared.git, &repo, &shared.host)?
        .ok_or_else(|| anyhow!("no snapshot after creating the unit"))?;
    let mut merged = BTreeMap::new();
    merged.insert(host, sha);
    Ok(Made::Done(Box::new((local, merged))))
}

/// Applies or drops a held snapshot.
fn held(
    shared: &Shared,
    unit: &str,
    kind: UnitKind,
    rules: &Rules,
    action: HeldAction,
    local: Option<&Local>,
) -> Result<Option<Local>> {
    let repo = shared.layout.repo(unit, kind)?;
    let held_ref = format!("{HELD_REFS}/{}", shared.host);
    let held = shared
        .git
        .rev(&repo, &held_ref)?
        .ok_or_else(|| anyhow!("no held snapshot"))?;
    match action {
        HeldAction::Apply => {
            shared
                .git
                .update_ref(&repo, &format!("{SYNC_REFS}/{}", shared.host), &held)?;
            shared.git.delete_ref(&repo, &held_ref)?;
            apply::own_snapshot(&shared.git, &repo, &shared.host)
        }
        HeldAction::Drop => {
            let previous = local.ok_or_else(|| anyhow!("no previous snapshot to go back to"))?;
            let snap = apply::read_snapshot(&shared.git, &repo, &held)?;
            let scratch = shared.layout.scratch(unit);
            apply::roll_back(&shared.git, &repo, &scratch, rules, &snap.work, previous)?;
            shared.git.delete_ref(&repo, &held_ref)?;
            Ok(None)
        }
    }
}

/// What the scheduler keeps per unit between runs.
struct Saved {
    adopted: bool,
    tracked: u64,
    pushed: Option<String>,
    merged: BTreeMap<String, String>,
    conflicts: Vec<ConflictReport>,
    held: Option<HeldReport>,
    lagging: BTreeSet<String>,
    /// The owner's pending adopt request, with its preference.
    adopt: Option<Option<Side>>,
}

/// `$XDG_STATE_HOME/ccdash/workspace/workspace.db`.
struct Db {
    conn: Connection,
}

const DB_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS units (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    adopted INTEGER NOT NULL DEFAULT 0,
    tracked INTEGER NOT NULL DEFAULT 0,
    pushed TEXT
);
CREATE TABLE IF NOT EXISTS merged (
    unit TEXT NOT NULL,
    host TEXT NOT NULL,
    commit_id TEXT NOT NULL,
    PRIMARY KEY (unit, host)
);
CREATE TABLE IF NOT EXISTS conflicts (
    unit TEXT NOT NULL,
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    ours TEXT NOT NULL,
    theirs TEXT NOT NULL,
    other_host TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (unit, path)
);
CREATE TABLE IF NOT EXISTS held (
    unit TEXT PRIMARY KEY,
    commit_id TEXT NOT NULL,
    added INTEGER NOT NULL,
    deleted INTEGER NOT NULL,
    tracked INTEGER NOT NULL,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS lagging (
    unit TEXT NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY (unit, path)
);
CREATE TABLE IF NOT EXISTS adopt (
    unit TEXT PRIMARY KEY,
    prefer TEXT
);
";

impl Db {
    fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(DB_SCHEMA)?;
        Ok(Self { conn })
    }

    fn load(&self, unit: &str) -> Result<Saved> {
        let (adopted, tracked, pushed): (bool, i64, Option<String>) = self
            .conn
            .query_row(
                "SELECT adopted, tracked, pushed FROM units WHERE id = ?1",
                [unit],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap_or((false, 0, None));
        let merged = self
            .conn
            .prepare("SELECT host, commit_id FROM merged WHERE unit = ?1")?
            .query_map([unit], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let conflicts = self
            .conn
            .prepare(
                "SELECT path, kind, ours, theirs, other_host, at FROM conflicts WHERE unit = ?1",
            )?
            .query_map([unit], |r| {
                Ok(ConflictReport {
                    path: r.get(0)?,
                    kind: r.get(1)?,
                    ours: r.get(2)?,
                    theirs: r.get(3)?,
                    other_host: r.get(4)?,
                    at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let held = self
            .conn
            .query_row(
                "SELECT commit_id, added, deleted, tracked, at FROM held WHERE unit = ?1",
                [unit],
                |r| {
                    Ok(HeldReport {
                        commit: r.get(0)?,
                        added: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
                        deleted: r.get::<_, i64>(2)?.try_into().unwrap_or(0),
                        tracked: r.get::<_, i64>(3)?.try_into().unwrap_or(0),
                        at: r.get(4)?,
                    })
                },
            )
            .ok();
        let lagging = self
            .conn
            .prepare("SELECT path FROM lagging WHERE unit = ?1")?
            .query_map([unit], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let adopt = self
            .conn
            .query_row("SELECT prefer FROM adopt WHERE unit = ?1", [unit], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .map(|p| match p.as_deref() {
                Some("mine") => Some(Side::Mine),
                Some("theirs") => Some(Side::Theirs),
                _ => None,
            });
        Ok(Saved {
            adopted,
            tracked: tracked.try_into().unwrap_or(0),
            pushed,
            merged,
            conflicts,
            held,
            lagging,
            adopt,
        })
    }

    fn save(&mut self, unit: &str, e: &Entry) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO units (id, kind, adopted, tracked, pushed) VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT (id) DO UPDATE SET kind = ?2, adopted = ?3, tracked = ?4, pushed = ?5",
            params![
                unit,
                match e.kind {
                    UnitKind::Git => "git",
                    UnitKind::Plain => "plain",
                },
                e.adopted,
                i64::try_from(e.files).unwrap_or(0),
                e.pushed
            ],
        )?;
        tx.execute("DELETE FROM merged WHERE unit = ?1", [unit])?;
        for (host, sha) in &e.merged {
            tx.execute(
                "INSERT INTO merged (unit, host, commit_id) VALUES (?1, ?2, ?3)",
                params![unit, host, sha],
            )?;
        }
        tx.execute("DELETE FROM conflicts WHERE unit = ?1", [unit])?;
        for c in &e.conflicts {
            tx.execute(
                "INSERT INTO conflicts (unit, path, kind, ours, theirs, other_host, at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![unit, c.path, c.kind, c.ours, c.theirs, c.other_host, c.at],
            )?;
        }
        tx.execute("DELETE FROM adopt WHERE unit = ?1", [unit])?;
        if e.adopt {
            let prefer = e.adopt_prefer.map(|s| match s {
                Side::Mine => "mine",
                Side::Theirs => "theirs",
            });
            tx.execute(
                "INSERT INTO adopt (unit, prefer) VALUES (?1, ?2)",
                params![unit, prefer],
            )?;
        }
        tx.execute("DELETE FROM lagging WHERE unit = ?1", [unit])?;
        for path in &e.lagging {
            tx.execute(
                "INSERT INTO lagging (unit, path) VALUES (?1, ?2)",
                params![unit, path],
            )?;
        }
        tx.execute("DELETE FROM held WHERE unit = ?1", [unit])?;
        if let Some(h) = &e.held {
            tx.execute(
                "INSERT INTO held (unit, commit_id, added, deleted, tracked, at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    unit,
                    h.commit,
                    i64::try_from(h.added).unwrap_or(0),
                    i64::try_from(h.deleted).unwrap_or(0),
                    i64::try_from(h.tracked).unwrap_or(0),
                    h.at
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_covers_units_under_and_over_a_tool_call() {
        let busy = Busy::default();
        busy.start("s1", "/w/tsandrini", 1000);
        busy.start("s2", "/w/meteopress/radar/src", 1000);
        assert!(
            busy.covers(Path::new("/w/tsandrini/ccdash"), 2000)
                .is_some()
        );
        assert!(
            busy.covers(Path::new("/w/meteopress/radar"), 2000)
                .is_some()
        );
        assert!(
            busy.covers(Path::new("/w/meteopress/other"), 2000)
                .is_none()
        );
        busy.end("s1");
        assert!(
            busy.covers(Path::new("/w/tsandrini/ccdash"), 2000)
                .is_none()
        );
        // a stale call no longer counts
        assert!(
            busy.covers(Path::new("/w/meteopress/radar"), 1000 + TOOL_STALE_MS + 1)
                .is_none()
        );
    }

    #[test]
    fn the_unit_db_round_trips() {
        let dir = std::env::temp_dir().join(format!("ccdash-wsdb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("w.db")).unwrap();
        let mut e = Entry::new(UnitKind::Git, WorkspaceConfig::default().rules("a/b"), None);
        e.adopted = true;
        e.files = 42;
        e.pushed = Some("p".repeat(40));
        e.merged.insert("flat".into(), "f".repeat(40));
        e.conflicts.push(ConflictReport {
            path: "x.txt".into(),
            kind: "text".into(),
            ours: "1".repeat(40),
            theirs: "2".repeat(40),
            other_host: "flat".into(),
            at: 5,
        });
        e.held = Some(HeldReport {
            commit: "h".repeat(40),
            added: 3000,
            deleted: 0,
            tracked: 100,
            at: 6,
        });
        db.save("a/b", &e).unwrap();
        let saved = db.load("a/b").unwrap();
        assert!(saved.adopted);
        assert_eq!(saved.tracked, 42);
        assert_eq!(saved.pushed.as_deref(), Some("p".repeat(40).as_str()));
        assert_eq!(saved.merged["flat"], "f".repeat(40));
        assert_eq!(saved.conflicts.len(), 1);
        assert_eq!(saved.held.unwrap().added, 3000);
        assert!(!db.load("none").unwrap().adopted);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn entries_settle_bursts_and_back_off() {
        let mut e = Entry::new(UnitKind::Git, WorkspaceConfig::default().rules("a"), None);
        let now = Instant::now();
        e.due_fetch = false;
        e.due_apply = false;
        assert_eq!(e.due(now), (false, None));
        e.burst = Some((now, now));
        assert!(!e.due(now).0);
        assert!(e.due(now + QUIET).0);
        e.burst = Some((now, now + Duration::from_secs(3)));
        assert!(e.due(now + BURST_MAX).0, "a steady stream still settles");
        e.burst = None;
        e.retry = Some((now + Duration::from_secs(1), Duration::from_secs(1)));
        assert_eq!(e.due(now), (false, Some(now + Duration::from_secs(1))));
        assert!(e.due(now + Duration::from_secs(1)).0);
        e.rules.paused = true;
        assert!(!e.due(now + Duration::from_secs(1)).0);
    }
}
