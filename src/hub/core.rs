//! The task that owns the hub's state: one reducer per host, the store,
//! the collector links, open asks and notifications. Everything else
//! talks to it through `Cmd`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    net::IpAddr,
    sync::Arc,
};

/// Workspace name → the directories under it on one host.
pub type Dirs = BTreeMap<String, Vec<String>>;

/// Where a knowledge call stands (see `Caller`), for the HTTP side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub workspace: String,
    pub project: Option<String>,
    pub session: Option<String>,
}

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{info, warn};

use crate::{
    ask::{self, Answer},
    claims,
    config::Knowledge as KnowledgeCfg,
    drafts,
    knowledge::{self, Query},
    machine::{self, MachineReport, Power, PowerAction, Wake},
    messages,
    model::{
        Activity, Ask, Draft, Editing, HostFeatures, HostStatus, HubFeatures, HubStatus, Launch,
        LaunchState, Message, RateLimits, ScopedLimit, Session, Snapshot, Update,
    },
    notify::{self, Category, Composer, Note},
    paths,
    protocol::{Call, Injection, Reply, TelemetryRecord, ToCollector, Who},
    state::{AgentEntry, Effects, NiriEvent, PREVIEW_LEN, ProcPoll, RanOn, State, StoreMsg},
    store::{self, Store},
    sync::{SyncHost, SyncStatus},
    util::{self, now_ms},
    wire::Envelope,
    workspace::{
        Cost, HeldAction, NewDir, SeenUnit, Transfer, UnitStatus, WorkspaceConfig, WorkspaceHost,
        WorkspaceSetup, WorkspaceStatus, Workspaces,
        hub::{self as wshub, Reported, Repos, TRANSFER_MIN, TrafficDelta},
    },
    wrap_up,
};

/// A unit failing for this long notifies.
const FAILING_MS: i64 = 600_000;
/// Traffic and cost deltas go to the database this often.
const FLUSH_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;

const MAX_TITLE: usize = 100;
const MAX_REPLY: usize = 20_000;
const GONE: &str = "that prompt is no longer open";
const MINUTE_MS: i64 = 60_000;
/// A launch the session has not reported for within this fails.
const LAUNCH_WAIT_MS: i64 = 2 * MINUTE_MS;
/// Launches are kept this long for `GET /launches/{id}`.
const LAUNCH_KEEP_MS: i64 = 15 * MINUTE_MS;
/// A machine that has not connected this long after its magic packets is
/// shown as offline again.
const WAKING_KEEP_MS: i64 = 10 * MINUTE_MS;
/// A report this long after a suspend says the machine did not sleep, or
/// is awake again on the same link.
const ASLEEP_STALE_MS: i64 = 15_000;

/// What a collector reports about its host.
pub enum Input {
    Envelope(Envelope),
    /// The poll and the pids of known sessions that exited.
    Agents(Vec<AgentEntry>, i64, Vec<u32>),
    Niri(NiriEvent),
    Procs(Vec<ProcPoll>, i64),
    Usage(Vec<ScopedLimit>, i64),
    Telemetry(Vec<TelemetryRecord>),
}

pub enum Cmd {
    /// Something a collector reported about `host`.
    Input(String, Input),
    /// Where a caller on `host` stands: its workspace, project and session.
    Locate {
        host: String,
        who: Who,
        reply: oneshot::Sender<Result<Located, String>>,
    },
    Tick(i64),
    Snapshot(oneshot::Sender<Snapshot>),
    Resolve(String, oneshot::Sender<Option<Session>>),
    /// Renames a session; replies with the updated session or why not.
    Rename(String, String, oneshot::Sender<Result<Session, String>>),
    /// Pins a session's project (None: the directory's again).
    SetProject {
        key: String,
        project: Option<String>,
        reply: oneshot::Sender<Result<Session, String>>,
    },
    /// Sends a message to a session through its channel.
    Reply(String, String, oneshot::Sender<Result<Session, String>>),
    /// The owner's message to the session `key` names, for its next prompt.
    Message {
        key: String,
        text: String,
        reply: oneshot::Sender<Result<Message, String>>,
    },
    /// The messages waiting for `session` changed.
    Inbox {
        session: String,
        inbox: Vec<Message>,
    },
    /// An injection handed `session` its waiting messages; the hook's
    /// acknowledgement marks them read.
    Delivered {
        session: String,
    },
    /// The prompt hook of `session` on `host` printed the hub's context.
    Injected {
        host: String,
        session: String,
    },
    /// Answers ask `ask` of the session `key` names.
    Answer {
        key: String,
        ask: String,
        answer: Answer,
        reply: oneshot::Sender<Result<Session, String>>,
    },
    /// A notification button: `action` on the ask with button key `key`.
    Button {
        key: String,
        action: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// A collector link came up; `conn` tells its connections apart.
    CollectorOpen {
        host: String,
        conn: u64,
        terminals: Vec<String>,
        /// What its collector does besides reporting.
        features: HostFeatures,
        /// Where it connects from.
        addr: Option<IpAddr>,
        tx: mpsc::Sender<ToCollector>,
    },
    CollectorClosed {
        host: String,
        conn: u64,
    },
    ChannelOpen {
        host: String,
        pid: u32,
    },
    ChannelClosed {
        host: String,
        pid: u32,
    },
    /// `ccdash ask` on `host` holds a prompt open under `id`.
    Ask {
        host: String,
        id: String,
        envelope: Box<Envelope>,
    },
    /// The hook holding ask `id` on `host` went away.
    AskGone {
        host: String,
        id: String,
    },
    /// A collector's local API asks for something only the hub can do.
    Call {
        host: String,
        id: u64,
        call: Call,
    },
    /// A write a collector made while the link was down, replayed.
    Queued {
        host: String,
        call: Call,
        at: i64,
    },
    /// Notes changed (written here or by the panel): pushed to every
    /// collector's cache and announced to the panel.
    NotesChanged {
        notes: Vec<knowledge::Note>,
    },
    /// Resends the whole cache of one workspace (or all) to collectors.
    NotesResync {
        workspace: Option<String>,
    },
    /// The daily lifecycle pass.
    Reflect,
    /// Starts (or continues) a session; replies with the launch.
    Launch {
        req: LaunchReq,
        reply: oneshot::Sender<Result<Launch, String>>,
    },
    /// The launch with this id, while the hub keeps it.
    LaunchOf(String, oneshot::Sender<Option<Launch>>),
    /// The connected host whose collector links from `addr`: the machine
    /// a panel at that address sits on.
    Here {
        addr: IpAddr,
        reply: oneshot::Sender<Option<String>>,
    },
    /// The panel wrote draft `key`; an empty text drops it.
    Draft {
        key: String,
        text: String,
        reply: oneshot::Sender<Result<Draft, String>>,
    },
    /// Opens draft `key` (with `text`, the panel's latest) in `host`'s
    /// editor; the draft waits until the editor closes.
    Edit {
        key: String,
        host: String,
        text: String,
        reply: oneshot::Sender<Result<Draft, String>>,
    },
    /// Closes the editor open on draft `key`; the text stays as it was.
    EditCancel {
        key: String,
        reply: oneshot::Sender<Result<Draft, String>>,
    },
    /// The editor `host` opened for edit `id` closed.
    Edited {
        host: String,
        id: String,
        key: String,
        text: Option<String>,
        error: Option<String>,
    },
    /// What `claude --bg` on `host` made of a launch.
    Launched {
        host: String,
        launch: String,
        session: Option<String>,
        error: Option<String>,
    },
    /// Stops the session `key` names; replies once its host has.
    Stop {
        key: String,
        reply: oneshot::Sender<Result<Session, String>>,
    },
    Stopped {
        host: String,
        session: String,
        error: Option<String>,
    },
    /// Interrupts the turn of the session `key` names: an open prompt is
    /// denied with a stop, otherwise its next tool hook ends the turn.
    Interrupt {
        key: String,
        reply: oneshot::Sender<Result<Session, String>>,
    },
    /// A tool hook on `host` stopped `session`'s turn at `at`.
    Interrupted {
        host: String,
        session: String,
        at: i64,
    },
    /// Marks the session `key` names resolved: the wrap-up prompt goes
    /// through its channel, and the session closes when that turn ends.
    MarkResolved {
        key: String,
        reply: oneshot::Sender<Result<Session, String>>,
    },
    /// Takes the resolved mark of the session `key` names back.
    KeepOpen {
        key: String,
        reply: oneshot::Sender<Result<Session, String>>,
    },
    /// Marks the session `key` names gone (a ghost, an offline host).
    Forget {
        key: String,
        reply: oneshot::Sender<Result<Session, String>>,
    },
    /// Focuses or attaches the session `key` names on its host.
    Open {
        key: String,
        reply: oneshot::Sender<Result<Session, String>>,
    },
    /// The project directories a collector found on its host.
    Dirs {
        host: String,
        dirs: Dirs,
    },
    /// The connected hosts with their directories.
    HostDirs(oneshot::Sender<Vec<(String, Dirs)>>),
    /// The skills a collector found on its host.
    Skills {
        host: String,
        skills: Vec<crate::model::Skill>,
    },
    /// The colours of a collector's desktop.
    Palette {
        host: String,
        palette: Option<crate::model::Palette>,
    },
    /// One host's skills (none for a host never heard from).
    HostSkills {
        host: String,
        reply: oneshot::Sender<Vec<crate::model::Skill>>,
    },
    /// How a collector's copy of Claude Code's state stands.
    SyncStatus {
        host: String,
        status: SyncStatus,
    },
    /// Every host with its sync status, for the panel.
    SyncHosts(oneshot::Sender<Vec<SyncHost>>),
    /// The host holding session `session` when it is not `host`.
    Holder {
        host: String,
        session: String,
        reply: oneshot::Sender<Option<String>>,
    },
    /// What `host` made of a conflict resolution.
    SyncResolved {
        host: String,
        id: i64,
        error: Option<String>,
        copy: Option<String>,
    },
    /// Hands `msg` to `host`'s collector; fails when it is not connected.
    SendTo {
        host: String,
        msg: Box<ToCollector>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Asks `host`'s collector to compare everything with the hub again.
    SyncResync {
        host: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// How the workspace stands on `host`: the changed units, or all.
    WorkspaceReport {
        host: String,
        status: WorkspaceStatus,
        units: Vec<UnitStatus>,
        full: bool,
    },
    /// What `host` sees at unit depth.
    WorkspaceSeen {
        host: String,
        units: Vec<SeenUnit>,
    },
    /// A receive-pack for `unit` from `host` ran; `bytes` came in.
    WorkspacePushed {
        unit: String,
        host: String,
        bytes: u64,
    },
    /// A smart HTTP request of `host` for `unit` finished.
    WorkspaceTransfer {
        host: String,
        unit: String,
        /// `push` or `fetch`.
        kind: String,
        bytes_in: u64,
        bytes_out: u64,
        ms: u64,
        ok: bool,
    },
    /// Apply or drop a held snapshot on `host`; answered once the
    /// collector took (or refused) it.
    WorkspaceHeld {
        host: String,
        unit: String,
        action: HeldAction,
        commit: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// What `host` made of a `WorkspaceHeld`.
    WorkspaceHeldDone {
        host: String,
        unit: String,
        error: Option<String>,
    },
    /// How a collector's machine is doing.
    Machine {
        host: String,
        report: Box<MachineReport>,
    },
    /// The hub's own machine.
    HubMachine(Box<MachineReport>),
    /// Suspends `host` or locks its session; a suspend with `force` even
    /// while sessions there work.
    Power {
        host: String,
        action: PowerAction,
        force: bool,
        reply: oneshot::Sender<Result<HostStatus, String>>,
    },
    /// What came of power action `id` on `host`.
    PowerDone {
        host: String,
        id: String,
        error: Option<String>,
    },
    /// Sends magic packets to `host`.
    Wake {
        host: String,
        reply: oneshot::Sender<Result<HostStatus, String>>,
    },
    /// The magic packets for `host` went out.
    WakeSent {
        host: String,
        reply: oneshot::Sender<Result<HostStatus, String>>,
    },
    /// Drops an offline host from the list the hub keeps.
    ForgetHost {
        host: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// The hub's ping to `host`'s collector came back.
    CollectorRtt {
        host: String,
        conn: u64,
        ms: u64,
    },
    /// `host`'s snapshot of `unit` moved on the hub: tell the others.
    WorkspaceMoved {
        unit: String,
        host: String,
        version: i64,
    },
    /// The configuration or the unit set changed in the database: reload
    /// it and hand it to every collector.
    WorkspaceSetupChanged,
    /// What `host` made of a conflict resolution.
    WorkspaceResolved {
        host: String,
        id: i64,
        error: Option<String>,
    },
    /// The hosts, their reports and the setup, for the panel.
    WorkspaceState(oneshot::Sender<Option<WorkspaceState>>),
    /// A collector's starting configuration, kept when the hub has none.
    WorkspaceStarting {
        host: String,
        config: WorkspaceConfig,
    },
}

/// The live part of the Workspace page.
#[derive(Debug, Clone)]
pub struct WorkspaceState {
    pub hosts: Vec<WorkspaceHost>,
    /// Host → unit → its last report.
    pub reports: HashMap<String, HashMap<String, UnitStatus>>,
    pub config: WorkspaceConfig,
    pub version: i64,
}

/// The hub's side of the workspace sync, as the core holds it.
struct WorkspaceCore {
    repos: Arc<Repos>,
    setup: WorkspaceSetup,
    reports: HashMap<String, Reported>,
    /// New directories over the threshold, for the notifications.
    waiting: Vec<NewDir>,
    started: i64,
    /// (host, direction) → traffic since the last flush.
    traffic: HashMap<(String, String), TrafficDelta>,
    /// Host → the counters it reported last, to take deltas from.
    cost_last: HashMap<String, Cost>,
    /// Host → the deltas since the last flush (the gauges as last seen).
    cost_acc: HashMap<String, Cost>,
    last_flush: i64,
    /// (host, unit) → who waits for the collector's answer to a held action.
    held_waits: HashMap<(String, String), oneshot::Sender<Result<(), String>>>,
}

/// `new - last` for one counter, or `new` when the counter went down (a
/// restarted collector).
fn delta(new: u64, last: u64, restarted: bool) -> u64 {
    if restarted {
        new
    } else {
        new.saturating_sub(last)
    }
}

impl WorkspaceCore {
    /// Folds a host's cumulative counters into its deltas. The first
    /// report after a hub start is the baseline only (it would count the
    /// collector's whole run); a decrease means the collector restarted
    /// and its counters are the delta.
    fn take_cost(&mut self, host: &str, cost: &Cost) {
        let Some(last) = self.cost_last.insert(host.to_owned(), cost.clone()) else {
            return;
        };
        let restarted = last.since != cost.since
            || cost.passes < last.passes
            || cost.git_calls < last.git_calls
            || cost.cpu_ms < last.cpu_ms;
        let d = |new: u64, old: u64| delta(new, old, restarted);
        let acc = self.cost_acc.entry(host.to_owned()).or_default();
        acc.cpu_ms += d(cost.cpu_ms, last.cpu_ms);
        acc.git_cpu_ms += d(cost.git_cpu_ms, last.git_cpu_ms);
        acc.git_calls += d(cost.git_calls, last.git_calls);
        acc.git_wall_ms += d(cost.git_wall_ms, last.git_wall_ms);
        acc.io_read += d(cost.io_read, last.io_read);
        acc.io_write += d(cost.io_write, last.io_write);
        acc.passes += d(cost.passes, last.passes);
        acc.snapshots += d(cost.snapshots, last.snapshots);
        acc.pushes += d(cost.pushes, last.pushes);
        acc.fetches += d(cost.fetches, last.fetches);
        acc.push_ms += d(cost.push_ms, last.push_ms);
        acc.fetch_ms += d(cost.fetch_ms, last.fetch_ms);
        acc.failures += d(cost.failures, last.failures);
        acc.max_rss_kb = acc.max_rss_kb.max(cost.max_rss_kb);
        acc.rss_kb = acc.rss_kb.max(cost.rss_kb);
        acc.watches = cost.watches;
    }
}

/// A session to start, with the directory already resolved from a known
/// workspace and project.
#[derive(Debug, Clone)]
pub struct LaunchReq {
    pub host: String,
    pub workspace: String,
    pub project: Option<String>,
    pub cwd: String,
    pub prompt: String,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub effort: Option<String>,
    pub resume: Option<String>,
    pub issue: Option<LaunchIssue>,
}

/// The open issue a session is started for.
#[derive(Debug, Clone)]
pub struct LaunchIssue {
    pub slug: String,
    pub title: String,
}

/// Room for an issue's title in a claim's `what`.
const MAX_CLAIM_TITLE: usize = 150;

/// A launch and what the hub needs to finish it.
struct Slot {
    launch: Launch,
    cwd: String,
    /// What the session claims when it reports.
    issue: Option<LaunchIssue>,
    /// The id `claude --bg` printed, until the session shows up.
    short: Option<String>,
    /// The sleeping session this launch wakes: a failure lands on it.
    wakes: Option<String>,
}

/// What answering an open ask needs and the session doesn't show.
struct Pending {
    host: String,
    session: String,
    /// Secret in the notification buttons' links.
    key: String,
    input: Value,
    updates: Vec<Value>,
}

struct Link {
    conn: u64,
    /// Where the collector connects from, for `here`.
    addr: Option<IpAddr>,
    tx: mpsc::Sender<ToCollector>,
}

/// Notification candidates for the notifier task.
pub struct Notes {
    pub composer: Composer,
    pub tx: watch::Sender<Vec<Note>>,
}

#[derive(Clone)]
pub struct Hub {
    tx: mpsc::Sender<Cmd>,
    updates: broadcast::Sender<Update>,
}

impl Hub {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        host: String,
        public_url: String,
        workspaces: Workspaces,
        store: Store,
        notes: Option<Notes>,
        knowledge: KnowledgeCfg,
        tool_output_bytes: usize,
        repos: Option<Arc<Repos>>,
        wake_via: BTreeMap<String, String>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let (updates, _) = broadcast::channel(256);
        let reader = store
            .reader()
            .inspect_err(|e| warn!("store reader: {e:#}"))
            .ok();
        let workspace = repos.map(|repos| {
            let (setup, waiting) = reader
                .as_ref()
                .map(|conn| {
                    let setup = wshub::setup(conn).unwrap_or_default();
                    let waiting = wshub::new_dirs(conn, &setup.config)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|n| n.state == "waiting")
                        .collect();
                    (setup, waiting)
                })
                .unwrap_or_default();
            WorkspaceCore {
                repos,
                setup,
                reports: HashMap::new(),
                waiting,
                started: now_ms(),
                traffic: HashMap::new(),
                cost_last: HashMap::new(),
                cost_acc: HashMap::new(),
                last_flush: now_ms(),
                held_waits: HashMap::new(),
            }
        });
        let started = now_ms();
        // NOTE: a host connected when the hub stopped is offline from now on until it reconnects
        let hosts = reader
            .as_ref()
            .map(store::hosts)
            .transpose()
            .unwrap_or_else(|e| {
                warn!("reading the kept hosts: {e:#}");
                None
            })
            .unwrap_or_default()
            .into_iter()
            .map(|mut h| {
                if h.connected {
                    h.connected = false;
                    h.since = started;
                }
                h.rtt_ms = None;
                (h.name.clone(), h)
            })
            .collect();
        let drafts = reader
            .as_ref()
            .map(drafts::all)
            .transpose()
            .unwrap_or_else(|e| {
                warn!("reading the kept drafts: {e:#}");
                None
            })
            .unwrap_or_default()
            .into_iter()
            .map(|d| (d.key.clone(), d))
            .collect();
        let mut core = Core {
            hub: HubStatus {
                name: host.clone(),
                started_at: started,
                machine: None,
                features: HubFeatures {
                    workspace: workspace.is_some(),
                },
            },
            host,
            public_url,
            workspaces,
            states: HashMap::new(),
            links: HashMap::new(),
            hosts,
            drafts,
            cancelled_edits: HashSet::new(),
            channels: HashSet::new(),
            store,
            reader,
            published: HashMap::new(),
            shown: HashMap::new(),
            sync: HashMap::new(),
            pending: HashMap::new(),
            acks: HashSet::new(),
            updates: updates.clone(),
            notes,
            knowledge,
            tool_output_bytes,
            cmds: tx.clone(),
            launches: HashMap::new(),
            stops: HashMap::new(),
            refused_projects: HashMap::new(),
            dirs: HashMap::new(),
            skills: HashMap::new(),
            workspace,
            wake_via,
            power_waits: HashMap::new(),
            next_power: 0,
        };
        let names: Vec<String> = core.hosts.keys().cloned().collect();
        for name in names {
            core.refresh_wake(&name);
        }
        tokio::spawn(run(core, rx));
        Self { tx, updates }
    }

    /// The live part of the Workspace page; None when the workspace sync
    /// is off on the hub.
    pub async fn workspace_state(&self) -> Result<Option<WorkspaceState>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::WorkspaceState(reply)).await?;
        rx.await.context("hub stopped")
    }

    /// Tells collectors and the panel that `notes` changed.
    pub async fn notes_changed(&self, notes: Vec<knowledge::Note>) -> Result<()> {
        self.send(Cmd::NotesChanged { notes }).await
    }

    pub fn sender(&self) -> mpsc::Sender<Cmd> {
        self.tx.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Update> {
        self.updates.subscribe()
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Snapshot(tx)).await?;
        rx.await.context("hub stopped")
    }

    pub async fn rename(&self, key: &str, title: &str) -> Result<Result<Session, String>> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Rename(key.to_owned(), title.to_owned(), tx))
            .await?;
        rx.await.context("hub stopped")
    }

    pub async fn reply(&self, key: &str, text: &str) -> Result<Result<Session, String>> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Reply(key.to_owned(), text.to_owned(), tx))
            .await?;
        rx.await.context("hub stopped")
    }

    /// Pins the session's project, or with None lets its directory decide.
    pub async fn set_project(
        &self,
        key: &str,
        project: Option<String>,
    ) -> Result<Result<Session, String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::SetProject {
            key: key.to_owned(),
            project,
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    /// Leaves the owner's message for the session's next prompt.
    pub async fn message(&self, key: &str, text: &str) -> Result<Result<Message, String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::Message {
            key: key.to_owned(),
            text: text.to_owned(),
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    pub async fn answer(
        &self,
        key: &str,
        ask: &str,
        answer: Answer,
    ) -> Result<Result<Session, String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::Answer {
            key: key.to_owned(),
            ask: ask.to_owned(),
            answer,
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    pub async fn button(&self, key: &str, action: &str) -> Result<Result<(), String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::Button {
            key: key.to_owned(),
            action: action.to_owned(),
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    pub async fn send(&self, cmd: Cmd) -> Result<()> {
        self.tx.send(cmd).await.ok().context("hub stopped")
    }

    pub async fn resolve(&self, key: &str) -> Result<Option<Session>> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Resolve(key.to_owned(), tx)).await?;
        rx.await.context("hub stopped")
    }

    pub async fn launch(&self, req: LaunchReq) -> Result<Result<Launch, String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::Launch { req, reply }).await?;
        rx.await.context("hub stopped")
    }

    /// Connected hosts and the project directories they reported.
    pub async fn host_dirs(&self) -> Result<Vec<(String, Dirs)>> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::HostDirs(tx)).await?;
        rx.await.context("hub stopped")
    }

    /// The workspace, project and session of a caller on `host`, as the
    /// knowledge tools place it; or why it cannot be placed.
    pub async fn locate(&self, host: &str, who: &Who) -> Result<Result<Located, String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::Locate {
            host: host.to_owned(),
            who: who.clone(),
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    /// The skills `host`'s collector reported, as the composer offers them.
    pub async fn host_skills(&self, host: &str) -> Result<Vec<crate::model::Skill>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::HostSkills {
            host: host.to_owned(),
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    pub async fn launch_of(&self, id: &str) -> Result<Option<Launch>> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::LaunchOf(id.to_owned(), tx)).await?;
        rx.await.context("hub stopped")
    }

    /// The connected host whose collector links from `addr`.
    pub async fn here(&self, addr: IpAddr) -> Result<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::Here { addr, reply }).await?;
        rx.await.context("hub stopped")
    }

    /// Writes a draft (an empty text drops it); the draft as kept.
    pub async fn draft(&self, key: &str, text: &str) -> Result<Result<Draft, String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::Draft {
            key: key.to_owned(),
            text: text.to_owned(),
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    /// Opens a draft in `host`'s editor.
    pub async fn edit_draft(
        &self,
        key: &str,
        host: &str,
        text: &str,
    ) -> Result<Result<Draft, String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::Edit {
            key: key.to_owned(),
            host: host.to_owned(),
            text: text.to_owned(),
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    /// Closes the editor open on a draft.
    pub async fn cancel_edit(&self, key: &str) -> Result<Result<Draft, String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::EditCancel {
            key: key.to_owned(),
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    /// Sends a session command (`Stop`, `Forget`, `Open`) and waits for it.
    /// Every host with its sync status.
    pub async fn sync_hosts(&self) -> Result<Vec<SyncHost>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::SyncHosts(reply)).await?;
        rx.await.context("hub stopped")
    }

    /// The host holding `session` when that is not `host`: another
    /// machine opened it first and has it open.
    pub async fn holder(&self, host: &str, session: &str) -> Result<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::Holder {
            host: host.to_owned(),
            session: session.to_owned(),
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    /// Hands `msg` to `host`'s collector.
    pub async fn send_to(&self, host: &str, msg: ToCollector) -> Result<Result<(), String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::SendTo {
            host: host.to_owned(),
            msg: Box::new(msg),
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    /// Asks `host`'s collector to apply or drop a held snapshot; the
    /// answer is the collector's (taken, or why not).
    pub async fn workspace_held(
        &self,
        host: &str,
        unit: &str,
        action: HeldAction,
        commit: Option<String>,
    ) -> Result<oneshot::Receiver<Result<(), String>>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::WorkspaceHeld {
            host: host.to_owned(),
            unit: unit.to_owned(),
            action,
            commit,
            reply,
        })
        .await?;
        Ok(rx)
    }

    /// Asks `host`'s collector to compare everything again.
    pub async fn sync_resync(&self, host: &str) -> Result<Result<(), String>> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::SyncResync {
            host: host.to_owned(),
            reply,
        })
        .await?;
        rx.await.context("hub stopped")
    }

    pub async fn session_cmd(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<Session, String>>) -> Cmd,
    ) -> Result<Result<Session, String>> {
        let (reply, rx) = oneshot::channel();
        self.send(make(reply)).await?;
        rx.await.context("hub stopped")
    }
}

/// A power action a host is carrying out for the panel.
struct PowerWait {
    id: String,
    action: PowerAction,
    reply: oneshot::Sender<Result<HostStatus, String>>,
}

struct Core {
    host: String,
    /// Where the web UI lives, told to collectors for the links they open.
    public_url: String,
    workspaces: Workspaces,
    /// One reducer per collector host.
    states: HashMap<String, State>,
    links: HashMap<String, Link>,
    /// Every host seen, with its link status.
    hosts: BTreeMap<String, HostStatus>,
    /// The prompts being written in the panel, by key (see `drafts`).
    drafts: BTreeMap<String, Draft>,
    /// Edits the owner cancelled: what their editor still reports is dropped.
    cancelled_edits: HashSet<String>,
    /// `(host, pid)` of Claude processes connected through `ccdash channel`.
    channels: HashSet<(String, u32)>,
    store: Store,
    reader: Option<Connection>,
    /// What each host's collector last got of each of its sessions.
    published: HashMap<(String, String), Session>,
    /// What the panel last got of each session id (see `preferred`).
    shown: HashMap<String, Session>,
    /// The last sync status of each host's collector.
    sync: HashMap<String, SyncStatus>,
    /// Open asks by id.
    pending: HashMap<String, Pending>,
    /// Sessions whose prompt hook still has to acknowledge delivered messages.
    acks: HashSet<String>,
    updates: broadcast::Sender<Update>,
    notes: Option<Notes>,
    knowledge: KnowledgeCfg,
    /// `store.tool_output_bytes`, handed to every host's reducer.
    tool_output_bytes: usize,
    /// For tasks that finish a knowledge call and report back.
    cmds: mpsc::Sender<Cmd>,
    /// Sessions the panel asked to start, by launch id.
    launches: HashMap<String, Slot>,
    /// Who waits for a session's host to report its stop.
    stops: HashMap<String, Vec<oneshot::Sender<Result<Session, String>>>>,
    /// Why the project a session asked for at its start was refused, until
    /// its startup context tells Claude.
    refused_projects: HashMap<String, String>,
    /// The project directories each host reported.
    dirs: HashMap<String, Dirs>,
    /// The skills each host reported.
    skills: HashMap<String, Vec<crate::model::Skill>>,
    /// The workspace sync, when it is on.
    workspace: Option<WorkspaceCore>,
    /// The hub's own machine.
    hub: HubStatus,
    /// `[wake] via`: where magic packets go for hosts on other networks.
    wake_via: BTreeMap<String, String>,
    /// The power action each host is carrying out, and who waits for it.
    power_waits: HashMap<String, PowerWait>,
    next_power: u64,
}

/// The session behind a knowledge call, placed in its workspace.
#[derive(Debug, Clone)]
struct Caller {
    host: String,
    workspace: String,
    project: Option<String>,
    session: Option<String>,
    title: Option<String>,
    cwd: String,
}

impl Caller {
    fn label(&self) -> String {
        self.title
            .clone()
            .or_else(|| self.session.as_ref().map(|s| s.chars().take(8).collect()))
            .unwrap_or_else(|| "another session".into())
    }
}

/// A `claim` call's arguments, as the tool passed them.
struct ClaimSpec<'a> {
    what: &'a str,
    paths: &'a [String],
    until: Option<i64>,
    issue: Option<&'a str>,
}

/// What the core adds to an injection from the live state.
#[derive(Debug, Default)]
struct Around {
    /// Other sessions of the project, one line each (prompts).
    others: Vec<String>,
    /// Live claims covering the file, one line each (edits).
    claimed: Vec<String>,
}

async fn run(mut core: Core, mut rx: mpsc::Receiver<Cmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Input(host, input) => {
                let mut fx = Effects::default();
                let Some(state) = core.states.get_mut(&host) else {
                    continue;
                };
                let mut nonce = None;
                let mut asked_project = None;
                match input {
                    Input::Envelope(env) => {
                        state.apply(&env, &mut fx);
                        if let (Some(launch), Some(id)) =
                            (&env.launch, env.payload["session_id"].as_str())
                        {
                            nonce = Some((launch.clone(), id.to_owned()));
                        }
                        asked_project = start_project(&env);
                    }
                    Input::Agents(list, now, dead) => {
                        state.report_dead(&dead);
                        state.agents(&list, now, &mut fx);
                    }
                    Input::Niri(event) => state.niri(event, &mut fx),
                    Input::Procs(polls, now) => state.resources(polls, now, &mut fx),
                    Input::Usage(limits, now) => state.usage(limits, now, &mut fx),
                    Input::Telemetry(records) => {
                        for r in records {
                            fx.store.push(StoreMsg::Telemetry {
                                ts: r.ts,
                                session: r.session,
                                name: r.name,
                                data: r.data,
                            });
                        }
                    }
                }
                core.flush(&host, fx);
                if let Some((launch, session)) = nonce {
                    core.link(&host, &launch, &session);
                }
                if let Some((session, project)) = asked_project {
                    core.ask_project(&host, &session, project);
                }
                core.link_printed(&host);
            }
            Cmd::Tick(now) => {
                core.expire_launches(now);
                core.expire_waking(now);
                core.workspace_flush(now);
                let hosts: Vec<String> = core.states.keys().cloned().collect();
                for host in hosts {
                    let mut fx = Effects::default();
                    if let Some(state) = core.states.get_mut(&host) {
                        state.tick(now, &mut fx);
                    }
                    core.flush(&host, fx);
                }
            }
            Cmd::Snapshot(reply) => {
                let _ = reply.send(core.snapshot(now_ms()));
            }
            Cmd::Resolve(key, reply) => {
                let _ = reply.send(core.resolve(&key).map(|(_, s)| s.clone()));
            }
            Cmd::Rename(key, title, reply) => {
                let _ = reply.send(core.rename(&key, &title));
            }
            Cmd::SetProject {
                key,
                project,
                reply,
            } => {
                let _ = reply.send(core.set_project(&key, project));
            }
            Cmd::Reply(key, text, reply) => {
                let _ = reply.send(core.reply(&key, &text, false));
            }
            Cmd::MarkResolved { key, reply } => {
                let _ = reply.send(core.reply(&key, wrap_up::PROMPT, true));
            }
            Cmd::KeepOpen { key, reply } => {
                let _ = reply.send(core.keep_open(&key));
            }
            Cmd::Message { key, text, reply } => core.message(&key, &text, reply),
            Cmd::Inbox { session, inbox } => core.set_inbox(&session, inbox),
            Cmd::Delivered { session } => {
                core.acks.insert(session.clone());
                core.set_inbox(&session, Vec::new());
            }
            Cmd::Injected { host, session } => core.injected(&host, &session),
            Cmd::Answer {
                key,
                ask,
                answer,
                reply,
            } => {
                let _ = reply.send(core.answer_session(&key, &ask, &answer, "web"));
            }
            Cmd::Button { key, action, reply } => {
                let _ = reply.send(core.button(&key, &action));
            }
            Cmd::CollectorOpen {
                host,
                conn,
                terminals,
                features,
                addr,
                tx,
            } => core.collector_open(host, conn, terminals, features, addr, tx),
            Cmd::Here { addr, reply } => {
                let _ = reply.send(
                    core.links
                        .iter()
                        .find(|(_, l)| l.addr == Some(addr))
                        .map(|(host, _)| host.clone()),
                );
            }
            Cmd::CollectorClosed { host, conn } => core.collector_closed(&host, conn),
            Cmd::ChannelOpen { host, pid } => core.channel(&host, pid, true),
            Cmd::ChannelClosed { host, pid } => core.channel(&host, pid, false),
            Cmd::Ask { host, id, envelope } => core.open_ask(&host, id, &envelope),
            Cmd::AskGone { host, id } => {
                if let Some(p) = core.pending.get(&id).filter(|p| p.host == host) {
                    let session = p.session.clone();
                    core.pending.remove(&id);
                    let mut fx = Effects::default();
                    if let Some(state) = core.states.get_mut(&host) {
                        state.remove_ask(&session, &id, &mut fx);
                    }
                    core.flush(&host, fx);
                }
            }
            Cmd::Call { host, id, call } => {
                let result = match call {
                    Call::Guard { who } => {
                        let reason = core.guard(&host, &who, now_ms());
                        core.tell(
                            &host,
                            ToCollector::Result {
                                id,
                                result: Ok(Reply::Guard(reason)),
                            },
                        );
                        continue;
                    }
                    Call::Rename { session, title } => core.rename(&session, &title),
                    Call::Reply { session, text } => core.reply(&session, &text, false),
                    Call::Answer {
                        session,
                        ask,
                        answer,
                    } => core.answer_session(&session, &ask, &answer, "desktop"),
                    other => {
                        core.knowledge_call(&host, Some(id), other, now_ms());
                        continue;
                    }
                };
                core.tell(
                    &host,
                    ToCollector::Result {
                        id,
                        result: result.map(|s| Reply::Session(Box::new(s))),
                    },
                );
            }
            Cmd::Queued { host, call, at } => core.knowledge_call(&host, None, call, at),
            Cmd::NotesChanged { notes } => core.notes_changed(notes),
            Cmd::NotesResync { workspace } => core.notes_resync(workspace),
            Cmd::Reflect => core.reflect(),
            Cmd::Launch { req, reply } => {
                let _ = reply.send(core.launch(req));
            }
            Cmd::LaunchOf(id, reply) => {
                let _ = reply.send(core.launches.get(&id).map(|s| s.launch.clone()));
            }
            Cmd::Draft { key, text, reply } => {
                let _ = reply.send(core.set_draft(&key, text));
            }
            Cmd::Edit {
                key,
                host,
                text,
                reply,
            } => {
                let _ = reply.send(core.edit_draft(&key, &host, text));
            }
            Cmd::EditCancel { key, reply } => {
                let _ = reply.send(core.cancel_edit(&key));
            }
            Cmd::Edited {
                host,
                id,
                key,
                text,
                error,
            } => core.edited(&host, &id, &key, text, error),
            Cmd::Launched {
                host,
                launch,
                session,
                error,
            } => core.launched(&host, &launch, session, error),
            Cmd::Stop { key, reply } => match core.stop(&key) {
                Ok(id) => core.stops.entry(id).or_default().push(reply),
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            },
            Cmd::Stopped {
                host,
                session,
                error,
            } => core.stopped(&host, &session, error),
            Cmd::Interrupt { key, reply } => {
                let _ = reply.send(core.interrupt(&key));
            }
            Cmd::Interrupted { host, session, at } => core.interrupted(&host, &session, at),
            Cmd::Forget { key, reply } => {
                let _ = reply.send(core.forget(&key));
            }
            Cmd::Open { key, reply } => {
                let _ = reply.send(core.open(&key));
            }
            Cmd::Dirs { host, dirs } => {
                core.dirs.insert(host, dirs);
            }
            Cmd::Skills { host, skills } => {
                core.skills.insert(host, skills);
            }
            Cmd::Palette { host, palette } => {
                if let Some(status) = core.hosts.get_mut(&host)
                    && status.palette != palette
                {
                    status.palette = palette;
                    core.save_host(&host);
                    core.hosts_changed();
                }
            }
            Cmd::Machine { host, report } => core.machine_report(&host, *report),
            Cmd::HubMachine(report) => {
                core.hub.machine = Some(*report);
                let _ = core.updates.send(Update::Hub {
                    hub: core.hub.clone(),
                });
            }
            Cmd::Power {
                host,
                action,
                force,
                reply,
            } => {
                if let Err(e) = core.power(&host, action, force, reply) {
                    info!("no {} of {host}: {e}", action.name());
                }
            }
            Cmd::PowerDone { host, id, error } => core.power_done(&host, &id, error),
            Cmd::Wake { host, reply } => core.wake_machine(&host, reply),
            Cmd::WakeSent { host, reply } => {
                info!("magic packets sent for {host}");
                if let Some(status) = core.hosts.get_mut(&host) {
                    status.power = Some(Power::Waking { since: now_ms() });
                }
                core.save_host(&host);
                core.hosts_changed();
                let _ = reply.send(core.host_status(&host));
            }
            Cmd::ForgetHost { host, reply } => {
                let _ = reply.send(core.forget_host(&host));
            }
            Cmd::HostSkills { host, reply } => {
                let _ = reply.send(core.skills.get(&host).cloned().unwrap_or_default());
            }
            Cmd::SyncStatus { host, status } => {
                core.sync.insert(host, status);
            }
            Cmd::SyncHosts(reply) => {
                let _ = reply.send(core.sync_hosts());
            }
            Cmd::Holder {
                host,
                session,
                reply,
            } => {
                let _ = reply.send(core.holder(&host, &session).map(|(h, _)| h.to_owned()));
            }
            Cmd::SyncResolved {
                host,
                id,
                error,
                copy,
            } => {
                let store = core.store.clone();
                tokio::spawn(async move {
                    let done = store
                        .exec(move |conn| {
                            crate::sync::hub::resolved(
                                conn,
                                &host,
                                id,
                                error.as_deref(),
                                copy.as_deref(),
                                now_ms(),
                            )
                        })
                        .await;
                    if let Err(e) = done.and_then(|r| r) {
                        warn!("recording a conflict resolution: {e:#}");
                    }
                });
            }
            Cmd::SendTo { host, msg, reply } => {
                let result = if core.links.contains_key(&host) {
                    core.tell(&host, *msg);
                    Ok(())
                } else {
                    Err(format!("{host} is not connected"))
                };
                let _ = reply.send(result);
            }
            Cmd::SyncResync { host, reply } => {
                let _ = reply.send(core.sync_resync(&host));
            }
            Cmd::Locate { host, who, reply } => {
                let _ = reply.send(core.locate(&host, &who).map(|c| Located {
                    workspace: c.workspace,
                    project: c.project,
                    session: c.session,
                }));
            }
            Cmd::HostDirs(reply) => {
                let mut hosts: Vec<(String, Dirs)> = core
                    .links
                    .keys()
                    .map(|h| (h.clone(), core.dirs.get(h).cloned().unwrap_or_default()))
                    .collect();
                hosts.sort();
                let _ = reply.send(hosts);
            }
            Cmd::WorkspaceReport {
                host,
                status,
                units,
                full,
            } => core.workspace_report(&host, status, units, full),
            Cmd::WorkspaceSeen { host, units } => core.workspace_seen(&host, units),
            Cmd::WorkspacePushed { unit, host, bytes } => {
                core.workspace_pushed(&unit, &host, bytes)
            }
            Cmd::WorkspaceTransfer {
                host,
                unit,
                kind,
                bytes_in,
                bytes_out,
                ms,
                ok,
            } => core.workspace_transfer(&host, &unit, &kind, bytes_in, bytes_out, ms, ok),
            Cmd::WorkspaceHeld {
                host,
                unit,
                action,
                commit,
                reply,
            } => core.workspace_held(host, unit, action, commit, reply),
            Cmd::WorkspaceHeldDone { host, unit, error } => {
                if let Some(ws) = &mut core.workspace
                    && let Some(reply) = ws.held_waits.remove(&(host, unit))
                {
                    let _ = reply.send(error.map_or(Ok(()), Err));
                }
            }
            Cmd::CollectorRtt { host, conn, ms } => {
                if core.links.get(&host).is_some_and(|l| l.conn == conn)
                    && let Some(status) = core.hosts.get_mut(&host)
                {
                    status.rtt_ms = Some(ms);
                }
            }
            Cmd::WorkspaceMoved {
                unit,
                host,
                version,
            } => {
                let others: Vec<String> =
                    core.links.keys().filter(|h| **h != host).cloned().collect();
                for other in others {
                    core.tell(
                        &other,
                        ToCollector::WorkspaceChanged {
                            unit: unit.clone(),
                            host: host.clone(),
                            version,
                        },
                    );
                }
            }
            Cmd::WorkspaceSetupChanged => core.workspace_setup_changed(),
            Cmd::WorkspaceResolved { host, id, error } => {
                let store = core.store.clone();
                tokio::spawn(async move {
                    let done = store
                        .exec(move |conn| {
                            wshub::conflict_resolved(conn, &host, id, error.as_deref(), now_ms())
                        })
                        .await;
                    if let Err(e) = done.and_then(|r| r) {
                        warn!("recording a workspace conflict resolution: {e:#}");
                    }
                });
            }
            Cmd::WorkspaceState(reply) => {
                let _ = reply.send(core.workspace_state());
            }
            Cmd::WorkspaceStarting { host, config } => {
                if core
                    .workspace
                    .as_ref()
                    .is_some_and(|ws| ws.setup.version == 0)
                {
                    info!("workspace: taking {host}'s starting configuration");
                    let (store, cmds) = (core.store.clone(), core.cmds.clone());
                    tokio::spawn(async move {
                        let saved = store
                            .exec(move |conn| -> Result<bool> {
                                let (_, version) = wshub::load_config(conn)?;
                                if version != 0 {
                                    return Ok(false);
                                }
                                wshub::save_config(conn, &config, now_ms())?;
                                Ok(true)
                            })
                            .await;
                        match saved {
                            Ok(Ok(true)) => {
                                let _ = cmds.send(Cmd::WorkspaceSetupChanged).await;
                            }
                            Ok(Ok(false)) => {}
                            Ok(Err(e)) => warn!("keeping the starting configuration: {e:#}"),
                            Err(_) => {}
                        }
                    });
                }
            }
        }
    }
}

impl Core {
    fn snapshot(&self, now: i64) -> Snapshot {
        let ids: BTreeSet<&str> = self
            .states
            .values()
            .flat_map(|s| s.sessions().map(|s| s.id.as_str()))
            .collect();
        let mut sessions: Vec<Session> = ids
            .into_iter()
            .filter_map(|id| self.preferred(id).map(|(_, s)| s.clone()))
            .collect();
        sessions.sort_by_key(|s| s.started_at);
        Snapshot {
            host: self.host.clone(),
            now,
            workspaces: self.workspaces.names(),
            hosts: self.hosts.values().cloned().collect(),
            sessions,
            rate_limits: self.rate_limits(),
            hub: Some(self.hub.clone()),
            drafts: self.drafts.values().cloned().collect(),
        }
    }

    /// The account's limits as the freshest host reported them.
    fn rate_limits(&self) -> Option<RateLimits> {
        let mut limits = self
            .states
            .values()
            .filter_map(State::rate_limits)
            .max_by_key(|l| l.updated_at)
            .cloned()?;
        let scoped = self
            .states
            .values()
            .filter_map(State::rate_limits)
            .filter(|l| !l.scoped.is_empty())
            .max_by_key(|l| l.scoped.iter().map(|s| s.read_at).max().unwrap_or(0));
        if let Some(scoped) = scoped {
            limits.scoped = scoped.scoped.clone();
        }
        Some(limits)
    }

    /// Finds a session on any host by id, background short id or unique
    /// id prefix; a session handed between hosts is its `preferred` copy.
    fn resolve(&self, key: &str) -> Option<(&str, &Session)> {
        let mut ids: Vec<&str> = self
            .states
            .values()
            .filter_map(|state| state.resolve(key))
            .map(|s| s.id.as_str())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        match ids.as_slice() {
            [id] => self.preferred(id),
            many if many.contains(&key) => self.preferred(key),
            _ => None,
        }
    }

    /// The copy of session `id` that stands for it when several hosts
    /// know it (one ran it, then another resumed it): a live one, the
    /// earliest opened when two are live, else the one that ended last.
    fn preferred(&self, id: &str) -> Option<(&str, &Session)> {
        self.states
            .iter()
            .filter_map(|(host, st)| Some((host.as_str(), st.session(id)?)))
            .max_by_key(|(_, s)| {
                if s.activity == Activity::Gone {
                    (false, s.activity_since)
                } else {
                    (true, -s.opened_at)
                }
            })
    }

    /// Why the caller's session must not take a prompt on `host`: another
    /// host opened it earlier and still has it open. One session runs on
    /// one machine at a time (`h7.md`).
    fn guard(&self, host: &str, who: &Who, now: i64) -> Option<String> {
        let id = who.session.as_deref()?;
        let (other, _) = self.holder(host, id)?;
        let link = format!("{}/sessions/{id}", self.public_url.trim_end_matches('/'));
        Some(if self.links.contains_key(other) {
            format!(
                "This session is open on {other}, and a session runs on one machine at a time. \
                Stop it there or in ccdash ({link}), then send your prompt again."
            )
        } else {
            let since = self.hosts.get(other).map_or(now, |h| h.since);
            format!(
                "This session was open on {other} when {other} went offline {} ago. If it is \
                closed there, Forget it in ccdash ({link}), then send your prompt again.",
                util::age(now - since)
            )
        })
    }

    /// The host (and its copy) that holds session `id` against `host`: it
    /// has the session open and opened it no later than `host` did.
    fn holder(&self, host: &str, id: &str) -> Option<(&str, &Session)> {
        let opened = self
            .states
            .get(host)
            .and_then(|st| st.session(id))
            .filter(|s| s.activity != Activity::Gone)
            .map(|s| s.opened_at);
        self.states
            .iter()
            .filter(|(h, _)| h.as_str() != host)
            .filter_map(|(h, st)| Some((h.as_str(), st.session(id)?)))
            .filter(|(_, s)| s.activity != Activity::Gone)
            .filter(|(_, s)| opened.is_none_or(|o| s.opened_at <= o))
            .min_by_key(|(_, s)| s.opened_at)
    }

    fn sync_hosts(&self) -> Vec<SyncHost> {
        self.hosts
            .values()
            .map(|h| SyncHost {
                host: h.name.clone(),
                connected: h.connected,
                status: self.sync.get(&h.name).cloned(),
            })
            .collect()
    }

    fn sync_resync(&mut self, host: &str) -> Result<(), String> {
        if !self.links.contains_key(host) {
            return Err(format!("{host} is not connected"));
        }
        if !self.sync.contains_key(host) {
            return Err(format!("{host} does not sync (`sync.enable` is off there)"));
        }
        info!("asking {host} to compare everything again");
        self.tell(host, ToCollector::SyncResync);
        Ok(())
    }

    /// Tells the panel about session `id` as `preferred` has it now; a
    /// session no host holds any more is removed.
    fn show(&mut self, id: &str, host: &str) {
        match self.preferred(id).map(|(_, s)| s.clone()) {
            Some(s) => {
                if self.shown.get(id) != Some(&s) {
                    self.shown.insert(id.to_owned(), s.clone());
                    let _ = self.updates.send(Update::Session {
                        session: Box::new(s),
                    });
                }
            }
            None => {
                if self.shown.remove(id).is_some() {
                    let _ = self.updates.send(Update::Removed {
                        host: host.to_owned(),
                        id: id.to_owned(),
                    });
                }
            }
        }
    }

    fn tell(&mut self, host: &str, msg: ToCollector) {
        if let Some(link) = self.links.get(host)
            && link.tx.try_send(msg).is_err()
        {
            warn!("collector {host}: link backed up, dropping a message");
        }
    }

    fn collector_open(
        &mut self,
        host: String,
        conn: u64,
        terminals: Vec<String>,
        features: HostFeatures,
        addr: Option<IpAddr>,
        tx: mpsc::Sender<ToCollector>,
    ) {
        let now = now_ms();
        info!("collector {host} connected");
        if !self.states.contains_key(&host) {
            let open = self
                .reader
                .as_ref()
                .map(|conn| store::open_background(conn, &host))
                .transpose()
                .unwrap_or_else(|e| {
                    warn!("reading the open sessions of {host}: {e:#}");
                    None
                })
                .unwrap_or_default();
            let state = State::new(host.clone(), terminals, self.workspaces.clone())
                .with_tool_output_bytes(self.tool_output_bytes)
                .with_open_jobs(open);
            self.states.insert(host.clone(), state);
        }
        self.links.insert(
            host.clone(),
            Link {
                conn,
                addr,
                tx: tx.clone(),
            },
        );
        let old = self.hosts.remove(&host);
        if let Some(Power::Waking { since }) = old.as_ref().and_then(|h| h.power) {
            info!(
                "{host} is back {} s after the magic packets",
                (now - since) / 1000
            );
        }
        let (palette, machine) = old.map(|h| (h.palette, h.machine)).unwrap_or_default();
        self.hosts.insert(
            host.clone(),
            HostStatus {
                name: host.clone(),
                connected: true,
                since: now,
                rtt_ms: None,
                palette,
                machine,
                power: None,
                wake: None,
                features,
            },
        );
        self.refresh_wake(&host);
        self.save_host(&host);
        let state = &self.states[&host];
        let mut sessions: Vec<Session> = state.sessions().cloned().collect();
        sessions.sort_by_key(|s| s.started_at);
        let welcome = ToCollector::Welcome {
            v: crate::protocol::VERSION,
            host: self.host.clone(),
            web_url: self.public_url.clone(),
            workspaces: self.workspaces.entries(),
        };
        let snapshot = ToCollector::Snapshot {
            sessions,
            rate_limits: self.rate_limits(),
        };
        self.tell(&host, welcome);
        self.tell(&host, snapshot);
        if let Some(ws) = &self.workspace {
            let setup = ws.setup.clone();
            self.tell(
                &host,
                ToCollector::WorkspaceSetup {
                    setup: Box::new(setup),
                },
            );
        }
        self.push_notes(vec![tx], None);
        self.hosts_changed();
    }

    // ---- the workspace sync ----

    fn workspace_report(
        &mut self,
        host: &str,
        status: WorkspaceStatus,
        units: Vec<UnitStatus>,
        full: bool,
    ) {
        let Some(ws) = &mut self.workspace else {
            return;
        };
        if let Some(cost) = &status.cost {
            ws.take_cost(host, cost);
        }
        let report = ws.reports.entry(host.to_owned()).or_default();
        report.status = status;
        report.at = now_ms();
        if full {
            report.units.clear();
        }
        let store = self.store.clone();
        let host_owned = host.to_owned();
        let changed: Vec<(String, Vec<crate::workspace::ConflictReport>)> = units
            .iter()
            .map(|u| (u.unit.clone(), u.conflicts.clone()))
            .collect();
        for u in units {
            report.units.insert(u.unit.clone(), u);
        }
        tokio::spawn(async move {
            for (unit, conflicts) in changed {
                let host = host_owned.clone();
                let done = store
                    .exec(move |conn| {
                        wshub::reconcile_conflicts(conn, &host, &unit, &conflicts, now_ms())
                    })
                    .await;
                if let Err(e) = done.and_then(|r| r) {
                    warn!("recording workspace conflicts: {e:#}");
                }
            }
        });
        self.refresh_notes();
    }

    fn workspace_seen(&mut self, host: &str, units: Vec<SeenUnit>) {
        if self.workspace.is_none() {
            return;
        }
        let (store, cmds, host) = (self.store.clone(), self.cmds.clone(), host.to_owned());
        tokio::spawn(async move {
            match store
                .exec(move |conn| wshub::seen(conn, &host, &units, now_ms()))
                .await
            {
                Ok(Ok(changed)) => {
                    // NOTE: the waiting list changes with every report; the setup only when something was included
                    let _ = cmds.send(Cmd::WorkspaceSetupChanged).await;
                    let _ = changed;
                }
                Ok(Err(e)) => warn!("recording what a host sees: {e:#}"),
                Err(_) => {}
            }
        });
    }

    fn workspace_pushed(&mut self, unit: &str, host: &str, bytes: u64) {
        let Some(ws) = &self.workspace else {
            return;
        };
        ws.repos.sizes_stale();
        let (repos, store, cmds) = (ws.repos.clone(), self.store.clone(), self.cmds.clone());
        let (unit, host) = (unit.to_owned(), host.to_owned());
        let new_unit = !ws.setup.units.contains_key(&unit);
        tokio::spawn(async move {
            let (repos2, unit2) = (repos.clone(), unit.clone());
            let refs = match tokio::task::spawn_blocking(move || repos2.sync_refs(&unit2)).await {
                Ok(Ok(refs)) => refs,
                Ok(Err(e)) => {
                    warn!("reading the refs of {unit}: {e:#}");
                    return;
                }
                Err(_) => return,
            };
            let (unit3, host3) = (unit.clone(), host.clone());
            let result = store
                .exec(move |conn| wshub::set_refs(conn, &unit3, &refs, &host3, bytes, now_ms()))
                .await;
            match result {
                Ok(Ok((moved, version))) => {
                    if !moved.is_empty() {
                        let _ = cmds
                            .send(Cmd::WorkspaceMoved {
                                unit,
                                host,
                                version,
                            })
                            .await;
                    }
                    if new_unit {
                        let _ = cmds.send(Cmd::WorkspaceSetupChanged).await;
                    }
                }
                Ok(Err(e)) => warn!("recording the refs of {unit}: {e:#}"),
                Err(_) => {}
            }
        });
    }

    fn workspace_setup_changed(&mut self) {
        let Some(ws) = &mut self.workspace else {
            return;
        };
        let Some(conn) = &self.reader else {
            return;
        };
        let setup = match wshub::setup(conn) {
            Ok(setup) => setup,
            Err(e) => {
                warn!("loading the workspace setup: {e:#}");
                return;
            }
        };
        ws.waiting = wshub::new_dirs(conn, &setup.config)
            .unwrap_or_default()
            .into_iter()
            .filter(|n| n.state == "waiting")
            .collect();
        if setup != ws.setup {
            ws.setup = setup.clone();
            let hosts: Vec<String> = self.links.keys().cloned().collect();
            for host in hosts {
                self.tell(
                    &host,
                    ToCollector::WorkspaceSetup {
                        setup: Box::new(setup.clone()),
                    },
                );
            }
        }
        self.refresh_notes();
    }

    fn workspace_state(&self) -> Option<WorkspaceState> {
        let ws = self.workspace.as_ref()?;
        let mut names: BTreeSet<String> = self.hosts.keys().cloned().collect();
        names.extend(ws.reports.keys().cloned());
        let hosts = names
            .into_iter()
            .map(|name| WorkspaceHost {
                connected: self.hosts.get(&name).is_some_and(|h| h.connected),
                rtt_ms: self.hosts.get(&name).and_then(|h| h.rtt_ms),
                status: ws.reports.get(&name).map(|r| r.status.clone()),
                host: name,
            })
            .collect();
        Some(WorkspaceState {
            hosts,
            reports: ws
                .reports
                .iter()
                .map(|(h, r)| (h.clone(), r.units.clone()))
                .collect(),
            config: ws.setup.config.clone(),
            version: ws.setup.version,
        })
    }

    /// Counts a smart HTTP request into the host's traffic; a large one
    /// is kept on its own for the throughput view.
    #[allow(clippy::too_many_arguments)]
    fn workspace_transfer(
        &mut self,
        host: &str,
        unit: &str,
        kind: &str,
        bytes_in: u64,
        bytes_out: u64,
        ms: u64,
        ok: bool,
    ) {
        let Some(ws) = &mut self.workspace else {
            return;
        };
        let d = ws
            .traffic
            .entry((host.to_owned(), kind.to_owned()))
            .or_default();
        d.requests += 1;
        d.bytes_in += bytes_in;
        d.bytes_out += bytes_out;
        d.ms += ms;
        if !ok {
            d.failures += 1;
        }
        let bytes = bytes_in.max(bytes_out);
        if ok && bytes >= TRANSFER_MIN {
            let t = Transfer {
                host: host.to_owned(),
                unit: unit.to_owned(),
                kind: kind.to_owned(),
                bytes,
                ms,
                at: now_ms(),
            };
            let store = self.store.clone();
            tokio::spawn(async move {
                if let Err(e) = store
                    .exec(move |conn| wshub::add_transfer(conn, &t))
                    .await
                    .and_then(|r| r)
                {
                    warn!("recording a workspace transfer: {e:#}");
                }
            });
        }
    }

    /// Writes the traffic and cost deltas of the last minute to their
    /// rows of the hour.
    fn workspace_flush(&mut self, now: i64) {
        let Some(ws) = &mut self.workspace else {
            return;
        };
        if now - ws.last_flush < FLUSH_MS {
            return;
        }
        ws.last_flush = now;
        let traffic = std::mem::take(&mut ws.traffic);
        let cost = std::mem::take(&mut ws.cost_acc);
        if traffic.is_empty() && cost.is_empty() {
            return;
        }
        let hour = now - now.rem_euclid(HOUR_MS);
        let store = self.store.clone();
        tokio::spawn(async move {
            let result = store
                .exec(move |conn| {
                    for ((host, kind), d) in &traffic {
                        wshub::add_traffic(conn, host, hour, kind, d)?;
                    }
                    for (host, c) in &cost {
                        wshub::add_cost(conn, host, hour, c)?;
                    }
                    Ok(())
                })
                .await
                .and_then(|r| r);
            if let Err(e) = result {
                warn!("recording workspace traffic and cost: {e:#}");
            }
        });
    }

    /// Hands a held action to `host` and keeps the asker until the
    /// collector answers (`WorkspaceHeldDone`).
    fn workspace_held(
        &mut self,
        host: String,
        unit: String,
        action: HeldAction,
        commit: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    ) {
        let Some(ws) = &mut self.workspace else {
            let _ = reply.send(Err("the workspace sync is off".into()));
            return;
        };
        if !self.links.contains_key(&host) {
            let _ = reply.send(Err(format!("{host} is not connected")));
            return;
        }
        if let Some(previous) = ws.held_waits.insert((host.clone(), unit.clone()), reply) {
            let _ = previous.send(Err("superseded by a later choice".into()));
        }
        self.tell(
            &host,
            ToCollector::WorkspaceHeld {
                unit,
                action,
                commit,
            },
        );
    }

    /// Notification candidates of the workspace sync: held snapshots,
    /// conflicts, new directories over the threshold, units failing for a
    /// while.
    fn workspace_notes(&self, now: i64) -> Vec<Note> {
        let Some(ws) = &self.workspace else {
            return Vec::new();
        };
        let page = format!("{}/workspace/unit", self.public_url);
        let note =
            |seq: String, at: i64, title: String, body: String, unit: &str, category: Category| {
                Note {
                    seq,
                    opened_at: at,
                    focused: false,
                    content: crate::notify::Content {
                        title,
                        body,
                        session: String::new(),
                        url: format!("{page}/{unit}"),
                        key: None,
                        buttons: Vec::new(),
                        more: false,
                        urgent: true,
                        category,
                    },
                }
            };
        let mut out = Vec::new();
        for (host, report) in &ws.reports {
            for (unit, u) in &report.units {
                if let Some(h) = &u.held
                    && h.at >= ws.started
                {
                    out.push(note(
                        format!("ws-held-{host}-{unit}"),
                        h.at,
                        format!(
                            "{unit}: {} files added, {} deleted on {host}",
                            h.added, h.deleted
                        ),
                        "The snapshot is held: apply or drop it on the Workspace page.".into(),
                        unit,
                        Category::WsHeld,
                    ));
                }
                for c in &u.conflicts {
                    if c.at >= ws.started {
                        out.push(note(
                            format!("ws-conflict-{host}-{unit}-{}", c.path),
                            c.at,
                            format!("{unit}: conflict in {}", c.path),
                            format!(
                                "Edited on {host} and {} at once: fix the markers or choose a side on the Workspace page.",
                                c.other_host
                            ),
                            unit,
                            Category::WsConflict,
                        ));
                    }
                }
                if let Some(since) = u.failing_since
                    && now - since >= FAILING_MS
                    && since >= ws.started
                {
                    out.push(note(
                        format!("ws-fail-{host}-{unit}"),
                        since + FAILING_MS,
                        format!("{unit} keeps failing on {host}"),
                        u.detail
                            .clone()
                            .unwrap_or_else(|| "see the Workspace page".into()),
                        unit,
                        Category::WsFailing,
                    ));
                }
            }
        }
        for n in &ws.waiting {
            if n.at >= ws.started {
                out.push(note(
                    format!("ws-new-{}", n.unit),
                    n.at,
                    format!("New directory {} ({} MB)", n.unit, n.size >> 20),
                    "Larger than the auto-include threshold: include or ignore it on the Workspace page.".into(),
                    &n.unit,
                    Category::WsNew,
                ));
            }
        }
        out
    }

    /// Recomputes the notification candidates (sessions and the workspace).
    fn refresh_notes(&self) {
        let Some(notes) = &self.notes else {
            return;
        };
        let keys: HashMap<&str, &str> = self
            .pending
            .iter()
            .map(|(id, p)| (id.as_str(), p.key.as_str()))
            .collect();
        let now = now_ms();
        let mut next: Vec<Note> = self
            .states
            .values()
            .flat_map(|state| notes.composer.notes(state, &keys, now))
            .collect();
        next.extend(self.workspace_notes(now));
        next.sort_by(|a, b| a.seq.cmp(&b.seq));
        notes.tx.send_if_modified(|current| {
            let changed = *current != next;
            if changed {
                *current = next;
            }
            changed
        });
    }

    /// Sends the live notes of `workspace` (or every workspace) to the
    /// given collector links.
    fn push_notes(&self, links: Vec<mpsc::Sender<ToCollector>>, workspace: Option<String>) {
        let names = match workspace {
            Some(ws) => vec![ws],
            None => self.workspaces.names(),
        };
        let store = self.store.clone();
        tokio::spawn(async move {
            for ws in names {
                let name = ws.clone();
                let notes = match store.exec(move |conn| knowledge::active(conn, &name)).await {
                    Ok(Ok(notes)) => notes,
                    Ok(Err(e)) => {
                        warn!("loading notes of {ws}: {e:#}");
                        continue;
                    }
                    Err(_) => return,
                };
                for tx in &links {
                    let _ = tx
                        .send(ToCollector::Notes {
                            workspace: ws.clone(),
                            notes: notes.clone(),
                        })
                        .await;
                }
            }
        });
    }

    fn notes_changed(&mut self, notes: Vec<knowledge::Note>) {
        let mut workspaces: Vec<String> = Vec::new();
        for note in notes {
            if !workspaces.contains(&note.workspace) {
                workspaces.push(note.workspace.clone());
            }
            let hosts: Vec<String> = self.links.keys().cloned().collect();
            for host in hosts {
                self.tell(
                    &host,
                    ToCollector::NoteChanged {
                        note: Box::new(note.clone()),
                    },
                );
            }
        }
        for workspace in workspaces {
            let _ = self.updates.send(Update::Notes { workspace });
        }
    }

    fn notes_resync(&self, workspace: Option<String>) {
        let links: Vec<_> = self.links.values().map(|l| l.tx.clone()).collect();
        let names = match &workspace {
            Some(ws) => vec![ws.clone()],
            None => self.workspaces.names(),
        };
        if !links.is_empty() {
            self.push_notes(links, workspace);
        }
        for workspace in names {
            let _ = self.updates.send(Update::Notes { workspace });
        }
    }

    /// Runs the lifecycle pass and resends the caches it changed.
    fn reflect(&self) {
        if !self.knowledge.reflect {
            return;
        }
        let (unverified, unrecalled) = (
            self.knowledge.stale_unverified_days,
            self.knowledge.stale_unrecalled_days,
        );
        let (store, cmds) = (self.store.clone(), self.cmds.clone());
        tokio::spawn(async move {
            let now = now_ms();
            match store
                .exec(move |conn| knowledge::reflect(conn, now, unverified, unrecalled))
                .await
            {
                Ok(Ok(flagged)) => {
                    if !flagged.is_empty() {
                        info!("reflect: {} notes flagged", flagged.len());
                        let _ = cmds.send(Cmd::NotesResync { workspace: None }).await;
                    }
                }
                Ok(Err(e)) => warn!("reflect: {e:#}"),
                Err(_) => {}
            }
        });
    }

    /// Places a knowledge call's author: the session the collector's pid
    /// or id names, else the directory it gave.
    fn locate(&self, host: &str, who: &Who) -> Result<Caller, String> {
        let state = self.states.get(host);
        let session = state.and_then(|st| {
            who.pid
                .and_then(|pid| {
                    st.sessions()
                        .find(|s| s.pid == Some(pid) && s.activity != Activity::Gone)
                })
                .or_else(|| who.session.as_deref().and_then(|id| st.session(id)))
        });
        let cwd = session
            .map(|s| s.cwd.as_str())
            .or(who.cwd.as_deref())
            .filter(|c| !c.is_empty())
            .ok_or("cannot tell which session is asking")?;
        // NOTE: a session's project may be pinned (launch, claim, panel): never re-derive it
        let (workspace, project) = match session {
            Some(s) => (
                s.workspace
                    .clone()
                    .ok_or_else(|| self.workspaces.outside(cwd))?,
                s.project.clone(),
            ),
            None => self
                .workspaces
                .locate(cwd)
                .ok_or_else(|| self.workspaces.outside(cwd))?,
        };
        Ok(Caller {
            host: host.to_owned(),
            workspace,
            project,
            session: session
                .map(|s| s.id.clone())
                .or_else(|| who.session.clone()),
            title: session.and_then(|s| s.title.clone()),
            cwd: cwd.to_owned(),
        })
    }

    /// Places a session started with `claude --project`, or keeps why not
    /// for its startup context: a bad name does not stop the session.
    fn ask_project(&mut self, host: &str, session: &str, project: String) {
        if let Err(e) = self.set_project(session, Some(project)) {
            warn!("session {session} on {host}: {e}");
            self.refused_projects.insert(session.to_owned(), e);
        }
    }

    /// The directory of `project` in the workspace holding `cwd`, else the
    /// workspace's, else `cwd` itself: the base claims resolve against and
    /// are shown relative to.
    fn root_of(&self, cwd: &str, project: Option<&str>) -> String {
        match self.workspaces.of(cwd) {
            Some(ws) => {
                let dir = self.workspaces.dir(&ws).unwrap_or(cwd);
                match project {
                    Some(p) => format!("{dir}/{p}"),
                    None => dir.to_owned(),
                }
            }
            None => cwd.to_owned(),
        }
    }

    /// The projects a session of `workspace` may be placed under: the
    /// directories directly under it on any host that reported them.
    fn known_projects(&self, workspace: &str) -> BTreeSet<String> {
        self.dirs
            .values()
            .filter_map(|dirs| dirs.get(workspace))
            .flatten()
            .cloned()
            .collect()
    }

    /// Pins the project of the session `key` names (None: the directory's
    /// again); the updated session, or why not.
    fn set_project(&mut self, key: &str, project: Option<String>) -> Result<Session, String> {
        let (host, s) = self
            .resolve(key)
            .ok_or_else(|| format!("no session matches {key:?}"))?;
        let (host, id) = (host.to_owned(), s.id.clone());
        let workspace = s
            .workspace
            .clone()
            .ok_or("the session runs outside every ccdash workspace")?;
        if let Some(p) = &project
            && !self.known_projects(&workspace).contains(p)
        {
            return Err(format!(
                "{p:?} is not a project of {workspace}: no such directory under it on any host"
            ));
        }
        let mut fx = Effects::default();
        if let Some(state) = self.states.get_mut(&host) {
            state.set_project(&id, project, &mut fx);
        }
        self.flush(&host, fx);
        self.resolve(&id)
            .map(|(_, s)| s.clone())
            .ok_or_else(|| "the session is gone".into())
    }

    /// Carries out a knowledge call from `host`; with an `id` the result
    /// goes back to the collector, without one (a replayed write) it is
    /// only logged.
    fn knowledge_call(&mut self, host: &str, id: Option<u64>, call: Call, at: i64) {
        let link = self.links.get(host).map(|l| l.tx.clone());
        let host = host.to_owned();
        let origin = host.clone();
        let answer = move |result: Result<Reply, String>| {
            let Some(id) = id else {
                if let Err(e) = &result {
                    warn!("replayed knowledge write from {origin}: {e}");
                }
                return;
            };
            if let Some(tx) = link
                && tx.try_send(ToCollector::Result { id, result }).is_err()
            {
                warn!("collector {origin}: link backed up, dropping a result");
            }
        };
        let who = match &call {
            Call::Note { who, .. }
            | Call::Recall { who, .. }
            | Call::Inject { who, .. }
            | Call::Tell { who, .. }
            | Call::Claim { who, .. }
            | Call::Release { who } => Some(who),
            _ => None,
        };
        let caller = match who {
            Some(who) => match self.locate(&host, who) {
                Ok(c) => Some(c),
                Err(e) => return answer(Err(e)),
            },
            None => None,
        };
        let store = self.store.clone();
        let cmds = self.cmds.clone();
        match call {
            Call::Note { op, .. } => {
                let c = caller.expect("located above");
                let by = c.session.clone();
                let known = self.known_projects(&c.workspace);
                tokio::spawn(async move {
                    let result = store
                        .exec(move |conn| {
                            if let Some(p) = op.project()
                                && !known.contains(p)
                                && !knowledge::has_project(conn, &c.workspace, p)?
                            {
                                bail!(
                                    "{p:?} is not a project of {}: no such directory under the \
                                        workspace on any host, and no notes under it",
                                    c.workspace
                                );
                            }
                            let w = knowledge::apply(
                                conn,
                                &c.workspace,
                                c.project.as_deref(),
                                by.as_deref(),
                                &op,
                                at,
                            )?;
                            let mut slugs = vec![w.slug.clone()];
                            if let knowledge::NoteOp::Remember {
                                supersedes: Some(target),
                                ..
                            } = &op
                            {
                                slugs.push(target.clone());
                            }
                            let mut notes = Vec::new();
                            for slug in slugs {
                                if let Some(n) = knowledge::get(conn, &c.workspace, &slug)? {
                                    notes.push(n);
                                }
                            }
                            Ok::<_, anyhow::Error>((w, notes))
                        })
                        .await;
                    match result {
                        Ok(Ok((w, notes))) => {
                            answer(Ok(Reply::Written(w)));
                            let _ = cmds.send(Cmd::NotesChanged { notes }).await;
                        }
                        Ok(Err(e)) => answer(Err(format!("{e:#}"))),
                        Err(e) => answer(Err(format!("{e:#}"))),
                    }
                });
            }
            Call::Recall { query, .. } => {
                let c = caller.expect("located above");
                tokio::spawn(async move {
                    let result = store
                        .exec(move |conn| recall(conn, &c.workspace, &query, at))
                        .await;
                    answer(flatten(result));
                });
            }
            Call::Inject { what, .. } => {
                let c = caller.expect("located above");
                let around = match &what {
                    Injection::Prompt { .. } => Around {
                        others: self.others_active(&c, at),
                        ..Around::default()
                    },
                    Injection::Edit { path } => Around {
                        claimed: self.claimed_by_others(&c, path, at),
                        ..Around::default()
                    },
                    Injection::SessionStart => Around::default(),
                };
                let refused = (what == Injection::SessionStart)
                    .then_some(c.session.as_ref())
                    .flatten()
                    .and_then(|s| self.refused_projects.remove(s));
                tokio::spawn(async move {
                    let session = c.session.clone();
                    let result = store
                        .exec(move |conn| inject(conn, &c, &what, &around, at))
                        .await;
                    let delivered = matches!(&result, Ok(Ok((_, true))));
                    answer(match result {
                        Ok(Ok((text, _))) => Ok(Reply::Context(refused_note(refused, text))),
                        Ok(Err(e)) | Err(e) => Err(format!("{e:#}")),
                    });
                    if delivered && let Some(session) = session {
                        let _ = cmds.send(Cmd::Delivered { session }).await;
                    }
                });
            }
            Call::Tell { to, text, .. } => {
                let c = caller.expect("located above");
                self.tell_session(&c, &to, &text, id.is_none(), at, answer);
            }
            Call::Claim {
                what,
                paths,
                until,
                issue,
                project,
                ..
            } => {
                let mut c = caller.expect("located above");
                if let Some(p) = project {
                    if !self.known_projects(&c.workspace).contains(&p) {
                        return answer(Err(format!(
                            "{p:?} is not a project of {}: no such directory under the workspace \
                                on any host",
                            c.workspace
                        )));
                    }
                    let Some(session) = c.session.clone() else {
                        return answer(Err("cannot tell which session is claiming".into()));
                    };
                    let mut fx = Effects::default();
                    if let Some(state) = self.states.get_mut(&c.host) {
                        state.set_project(&session, Some(p.clone()), &mut fx);
                    }
                    self.flush(&c.host, fx);
                    c.project = Some(p);
                }
                let spec = ClaimSpec {
                    what: &what,
                    paths: &paths,
                    until,
                    issue: issue.as_deref(),
                };
                answer(self.claim(&c, Some(spec), id.is_none(), at));
            }
            Call::Release { .. } => {
                let c = caller.expect("located above");
                answer(self.claim(&c, None, id.is_none(), at));
            }
            Call::Import { dir, notes } => {
                let Some((workspace, _)) = self.workspaces.locate(&dir) else {
                    return answer(Err(self.workspaces.outside(&dir)));
                };
                tokio::spawn(async move {
                    let ws = workspace.clone();
                    let result = store
                        .exec(move |conn| {
                            let tx = conn.unchecked_transaction()?;
                            let (mut added, mut updated, mut unchanged, mut kept) = (0, 0, 0, 0);
                            for up in &notes {
                                let w = knowledge::upsert(&tx, &ws, up, Some("import"), at)
                                    .with_context(|| format!("importing {}", up.slug))?;
                                match (w.created, w.changed, w.kept) {
                                    (true, _, _) => added += 1,
                                    (false, true, _) => updated += 1,
                                    (false, false, true) => kept += 1,
                                    (false, false, false) => unchanged += 1,
                                }
                            }
                            tx.commit()?;
                            Ok::<_, anyhow::Error>(Reply::Imported {
                                added,
                                updated,
                                unchanged,
                                kept,
                            })
                        })
                        .await;
                    let changed = matches!(
                        &result,
                        Ok(Ok(Reply::Imported {
                            added,
                            updated,
                            ..
                        })) if added + updated > 0
                    );
                    answer(flatten(result));
                    if changed {
                        let _ = cmds
                            .send(Cmd::NotesResync {
                                workspace: Some(workspace),
                            })
                            .await;
                    }
                });
            }
            Call::Rename { .. } | Call::Reply { .. } | Call::Answer { .. } | Call::Guard { .. } => {
                answer(Err("not a knowledge call".into()));
            }
        }
    }

    /// Live sessions of the caller's project, on any host, as lines for
    /// the prompt injection; parked ones only when they hold a claim.
    fn others_active(&self, c: &Caller, now: i64) -> Vec<String> {
        let mut lines = Vec::new();
        for (host, state) in &self.states {
            for s in state.sessions() {
                if Some(&s.id) == c.session.as_ref()
                    || s.activity == Activity::Gone
                    || (s.activity == Activity::Parked && s.claim.is_none())
                    || s.workspace.as_deref() != Some(c.workspace.as_str())
                {
                    continue;
                }
                if s.project != c.project {
                    continue;
                }
                let doing = match s.activity {
                    Activity::Working => "working",
                    Activity::Permission => "waiting for a permission",
                    Activity::YourTurn => "waiting for the user",
                    Activity::Idle => "idle",
                    Activity::Parked => "parked",
                    Activity::Gone => continue,
                };
                let since = (now - s.activity_since).max(0) / MINUTE_MS;
                let files = s
                    .turn
                    .as_ref()
                    .filter(|t| !t.files.is_empty())
                    .map(|t| {
                        let shown: Vec<&str> = t.files.iter().take(4).map(String::as_str).collect();
                        format!("; editing {}", shown.join(", "))
                    })
                    .unwrap_or_default();
                let claim = s
                    .claim
                    .as_ref()
                    .map(|claim| {
                        format!(
                            "; claims {}",
                            claims::describe(
                                claim,
                                &self.root_of(&s.cwd, s.project.as_deref()),
                                now
                            )
                        )
                    })
                    .unwrap_or_default();
                lines.push(format!(
                    "- {}: {doing} for {since} min{files}{claim}",
                    session_ref(s, host)
                ));
            }
        }
        lines.sort();
        lines
    }

    /// Other live sessions of the caller's workspace whose claim covers
    /// `path`, one line each.
    fn claimed_by_others(&self, c: &Caller, path: &str, now: i64) -> Vec<String> {
        let mut lines = Vec::new();
        for (host, state) in &self.states {
            for s in state.sessions() {
                let Some(claim) = &s.claim else { continue };
                if Some(&s.id) == c.session.as_ref()
                    || s.activity == Activity::Gone
                    || s.workspace.as_deref() != Some(c.workspace.as_str())
                    || !claims::covers(claim, path)
                {
                    continue;
                }
                let since = (now - claim.since).max(0) / MINUTE_MS;
                lines.push(format!(
                    "{} claims {}, since {since} min ago",
                    session_ref(s, host),
                    claims::describe(claim, &self.root_of(&s.cwd, s.project.as_deref()), now)
                ));
            }
        }
        lines.sort();
        lines
    }

    /// Stores `text` from the caller for another session of its
    /// workspace, to be handed over at that session's next prompt. A
    /// `replayed` message (queued while the hub was away) may name a
    /// session that has ended meanwhile: it is kept for a resume.
    fn tell_session(
        &mut self,
        c: &Caller,
        to: &str,
        text: &str,
        replayed: bool,
        at: i64,
        answer: impl FnOnce(Result<Reply, String>) + Send + 'static,
    ) {
        let live = self.resolve(to).map(|(host, s)| {
            (
                s.id.clone(),
                host.to_owned(),
                s.workspace.clone(),
                s.activity == Activity::Gone,
            )
        });
        let target = match live {
            Some((id, host, workspace, gone)) => {
                if workspace.as_deref() != Some(c.workspace.as_str()) {
                    return answer(Err("that session works in another workspace".into()));
                }
                if Some(&id) == c.session.as_ref() {
                    return answer(Err("that is this session".into()));
                }
                if gone && !replayed {
                    return answer(Err("that session has ended".into()));
                }
                Some((id, host))
            }
            // NOTE: right after a hub restart a live session is only in the store
            None => None,
        };
        let out = Stored {
            workspace: c.workspace.clone(),
            from_session: c.session.clone(),
            from_host: Some(c.host.clone()),
            from_label: c.label(),
            text: text.to_owned(),
        };
        // NOTE: a replayed message may name a session the state has dropped
        let (to, workspaces, workspace) =
            (to.to_owned(), self.workspaces.clone(), c.workspace.clone());
        let find = move |conn: &Connection| -> Result<(String, String)> {
            let mut stmt = conn.prepare(
                "SELECT id, host, cwd, ended_at FROM sessions
                    WHERE id = ?1 OR id LIKE ?1 || '%' LIMIT 2",
            )?;
            type Row = (String, String, String, Option<i64>);
            let rows: Vec<Row> = stmt
                .query_map([&to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let (id, host, cwd, ended) = match rows.as_slice() {
                [one] => one.clone(),
                [] => bail!("no session matches {to:?}"),
                _ => bail!("{to:?} matches more than one session"),
            };
            if workspaces.of(&cwd).as_deref() != Some(workspace.as_str()) {
                bail!("that session works in another workspace");
            }
            if ended.is_some() && !replayed {
                bail!("that session has ended");
            }
            Ok((id, host))
        };
        self.store_message(target, Some(Box::new(find)), out, at, move |result| {
            answer(result.map(|_| Reply::Sent))
        });
    }

    /// The owner's message to the session `key` names, from the panel.
    fn message(&mut self, key: &str, text: &str, reply: oneshot::Sender<Result<Message, String>>) {
        let found = self
            .resolve(key)
            .ok_or_else(|| format!("no session matches {key:?}"))
            .and_then(|(host, s)| {
                if s.activity == Activity::Gone {
                    return Err("the session has ended".to_owned());
                }
                if !s.hooked {
                    return Err(
                        "the session runs without the ccdash hooks, so nothing reaches it".into(),
                    );
                }
                Ok((
                    s.id.clone(),
                    host.to_owned(),
                    s.workspace.clone().unwrap_or_default(),
                ))
            });
        let (id, host, workspace) = match found {
            Ok(found) => found,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        let out = Stored {
            workspace,
            from_session: None,
            from_host: None,
            from_label: messages::OWNER.into(),
            text: text.to_owned(),
        };
        self.store_message(Some((id, host)), None, out, now_ms(), move |result| {
            let _ = reply.send(result);
        });
    }

    /// Stores a message for `target` (session id and host), found through
    /// `find` on the sessions table when the live state has no such
    /// session; refreshes the recipient's inbox.
    fn store_message(
        &self,
        target: Option<(String, String)>,
        find: Option<Finder>,
        out: Stored,
        at: i64,
        done: impl FnOnce(Result<Message, String>) + Send + 'static,
    ) {
        let (store, cmds) = (self.store.clone(), self.cmds.clone());
        tokio::spawn(async move {
            let result = store
                .exec(move |conn| -> Result<(Message, Vec<Message>)> {
                    let (id, host) = match target {
                        Some(target) => target,
                        None => find.context("no recipient")?(conn)?,
                    };
                    let message = messages::send(
                        conn,
                        &messages::Outgoing {
                            workspace: &out.workspace,
                            from_session: out.from_session.as_deref(),
                            from_host: out.from_host.as_deref(),
                            from_label: &out.from_label,
                            to_session: &id,
                            to_host: Some(&host),
                            text: &out.text,
                        },
                        at,
                    )?;
                    let inbox = messages::previews(&messages::pending(conn, &id)?, PREVIEW_LEN);
                    Ok((message, inbox))
                })
                .await;
            match result {
                Ok(Ok((message, inbox))) => {
                    let session = message.to_session.clone();
                    done(Ok(message));
                    let _ = cmds.send(Cmd::Inbox { session, inbox }).await;
                }
                Ok(Err(e)) | Err(e) => done(Err(format!("{e:#}"))),
            }
        });
    }

    /// Shows the messages waiting for `session`, wherever it runs.
    fn set_inbox(&mut self, session: &str, inbox: Vec<Message>) {
        let host = self
            .states
            .iter()
            .find(|(_, st)| st.session(session).is_some())
            .map(|(host, _)| host.clone());
        let Some(host) = host else { return };
        let mut fx = Effects::default();
        if let Some(state) = self.states.get_mut(&host) {
            state.set_inbox(session, inbox, &mut fx);
        }
        self.flush(&host, fx);
    }

    /// The prompt hook of `session` printed the context that carried its
    /// messages: they were read.
    fn injected(&mut self, host: &str, session: &str) {
        if !self.acks.remove(session)
            || self
                .states
                .get(host)
                .is_none_or(|st| st.session(session).is_none())
        {
            return;
        }
        let (store, session) = (self.store.clone(), session.to_owned());
        tokio::spawn(async move {
            let now = now_ms();
            if let Ok(Err(e)) = store
                .exec(move |conn| messages::acknowledge(conn, &session, now))
                .await
            {
                warn!("marking messages read: {e:#}");
            }
        });
    }

    /// Sets the caller's claim (`what`, paths, minutes until it lapses) or
    /// releases it. A `replayed` call may come before the collector has
    /// reported the session again after a hub restart: the claim then
    /// goes to the database and is restored with the session.
    fn claim(
        &mut self,
        c: &Caller,
        spec: Option<ClaimSpec<'_>>,
        replayed: bool,
        at: i64,
    ) -> Result<Reply, String> {
        let id = c
            .session
            .clone()
            .ok_or("cannot tell which session is claiming")?;
        let live = self
            .states
            .get(&c.host)
            .and_then(|st| st.session(&id))
            .is_some_and(|s| s.activity != Activity::Gone);
        if !live && !replayed {
            return Err("the hub does not see this session running".into());
        }
        let claim = match spec {
            Some(spec) => {
                // NOTE: paths are relative to the project (a root session has a pinned one), not to cwd
                let root = self.root_of(&c.cwd, c.project.as_deref());
                let claim = claims::new(
                    spec.what, spec.paths, spec.until, spec.issue, &root, &root, at,
                )?;
                if claim.until.is_some_and(|u| u <= now_ms()) {
                    return Err("that claim has already lapsed".into());
                }
                Some(claim)
            }
            None => None,
        };
        if !live {
            let (store, saved) = (self.store.clone(), claim.clone());
            tokio::spawn(async move {
                if let Ok(Err(e)) = store
                    .exec(move |conn| claims::save(conn, &id, saved.as_ref()))
                    .await
                {
                    warn!("storing a replayed claim: {e:#}");
                }
            });
            return Ok(Reply::Claimed(claim));
        }
        let mut fx = Effects::default();
        if let Some(state) = self.states.get_mut(&c.host) {
            state.set_claim(&id, claim.clone(), &mut fx);
        }
        self.flush(&c.host, fx);
        Ok(Reply::Claimed(claim))
    }

    /// Asks `req.host` to start the session and keeps the launch until
    /// the session reports (its hooks carry the launch id) or it fails.
    fn launch(&mut self, req: LaunchReq) -> Result<Launch, String> {
        if !self.links.contains_key(&req.host) {
            return Err(format!("{} is not connected", req.host));
        }
        let id = util::random_hex(16).map_err(|e| format!("{e:#}"))?;
        let launch = Launch {
            id: id.clone(),
            host: req.host.clone(),
            workspace: req.workspace,
            project: req.project,
            resume: req.resume.clone(),
            at: now_ms(),
            state: LaunchState::Pending,
        };
        info!(
            "launch {id} on {}: {}{}",
            req.host,
            req.cwd,
            req.resume
                .as_deref()
                .map(|r| format!(", continuing {r}"))
                .unwrap_or_default()
        );
        self.tell(
            &req.host,
            ToCollector::Launch {
                launch: id.clone(),
                cwd: req.cwd.clone(),
                prompt: req.prompt,
                model: req.model,
                permission_mode: req.permission_mode,
                effort: req.effort,
                resume: req.resume,
            },
        );
        self.launches.insert(
            id,
            Slot {
                launch: launch.clone(),
                cwd: req.cwd,
                issue: req.issue,
                short: None,
                wakes: None,
            },
        );
        let _ = self.updates.send(Update::Launch {
            launch: launch.clone(),
        });
        Ok(launch)
    }

    fn launched(
        &mut self,
        host: &str,
        launch: &str,
        session: Option<String>,
        error: Option<String>,
    ) {
        let Some(slot) = self
            .launches
            .get_mut(launch)
            .filter(|s| s.launch.host == host && s.launch.state == LaunchState::Pending)
        else {
            return;
        };
        match (session, error) {
            (_, Some(error)) => self.settle(launch, LaunchState::Failed { error }),
            (Some(short), None) => {
                slot.short = Some(short);
                self.link_printed(host);
            }
            (None, None) => self.settle(
                launch,
                LaunchState::Failed {
                    error: "claude --bg reported nothing".into(),
                },
            ),
        }
    }

    /// The session behind a pending launch reported: flag it and settle.
    fn link(&mut self, host: &str, launch: &str, session: &str) {
        let pending = self
            .launches
            .get(launch)
            .is_some_and(|s| s.launch.host == host && s.launch.state == LaunchState::Pending);
        if !pending {
            return;
        }
        let slot = self.launches.get(launch);
        let project = slot.and_then(|s| s.launch.project.clone());
        let woken = slot
            .and_then(|s| s.wakes.clone())
            .filter(|old| old != session);
        // NOTE: Claude Code continues a session as a copy when options change or it still runs
        let copied = slot
            .and_then(|s| s.launch.resume.clone())
            .filter(|old| old != session);
        let claim = slot.and_then(|s| s.issue.as_ref()).and_then(|issue| {
            let what = util::truncate(&issue.title, MAX_CLAIM_TITLE);
            claims::new(&what, &[], None, Some(&issue.slug), "", "", now_ms())
                .inspect_err(|e| warn!("claiming {} for {session}: {e}", issue.slug))
                .ok()
        });
        let mut fx = Effects::default();
        let known = self.states.get_mut(host).is_some_and(|st| {
            let known = st.set_launched(session, project.as_deref(), &mut fx);
            if let Some(old) = woken.as_deref().filter(|_| known) {
                st.replaced(old, now_ms(), &mut fx);
            }
            if known && claim.is_some() {
                st.set_claim(session, claim, &mut fx);
            }
            known
        });
        self.flush(host, fx);
        if known {
            if let Some(old) = copied {
                self.continued(host, session, &old);
            }
            self.settle(
                launch,
                LaunchState::Started {
                    session: session.to_owned(),
                },
            );
        }
    }

    /// `session` on `host` is the copy a resume of `old` came back as:
    /// links the two (the old one may be another host's, or only the
    /// store's) and hands the copy the conversation's title, so the
    /// panel shows one conversation that went on.
    fn continued(&mut self, host: &str, session: &str, old: &str) {
        let live = self.states.iter().find_map(|(h, st)| {
            let s = st.session(old)?;
            Some((
                h.clone(),
                RanOn::from(s),
                s.title.clone().or(s.name.clone()),
            ))
        });
        let (old_host, ran_on, title) = match live {
            Some((h, ran_on, title)) => (Some(h), ran_on, title),
            None => {
                let stored = self.reader.as_ref().and_then(|conn| {
                    store::brief(conn, old)
                        .inspect_err(|e| warn!("reading what {old} ran on: {e:#}"))
                        .ok()
                        .flatten()
                });
                let (title, ran_on) = stored.unwrap_or_default();
                (None, ran_on, title)
            }
        };
        let mut fx = Effects::default();
        let linked = self
            .states
            .get_mut(host)
            .is_some_and(|st| st.continues(session, old, &ran_on, &mut fx));
        self.flush(host, fx);
        if !linked {
            return;
        }
        info!("{session} on {host} continues {old} as a copy");
        if let Some(old_host) = old_host {
            let mut fx = Effects::default();
            if let Some(st) = self.states.get_mut(&old_host) {
                st.continued_as(old, session, &mut fx);
            }
            self.flush(&old_host, fx);
        }
        let own = self
            .states
            .get(host)
            .and_then(|st| st.session(session))
            .and_then(|s| s.title.clone());
        let title = title.map(|t| util::truncate(&t, MAX_TITLE));
        if let Some(title) = title.filter(|t| own.as_ref() != Some(t))
            && let Err(e) = self.rename(session, &title)
        {
            warn!("{session} keeps its own title: {e}");
        }
    }

    /// Links pending launches on `host` whose printed id names a live
    /// session (for a session whose hooks did not carry the launch).
    fn link_printed(&mut self, host: &str) {
        let Some(state) = self.states.get(host) else {
            return;
        };
        let found: Vec<(String, String)> = self
            .launches
            .iter()
            .filter(|(_, s)| s.launch.host == host && s.launch.state == LaunchState::Pending)
            .filter_map(|(id, slot)| {
                let short = slot.short.as_deref()?;
                // NOTE: a continued session may come back as a copy under a new id
                let session = state.sessions().find(|s| {
                    s.activity != Activity::Gone
                        && (s.short_id.as_deref() == Some(short) || s.id.starts_with(short))
                })?;
                Some((id.clone(), session.id.clone()))
            })
            .collect();
        for (launch, session) in found {
            self.link(host, &launch, &session);
        }
    }

    fn settle(&mut self, launch: &str, state: LaunchState) {
        let Some(slot) = self.launches.get_mut(launch) else {
            return;
        };
        if let LaunchState::Failed { error } = &state {
            warn!("launch {launch} in {}: {error}", slot.cwd);
        }
        let failed = match &state {
            LaunchState::Failed { error } => slot
                .wakes
                .clone()
                .map(|session| (slot.launch.host.clone(), session, error.clone())),
            _ => None,
        };
        slot.launch.state = state;
        let _ = self.updates.send(Update::Launch {
            launch: slot.launch.clone(),
        });
        if let Some((host, session, error)) = failed {
            let mut fx = Effects::default();
            if let Some(state) = self.states.get_mut(&host) {
                state.wake_failed(&session, &error, now_ms(), &mut fx);
            }
            self.flush(&host, fx);
        }
    }

    /// Restarts a sleeping session with `text` as its prompt: `claude --bg
    /// --resume`, which its host runs as a launch. Answers at once; a
    /// failure lands on the session (see [`Self::settle`]).
    fn wake(&mut self, host: &str, session: &Session, text: &str) -> Result<Session, String> {
        let id = session.id.clone();
        if !self.links.contains_key(host) {
            return Err(format!("{host} is not connected"));
        }
        // NOTE: `--resume` of a job that is starting would make a copy of it
        if session.activity == Activity::Working {
            return Err("the session is waking up: send this once it has answered".into());
        }
        let workspace = session
            .workspace
            .clone()
            .ok_or("the session ran outside every ccdash workspace: attach to it on its host")?;
        info!("waking {id} on {host}");
        let launch = self.launch(LaunchReq {
            host: host.to_owned(),
            workspace,
            project: None,
            cwd: session.cwd.clone(),
            prompt: text.to_owned(),
            model: None,
            permission_mode: None,
            effort: None,
            resume: Some(id.clone()),
            issue: None,
        })?;
        if let Some(slot) = self.launches.get_mut(&launch.id) {
            slot.wakes = Some(id.clone());
        }
        let mut fx = Effects::default();
        let state = self.states.get_mut(host).ok_or("session vanished")?;
        state.waking(&id, text, now_ms(), &mut fx);
        let woken = state.session(&id).cloned().ok_or("session vanished")?;
        self.flush(host, fx);
        Ok(woken)
    }

    /// Fails launches the session never reported for; forgets old ones.
    fn expire_launches(&mut self, now: i64) {
        let late: Vec<(String, Option<String>)> = self
            .launches
            .iter()
            .filter(|(_, s)| {
                s.launch.state == LaunchState::Pending && now - s.launch.at > LAUNCH_WAIT_MS
            })
            .map(|(id, s)| (id.clone(), s.short.clone()))
            .collect();
        for (id, short) in late {
            let error = match short {
                Some(short) => format!("started as {short}, but the session never reported"),
                None => "the host did not answer".into(),
            };
            self.settle(&id, LaunchState::Failed { error });
        }
        self.launches
            .retain(|_, s| now - s.launch.at <= LAUNCH_KEEP_MS);
    }

    /// The live session `key` names on a connected host, with its host.
    fn running(&self, key: &str) -> Result<(String, Session), String> {
        let (host, s) = self
            .resolve(key)
            .ok_or_else(|| format!("no session matches {key:?}"))?;
        if s.activity == Activity::Gone {
            return Err("the session has ended".into());
        }
        if !self.links.contains_key(host) {
            return Err(format!("{host} is not connected"));
        }
        Ok((host.to_owned(), s.clone()))
    }

    /// Asks the session's host to stop it; the session id to answer under.
    fn stop(&mut self, key: &str) -> Result<String, String> {
        let (host, s) = self.running(key)?;
        info!("stopping {} on {host}", s.id);
        self.tell(
            &host,
            ToCollector::Stop {
                session: s.id.clone(),
                pid: s.pid,
                kind: s.kind,
            },
        );
        Ok(s.id)
    }

    /// Interrupts the session's turn. Open prompts are denied with Claude
    /// Code's stop (the turn ends at once); otherwise the session is marked
    /// and its collector makes the next tool hook end the turn.
    fn interrupt(&mut self, key: &str) -> Result<Session, String> {
        let (host, s) = self.running(key)?;
        if !s.hooked {
            return Err("the session runs without hooks: interrupt it in its terminal".into());
        }
        if !matches!(s.activity, Activity::Working | Activity::Permission) {
            return Err("the session is not working".into());
        }
        if s.interrupting.is_some() {
            return Err("asked already: Claude stops at its next step".into());
        }
        let id = s.id.clone();
        let asks: Vec<String> = s.asks.iter().map(|a| a.id.clone()).collect();
        info!(
            "interrupting {id} on {host} ({} open prompt(s))",
            asks.len()
        );
        let now = now_ms();
        let mut fx = Effects::default();
        if asks.is_empty() {
            let state = self.states.get_mut(&host).ok_or("session vanished")?;
            state.interrupting(&id, now, &mut fx);
            self.tell(
                &host,
                ToCollector::Interrupt {
                    session: id.clone(),
                    at: now,
                },
            );
        } else {
            for ask in &asks {
                self.answer(&host, &id, ask, &Answer::Deny { message: None }, "web")?;
            }
            let state = self.states.get_mut(&host).ok_or("session vanished")?;
            state.interrupted(&id, now, &mut fx);
        }
        self.flush(&host, fx);
        self.states[&host]
            .session(&id)
            .cloned()
            .ok_or_else(|| "session vanished".to_owned())
    }

    fn interrupted(&mut self, host: &str, session: &str, at: i64) {
        info!("interrupted {session} on {host}");
        let mut fx = Effects::default();
        if let Some(state) = self.states.get_mut(host) {
            state.interrupted(session, at, &mut fx);
        }
        self.flush(host, fx);
    }

    fn stopped(&mut self, host: &str, session: &str, error: Option<String>) {
        if let Some(e) = &error {
            self.close_failed(host, session, e);
        } else {
            let mut fx = Effects::default();
            if let Some(state) = self.states.get_mut(host) {
                state.stopped(session, now_ms(), &mut fx);
            }
            self.flush(host, fx);
        }
        let current = self
            .states
            .get(host)
            .and_then(|st| st.session(session))
            .cloned();
        for reply in self.stops.remove(session).unwrap_or_default() {
            let result = match (&error, &current) {
                (Some(e), _) => Err(e.clone()),
                (None, Some(s)) => Ok(s.clone()),
                (None, None) => Err("the session vanished".into()),
            };
            let _ = reply.send(result);
        }
    }

    fn forget(&mut self, key: &str) -> Result<Session, String> {
        let (host, id) = self
            .resolve(key)
            .map(|(host, s)| (host.to_owned(), s.id.clone()))
            .ok_or_else(|| format!("no session matches {key:?}"))?;
        info!("forgetting {id} on {host}");
        let mut fx = Effects::default();
        let state = self.states.get_mut(&host).ok_or("session vanished")?;
        state.forget(&id, now_ms(), &mut fx);
        let session = state.session(&id).cloned().ok_or("session vanished")?;
        self.flush(&host, fx);
        Ok(session)
    }

    fn open(&mut self, key: &str) -> Result<Session, String> {
        let (host, s) = self.running(key)?;
        self.tell(
            &host,
            ToCollector::Open {
                session: s.id.clone(),
            },
        );
        Ok(s)
    }

    fn collector_closed(&mut self, host: &str, conn: u64) {
        if self.links.get(host).is_none_or(|l| l.conn != conn) {
            return;
        }
        info!("collector {host} disconnected");
        self.links.remove(host);
        let waiting: Vec<String> = self
            .stops
            .keys()
            .filter(|id| {
                self.states
                    .get(host)
                    .is_some_and(|st| st.session(id).is_some())
            })
            .cloned()
            .collect();
        for id in waiting {
            for reply in self.stops.remove(&id).unwrap_or_default() {
                let _ = reply.send(Err(format!("{host} disconnected")));
            }
        }
        let now = now_ms();
        let waiting = self.power_waits.remove(host);
        let asleep = waiting
            .as_ref()
            .is_some_and(|w| w.action == PowerAction::Suspend);
        if let Some(status) = self.hosts.get_mut(host) {
            status.connected = false;
            status.since = now;
            // NOTE: the link died before the answer: the freeze came first
            if asleep {
                status.power = Some(Power::Asleep { since: now });
            }
        }
        self.save_host(host);
        if let Some(w) = waiting {
            let result = if asleep {
                info!("{host} went to sleep before it answered");
                self.host_status(host)
            } else {
                Err(format!("{host} disconnected"))
            };
            let _ = w.reply.send(result);
        }
        let mut fx = Effects::default();
        if let Some(state) = self.states.get_mut(host) {
            let pids: Vec<u32> = self
                .channels
                .iter()
                .filter(|(h, _)| h == host)
                .map(|(_, pid)| *pid)
                .collect();
            for pid in pids {
                self.channels.remove(&(host.to_owned(), pid));
                state.channel(pid, false, &mut fx);
            }
            let asks: Vec<(String, String)> = self
                .pending
                .iter()
                .filter(|(_, p)| p.host == host)
                .map(|(id, p)| (id.clone(), p.session.clone()))
                .collect();
            for (id, session) in asks {
                self.pending.remove(&id);
                state.remove_ask(&session, &id, &mut fx);
            }
        }
        self.flush(host, fx);
        self.hosts_changed();
        let open: Vec<String> = self
            .drafts
            .values()
            .filter(|d| d.editing.as_ref().is_some_and(|e| e.host == host))
            .map(|d| d.key.clone())
            .collect();
        for key in open {
            self.settle_edit(
                &key,
                None,
                Some(format!("{host} disconnected while the editor was open")),
            );
        }
    }

    // ---- drafts: the panel's prompts, and editors opened on them ----

    fn publish_draft(&self, draft: &Draft) {
        let _ = self.updates.send(Update::Draft {
            draft: draft.clone(),
        });
    }

    /// The draft's text as the panel wrote it; an empty text drops it. A
    /// draft open in an editor keeps its text until the editor closes.
    fn set_draft(&mut self, key: &str, text: String) -> Result<Draft, String> {
        drafts::check(key, &text)?;
        if let Some(e) = self.drafts.get(key).and_then(|d| d.editing.as_ref()) {
            return Err(format!(
                "the draft is open in an editor on {}; close or cancel that first",
                e.host
            ));
        }
        let now = now_ms();
        let draft = if text.trim().is_empty() {
            self.drafts.remove(key);
            self.store.send(
                &self.host,
                StoreMsg::DraftRemove {
                    key: key.to_owned(),
                },
            );
            Draft {
                key: key.to_owned(),
                text: String::new(),
                updated_at: now,
                editing: None,
                notice: None,
            }
        } else {
            self.store.send(
                &self.host,
                StoreMsg::Draft {
                    key: key.to_owned(),
                    text: text.clone(),
                    at: now,
                },
            );
            let draft = Draft {
                key: key.to_owned(),
                text,
                updated_at: now,
                editing: None,
                notice: None,
            };
            self.drafts.insert(key.to_owned(), draft.clone());
            draft
        };
        self.publish_draft(&draft);
        Ok(draft)
    }

    fn edit_draft(&mut self, key: &str, host: &str, text: String) -> Result<Draft, String> {
        drafts::check(key, &text)?;
        if !self.links.contains_key(host) {
            return Err(format!("{host} is not connected"));
        }
        if !self.hosts.get(host).is_some_and(|h| h.features.edit) {
            return Err(format!(
                "{host} does not open an editor from the panel (`[edit] command` is off)"
            ));
        }
        if let Some(e) = self.drafts.get(key).and_then(|d| d.editing.as_ref()) {
            return Err(format!(
                "the draft is open in an editor on {} already",
                e.host
            ));
        }
        let id = util::random_hex(8).map_err(|e| format!("{e:#}"))?;
        let now = now_ms();
        self.store.send(
            &self.host,
            StoreMsg::Draft {
                key: key.to_owned(),
                text: text.clone(),
                at: now,
            },
        );
        let draft = Draft {
            key: key.to_owned(),
            text: text.clone(),
            updated_at: now,
            editing: Some(Editing {
                id: id.clone(),
                host: host.to_owned(),
                since: now,
            }),
            notice: None,
        };
        self.drafts.insert(key.to_owned(), draft.clone());
        info!("edit {id}: draft {key} opens on {host}");
        self.tell(
            host,
            ToCollector::Edit {
                id,
                key: key.to_owned(),
                text,
            },
        );
        self.publish_draft(&draft);
        Ok(draft)
    }

    fn cancel_edit(&mut self, key: &str) -> Result<Draft, String> {
        let Some(e) = self.drafts.get(key).and_then(|d| d.editing.clone()) else {
            return Err("no editor is open on that draft".into());
        };
        self.tell(&e.host, ToCollector::EditCancel { id: e.id.clone() });
        self.cancelled_edits.insert(e.id);
        self.settle_edit(key, None, Some("cancelled".into()));
        self.drafts
            .get(key)
            .cloned()
            .ok_or_else(|| "the draft is gone".into())
    }

    /// What `host`'s editor made of edit `id`: the text when it saved, a
    /// notice when it did not. A result for an edit the hub no longer waits
    /// on (cancelled, or another one opened since) is dropped, except text
    /// for a draft with no editor at all: the hub restarted, or the machine
    /// was away, meanwhile, and the text is still the owner's.
    fn edited(
        &mut self,
        host: &str,
        id: &str,
        key: &str,
        text: Option<String>,
        error: Option<String>,
    ) {
        if !drafts::valid_key(key) {
            warn!("edit {id} from {host}: {key:?} is not a draft key");
            return;
        }
        if self.cancelled_edits.remove(id) {
            info!("edit {id} from {host}: dropped, it was cancelled");
            return;
        }
        match self.drafts.get(key).and_then(|d| d.editing.as_ref()) {
            Some(e) if e.id == id => {}
            Some(e) => {
                info!("edit {id} from {host}: dropped, edit {} is open now", e.id);
                return;
            }
            None if text.is_some() => info!("edit {id} from {host}: late text taken"),
            None => return,
        }
        match (&text, &error) {
            (Some(_), _) => info!("edit {id}: saved"),
            (None, Some(e)) => info!("edit {id}: {e}"),
            (None, None) => info!("edit {id}: closed without text"),
        }
        self.settle_edit(key, text, error);
    }

    /// Closes the editor state of draft `key`, with the new text or why
    /// there is none.
    fn settle_edit(&mut self, key: &str, text: Option<String>, notice: Option<String>) {
        let now = now_ms();
        let Some(draft) = self.drafts.get_mut(key) else {
            return;
        };
        draft.editing = None;
        draft.notice = notice;
        if let Some(text) = text {
            if text.chars().count() > drafts::MAX_TEXT {
                draft.notice = Some(format!(
                    "the editor's text is longer than {} characters; the draft kept the old one",
                    drafts::MAX_TEXT
                ));
            } else {
                draft.text = text;
                draft.updated_at = now;
            }
        }
        let draft = draft.clone();
        if draft.text.trim().is_empty() {
            self.drafts.remove(key);
            self.store.send(
                &self.host,
                StoreMsg::DraftRemove {
                    key: key.to_owned(),
                },
            );
        } else {
            self.store.send(
                &self.host,
                StoreMsg::Draft {
                    key: key.to_owned(),
                    text: draft.text.clone(),
                    at: draft.updated_at,
                },
            );
        }
        self.publish_draft(&draft);
    }

    fn hosts_changed(&self) {
        let _ = self.updates.send(Update::Hosts {
            hosts: self.hosts.values().cloned().collect(),
        });
    }

    // ---- machines: reports, suspend, lock, wake ----

    fn host_status(&self, host: &str) -> Result<HostStatus, String> {
        self.hosts
            .get(host)
            .cloned()
            .ok_or_else(|| format!("no machine called {host}"))
    }

    /// Keeps `host`'s status for the next start of the hub.
    fn save_host(&self, host: &str) {
        if let Some(status) = self.hosts.get(host) {
            self.store
                .send(host, StoreMsg::Host(Box::new(status.clone())));
        }
    }

    /// Works out again whether and how the hub can wake `host`.
    fn refresh_wake(&mut self, host: &str) {
        let own = machine::ipv4_addresses();
        let via = self.wake_via.get(host).cloned();
        if let Some(status) = self.hosts.get_mut(host) {
            let report = status.machine.as_ref();
            // NOTE: the panel says why; a log line would repeat with every report
            status.wake = Some(match &own {
                Ok(own) => machine::wake_path(report, own, via.as_deref()),
                Err(e) if via.is_none() => Wake::Unavailable {
                    why: format!("the hub cannot list its own addresses ({e})"),
                },
                Err(_) => machine::wake_path(report, &[], via.as_deref()),
            });
        }
    }

    fn machine_report(&mut self, host: &str, mut report: MachineReport) {
        let Some(status) = self.hosts.get_mut(host) else {
            return;
        };
        if let Some(old) = &status.machine {
            machine::keep_addresses(&mut report, old);
        }
        if let Some(Power::Asleep { since }) = status.power
            && status.connected
            && report.at > since + ASLEEP_STALE_MS
        {
            status.power = None;
        }
        status.machine = Some(report);
        self.refresh_wake(host);
        self.save_host(host);
        self.hosts_changed();
    }

    /// Asks `host` to suspend or lock; the reply waits for its answer (or,
    /// for a suspend, for the link to drop, which the freeze does). Refused
    /// for a machine that is offline or has not opted in, and a suspend
    /// while sessions there work or wait on a prompt, unless `force`.
    fn power(
        &mut self,
        host: &str,
        action: PowerAction,
        force: bool,
        reply: oneshot::Sender<Result<HostStatus, String>>,
    ) -> Result<(), String> {
        if let Some(why) = self.power_refusal(host, action, force) {
            let _ = reply.send(Err(why.clone()));
            return Err(why);
        }
        self.next_power += 1;
        let id = format!("{}-{}", action.name(), self.next_power);
        info!("{id}: {host}, for the panel");
        self.power_waits.insert(
            host.to_owned(),
            PowerWait {
                id: id.clone(),
                action,
                reply,
            },
        );
        self.tell(host, ToCollector::Power { id, action });
        Ok(())
    }

    fn power_refusal(&self, host: &str, action: PowerAction, force: bool) -> Option<String> {
        let Some(status) = self.hosts.get(host) else {
            return Some(format!("no machine called {host}"));
        };
        if !status.connected || !self.links.contains_key(host) {
            return Some(format!("{host} is not connected"));
        }
        if let Some(w) = self.power_waits.get(host) {
            return Some(match w.action {
                PowerAction::Suspend => format!("{host} is already on its way to sleep"),
                PowerAction::Lock => format!("{host} is locking already"),
            });
        }
        let allowed = status.machine.as_ref().is_some_and(|m| match action {
            PowerAction::Suspend => m.can_suspend,
            PowerAction::Lock => m.can_lock,
        });
        if !allowed {
            return Some(format!(
                "{host} does not allow it: turn on `power.{}` in its collector's settings",
                action.name()
            ));
        }
        if force || action == PowerAction::Lock {
            return None;
        }
        let busy: Vec<String> = self
            .states
            .get(host)
            .map(|st| {
                st.sessions()
                    .filter(|s| matches!(s.activity, Activity::Working | Activity::Permission))
                    .map(notify::label)
                    .collect()
            })
            .unwrap_or_default();
        (!busy.is_empty()).then(|| {
            format!(
                "{} on {host}: {}; suspend anyway to pause {}",
                if busy.len() == 1 {
                    "a session is working".to_owned()
                } else {
                    format!("{} sessions are working", busy.len())
                },
                busy.join(", "),
                if busy.len() == 1 { "it" } else { "them" },
            )
        })
    }

    fn power_done(&mut self, host: &str, id: &str, error: Option<String>) {
        if self.power_waits.get(host).is_none_or(|w| w.id != id) {
            return;
        }
        let Some(w) = self.power_waits.remove(host) else {
            return;
        };
        if let Some(e) = error {
            warn!("{id}: {host} refused: {e}");
            let _ = w.reply.send(Err(e));
            return;
        }
        if w.action == PowerAction::Suspend {
            info!("{host} is on its way to sleep");
            if let Some(status) = self.hosts.get_mut(host) {
                status.power = Some(Power::Asleep { since: now_ms() });
            }
            self.save_host(host);
            self.hosts_changed();
        } else {
            info!("{host} locked its session");
        }
        let _ = w.reply.send(self.host_status(host));
    }

    /// Sends magic packets to `host` (in a task: they go out a few hundred
    /// milliseconds apart); `WakeSent` marks it waking.
    fn wake_machine(&mut self, host: &str, reply: oneshot::Sender<Result<HostStatus, String>>) {
        self.refresh_wake(host);
        let target = match self.hosts.get(host) {
            None => Err(format!("no machine called {host}")),
            Some(s) if s.connected && !matches!(s.power, Some(Power::Asleep { .. })) => {
                Err(format!("{host} is awake"))
            }
            Some(s) => match &s.wake {
                Some(Wake::Ready { mac, to }) => Ok((mac.clone(), to.clone())),
                Some(Wake::Unavailable { why }) => {
                    Err(format!("the hub cannot wake {host}: {why}"))
                }
                None => Err(format!("the hub cannot wake {host} yet")),
            },
        };
        let (mac, to) = match target {
            Ok(target) => target,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        info!("waking {host}: magic packets for {mac} to {to}");
        let (cmds, host) = (self.cmds.clone(), host.to_owned());
        tokio::spawn(async move {
            match machine::send_wake(&mac, &to).await {
                Ok(()) => {
                    let _ = cmds.send(Cmd::WakeSent { host, reply }).await;
                }
                Err(e) => {
                    warn!("waking {host}: {e}");
                    let _ = reply.send(Err(e));
                }
            }
        });
    }

    fn expire_waking(&mut self, now: i64) {
        let stale: Vec<String> = self
            .hosts
            .iter()
            .filter(|(_, h)| {
                matches!(h.power, Some(Power::Waking { since }) if now - since > WAKING_KEEP_MS)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for host in &stale {
            warn!("{host} did not come back after its magic packets");
            if let Some(status) = self.hosts.get_mut(host) {
                status.power = None;
            }
            self.save_host(host);
        }
        if !stale.is_empty() {
            self.hosts_changed();
        }
    }

    fn forget_host(&mut self, host: &str) -> Result<(), String> {
        match self.hosts.get(host) {
            None => return Err(format!("no machine called {host}")),
            Some(s) if s.connected => {
                return Err(format!(
                    "{host} is connected: only an offline machine can be forgotten"
                ));
            }
            Some(_) => {}
        }
        info!("forgetting the machine {host}");
        self.hosts.remove(host);
        self.store.send(host, StoreMsg::ForgetHost);
        self.hosts_changed();
        Ok(())
    }

    fn channel(&mut self, host: &str, pid: u32, up: bool) {
        if up {
            self.channels.insert((host.to_owned(), pid));
        } else {
            self.channels.remove(&(host.to_owned(), pid));
        }
        let mut fx = Effects::default();
        if let Some(state) = self.states.get_mut(host) {
            state.channel(pid, up, &mut fx);
        }
        self.flush(host, fx);
    }

    /// Shows the prompt in `env` under the collector's `id` and keeps what
    /// answering it needs, or lets the hook go.
    fn open_ask(&mut self, host: &str, id: String, env: &Envelope) {
        let mut fx = Effects::default();
        let shown = self.states.get_mut(host).and_then(|state| {
            state.apply(env, &mut fx);
            let session = env.payload["session_id"].as_str()?.to_owned();
            let (kind, input, updates) = ask::parse(&env.payload);
            let key = util::random_hex(16).ok()?;
            let ask = Ask {
                id: id.clone(),
                agent_id: env.payload["agent_id"].as_str().map(str::to_owned),
                at: env.ts,
                kind,
            };
            state.add_ask(&session, ask, &mut fx).then_some(Pending {
                host: host.to_owned(),
                session,
                key,
                input,
                updates,
            })
        });
        match shown {
            Some(pending) => {
                self.pending.insert(id, pending);
            }
            None => self.tell(host, ToCollector::AskRelease { id }),
        }
        self.flush(host, fx);
    }

    fn answer_session(
        &mut self,
        key: &str,
        ask: &str,
        answer: &Answer,
        via: &str,
    ) -> Result<Session, String> {
        let (host, id) = self
            .resolve(key)
            .map(|(host, s)| (host.to_owned(), s.id.clone()))
            .ok_or_else(|| format!("no session matches {key:?}"))?;
        self.answer(&host, &id, ask, answer, via)?;
        self.states[&host]
            .session(&id)
            .cloned()
            .ok_or_else(|| GONE.to_owned())
    }

    /// Hands the decision for `answer` to the collector whose hook holds
    /// ask `id`.
    fn answer(
        &mut self,
        host: &str,
        session: &str,
        id: &str,
        answer: &Answer,
        via: &str,
    ) -> Result<(), String> {
        let decision = {
            let state = self.states.get(host).ok_or(GONE)?;
            let p = self
                .pending
                .get(id)
                .filter(|p| p.session == session && p.host == host)
                .ok_or(GONE)?;
            let ask = state.ask(session, id).ok_or(GONE)?;
            ask::decision(&ask.kind, &p.input, &p.updates, answer)?
        };
        if !self.links.contains_key(host) {
            return Err(format!("{host} is not connected"));
        }
        self.pending.remove(id);
        let mut fx = Effects::default();
        if let Some(state) = self.states.get_mut(host) {
            state.answered(session, id, answer, via, now_ms(), &mut fx);
        }
        self.tell(
            host,
            ToCollector::Decision {
                id: id.to_owned(),
                decision,
            },
        );
        self.flush(host, fx);
        Ok(())
    }

    fn button(&mut self, key: &str, action: &str) -> Result<(), String> {
        let (id, host, session) = self
            .pending
            .iter()
            .find(|(_, p)| util::constant_time_eq(p.key.as_bytes(), key.as_bytes()))
            .map(|(id, p)| (id.clone(), p.host.clone(), p.session.clone()))
            .ok_or(GONE)?;
        let ask = self
            .states
            .get(&host)
            .and_then(|s| s.ask(&session, &id))
            .ok_or(GONE)?;
        let answer = Answer::from_button(&ask.kind, action).ok_or("unknown button")?;
        self.answer(&host, &session, &id, &answer, "notification")
    }

    /// Validates `title`, shows it and has the collector queue it for the
    /// session's next prompt.
    fn rename(&mut self, key: &str, title: &str) -> Result<Session, String> {
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if title.is_empty() {
            return Err("the title is empty".into());
        }
        if title.chars().count() > MAX_TITLE {
            return Err(format!("titles are at most {MAX_TITLE} characters"));
        }
        let (host, id) = self
            .resolve(key)
            .map(|(host, s)| (host.to_owned(), s.id.clone()))
            .ok_or_else(|| format!("no session matches {key:?}"))?;
        if !paths::plain_session_id(&id) {
            return Err("unusual session id".into());
        }
        if !self.links.contains_key(&host) {
            return Err(format!("{host} is not connected"));
        }
        let mut fx = Effects::default();
        let state = self.states.get_mut(&host).ok_or("session vanished")?;
        state.rename(&id, &title, &mut fx)?;
        let session = state.session(&id).cloned().ok_or("session vanished")?;
        self.tell(&host, ToCollector::Title { session: id, title });
        self.flush(&host, fx);
        Ok(session)
    }

    /// Passes `text` to the session through the collector that runs its
    /// channel and shows it as queued until Claude takes it; `resolve`
    /// marks the session resolved with it (`text` is the wrap-up prompt).
    fn reply(&mut self, key: &str, text: &str, resolve: bool) -> Result<Session, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("the message is empty".into());
        }
        if text.chars().count() > MAX_REPLY {
            return Err(format!("messages are at most {MAX_REPLY} characters"));
        }
        let (host, s) = self
            .resolve(key)
            .ok_or_else(|| format!("no session matches {key:?}"))?;
        if s.activity == Activity::Gone {
            return Err("the session has ended".into());
        }
        if resolve && s.resolved.is_some() {
            return Err("the session is resolved already: it closes after its wrap-up".into());
        }
        if s.asleep {
            if resolve {
                return Err("the session is asleep: send it a message first, or kill it".into());
            }
            let (host, s) = (host.to_owned(), s.clone());
            return self.wake(&host, &s, text);
        }
        let (host, id) = (host.to_owned(), s.id.clone());
        let pid = s
            .pid
            .filter(|_| s.channel)
            .filter(|pid| self.channels.contains(&(host.clone(), *pid)))
            .ok_or(
                "the session runs without the ccdash channel; restart it with `claude --resume`",
            )?;
        if !self.links.contains_key(&host) {
            return Err(format!("{host} is not connected"));
        }
        let mut fx = Effects::default();
        let state = self.states.get_mut(&host).ok_or("session vanished")?;
        let now = now_ms();
        state.sent_reply(&id, text, now, &mut fx);
        if resolve {
            info!("resolving {id} on {host}");
            state.mark_resolved(&id, now, &mut fx);
        }
        let session = state.session(&id).cloned().ok_or("session vanished")?;
        self.tell(
            &host,
            ToCollector::Say {
                pid,
                text: text.to_owned(),
            },
        );
        self.flush(&host, fx);
        Ok(session)
    }

    /// Takes the session's resolved mark back: it stays open after its
    /// wrap-up.
    fn keep_open(&mut self, key: &str) -> Result<Session, String> {
        let (host, id) = self
            .resolve(key)
            .map(|(host, s)| (host.to_owned(), s.id.clone()))
            .ok_or_else(|| format!("no session matches {key:?}"))?;
        let mut fx = Effects::default();
        let state = self.states.get_mut(&host).ok_or("session vanished")?;
        if !state.keep_open(&id, now_ms(), &mut fx) {
            return Err("the session is not marked resolved".into());
        }
        let session = state.session(&id).cloned().ok_or("session vanished")?;
        self.flush(&host, fx);
        Ok(session)
    }

    /// Closes a resolved session whose wrap-up turn ended, as Kill does;
    /// when that cannot start, the session is back on your turn with why.
    fn close_resolved(&mut self, host: &str, id: &str) {
        let Some(s) = self.states.get(host).and_then(|st| st.session(id)) else {
            return;
        };
        let stop = ToCollector::Stop {
            session: s.id.clone(),
            pid: s.pid,
            kind: s.kind,
        };
        if self.links.contains_key(host) {
            info!("closing {id} on {host}: resolved and wrapped up");
            self.tell(host, stop);
        } else {
            self.close_failed(host, id, &format!("{host} is not connected"));
        }
    }

    fn close_failed(&mut self, host: &str, id: &str, error: &str) {
        warn!("closing {id} on {host} after its wrap-up: {error}");
        let mut fx = Effects::default();
        if let Some(state) = self.states.get_mut(host) {
            state.close_failed(id, error, now_ms(), &mut fx);
        }
        self.flush(host, fx);
    }

    /// Restores, stores and broadcasts what an input changed on `host`;
    /// lets go of asks the state closed, refreshes the notification
    /// candidates and closes the sessions that wrapped up.
    fn flush(&mut self, host: &str, mut fx: Effects) {
        let close = std::mem::take(&mut fx.close);
        let Some(state) = self.states.get_mut(host) else {
            return;
        };
        if let Some(conn) = &self.reader {
            for id in &fx.created {
                match store::restored(conn, id) {
                    Ok(saved) => state.restore(id, saved),
                    Err(e) => warn!("restoring {id}: {e:#}"),
                }
            }
        }
        for msg in fx.store {
            self.store.send(host, msg);
        }
        let mut down: Vec<ToCollector> = Vec::new();
        let mut changed = Vec::new();
        for id in fx.touched {
            let Some(session) = state.session(&id) else {
                continue;
            };
            let key = (host.to_owned(), id.clone());
            if self.published.get(&key) != Some(session) {
                self.published.insert(key, session.clone());
                down.push(ToCollector::Update {
                    update: Box::new(Update::Session {
                        session: Box::new(session.clone()),
                    }),
                });
                changed.push(id);
            }
        }
        for id in fx.removed {
            self.published.remove(&(host.to_owned(), id.clone()));
            down.push(ToCollector::Update {
                update: Box::new(Update::Removed {
                    host: host.to_owned(),
                    id: id.clone(),
                }),
            });
            changed.push(id);
        }
        for id in &changed {
            self.show(id, host);
        }
        let limits_changed = fx.rate_limits;
        let closed: Vec<(String, String)> = self
            .pending
            .iter()
            .filter(|(id, p)| {
                p.host == host
                    && self
                        .states
                        .get(host)
                        .is_none_or(|s| s.ask(&p.session, id).is_none())
            })
            .map(|(id, p)| (id.clone(), p.host.clone()))
            .collect();
        for (id, _) in &closed {
            self.pending.remove(id);
            down.push(ToCollector::AskRelease { id: id.clone() });
        }
        for msg in down {
            self.tell(host, msg);
        }
        if limits_changed && let Some(limits) = self.rate_limits() {
            let update = Update::RateLimits {
                rate_limits: limits,
            };
            let hosts: Vec<String> = self.links.keys().cloned().collect();
            for h in hosts {
                self.tell(
                    &h,
                    ToCollector::Update {
                        update: Box::new(update.clone()),
                    },
                );
            }
            let _ = self.updates.send(update);
        }
        self.refresh_notes();
        for id in close {
            self.close_resolved(host, &id);
        }
    }
}

/// A message before its recipient is settled.
struct Stored {
    workspace: String,
    from_session: Option<String>,
    from_host: Option<String>,
    from_label: String,
    text: String,
}

/// Looks a recipient up on the sessions table: `(id, host)`.
type Finder = Box<dyn FnOnce(&Connection) -> Result<(String, String)> + Send>;

/// `"title" on host (id prefix)`, how injections name a session.
fn session_ref(s: &Session, host: &str) -> String {
    let label = s
        .title
        .clone()
        .or_else(|| s.name.clone())
        .unwrap_or_else(|| s.id.chars().take(8).collect());
    format!(
        "\"{label}\" on {host} ({})",
        s.id.chars().take(8).collect::<String>()
    )
}

fn flatten(result: Result<Result<Reply>>) -> Result<Reply, String> {
    match result {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(e)) | Err(e) => Err(format!("{e:#}")),
    }
}

/// A read by slug (the full note) or a search; both count as recalls.
fn recall(conn: &Connection, workspace: &str, query: &Query, now: i64) -> Result<Reply> {
    if let Some(slug) = &query.slug {
        let view = knowledge::view(conn, workspace, slug)?
            .with_context(|| format!("no note {slug:?} in this workspace"))?;
        knowledge::recalled(conn, workspace, std::slice::from_ref(slug), now)?;
        return Ok(Reply::Note(Box::new(view)));
    }
    let hits = knowledge::search(conn, workspace, query)?;
    let slugs: Vec<String> = hits.iter().map(|h| h.card.slug.clone()).collect();
    knowledge::recalled(conn, workspace, &slugs, now)?;
    Ok(Reply::Hits(hits))
}

/// The session and the project its hooks ask for at its start (`claude
/// --project`); a compaction or `/clear` also fires SessionStart, and must
/// not undo a change made since.
fn start_project(env: &Envelope) -> Option<(String, String)> {
    let p = &env.payload;
    let start = p["hook_event_name"] == "SessionStart"
        && matches!(p["source"].as_str(), Some("startup" | "resume"));
    let session = p["session_id"].as_str().filter(|_| start)?;
    Some((session.to_owned(), env.project.clone()?))
}

/// The startup context, opening with why the asked project was refused.
fn refused_note(refused: Option<String>, text: Option<String>) -> Option<String> {
    let Some(why) = refused else {
        return text;
    };
    let note = format!(
        "The owner started this session for a project that could not be applied: {why}. \
        Tell them in your first reply.\n"
    );
    Some(note + &text.unwrap_or_default())
}

/// The text a hook injects, and whether it hands over waiting messages.
fn inject(
    conn: &Connection,
    c: &Caller,
    what: &Injection,
    around: &Around,
    now: i64,
) -> Result<(Option<String>, bool)> {
    let mut delivered = false;
    let text = match what {
        Injection::SessionStart => {
            let cards: Vec<knowledge::Card> = knowledge::active(conn, &c.workspace)?
                .iter()
                .map(knowledge::Note::card)
                .collect();
            knowledge::session_start(&cards, &c.workspace, c.project.as_deref(), now)
        }
        Injection::Prompt { since } => {
            let mut out = String::new();
            if let Some(since) = since {
                let cards: Vec<knowledge::Card> = knowledge::active(conn, &c.workspace)?
                    .iter()
                    .map(knowledge::Note::card)
                    .collect();
                let changed = knowledge::changed_since(&cards, c.project.as_deref(), *since);
                out.push_str("The ccdash hub is reachable again. ");
                if changed.is_empty() {
                    out.push_str("Nothing changed in this workspace's notes while it was away.\n");
                } else {
                    out.push_str("Notes changed while it was away:\n");
                    for card in changed.iter().take(20) {
                        out.push_str(&format!(
                            "- {} `{}` ({}, {})\n",
                            card.title,
                            card.slug,
                            card.kind,
                            card.status.name()
                        ));
                    }
                }
            }
            if let Some(session) = c.session.as_deref() {
                let messages = messages::deliver(conn, session, now)?;
                if !messages.is_empty() {
                    delivered = true;
                    out.push_str(&messages::injection(&messages, now));
                }
            }
            if !around.others.is_empty() {
                let scope = c.project.clone().unwrap_or_else(|| c.workspace.clone());
                out.push_str(&format!("Also active in {scope} right now (ccdash):\n"));
                for line in &around.others {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            (!out.is_empty()).then_some(out)
        }
        Injection::Edit { path } => {
            let Some(session) = c.session.as_deref() else {
                return Ok((None, false));
            };
            let mut parts = Vec::new();
            if !around.claimed.is_empty() {
                parts.push(format!(
                    "ccdash: this file is claimed by another session: {}. Coordinate through \
                        `tell_session` before changing it.",
                    around.claimed.join("; ")
                ));
            }
            let edits = knowledge::recent_edits(conn, session, path, now)?;
            if !edits.is_empty() {
                let who: Vec<String> = edits
                    .iter()
                    .map(|e| {
                        let label = e
                            .title
                            .clone()
                            .unwrap_or_else(|| e.session.chars().take(8).collect());
                        let ago = (now - e.at).max(0) / MINUTE_MS;
                        format!("\"{label}\" on {} {ago} min ago", e.host)
                    })
                    .collect();
                parts.push(format!(
                    "ccdash: another session edited this file recently ({}). Re-read it before \
                        changing it, and consider `tell_session` if you both work on it.",
                    who.join("; ")
                ));
            }
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
    };
    Ok((text, delivered))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::{
        model::Kind,
        wire::{Envelope, Source},
    };

    fn envelope(source: Source, launch: Option<&str>, payload: Value) -> Envelope {
        Envelope {
            v: 1,
            source,
            ts: now_ms(),
            pid: Some(std::process::id()),
            remote_id: None,
            applied_title: None,
            launch: launch.map(str::to_owned),
            project: None,
            branch: Some("main".into()),
            payload,
        }
    }

    #[test]
    fn cost_counters_fold_into_deltas_and_survive_restarts() {
        let dir = std::env::temp_dir().join(format!("ccdash-wscost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repos = Arc::new(Repos::open(&dir).unwrap());
        let mut ws = WorkspaceCore {
            repos,
            setup: WorkspaceSetup::default(),
            reports: HashMap::new(),
            waiting: Vec::new(),
            started: 0,
            traffic: HashMap::new(),
            cost_last: HashMap::new(),
            cost_acc: HashMap::new(),
            last_flush: 0,
            held_waits: HashMap::new(),
        };
        let at = |cpu: u64, passes: u64, since: i64| Cost {
            since,
            cpu_ms: cpu,
            git_cpu_ms: cpu * 2,
            passes,
            rss_kb: 1000 + cpu,
            watches: 7,
            ..Cost::default()
        };
        // the first report is the baseline only
        ws.take_cost("navi", &at(1000, 50, 1));
        assert!(ws.cost_acc.is_empty());
        ws.take_cost("navi", &at(1300, 53, 1));
        let acc = &ws.cost_acc["navi"];
        assert_eq!((acc.cpu_ms, acc.git_cpu_ms, acc.passes), (300, 600, 3));
        assert_eq!((acc.rss_kb, acc.watches), (2300, 7));
        // the collector restarted: its counters are the delta
        ws.take_cost("navi", &at(40, 2, 2));
        let acc = &ws.cost_acc["navi"];
        assert_eq!((acc.cpu_ms, acc.passes), (340, 5));
        // another host keeps its own
        ws.take_cost("flat", &at(10, 1, 9));
        ws.take_cost("flat", &at(15, 1, 9));
        assert_eq!(ws.cost_acc["flat"].cpu_ms, 5);
        assert_eq!(ws.cost_acc["navi"].cpu_ms, 340);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The next message of a kind the test waits for, skipping the rest.
    async fn next(
        rx: &mut mpsc::Receiver<ToCollector>,
        want: impl Fn(&ToCollector) -> bool,
    ) -> ToCollector {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("a message for the collector")
                .expect("link open");
            if want(&msg) {
                return msg;
            }
        }
    }

    /// The guard's answer for `session` on `host`.
    async fn guard(
        hub: &Hub,
        rx: &mut mpsc::Receiver<ToCollector>,
        host: &str,
        session: &str,
    ) -> Option<String> {
        let who = Who {
            session: Some(session.into()),
            pid: None,
            cwd: None,
        };
        hub.send(Cmd::Call {
            host: host.into(),
            id: 7,
            call: Call::Guard { who },
        })
        .await
        .unwrap();
        match next(rx, |m| matches!(m, ToCollector::Result { id: 7, .. })).await {
            ToCollector::Result {
                result: Ok(Reply::Guard(reason)),
                ..
            } => reason,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn one_session_runs_on_one_machine() {
        let dir = std::env::temp_dir().join(format!("ccdash-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("t.db"), 30).unwrap();
        let hub = Hub::spawn(
            "hub".into(),
            "http://hub".into(),
            Workspaces::new(&["/w".into()], "/home"),
            store,
            None,
            KnowledgeCfg::default(),
            0,
            None,
            BTreeMap::new(),
        );
        let mut rxs = HashMap::new();
        for (conn, host) in ["navi", "flat"].into_iter().enumerate() {
            let (tx, rx) = mpsc::channel(256);
            hub.send(Cmd::CollectorOpen {
                host: host.into(),
                conn: conn as u64,
                terminals: Vec::new(),
                features: HostFeatures::default(),
                addr: None,
                tx,
            })
            .await
            .unwrap();
            rxs.insert(host, rx);
        }
        let sid = "5e3f1c2a-0000-4000-8000-000000000001";
        let hook = |event: &str, source: &str| {
            Input::Envelope(envelope(
                Source::Hook,
                None,
                json!({"session_id": sid, "cwd": "/w/p", "hook_event_name": event, "source": source}),
            ))
        };
        let send = |host: &str, input: Input| hub.send(Cmd::Input(host.into(), input));
        send("navi", hook("SessionStart", "startup")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        send("flat", hook("SessionStart", "resume")).await.unwrap();

        // navi opened it first: flat is refused, navi is not
        let reason = guard(&hub, rxs.get_mut("flat").unwrap(), "flat", sid).await;
        assert!(
            reason
                .as_deref()
                .is_some_and(|r| r.contains("open on navi")
                    && r.contains(&format!("http://hub/sessions/{sid}"))),
            "{reason:?}"
        );
        assert_eq!(
            guard(&hub, rxs.get_mut("navi").unwrap(), "navi", sid).await,
            None
        );
        assert_eq!(
            hub.holder("flat", sid).await.unwrap().as_deref(),
            Some("navi")
        );
        assert_eq!(hub.holder("navi", sid).await.unwrap(), None);
        // one copy stands for the session: the holder's
        let snap = hub.snapshot().await.unwrap();
        let copies: Vec<&str> = snap
            .sessions
            .iter()
            .filter(|s| s.id == sid)
            .map(|s| s.host.as_str())
            .collect();
        assert_eq!(copies, ["navi"]);

        // navi closes it: flat goes on and stands for it
        send("navi", hook("SessionEnd", "")).await.unwrap();
        assert_eq!(
            guard(&hub, rxs.get_mut("flat").unwrap(), "flat", sid).await,
            None
        );
        assert_eq!(hub.resolve(sid).await.unwrap().unwrap().host, "flat");

        // flat closes, navi opens it again and goes offline with it open
        send("flat", hook("SessionEnd", "")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        send("navi", hook("SessionStart", "resume")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        hub.send(Cmd::CollectorClosed {
            host: "navi".into(),
            conn: 0,
        })
        .await
        .unwrap();
        send("flat", hook("SessionStart", "resume")).await.unwrap();
        let reason = guard(&hub, rxs.get_mut("flat").unwrap(), "flat", sid).await;
        assert!(
            reason
                .as_deref()
                .is_some_and(|r| r.contains("navi went offline") && r.contains("Forget")),
            "{reason:?}"
        );
        assert_eq!(
            hub.holder("flat", sid).await.unwrap().as_deref(),
            Some("navi")
        );
        // forgetting the offline copy lets flat go on
        let forgot = hub
            .session_cmd(|reply| Cmd::Forget {
                key: sid.into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(forgot.host, "navi");
        assert_eq!(hub.holder("flat", sid).await.unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hub with host `navi` connected, knowing workspace `w`'s project `p`.
    async fn hub_on_navi(name: &str) -> (Hub, mpsc::Receiver<ToCollector>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ccdash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("t.db"), 30).unwrap();
        let hub = Hub::spawn(
            "hub".into(),
            "http://hub".into(),
            Workspaces::new(&["/w".into()], "/home"),
            store,
            None,
            KnowledgeCfg::default(),
            0,
            None,
            BTreeMap::new(),
        );
        let (tx, rx) = mpsc::channel(64);
        hub.send(Cmd::CollectorOpen {
            host: "navi".into(),
            conn: 1,
            terminals: Vec::new(),
            features: HostFeatures::default(),
            addr: None,
            tx,
        })
        .await
        .unwrap();
        hub.send(Cmd::Dirs {
            host: "navi".into(),
            dirs: Dirs::from([("w".to_owned(), vec!["p".to_owned()])]),
        })
        .await
        .unwrap();
        (hub, rx, dir)
    }

    #[tokio::test]
    async fn a_reply_to_a_sleeping_session_wakes_it_and_a_failure_lands_on_it() {
        let (hub, mut rx, dir) = hub_on_navi("wake").await;
        let id = "dddd0000-0000-4000-8000-000000000000";
        let poll = |state: &str, pid: Option<u32>| {
            Input::Agents(
                vec![AgentEntry {
                    session_id: id.into(),
                    id: Some("dddd0000".into()),
                    cwd: Some("/w".into()),
                    kind: Some("background".into()),
                    name: Some("fix the parser".into()),
                    state: Some(state.into()),
                    pid,
                    ..AgentEntry::default()
                }],
                now_ms(),
                Vec::new(),
            )
        };
        hub.send(Cmd::Input(
            "navi".into(),
            poll("working", Some(std::process::id())),
        ))
        .await
        .unwrap();
        hub.send(Cmd::Input("navi".into(), poll("done", None)))
            .await
            .unwrap();
        let asleep = hub.resolve(id).await.unwrap().unwrap();
        assert!(asleep.asleep && asleep.activity != Activity::Gone);

        // the wrap-up needs a live session
        let resolved = hub
            .session_cmd(|reply| Cmd::MarkResolved {
                key: id.into(),
                reply,
            })
            .await
            .unwrap();
        assert!(resolved.unwrap_err().contains("asleep"));

        let waking = hub.reply(id, "carry on").await.unwrap().unwrap();
        assert_eq!(waking.activity, Activity::Working);
        let sent = next(&mut rx, |m| matches!(m, ToCollector::Launch { .. })).await;
        let ToCollector::Launch {
            launch,
            cwd,
            prompt,
            model,
            permission_mode,
            effort,
            resume,
        } = sent
        else {
            unreachable!()
        };
        assert_eq!((cwd.as_str(), prompt.as_str()), ("/w", "carry on"));
        assert_eq!(
            (model, permission_mode, effort),
            (None, None, None),
            "its saved options"
        );
        assert_eq!(resume.as_deref(), Some(id));

        // one wake at a time
        let twice = hub.reply(id, "and this").await.unwrap().unwrap_err();
        assert!(twice.contains("waking up"), "{twice}");
        assert_eq!(
            hub.resolve(id).await.unwrap().unwrap().outbox.len(),
            1,
            "the refused text is not shown as sent"
        );

        hub.send(Cmd::Launched {
            host: "navi".into(),
            launch,
            session: None,
            error: Some("no conversation found".into()),
        })
        .await
        .unwrap();
        let failed = hub.resolve(id).await.unwrap().unwrap();
        assert_eq!(failed.activity, Activity::YourTurn);
        assert!(
            failed
                .waiting_for
                .as_deref()
                .is_some_and(|w| w.contains("no conversation found")),
            "{failed:?}"
        );

        // a job that lost its settings file comes back as a copy
        assert!(hub.reply(id, "again").await.unwrap().is_ok());
        let ToCollector::Launch { launch, .. } =
            next(&mut rx, |m| matches!(m, ToCollector::Launch { .. })).await
        else {
            unreachable!()
        };
        hub.send(Cmd::Launched {
            host: "navi".into(),
            launch,
            session: Some("abcd0000".into()),
            error: None,
        })
        .await
        .unwrap();
        let copy = AgentEntry {
            session_id: "abcd0000-0000-4000-8000-000000000000".into(),
            id: Some("abcd0000".into()),
            cwd: Some("/w".into()),
            kind: Some("background".into()),
            state: Some("working".into()),
            pid: Some(std::process::id()),
            ..AgentEntry::default()
        };
        hub.send(Cmd::Input(
            "navi".into(),
            Input::Agents(vec![copy], now_ms(), Vec::new()),
        ))
        .await
        .unwrap();
        assert_eq!(
            hub.resolve(id).await.unwrap().unwrap().activity,
            Activity::Gone,
            "the copy goes on, the old row does not hang as working"
        );
        let copy = hub.resolve("abcd0000").await.unwrap().unwrap();
        assert!(copy.launched);
        assert_eq!(
            copy.continues.as_deref(),
            Some(id),
            "linked to the session it continues"
        );
        assert_eq!(
            (copy.title.as_deref(), copy.renaming),
            (Some("fix the parser"), true),
            "it takes the conversation's title over"
        );
        assert!(matches!(
            next(&mut rx, |m| matches!(m, ToCollector::Title { .. })).await,
            ToCollector::Title { ref session, ref title } if session == &copy.id && title == "fix the parser"
        ));
        assert_eq!(
            hub.resolve(id).await.unwrap().unwrap().continued_as,
            Some(copy.id)
        );

        // with no link there is nobody to wake it
        let (tx, mut rx) = mpsc::channel(64);
        hub.send(Cmd::CollectorOpen {
            host: "navi".into(),
            conn: 2,
            terminals: Vec::new(),
            features: HostFeatures::default(),
            addr: None,
            tx,
        })
        .await
        .unwrap();
        let sleeper = "bbbb0000-0000-4000-8000-000000000000";
        hub.send(Cmd::Input(
            "navi".into(),
            Input::Agents(
                vec![AgentEntry {
                    session_id: sleeper.into(),
                    id: Some("bbbb0000".into()),
                    cwd: Some("/w".into()),
                    kind: Some("background".into()),
                    state: Some("blocked".into()),
                    ..AgentEntry::default()
                }],
                now_ms(),
                Vec::new(),
            ),
        ))
        .await
        .unwrap();
        assert!(hub.resolve(sleeper).await.unwrap().unwrap().asleep);
        rx.close();
        hub.send(Cmd::CollectorClosed {
            host: "navi".into(),
            conn: 2,
        })
        .await
        .unwrap();
        let offline = hub.reply(sleeper, "hello").await.unwrap().unwrap_err();
        assert!(offline.contains("not connected"), "{offline}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_project_a_session_starts_for_is_pinned_before_its_context() {
        let (hub, mut rx, dir) = hub_on_navi("project").await;
        let start = |id: &str, project: Option<&str>, source: &str| {
            let mut env = envelope(
                Source::Hook,
                None,
                json!({"session_id": id, "cwd": "/w", "hook_event_name": "SessionStart",
                    "source": source}),
            );
            env.project = project.map(str::to_owned);
            Cmd::Input("navi".into(), Input::Envelope(env))
        };
        let who = |id: &str| Who {
            session: Some(id.to_owned()),
            pid: None,
            cwd: Some("/w".to_owned()),
        };
        let context = |id: u64, session: &str| Cmd::Call {
            host: "navi".into(),
            id,
            call: Call::Inject {
                who: who(session),
                what: Injection::SessionStart,
            },
        };
        async fn text(rx: &mut mpsc::Receiver<ToCollector>, want: u64) -> Option<String> {
            match next(
                rx,
                |m| matches!(m, ToCollector::Result { id, .. } if *id == want),
            )
            .await
            {
                ToCollector::Result {
                    result: Ok(Reply::Context(text)),
                    ..
                } => text,
                other => panic!("{other:?}"),
            }
        }

        let good = "eeee0000-0000-4000-8000-000000000000";
        hub.send(start(good, Some("p"), "startup")).await.unwrap();
        let session = hub.resolve(good).await.unwrap().unwrap();
        assert_eq!(session.project.as_deref(), Some("p"));
        assert!(session.project_pinned);

        // a compaction fires SessionStart too: it must not undo the panel
        hub.session_cmd(|reply| Cmd::SetProject {
            key: good.into(),
            project: None,
            reply,
        })
        .await
        .unwrap()
        .unwrap();
        hub.send(start(good, Some("p"), "compact")).await.unwrap();
        assert!(!hub.resolve(good).await.unwrap().unwrap().project_pinned);

        // a bad name leaves the session be, and Claude is told once
        let bad = "ffff0000-0000-4000-8000-000000000000";
        hub.send(start(bad, Some("nope"), "startup")).await.unwrap();
        let session = hub.resolve(bad).await.unwrap().unwrap();
        assert_eq!(session.project, None);
        hub.send(context(1, bad)).await.unwrap();
        let told = text(&mut rx, 1).await.unwrap_or_default();
        assert!(told.contains("not a project of w"), "{told}");
        hub.send(context(2, bad)).await.unwrap();
        let again = text(&mut rx, 2).await.unwrap_or_default();
        assert!(!again.contains("not a project of w"), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_launch_for_an_issue_claims_it_when_the_session_reports() {
        let dir = std::env::temp_dir().join(format!("ccdash-issue-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("t.db"), 30).unwrap();
        let hub = Hub::spawn(
            "hub".into(),
            "http://hub".into(),
            Workspaces::new(&["/w".into()], "/home"),
            store,
            None,
            KnowledgeCfg::default(),
            0,
            None,
            BTreeMap::new(),
        );
        let (tx, _rx) = mpsc::channel(64);
        hub.send(Cmd::CollectorOpen {
            host: "navi".into(),
            conn: 1,
            terminals: Vec::new(),
            features: HostFeatures::default(),
            addr: None,
            tx,
        })
        .await
        .unwrap();
        let launch = hub
            .launch(LaunchReq {
                host: "navi".into(),
                workspace: "w".into(),
                project: None,
                cwd: "/w".into(),
                prompt: "go".into(),
                model: None,
                permission_mode: None,
                effort: None,
                resume: None,
                issue: Some(LaunchIssue {
                    slug: "w--fix-it".into(),
                    title: "Fix it".into(),
                }),
            })
            .await
            .unwrap()
            .unwrap();
        let sid = "1c4719e5-9508-461b-b1f4-e7b5a4c26f9d";
        let start = envelope(
            Source::Hook,
            Some(&launch.id),
            json!({"session_id": sid, "cwd": "/w", "hook_event_name": "SessionStart"}),
        );
        hub.send(Cmd::Input("navi".into(), Input::Envelope(start)))
            .await
            .unwrap();
        let claim = hub.resolve(sid).await.unwrap().unwrap().claim.unwrap();
        assert_eq!(claim.what, "Fix it");
        assert_eq!(claim.issue.as_deref(), Some("w--fix-it"));
        assert!(claim.paths.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn launches_link_their_session_and_stops_answer() {
        let dir = std::env::temp_dir().join(format!("ccdash-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("t.db"), 30).unwrap();
        let saved = store.clone();
        let hub = Hub::spawn(
            "hub".into(),
            "http://hub".into(),
            Workspaces::new(&["/w".into()], "/home"),
            store,
            None,
            KnowledgeCfg::default(),
            0,
            None,
            BTreeMap::new(),
        );
        let req = |resume: Option<&str>| LaunchReq {
            host: "navi".into(),
            workspace: "w".into(),
            project: Some("p".into()),
            cwd: "/w".into(),
            prompt: "go".into(),
            model: None,
            permission_mode: Some("plan".into()),
            effort: None,
            resume: resume.map(str::to_owned),
            issue: None,
        };
        assert!(
            hub.launch(req(None)).await.unwrap().is_err(),
            "offline host"
        );

        let (tx, mut rx) = mpsc::channel(64);
        hub.send(Cmd::CollectorOpen {
            host: "navi".into(),
            conn: 1,
            terminals: Vec::new(),
            features: HostFeatures::default(),
            addr: None,
            tx,
        })
        .await
        .unwrap();
        hub.send(Cmd::Dirs {
            host: "navi".into(),
            dirs: Dirs::from([("w".to_owned(), vec!["p".to_owned()])]),
        })
        .await
        .unwrap();
        assert_eq!(
            hub.host_dirs().await.unwrap(),
            [(
                "navi".to_owned(),
                Dirs::from([("w".to_owned(), vec!["p".to_owned()])])
            )]
        );
        let first = hub.launch(req(None)).await.unwrap().unwrap();
        assert_eq!(first.state, LaunchState::Pending);
        let sent = next(&mut rx, |m| matches!(m, ToCollector::Launch { .. })).await;
        assert!(
            matches!(sent, ToCollector::Launch { ref launch, ref cwd, .. }
            if launch == &first.id && cwd == "/w")
        );

        // the session's hooks carry the launch id
        let sid = "9c4719e5-9508-461b-b1f4-e7b5a4c26f9d";
        let start = envelope(
            Source::Hook,
            Some(&first.id),
            json!({"session_id": sid, "cwd": "/w", "hook_event_name": "SessionStart"}),
        );
        hub.send(Cmd::Input("navi".into(), Input::Envelope(start)))
            .await
            .unwrap();
        let started = hub.launch_of(&first.id).await.unwrap().unwrap();
        assert_eq!(
            started.state,
            LaunchState::Started {
                session: sid.into()
            }
        );
        let session = hub.resolve(sid).await.unwrap().unwrap();
        assert!(session.launched);
        // at the workspace root, under the launch's project
        assert_eq!(session.cwd, "/w");
        assert_eq!(session.project.as_deref(), Some("p"));
        assert!(session.project_pinned);
        // the panel may move it; an unknown project is refused
        assert!(
            hub.set_project(sid, Some("nope".into()))
                .await
                .unwrap()
                .is_err()
        );
        let moved = hub.set_project(sid, None).await.unwrap().unwrap();
        assert_eq!(
            (moved.project, moved.project_pinned),
            (None, false),
            "the directory's again"
        );
        assert_eq!(
            hub.set_project(sid, Some("p".into()))
                .await
                .unwrap()
                .unwrap()
                .project
                .as_deref(),
            Some("p")
        );

        // a session whose hooks lack the id: linked through the printed one
        let second = hub.launch(req(None)).await.unwrap().unwrap();
        hub.send(Cmd::Launched {
            host: "navi".into(),
            launch: second.id.clone(),
            session: Some("aaaabbbb".into()),
            error: None,
        })
        .await
        .unwrap();
        assert_eq!(
            hub.launch_of(&second.id).await.unwrap().unwrap().state,
            LaunchState::Pending
        );
        let poll_again = || AgentEntry {
            session_id: "aaaabbbb-0000-4000-8000-000000000000".into(),
            id: Some("aaaabbbb".into()),
            cwd: Some("/w".into()),
            kind: Some("background".into()),
            pid: Some(std::process::id()),
            branch: Some("feat".into()),
            ..AgentEntry::default()
        };
        let poll = poll_again();
        hub.send(Cmd::Input(
            "navi".into(),
            Input::Agents(vec![poll], now_ms(), Vec::new()),
        ))
        .await
        .unwrap();
        assert!(matches!(
            hub.launch_of(&second.id).await.unwrap().unwrap().state,
            LaunchState::Started { ref session } if session.starts_with("aaaabbbb")
        ));
        let bg = hub.resolve("aaaabbbb").await.unwrap().unwrap();
        assert_eq!(
            (bg.kind, bg.branch.as_deref()),
            (Kind::Background, Some("feat"))
        );

        let third = hub.launch(req(None)).await.unwrap().unwrap();
        hub.send(Cmd::Launched {
            host: "navi".into(),
            launch: third.id.clone(),
            session: None,
            error: Some("not trusted".into()),
        })
        .await
        .unwrap();
        assert_eq!(
            hub.launch_of(&third.id).await.unwrap().unwrap().state,
            LaunchState::Failed {
                error: "not trusted".into()
            }
        );

        // continued as a copy under a new id: the printed id names it
        let prompt = envelope(
            Source::Hook,
            None,
            json!({"session_id": sid, "cwd": "/w", "hook_event_name": "UserPromptSubmit", "prompt": "go"}),
        );
        hub.send(Cmd::Input("navi".into(), Input::Envelope(prompt)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let copy = hub.launch(req(Some(sid))).await.unwrap().unwrap();
        hub.send(Cmd::Launched {
            host: "navi".into(),
            launch: copy.id.clone(),
            session: Some("cccc0000".into()),
            error: None,
        })
        .await
        .unwrap();
        let polled = AgentEntry {
            session_id: "cccc0000-0000-4000-8000-000000000000".into(),
            id: Some("cccc0000".into()),
            kind: Some("background".into()),
            pid: Some(std::process::id()),
            ..AgentEntry::default()
        };
        hub.send(Cmd::Input(
            "navi".into(),
            Input::Agents(vec![poll_again(), polled], now_ms(), Vec::new()),
        ))
        .await
        .unwrap();
        assert!(matches!(
            hub.launch_of(&copy.id).await.unwrap().unwrap().state,
            LaunchState::Started { ref session } if session.starts_with("cccc0000")
        ));
        // the copy is linked to the session it continues, and its chat starts with that one's
        let continued = hub.resolve("cccc0000").await.unwrap().unwrap();
        assert_eq!(continued.continues.as_deref(), Some(sid));
        assert_eq!(
            hub.resolve(sid).await.unwrap().unwrap().continued_as,
            Some(continued.id.clone())
        );
        saved.exec(|_| ()).await.unwrap();
        let chat =
            crate::conversation::conversation(&saved.reader().unwrap(), &continued.id, None, 10)
                .unwrap();
        assert_eq!(
            chat.exchanges
                .iter()
                .map(|e| (e.session.as_deref(), e.prompt.as_deref()))
                .collect::<Vec<_>>(),
            [(Some(sid), None), (Some(sid), Some("go")), (None, None)]
        );
        assert!(matches!(
            chat.exchanges[2].items[..],
            [crate::conversation::Item::Continued { ref session, known: true, .. }] if session == sid
        ));
        assert_eq!(
            store::continued_as(&saved.reader().unwrap(), sid).unwrap(),
            Some(continued.id.clone())
        );

        // stop: the host is asked, the request answers with its report
        let waiting = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.session_cmd(|reply| Cmd::Stop {
                    key: "aaaabbbb".into(),
                    reply,
                })
                .await
                .unwrap()
            })
        };
        let stop = next(&mut rx, |m| matches!(m, ToCollector::Stop { .. })).await;
        assert!(
            matches!(stop, ToCollector::Stop { kind: Kind::Background, ref session, .. }
            if session.starts_with("aaaabbbb"))
        );
        hub.send(Cmd::Stopped {
            host: "navi".into(),
            session: bg.id.clone(),
            error: None,
        })
        .await
        .unwrap();
        let killed = waiting.await.unwrap().unwrap();
        assert_eq!(
            (killed.id.clone(), killed.activity),
            (bg.id.clone(), Activity::Gone)
        );
        // Claude Code goes on listing a stopped job as finished
        hub.send(Cmd::Input(
            "navi".into(),
            Input::Agents(
                vec![AgentEntry {
                    state: Some("done".into()),
                    pid: None,
                    ..poll_again()
                }],
                now_ms(),
                Vec::new(),
            ),
        ))
        .await
        .unwrap();
        assert_eq!(
            hub.resolve("aaaabbbb").await.unwrap().unwrap().activity,
            Activity::Gone
        );

        let forgotten = hub
            .session_cmd(|reply| Cmd::Forget {
                key: sid.into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(forgotten.activity, Activity::Gone);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_message_finds_a_session_only_the_store_knows_unless_it_ended() {
        let dir = std::env::temp_dir().join(format!("ccdash-tell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("t.db"), 30).unwrap();
        let rows = store.clone();
        let hub = Hub::spawn(
            "hub".into(),
            "http://hub".into(),
            Workspaces::new(&["/w".into()], "/home"),
            store,
            None,
            KnowledgeCfg::default(),
            0,
            None,
            BTreeMap::new(),
        );
        // as after a hub restart: in the table, not yet in the live state
        let now = now_ms();
        rows.exec(move |conn: &mut Connection| {
            conn.execute(
                "INSERT INTO sessions (id, host, cwd, kind, started_at, ended_at) VALUES
                    ('2aaa', 'navi', '/w', 'interactive', ?1, NULL),
                    ('2bbb', 'navi', '/w', 'interactive', ?1, ?1)",
                [now],
            )
        })
        .await
        .unwrap()
        .unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        hub.send(Cmd::CollectorOpen {
            host: "navi".into(),
            conn: 1,
            terminals: Vec::new(),
            features: HostFeatures::default(),
            addr: None,
            tx,
        })
        .await
        .unwrap();
        let sid = "6c4719e5-9508-461b-b1f4-e7b5a4c26f9d";
        let start = envelope(
            Source::Hook,
            None,
            json!({"session_id": sid, "cwd": "/w", "hook_event_name": "SessionStart"}),
        );
        hub.send(Cmd::Input("navi".into(), Input::Envelope(start)))
            .await
            .unwrap();
        let tell = |id: u64, to: &str| Cmd::Call {
            host: "navi".into(),
            id,
            call: Call::Tell {
                who: Who {
                    session: Some(sid.into()),
                    pid: None,
                    cwd: None,
                },
                to: to.into(),
                text: "hello".into(),
            },
        };
        let mut answer = async |id: u64, to: &str| {
            hub.send(tell(id, to)).await.unwrap();
            match next(
                &mut rx,
                |m| matches!(m, ToCollector::Result { id: got, .. } if *got == id),
            )
            .await
            {
                ToCollector::Result { result, .. } => result,
                _ => unreachable!(),
            }
        };
        let first = answer(1, "2aaa").await;
        assert!(matches!(first, Ok(Reply::Sent)), "{first:?}");
        assert_eq!(
            answer(2, "2bbb").await.unwrap_err(),
            "that session has ended"
        );
        let unknown = answer(3, "2ccc").await.unwrap_err();
        assert!(unknown.contains("no session matches"), "{unknown}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn claims_place_a_root_session_under_a_project() {
        let dir = std::env::temp_dir().join(format!("ccdash-claimproj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("t.db"), 30).unwrap();
        let reader = store.clone();
        let hub = Hub::spawn(
            "hub".into(),
            "http://hub".into(),
            Workspaces::new(&["/w".into()], "/home"),
            store,
            None,
            KnowledgeCfg::default(),
            0,
            None,
            BTreeMap::new(),
        );
        let (tx, mut rx) = mpsc::channel(64);
        hub.send(Cmd::CollectorOpen {
            host: "navi".into(),
            conn: 1,
            terminals: Vec::new(),
            features: HostFeatures::default(),
            addr: None,
            tx,
        })
        .await
        .unwrap();
        hub.send(Cmd::Dirs {
            host: "navi".into(),
            dirs: Dirs::from([("w".to_owned(), vec!["p".to_owned()])]),
        })
        .await
        .unwrap();
        let sid = "7c4719e5-9508-461b-b1f4-e7b5a4c26f9d";
        let start = envelope(
            Source::Hook,
            None,
            json!({"session_id": sid, "cwd": "/w", "hook_event_name": "SessionStart"}),
        );
        hub.send(Cmd::Input("navi".into(), Input::Envelope(start)))
            .await
            .unwrap();
        let who = Who {
            session: Some(sid.into()),
            pid: None,
            cwd: None,
        };
        let call = |id: u64, call: Call| Cmd::Call {
            host: "navi".into(),
            id,
            call,
        };
        let claim = |project: &str| Call::Claim {
            who: who.clone(),
            what: "the hub".into(),
            paths: vec!["src".into()],
            until: None,
            issue: None,
            project: Some(project.into()),
        };
        async fn result(rx: &mut mpsc::Receiver<ToCollector>, want: u64) -> Result<Reply, String> {
            match next(
                rx,
                |m| matches!(m, ToCollector::Result { id, .. } if *id == want),
            )
            .await
            {
                ToCollector::Result { result, .. } => result,
                _ => unreachable!(),
            }
        }

        // a session at the root has no project until it claims one
        assert_eq!(hub.resolve(sid).await.unwrap().unwrap().project, None);
        hub.send(call(1, claim("nope"))).await.unwrap();
        let refused = result(&mut rx, 1).await.unwrap_err();
        assert!(refused.contains("not a project of w"), "{refused}");
        hub.send(call(2, claim("p"))).await.unwrap();
        match result(&mut rx, 2).await.unwrap() {
            Reply::Claimed(Some(c)) => {
                assert_eq!(c.paths, ["/w/p/src"], "relative to the project")
            }
            other => panic!("{other:?}"),
        }
        let session = hub.resolve(sid).await.unwrap().unwrap();
        assert_eq!(session.project.as_deref(), Some("p"));
        assert!(session.project_pinned);

        // notes default to the session's project; a named one must be known
        let remember = |project: Option<&str>, slug: &str| Call::Note {
            who: who.clone(),
            op: knowledge::NoteOp::Remember {
                title: "T".into(),
                body: "b".into(),
                slug: Some(slug.into()),
                project: project.map(str::to_owned),
                summary: None,
                supersedes: None,
                status: None,
            },
        };
        hub.send(call(3, remember(Some("nope"), "n--x")))
            .await
            .unwrap();
        assert!(result(&mut rx, 3).await.is_err());
        hub.send(call(4, remember(None, "p--x"))).await.unwrap();
        assert!(matches!(result(&mut rx, 4).await, Ok(Reply::Written(_))));
        let conn = reader.reader().unwrap();
        let note = knowledge::get(&conn, "w", "p--x").unwrap().unwrap();
        assert_eq!(note.project.as_deref(), Some("p"));
        assert!(knowledge::get(&conn, "w", "n--x").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn report(can_suspend: bool, ipv4: &str) -> Box<MachineReport> {
        Box::new(MachineReport {
            at: now_ms(),
            boot_at: 0,
            slept_ms: 0,
            resumed_at: None,
            os: None,
            kernel: "7.2".into(),
            reboot_pending: None,
            cpu: crate::machine::Cpu {
                cores: 4,
                usage_pct: None,
                load: [0.0; 3],
                temp_c: None,
            },
            memory: crate::machine::Memory::default(),
            disks: Vec::new(),
            battery: None,
            nics: vec![crate::machine::Nic {
                name: "enp1".into(),
                mac: "30:56:0f:74:65:de".into(),
                ipv4: vec![ipv4.into()],
                wake: true,
            }],
            can_suspend,
            can_lock: true,
            locked: Some(false),
        })
    }

    async fn ask<T>(hub: &Hub, make: impl FnOnce(oneshot::Sender<T>) -> Cmd) -> T {
        let (reply, rx) = oneshot::channel();
        hub.send(make(reply)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("an answer")
            .unwrap()
    }

    #[tokio::test]
    async fn machines_suspend_wake_and_are_kept() {
        let dir = std::env::temp_dir().join(format!("ccdash-power-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("t.db"), 30).unwrap();
        let listener = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let via = BTreeMap::from([(
            "navi".to_owned(),
            listener.local_addr().unwrap().to_string(),
        )]);
        let spawn = |store: Store| {
            Hub::spawn(
                "hub".into(),
                "http://hub".into(),
                Workspaces::new(&["/w".into()], "/home"),
                store,
                None,
                KnowledgeCfg::default(),
                0,
                None,
                via.clone(),
            )
        };
        let hub = spawn(store.clone());
        let (tx, mut rx) = mpsc::channel(256);
        hub.send(Cmd::CollectorOpen {
            host: "navi".into(),
            conn: 1,
            terminals: Vec::new(),
            features: HostFeatures::default(),
            addr: None,
            tx,
        })
        .await
        .unwrap();
        let suspend = |force| {
            move |reply| Cmd::Power {
                host: "navi".into(),
                action: PowerAction::Suspend,
                force,
                reply,
            }
        };

        // before a report: suspend is refused, waking an awake machine too
        let refused = ask(&hub, suspend(false)).await.unwrap_err();
        assert!(refused.contains("power.suspend"), "{refused}");
        let awake = ask(&hub, |reply| Cmd::Wake {
            host: "navi".into(),
            reply,
        })
        .await
        .unwrap_err();
        assert!(awake.contains("is awake"), "{awake}");

        // a working session holds the suspend back unless forced
        hub.send(Cmd::Machine {
            host: "navi".into(),
            report: report(true, "203.0.113.4/24"),
        })
        .await
        .unwrap();
        let sid = "5e3f1c2a-0000-4000-8000-0000000000aa";
        for event in ["SessionStart", "UserPromptSubmit"] {
            hub.send(Cmd::Input(
                "navi".into(),
                Input::Envelope(envelope(
                    Source::Hook,
                    None,
                    json!({"session_id": sid, "cwd": "/w/p", "hook_event_name": event, "prompt": "go"}),
                )),
            ))
            .await
            .unwrap();
        }
        let busy = ask(&hub, suspend(false)).await.unwrap_err();
        assert!(busy.contains("a session is working"), "{busy}");

        // a lock ignores working sessions and leaves the machine awake
        let (reply, answer) = oneshot::channel();
        hub.send(Cmd::Power {
            host: "navi".into(),
            action: PowerAction::Lock,
            force: false,
            reply,
        })
        .await
        .unwrap();
        let ToCollector::Power { id, action } =
            next(&mut rx, |m| matches!(m, ToCollector::Power { .. })).await
        else {
            unreachable!()
        };
        assert_eq!(action, PowerAction::Lock);
        hub.send(Cmd::PowerDone {
            host: "navi".into(),
            id,
            error: None,
        })
        .await
        .unwrap();
        assert!(answer.await.unwrap().is_ok_and(|s| s.power.is_none()));

        // forced: the collector is asked, and its refusal comes back
        let (reply, answer) = oneshot::channel();
        hub.send(suspend(true)(reply)).await.unwrap();
        let ToCollector::Power { id, .. } =
            next(&mut rx, |m| matches!(m, ToCollector::Power { .. })).await
        else {
            unreachable!()
        };
        let again = ask(&hub, suspend(true)).await.unwrap_err();
        assert!(again.contains("already on its way"), "{again}");
        hub.send(Cmd::PowerDone {
            host: "navi".into(),
            id: id.clone(),
            error: Some("Operation inhibited".into()),
        })
        .await
        .unwrap();
        assert_eq!(answer.await.unwrap().unwrap_err(), "Operation inhibited");

        // accepted: asleep, then the link drops
        let (reply, answer) = oneshot::channel();
        hub.send(suspend(true)(reply)).await.unwrap();
        let ToCollector::Power { id, .. } =
            next(&mut rx, |m| matches!(m, ToCollector::Power { .. })).await
        else {
            unreachable!()
        };
        hub.send(Cmd::PowerDone {
            host: "navi".into(),
            id,
            error: None,
        })
        .await
        .unwrap();
        let status = answer.await.unwrap().unwrap();
        assert!(matches!(status.power, Some(Power::Asleep { .. })));
        hub.send(Cmd::CollectorClosed {
            host: "navi".into(),
            conn: 1,
        })
        .await
        .unwrap();

        // no shared network, so `via` carries the packets
        let snap = hub.snapshot().await.unwrap();
        let navi = snap.hosts.iter().find(|h| h.name == "navi").unwrap();
        assert!(!navi.connected);
        assert!(matches!(&navi.wake, Some(Wake::Ready { to, .. }) if *to == via["navi"]));
        let status = ask(&hub, |reply| Cmd::Wake {
            host: "navi".into(),
            reply,
        })
        .await
        .unwrap();
        assert!(matches!(status.power, Some(Power::Waking { .. })));
        let mut buf = [0u8; 200];
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        assert_eq!(n, 102);
        assert_eq!(
            &buf[..12],
            &[
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x30, 0x56, 0x0f, 0x74, 0x65, 0xde
            ]
        );

        // a new hub knows the machine, offline, with its report and wake path
        tokio::time::sleep(Duration::from_millis(200)).await;
        let hub = spawn(store);
        let snap = hub.snapshot().await.unwrap();
        let navi = snap.hosts.iter().find(|h| h.name == "navi").expect("kept");
        assert!(!navi.connected);
        assert!(navi.machine.as_ref().is_some_and(|m| m.can_suspend));
        assert!(matches!(navi.wake, Some(Wake::Ready { .. })));
        assert!(snap.hub.is_some());
        ask(&hub, |reply| Cmd::ForgetHost {
            host: "navi".into(),
            reply,
        })
        .await
        .unwrap();
        assert!(hub.snapshot().await.unwrap().hosts.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn drafts_are_kept_and_edited_on_a_machine() {
        let (hub, rx, dir) = hub_on_navi("drafts").await;
        let mut updates = hub.subscribe();
        // bad keys and the wrong length are refused
        assert!(hub.draft("nope", "x").await.unwrap().is_err());
        assert!(
            hub.draft("start", &"x".repeat(drafts::MAX_TEXT + 1))
                .await
                .unwrap()
                .is_err()
        );
        // a write lands in the snapshot and on the stream
        let d = hub.draft("start", "first words").await.unwrap().unwrap();
        assert_eq!(d.text, "first words");
        assert!(d.editing.is_none());
        let snap = hub.snapshot().await.unwrap();
        assert_eq!(snap.drafts.len(), 1);
        assert_eq!(snap.drafts[0].key, "start");
        let streamed = loop {
            match tokio::time::timeout(Duration::from_secs(5), updates.recv())
                .await
                .expect("a draft update")
                .unwrap()
            {
                Update::Draft { draft } => break draft,
                _ => continue,
            }
        };
        assert_eq!(streamed.text, "first words");
        // navi came without an editor: refused; another machine: not connected
        let err = hub
            .edit_draft("start", "navi", "first words")
            .await
            .unwrap()
            .unwrap_err();
        assert!(err.contains("[edit] command"), "{err}");
        let err = hub
            .edit_draft("start", "flat", "first words")
            .await
            .unwrap()
            .unwrap_err();
        assert!(err.contains("not connected"), "{err}");
        // reconnect navi with an editor
        let (tx, rx2) = mpsc::channel(64);
        hub.send(Cmd::CollectorOpen {
            host: "navi".into(),
            conn: 2,
            terminals: Vec::new(),
            features: HostFeatures {
                edit: true,
                ..HostFeatures::default()
            },
            addr: None,
            tx,
        })
        .await
        .unwrap();
        drop(rx);
        let mut rx = rx2;
        let d = hub
            .edit_draft("start", "navi", "second words")
            .await
            .unwrap()
            .unwrap();
        let editing = d.editing.clone().expect("editing");
        assert_eq!(editing.host, "navi");
        let msg = next(&mut rx, |m| matches!(m, ToCollector::Edit { .. })).await;
        let ToCollector::Edit { id, key, text } = msg else {
            unreachable!()
        };
        assert_eq!(
            (id.as_str(), key.as_str(), text.as_str()),
            (editing.id.as_str(), "start", "second words")
        );
        // the panel may not write over an open editor; a second editor is refused
        assert!(
            hub.draft("start", "typed meanwhile")
                .await
                .unwrap()
                .is_err()
        );
        assert!(hub.edit_draft("start", "navi", "x").await.unwrap().is_err());
        // a stale result (another id) is dropped; the right one lands
        hub.send(Cmd::Edited {
            host: "navi".into(),
            id: "0ld".into(),
            key: "start".into(),
            text: Some("stale".into()),
            error: None,
        })
        .await
        .unwrap();
        hub.send(Cmd::Edited {
            host: "navi".into(),
            id: id.clone(),
            key: "start".into(),
            text: Some("edited words\n".into()),
            error: None,
        })
        .await
        .unwrap();
        let snap = hub.snapshot().await.unwrap();
        assert_eq!(snap.drafts[0].text, "edited words\n");
        assert!(snap.drafts[0].editing.is_none());
        // a cancel closes the editor and keeps the text; the collector hears
        let d = hub
            .edit_draft("start", "navi", "edited words\n")
            .await
            .unwrap()
            .unwrap();
        let open = d.editing.unwrap().id;
        next(&mut rx, |m| matches!(m, ToCollector::Edit { .. })).await;
        let d = hub.cancel_edit("start").await.unwrap().unwrap();
        assert!(d.editing.is_none());
        assert_eq!(d.notice.as_deref(), Some("cancelled"));
        assert_eq!(d.text, "edited words\n");
        assert!(matches!(
            next(&mut rx, |m| matches!(m, ToCollector::EditCancel { .. })).await,
            ToCollector::EditCancel { id } if id == open
        ));
        assert!(hub.cancel_edit("start").await.unwrap().is_err());
        // the editor's late text after a cancel is dropped
        hub.send(Cmd::Edited {
            host: "navi".into(),
            id: open,
            key: "start".into(),
            text: Some("after cancel".into()),
            error: None,
        })
        .await
        .unwrap();
        assert_eq!(
            hub.snapshot().await.unwrap().drafts[0].text,
            "edited words\n"
        );
        // the machine going away while an editor is open frees the draft
        hub.edit_draft("start", "navi", "edited words\n")
            .await
            .unwrap()
            .unwrap();
        hub.send(Cmd::CollectorClosed {
            host: "navi".into(),
            conn: 2,
        })
        .await
        .unwrap();
        let d = &hub.snapshot().await.unwrap().drafts[0];
        assert!(d.editing.is_none());
        assert!(
            d.notice.as_deref().unwrap_or("").contains("disconnected"),
            "{d:?}"
        );
        // an empty text drops the draft
        let d = hub.draft("start", "  ").await.unwrap().unwrap();
        assert_eq!(d.text, "");
        assert!(hub.snapshot().await.unwrap().drafts.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
