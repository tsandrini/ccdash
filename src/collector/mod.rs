//! `ccdash collector`: the per-machine daemon. It takes what Claude Code
//! and the desktop report (hooks, statusline, `claude agents`, niri,
//! `/proc`, `/usage`, OpenTelemetry), forwards it to the hub, and carries
//! out what the hub sends back: channel replies, ask decisions, queued
//! titles, sessions to start, stop or open. The local API keeps serving a
//! mirror of this host's sessions for the DMS plugin and the CLI.

mod api;
mod link;
mod queue;

use std::{
    collections::{BTreeMap, HashMap},
    fs::Permissions,
    os::unix::{fs::PermissionsExt, net::UnixStream as StdUnixStream},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::{
    net::{TcpListener, UnixDatagram, UnixListener},
    signal::unix::{SignalKind, signal},
    sync::{broadcast, mpsc, oneshot},
};
use tracing::{debug, info, warn};

use crate::{
    actions::{self, LaunchSpec},
    agents,
    channel::ToChannel,
    config::{CollectorConfig, Launch},
    git,
    knowledge::{self, Note, Query},
    model::{HostStatus, RateLimits, ScopedLimit, Session, Snapshot, Update},
    niri, otlp, paths, procs,
    protocol::{Call, Injection, Reply, TelemetryRecord, ToCollector, ToHub, Who, WorkspaceDir},
    state::{AgentEntry, NiriEvent, ProcPoll, StoreMsg, Window},
    sync::collector::{self as sync, Cmd as SyncCmd, Handle as SyncHandle},
    usage,
    util::{self, now_ms, pid_alive},
    wire::{self, Envelope, Source},
    workspace::{
        Workspaces,
        collector::{self as workspace, Cmd as WsCmd, Handle as WsHandle},
    },
};

/// Everything the local actor reacts to.
pub enum LocalCmd {
    Envelope(Envelope),
    Agents(Vec<AgentEntry>, i64),
    Niri(NiriEvent),
    Procs(Vec<ProcPoll>, i64),
    Usage(Vec<ScopedLimit>, i64),
    Telemetry(Vec<StoreMsg>),
    Snapshot(oneshot::Sender<Snapshot>),
    Resolve(String, oneshot::Sender<Option<Session>>),
    Pids(oneshot::Sender<Vec<(String, u32)>>),
    /// Something only the hub can do; answered with its result.
    Call(Call, oneshot::Sender<Result<Reply, String>>),
    /// Whether the hub link is up right now.
    Connected(oneshot::Sender<bool>),
    /// A knowledge write made while the hub is unreachable: parked in the
    /// outbox, carried out when the link returns.
    Queue(Call),
    /// A read answered from the notes cache.
    Cached(Who, Query, oneshot::Sender<Result<Reply, String>>),
    /// A hook injection while the hub is unreachable: the session is
    /// marked for a catch-up, and a session start is served from the cache.
    OfflineInject(Who, Injection, oneshot::Sender<Option<String>>),
    /// Takes the catch-up mark of a session: since when it has been
    /// missing the hub, if it has.
    CatchUp(String, oneshot::Sender<Option<i64>>),
    /// The prompt hook of this session printed the hub's context.
    Injected(String),
    /// A `ccdash channel` connected for the Claude process `pid`; `u64`
    /// tells its connections apart.
    ChannelOpen(u32, u64, mpsc::Sender<ToChannel>),
    ChannelClosed(u32, u64),
    /// `ccdash ask` holds a prompt open: the first sender gets the ask id,
    /// the second the decision; dropping the second lets the hook go.
    Ask(
        Box<Envelope>,
        oneshot::Sender<String>,
        oneshot::Sender<Value>,
    ),
    /// The hook holding ask `id` went away.
    AskGone(String),
    LinkUp,
    LinkDown,
    FromHub(Box<ToCollector>),
}

#[derive(Clone)]
pub struct Local {
    tx: mpsc::Sender<LocalCmd>,
    /// Tagged JSON lines for `subscribe` clients: snapshots and updates.
    updates: broadcast::Sender<Value>,
}

impl Local {
    pub fn sender(&self) -> mpsc::Sender<LocalCmd> {
        self.tx.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.updates.subscribe()
    }

    pub async fn send(&self, cmd: LocalCmd) -> Result<()> {
        self.tx.send(cmd).await.ok().context("collector stopped")
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        let (tx, rx) = oneshot::channel();
        self.send(LocalCmd::Snapshot(tx)).await?;
        rx.await.context("collector stopped")
    }

    pub async fn resolve(&self, key: &str) -> Result<Option<Session>> {
        let (tx, rx) = oneshot::channel();
        self.send(LocalCmd::Resolve(key.to_owned(), tx)).await?;
        rx.await.context("collector stopped")
    }

    /// `(session id, pid)` of live sessions on this host.
    pub async fn pids(&self) -> Result<Vec<(String, u32)>> {
        let (tx, rx) = oneshot::channel();
        self.send(LocalCmd::Pids(tx)).await?;
        rx.await.context("collector stopped")
    }

    pub async fn call(&self, call: Call) -> Result<Result<Reply, String>> {
        self.call_within(call, Duration::from_secs(15)).await
    }

    /// `call`, giving up after `limit`; the hub's late answer is dropped.
    pub async fn call_within(&self, call: Call, limit: Duration) -> Result<Result<Reply, String>> {
        let (tx, rx) = oneshot::channel();
        self.send(LocalCmd::Call(call, tx)).await?;
        match tokio::time::timeout(limit, rx).await {
            Ok(result) => result.context("collector stopped"),
            Err(_) => Ok(Err(TIMED_OUT.into())),
        }
    }

    pub async fn connected(&self) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(LocalCmd::Connected(tx)).await?;
        rx.await.context("collector stopped")
    }

    pub async fn cached(&self, who: Who, query: Query) -> Result<Result<Reply, String>> {
        let (tx, rx) = oneshot::channel();
        self.send(LocalCmd::Cached(who, query, tx)).await?;
        rx.await.context("collector stopped")
    }

    pub async fn offline_inject(&self, who: Who, what: Injection) -> Result<Option<String>> {
        let (tx, rx) = oneshot::channel();
        self.send(LocalCmd::OfflineInject(who, what, tx)).await?;
        rx.await.context("collector stopped")
    }

    pub async fn catch_up(&self, session: &str) -> Result<Option<i64>> {
        let (tx, rx) = oneshot::channel();
        self.send(LocalCmd::CatchUp(session.to_owned(), tx)).await?;
        rx.await.context("collector stopped")
    }
}

pub const NOT_CONNECTED: &str = "the hub is not connected";
/// How long a stop waits for the sync pass that uploads the session's
/// last lines; the hub gives the whole stop 30 s.
const STOP_FLUSH_WAIT: Duration = Duration::from_secs(12);
/// How long a launch that continues a session waits for its transcript.
const LAUNCH_FLUSH_WAIT: Duration = Duration::from_secs(60);
const DIRS_EVERY_MS: i64 = 60_000;
pub const TIMED_OUT: &str = "the hub did not answer in time";

/// What the collector keeps on disk of the hub's notes.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct NotesCache {
    workspaces: Vec<WorkspaceDir>,
    /// Workspace → its live notes.
    notes: HashMap<String, Vec<Note>>,
}

pub fn run(cfg: CollectorConfig) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(Arc::new(cfg)))
}

async fn serve(cfg: Arc<CollectorConfig>) -> Result<()> {
    let dir = paths::runtime_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, Permissions::from_mode(0o700))?;
    let state_dir = paths::state_dir();
    std::fs::create_dir_all(&state_dir)?;
    std::fs::set_permissions(&state_dir, Permissions::from_mode(0o700))?;
    let (ingest_path, api_path) = (paths::ingest_socket(), paths::api_socket());
    if StdUnixStream::connect(&api_path).is_ok() {
        bail!(
            "another ccdash collector is listening on {}",
            api_path.display()
        );
    }
    let host = cfg.host();
    let (hub_tx, hub_rx) = mpsc::unbounded_channel();
    let (local, rx) = Local::spawn();
    let sync = start_sync(&cfg, &host, &hub_tx);
    let workspace = start_workspace(&cfg, &host, &hub_tx);
    tokio::spawn(run_actor(
        Actor::new(
            host.clone(),
            hub_tx,
            local.updates.clone(),
            paths::notes_cache(),
        )
        .with_commands(cfg.launch.clone(), cfg.attach.command.clone())
        .with_sync(sync.clone())
        .with_workspace(workspace.clone()),
        rx,
    ));
    tokio::spawn(link::run(
        link::Link {
            cfg: cfg.hub.clone(),
            host: host.clone(),
            terminals: cfg.niri.terminals.clone(),
            queue: queue::Queue::open(paths::outbox_file(), cfg.queue.max_bytes),
        },
        hub_rx,
        local.sender(),
    ));

    let ingest = UnixDatagram::bind(replace(&ingest_path)?)
        .with_context(|| format!("binding {}", ingest_path.display()))?;
    let api = UnixListener::bind(replace(&api_path)?)
        .with_context(|| format!("binding {}", api_path.display()))?;
    for addr in &cfg.telemetry.listen {
        if !addr.ip().is_loopback() {
            bail!("telemetry.listen takes loopback addresses only (it has no token): {addr}");
        }
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding telemetry {addr}"))?;
        info!("telemetry listening on {addr}");
        let (sink, mut records) = mpsc::channel(64);
        let tx = local.sender();
        tokio::spawn(async move {
            while let Some(batch) = records.recv().await {
                if tx.send(LocalCmd::Telemetry(batch)).await.is_err() {
                    return;
                }
            }
        });
        tokio::spawn(otlp::serve(listener, sink));
    }
    tokio::spawn(ingest_loop(ingest, local.sender()));
    tokio::spawn(api::serve(api, local.clone(), cfg.clone(), sync, workspace));
    tokio::spawn(agents::run(
        cfg.agents.command.clone(),
        Duration::from_secs(cfg.agents.interval_secs.max(1)),
        local.sender(),
    ));
    if !cfg.usage.command.is_empty() {
        tokio::spawn(usage::run(
            cfg.usage.command.clone(),
            Duration::from_secs(cfg.usage.interval_secs.max(60)),
            local.sender(),
        ));
    }
    if cfg.niri.enable {
        tokio::spawn(niri::run(local.sender()));
    }
    if cfg.procs.enable {
        tokio::spawn(procs::run(
            local.clone(),
            Duration::from_secs(cfg.procs.interval_secs.max(1)),
        ));
    }
    info!(
        "ccdash collector up on {host}, sockets in {}, hub {}",
        dir.display(),
        cfg.hub.url
    );

    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    info!("shutting down");
    let _ = std::fs::remove_file(&ingest_path);
    let _ = std::fs::remove_file(&api_path);
    Ok(())
}

/// Starts keeping Claude Code's state in step with the hub (`[sync]`).
fn start_sync(
    cfg: &CollectorConfig,
    host: &str,
    hub: &mpsc::UnboundedSender<ToHub>,
) -> Option<SyncHandle> {
    if !cfg.sync.enable {
        return None;
    }
    let Some(token_file) = &cfg.hub.token_file else {
        warn!("sync is on but hub.token_file is not set: not syncing");
        return None;
    };
    let settings = sync::Settings {
        host: host.to_owned(),
        root: cfg.sync_root(),
        include: cfg.sync.include.clone(),
        exclude: cfg.sync.exclude.clone(),
        interval: Duration::from_secs(cfg.sync.interval_secs.max(1)),
        base_url: sync::base_url(&cfg.hub.url),
        token_file: PathBuf::from(util::expand_env(&token_file.to_string_lossy())),
        index: paths::state_dir().join("sync.db"),
    };
    info!(
        "syncing {} with the hub at {}",
        settings.root.display(),
        settings.base_url
    );
    sync::spawn(settings, hub.clone())
        .inspect_err(|e| warn!("sync: {e:#}"))
        .ok()
}

/// Starts keeping the workspace root in step with the hub (`[workspace]`).
fn start_workspace(
    cfg: &CollectorConfig,
    host: &str,
    hub: &mpsc::UnboundedSender<ToHub>,
) -> Option<WsHandle> {
    if !cfg.workspace.enable {
        return None;
    }
    let Some(token_file) = &cfg.hub.token_file else {
        warn!("the workspace sync is on but hub.token_file is not set: not syncing");
        return None;
    };
    let Some(root) = cfg.workspace_root() else {
        warn!("the workspace sync is on but workspace.root is not set: not syncing");
        return None;
    };
    if !root.is_dir() {
        warn!(
            "the workspace sync is on but {} is not a directory: not syncing",
            root.display()
        );
        return None;
    }
    let settings = workspace::Settings {
        host: host.to_owned(),
        root: root.clone(),
        state: paths::state_dir().join("workspace"),
        hub_base: format!("{}/git/", sync::base_url(&cfg.hub.url)),
        token_file: PathBuf::from(util::expand_env(&token_file.to_string_lossy())),
        starting: cfg.workspace.starting(),
    };
    info!(
        "syncing the workspace {} through the hub at {}",
        root.display(),
        settings.hub_base
    );
    workspace::spawn(settings, hub.clone())
        .inspect_err(|e| warn!("workspace: {e:#}"))
        .ok()
}

impl Local {
    fn spawn() -> (Self, mpsc::Receiver<LocalCmd>) {
        let (tx, rx) = mpsc::channel(1024);
        let (updates, _) = broadcast::channel(256);
        (Self { tx, updates }, rx)
    }
}

/// Workspace name → the directories directly under it (hidden ones left
/// out), for the workspaces whose directory exists on this machine.
fn project_dirs(workspaces: &Workspaces) -> BTreeMap<String, Vec<String>> {
    workspaces
        .entries()
        .into_iter()
        .filter_map(|ws| {
            let mut names: Vec<String> = std::fs::read_dir(&ws.dir)
                .ok()?
                .filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.'))
                .collect();
            names.sort();
            Some((ws.name, names))
        })
        .collect()
}

fn replace(path: &Path) -> Result<&Path> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing stale {}", path.display()))
        }
        _ => Ok(path),
    }
}

async fn ingest_loop(socket: UnixDatagram, tx: mpsc::Sender<LocalCmd>) {
    let mut buf = vec![0u8; wire::MAX_DATAGRAM + 1024];
    loop {
        let len = match socket.recv(&mut buf).await {
            Ok(len) => len,
            Err(e) => {
                warn!("ingest: {e}");
                continue;
            }
        };
        match serde_json::from_slice::<Envelope>(&buf[..len]) {
            Ok(env) if env.v == wire::VERSION => {
                if tx.send(LocalCmd::Envelope(env)).await.is_err() {
                    return;
                }
            }
            Ok(env) => debug!("ignoring envelope version {}", env.v),
            Err(e) => debug!("bad datagram: {e}"),
        }
    }
}

/// The collector's state: the mirror of this host's sessions, the live
/// channels, the held asks and pending calls.
struct Actor {
    host: String,
    /// Outbound messages, read by the link task.
    hub: mpsc::UnboundedSender<ToHub>,
    connected: bool,
    link_since: i64,
    hub_host: String,
    web_url: String,
    workspaces: Workspaces,
    sessions: HashMap<String, Session>,
    rate_limits: Option<RateLimits>,
    channels: HashMap<u32, (u64, mpsc::Sender<ToChannel>)>,
    /// Held prompts by ask id: the envelope (for a resend) and the hook's
    /// decision slot.
    held: HashMap<String, (Envelope, oneshot::Sender<Value>)>,
    calls: HashMap<u64, oneshot::Sender<Result<Reply, String>>>,
    next_call: u64,
    windows: HashMap<u64, Window>,
    focused: Option<u64>,
    niri_seen: bool,
    updates: broadcast::Sender<Value>,
    /// The hub's live notes by workspace and slug, kept on disk too.
    notes: HashMap<String, HashMap<String, Note>>,
    cache_path: PathBuf,
    /// Sessions that asked while the hub was unreachable, with the moment
    /// the link went down; the next prompt after reconnecting catches up.
    offline: HashMap<String, i64>,
    /// When the link went down, until it comes back.
    down_at: Option<i64>,
    /// How sessions start and stop here (`[launch]`).
    launch: Launch,
    /// `[attach] command`, for the hub's `Open`.
    attach: Vec<String>,
    /// The project directories last reported (`ToHub::Dirs`), and when
    /// they were last read.
    dirs: BTreeMap<String, Vec<String>>,
    dirs_at: i64,
    /// The sync thread, when `[sync]` is on.
    sync: Option<SyncHandle>,
    /// The workspace thread, when `[workspace]` is on.
    workspace: Option<WsHandle>,
}

impl Actor {
    fn new(
        host: String,
        hub: mpsc::UnboundedSender<ToHub>,
        updates: broadcast::Sender<Value>,
        cache_path: PathBuf,
    ) -> Self {
        let cache: NotesCache = std::fs::read(&cache_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let notes = cache
            .notes
            .into_iter()
            .map(|(ws, notes)| (ws, notes.into_iter().map(|n| (n.slug.clone(), n)).collect()))
            .collect();
        Self {
            host,
            hub,
            connected: false,
            link_since: now_ms(),
            hub_host: String::new(),
            web_url: String::new(),
            workspaces: Workspaces::from_entries(&cache.workspaces),
            sessions: HashMap::new(),
            rate_limits: None,
            channels: HashMap::new(),
            held: HashMap::new(),
            calls: HashMap::new(),
            next_call: 0,
            windows: HashMap::new(),
            focused: None,
            niri_seen: false,
            updates,
            notes,
            cache_path,
            offline: HashMap::new(),
            down_at: None,
            launch: Launch {
                command: Vec::new(),
                stop: Vec::new(),
                remove: Vec::new(),
                direnv: Vec::new(),
                env: Vec::new(),
            },
            attach: Vec::new(),
            dirs: BTreeMap::new(),
            dirs_at: 0,
            sync: None,
            workspace: None,
        }
    }

    /// Tells the hub which project directories exist here: always after a
    /// welcome, else when they changed, read at most once a minute.
    fn report_dirs(&mut self, force: bool) {
        let now = now_ms();
        if !force && now - self.dirs_at < DIRS_EVERY_MS {
            return;
        }
        self.dirs_at = now;
        let dirs = project_dirs(&self.workspaces);
        if force || dirs != self.dirs {
            self.dirs = dirs.clone();
            self.to_hub(ToHub::Dirs { dirs });
        }
    }

    fn with_commands(mut self, launch: Launch, attach: Vec<String>) -> Self {
        self.launch = launch;
        self.attach = attach;
        self
    }

    fn with_sync(mut self, sync: Option<SyncHandle>) -> Self {
        self.sync = sync;
        self
    }

    fn with_workspace(mut self, workspace: Option<WsHandle>) -> Self {
        self.workspace = workspace;
        self
    }

    fn kick_sync(&self, cmd: SyncCmd) {
        if let Some(sync) = &self.sync {
            sync.send(cmd);
        }
    }

    fn kick_workspace(&self, cmd: WsCmd) {
        if let Some(ws) = &self.workspace {
            ws.send(cmd);
        }
    }

    /// Notes tool calls in flight (PreToolUse without PostToolUse yet), so
    /// the workspace sync does not apply under a running tool.
    fn note_tool(&self, envelope: &Envelope) {
        let Some(ws) = &self.workspace else {
            return;
        };
        let p = &envelope.payload;
        let (Some(session), Some(event)) =
            (p["session_id"].as_str(), p["hook_event_name"].as_str())
        else {
            return;
        };
        let key = match p["agent_id"].as_str() {
            Some(agent) => format!("{session}:{agent}"),
            None => session.to_owned(),
        };
        match event {
            "PreToolUse" => {
                if let Some(cwd) = p["cwd"].as_str() {
                    ws.busy.start(&key, cwd, envelope.ts);
                }
            }
            "PostToolUse" | "PostToolUseFailure" => ws.busy.end(&key),
            "SessionEnd" => ws.busy.end_session(session),
            _ => {}
        }
    }

    /// The git branch of a session's directory: the one the hub knows,
    /// else the one reported with it.
    fn branch_of(&self, session: Option<&str>, cwd: Option<&str>) -> Option<String> {
        let dir = session
            .and_then(|id| self.sessions.get(id))
            .map(|s| s.cwd.as_str())
            .filter(|c| !c.is_empty())
            .or(cwd)?;
        git::branch(Path::new(dir))
    }

    /// Starts `spec` in the background and tells the hub what came of it;
    /// a session to continue is synced first (it may have run elsewhere).
    fn launch(&self, launch: String, spec: LaunchSpec) {
        let (hub, cfg) = (self.hub.clone(), self.launch.clone());
        let inside = self.workspaces.locate(&spec.cwd).is_some();
        let sync = self.sync.clone().filter(|_| spec.resume.is_some());
        tokio::spawn(async move {
            if let Some(sync) = sync
                && !sync.flush(LAUNCH_FLUSH_WAIT).await
            {
                warn!("launch {launch}: the sync pass did not finish in time");
            }
            let result = if inside {
                actions::launch(&cfg, &spec).await
            } else {
                Err(format!("{} is outside every ccdash workspace", spec.cwd))
            };
            match &result {
                Ok(id) => info!("launch {launch}: started {id} in {}", spec.cwd),
                Err(e) => warn!("launch {launch} in {}: {e}", spec.cwd),
            }
            let _ = hub.send(ToHub::Launched {
                launch,
                session: result.as_ref().ok().cloned(),
                error: result.err(),
            });
        });
    }

    fn stop(&self, id: String) {
        let hub = self.hub.clone();
        let Some(session) = self.sessions.get(&id).cloned() else {
            let _ = hub.send(ToHub::Stopped {
                session: id,
                error: Some("this host does not know the session".into()),
            });
            return;
        };
        let (stop, remove) = (self.launch.stop.clone(), self.launch.remove.clone());
        let sync = self.sync.clone();
        tokio::spawn(async move {
            let result = actions::stop(&stop, &remove, &session).await;
            match &result {
                Ok(how) => info!("stopping {id}: {how}"),
                Err(e) => warn!("stopping {id}: {e}"),
            }
            // NOTE: the hub has the final bytes when the panel says stopped
            if result.is_ok()
                && let Some(sync) = sync
                && !sync.flush(STOP_FLUSH_WAIT).await
            {
                warn!("stopping {id}: the sync pass did not finish in time");
            }
            let _ = hub.send(ToHub::Stopped {
                session: id,
                error: result.err(),
            });
        });
    }

    fn open(&self, id: &str) {
        let Some(session) = self.sessions.get(id).cloned() else {
            warn!("open: no session {id} on this host");
            return;
        };
        let attach = self.attach.clone();
        tokio::spawn(async move {
            if let Err(e) = actions::open_session(&session, &attach).await {
                warn!("open {}: {e:#}", session.id);
            }
        });
    }

    fn to_hub(&self, msg: ToHub) {
        let _ = self.hub.send(msg);
    }

    /// Writes the notes cache; a torn file would only cost offline reads.
    fn save_cache(&self) {
        let cache = NotesCache {
            workspaces: self.workspaces.entries(),
            notes: self
                .notes
                .iter()
                .map(|(ws, notes)| (ws.clone(), notes.values().cloned().collect()))
                .collect(),
        };
        let tmp = self.cache_path.with_extension("json.tmp");
        let result = serde_json::to_vec(&cache)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::set_permissions(&tmp, Permissions::from_mode(0o600)))
            .and_then(|()| std::fs::rename(&tmp, &self.cache_path));
        if let Err(e) = result {
            warn!("notes cache: {e}");
        }
    }

    /// The cached live notes of the workspace holding `cwd`.
    fn cached_notes(&self, who: &Who) -> Result<(String, Option<String>, Vec<Note>), String> {
        let cwd = who
            .pid
            .and_then(|pid| self.sessions.values().find(|s| s.pid == Some(pid)))
            .map(|s| s.cwd.clone())
            .or_else(|| {
                who.session
                    .as_deref()
                    .and_then(|id| self.sessions.get(id))
                    .map(|s| s.cwd.clone())
            })
            .or_else(|| who.cwd.clone())
            .ok_or("cannot tell which session is asking")?;
        let (ws, project) = self
            .workspaces
            .locate(&cwd)
            .ok_or_else(|| format!("{cwd} is outside every ccdash workspace"))?;
        let notes = self
            .notes
            .get(&ws)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        Ok((ws, project, notes))
    }

    fn cached(&self, who: &Who, query: &Query) -> Result<Reply, String> {
        let (_, _, notes) = self.cached_notes(who)?;
        if let Some(slug) = &query.slug {
            let note = notes
                .into_iter()
                .find(|n| &n.slug == slug)
                .ok_or_else(|| format!("no note {slug:?} in the offline cache"))?;
            return Ok(Reply::Note(Box::new(knowledge::NoteView {
                note,
                links: Vec::new(),
                backlinks: Vec::new(),
            })));
        }
        Ok(Reply::Hits(knowledge::search_cached(&notes, query)))
    }

    fn offline_inject(&mut self, who: &Who, what: &Injection) -> Option<String> {
        if let Some(session) = &who.session {
            self.offline
                .entry(session.clone())
                .or_insert(self.link_since);
        }
        match what {
            Injection::SessionStart => {
                let (ws, project, notes) = self.cached_notes(who).ok()?;
                let cards: Vec<knowledge::Card> = notes.iter().map(Note::card).collect();
                knowledge::session_start(&cards, &ws, project.as_deref(), now_ms()).map(|text| {
                    format!(
                        "{text}
(The ccdash hub is unreachable; this comes from the collector's cache and writes are queued.)
"
                    )
                })
            }
            _ => None,
        }
    }

    fn snapshot(&self, now: i64) -> Snapshot {
        let mut sessions: Vec<Session> = self.sessions.values().cloned().collect();
        sessions.sort_by_key(|s| s.started_at);
        Snapshot {
            host: self.host.clone(),
            now,
            workspaces: self.workspaces.names(),
            hosts: vec![HostStatus {
                name: self.host.clone(),
                connected: self.connected,
                since: self.link_since,
            }],
            sessions,
            rate_limits: self.rate_limits.clone(),
        }
    }

    /// Finds a session by full id, background short id, or unique id prefix.
    fn resolve(&self, key: &str) -> Option<&Session> {
        if let Some(s) = self.sessions.get(key) {
            return Some(s);
        }
        if let Some(s) = self
            .sessions
            .values()
            .find(|s| s.short_id.as_deref() == Some(key))
        {
            return Some(s);
        }
        let mut hits = self.sessions.values().filter(|s| s.id.starts_with(key));
        match (hits.next(), hits.next()) {
            (Some(s), None) => Some(s),
            _ => None,
        }
    }

    fn publish(&self, value: Value) {
        let _ = self.updates.send(value);
    }

    fn tagged(&self, kind: &str, value: &impl serde::Serialize) -> Value {
        let mut value = serde_json::to_value(value).unwrap_or(Value::Null);
        if let Value::Object(map) = &mut value {
            map.insert("type".into(), kind.into());
        }
        value
    }

    fn take(&mut self, msg: ToCollector) {
        match msg {
            ToCollector::Welcome {
                host,
                web_url,
                workspaces,
                ..
            } => {
                self.hub_host = host;
                self.web_url = web_url;
                self.workspaces = Workspaces::from_entries(&workspaces);
                self.report_dirs(true);
            }
            ToCollector::Notes { workspace, notes } => {
                self.notes.insert(
                    workspace,
                    notes.into_iter().map(|n| (n.slug.clone(), n)).collect(),
                );
                self.save_cache();
            }
            ToCollector::NoteChanged { note } => {
                let live = matches!(
                    note.status,
                    knowledge::Status::Active | knowledge::Status::Stale
                );
                let map = self.notes.entry(note.workspace.clone()).or_default();
                if live {
                    map.insert(note.slug.clone(), *note);
                } else {
                    map.remove(&note.slug);
                }
                self.save_cache();
            }
            ToCollector::Snapshot {
                sessions,
                rate_limits,
            } => {
                self.sessions = sessions.into_iter().map(|s| (s.id.clone(), s)).collect();
                self.rate_limits = rate_limits;
                self.publish(self.tagged("snapshot", &self.snapshot(now_ms())));
            }
            ToCollector::Update { update } => {
                match &*update {
                    Update::Session { session } => {
                        if session.host == self.host {
                            self.sessions
                                .insert(session.id.clone(), (**session).clone());
                        }
                    }
                    Update::Removed { id, .. } => {
                        self.sessions.remove(id);
                    }
                    Update::RateLimits { rate_limits } => {
                        self.rate_limits = Some(rate_limits.clone());
                    }
                    Update::Hosts { .. } | Update::Notes { .. } | Update::Launch { .. } => {
                        return;
                    }
                }
                self.publish(serde_json::to_value(&update).unwrap_or(Value::Null));
            }
            ToCollector::Say { pid, text } => {
                if let Some((_, tx)) = self.channels.get(&pid)
                    && tx.try_send(ToChannel::Say { text }).is_err()
                {
                    warn!("channel of pid {pid} is not taking messages");
                }
            }
            ToCollector::Decision { id, decision } => {
                if let Some((_, decide)) = self.held.remove(&id) {
                    let _ = decide.send(decision);
                }
            }
            ToCollector::AskRelease { id } => {
                self.held.remove(&id);
            }
            ToCollector::Title { session, title } => {
                if let Some(path) = paths::pending_title(&session) {
                    let result = std::fs::create_dir_all(path.parent().expect("titles dir"))
                        .and_then(|()| std::fs::write(&path, &title));
                    if let Err(e) = result {
                        warn!("queueing a title: {e}");
                    }
                }
            }
            ToCollector::Result { id, result } => {
                if let Some(reply) = self.calls.remove(&id) {
                    let _ = reply.send(result);
                }
            }
            ToCollector::Launch {
                launch,
                cwd,
                prompt,
                model,
                permission_mode,
                resume,
            } => {
                let spec = LaunchSpec {
                    cwd,
                    prompt,
                    model,
                    permission_mode,
                    resume,
                    nonce: launch.clone(),
                };
                self.launch(launch, spec);
            }
            ToCollector::Stop { session, .. } => self.stop(session),
            ToCollector::Open { session } => self.open(&session),
            ToCollector::SyncChanged { .. } => self.kick_sync(SyncCmd::Changed),
            ToCollector::SyncResync => self.kick_sync(SyncCmd::Resync),
            ToCollector::WorkspaceSetup { setup } => self.kick_workspace(WsCmd::Setup(*setup)),
            ToCollector::WorkspaceChanged { unit, host, .. } => {
                self.kick_workspace(WsCmd::Changed { unit, host });
            }
            ToCollector::WorkspaceResync { unit } => self.kick_workspace(WsCmd::Resync(unit)),
            ToCollector::WorkspaceResolve {
                id,
                unit,
                path,
                blob,
            } => {
                if self.workspace.is_some() {
                    self.kick_workspace(WsCmd::Resolve {
                        id,
                        unit,
                        path,
                        blob,
                    });
                } else {
                    self.to_hub(ToHub::WorkspaceResolved {
                        id,
                        error: Some("this machine does not sync its workspace".into()),
                    });
                }
            }
            ToCollector::WorkspaceHeld {
                unit,
                action,
                commit,
            } => {
                self.kick_workspace(WsCmd::Held {
                    unit,
                    action,
                    commit,
                });
            }
            ToCollector::WorkspaceAdopt { unit } => {
                self.kick_workspace(WsCmd::Adopt { unit, prefer: None })
            }
            ToCollector::SyncResolve {
                id,
                path,
                copy,
                keep,
            } => {
                if self.sync.is_some() {
                    self.kick_sync(SyncCmd::Resolve {
                        id,
                        path,
                        copy,
                        keep,
                    });
                } else {
                    self.to_hub(ToHub::SyncResolved {
                        id,
                        error: Some("this machine does not sync".into()),
                        copy: None,
                    });
                }
            }
        }
    }

    /// Tells the hub what it lost while the link was down.
    fn resync(&self) {
        for pid in self.channels.keys() {
            self.to_hub(ToHub::ChannelOpen { pid: *pid });
        }
        if self.niri_seen {
            self.to_hub(ToHub::Niri {
                event: NiriEvent::Windows(self.windows.values().cloned().collect()),
            });
            self.to_hub(ToHub::Niri {
                event: NiriEvent::Focus(self.focused),
            });
        }
        for (id, (envelope, _)) in &self.held {
            self.to_hub(ToHub::Ask {
                id: id.clone(),
                envelope: envelope.clone(),
            });
        }
    }

    fn link_down(&mut self) {
        self.connected = false;
        self.link_since = now_ms();
        self.down_at.get_or_insert(self.link_since);
        // NOTE: a held hook would otherwise wait for a decision nobody can give
        self.held.clear();
        for (_, reply) in self.calls.drain() {
            let _ = reply.send(Err(NOT_CONNECTED.into()));
        }
        self.publish(self.tagged("snapshot", &self.snapshot(now_ms())));
    }
}

async fn run_actor(mut a: Actor, mut rx: mpsc::Receiver<LocalCmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            LocalCmd::Envelope(mut envelope) => {
                if envelope.source == Source::Statusline {
                    let p = &envelope.payload;
                    envelope.branch = a.branch_of(p["session_id"].as_str(), p["cwd"].as_str());
                }
                if envelope.payload["hook_event_name"] == "SessionEnd" {
                    a.kick_sync(SyncCmd::Changed);
                }
                a.note_tool(&envelope);
                a.to_hub(ToHub::Envelope { envelope });
            }
            LocalCmd::Agents(mut list, at) => {
                for e in &mut list {
                    e.branch = a.branch_of(Some(&e.session_id), e.cwd.as_deref());
                }
                if a.connected {
                    a.report_dirs(false);
                }
                let dead: Vec<u32> = a
                    .sessions
                    .values()
                    .filter_map(|s| s.pid)
                    .filter(|pid| !pid_alive(Some(*pid)))
                    .collect();
                a.to_hub(ToHub::Agents { list, at, dead });
            }
            LocalCmd::Niri(event) => {
                match &event {
                    NiriEvent::Windows(list) => {
                        a.niri_seen = true;
                        a.focused = list.iter().find(|w| w.focused).map(|w| w.id);
                        a.windows = list.iter().map(|w| (w.id, w.clone())).collect();
                    }
                    NiriEvent::WindowChanged(w) => {
                        if w.focused {
                            a.focused = Some(w.id);
                        }
                        a.windows.insert(w.id, w.clone());
                    }
                    NiriEvent::WindowClosed(id) => {
                        a.windows.remove(id);
                        if a.focused == Some(*id) {
                            a.focused = None;
                        }
                    }
                    NiriEvent::Focus(id) => a.focused = *id,
                }
                a.to_hub(ToHub::Niri { event });
            }
            LocalCmd::Procs(polls, at) => a.to_hub(ToHub::Procs { polls, at }),
            LocalCmd::Usage(limits, at) => a.to_hub(ToHub::Usage { limits, at }),
            LocalCmd::Telemetry(records) => {
                let records: Vec<TelemetryRecord> = records
                    .into_iter()
                    .filter_map(|msg| match msg {
                        StoreMsg::Telemetry {
                            ts,
                            session,
                            name,
                            data,
                        } => Some(TelemetryRecord {
                            ts,
                            session,
                            name,
                            data,
                        }),
                        _ => None,
                    })
                    .collect();
                if !records.is_empty() {
                    a.to_hub(ToHub::Telemetry { records });
                }
            }
            LocalCmd::Snapshot(reply) => {
                let _ = reply.send(a.snapshot(now_ms()));
            }
            LocalCmd::Resolve(key, reply) => {
                let _ = reply.send(a.resolve(&key).cloned());
            }
            LocalCmd::Pids(reply) => {
                let pids = a
                    .sessions
                    .values()
                    .filter(|s| s.activity != crate::model::Activity::Gone)
                    .filter_map(|s| Some((s.id.clone(), s.pid?)))
                    .collect();
                let _ = reply.send(pids);
            }
            LocalCmd::Call(call, reply) => {
                if !a.connected {
                    let _ = reply.send(Err(NOT_CONNECTED.into()));
                    continue;
                }
                let id = a.next_call;
                a.next_call += 1;
                a.calls.insert(id, reply);
                a.to_hub(ToHub::Call { id, call });
            }
            LocalCmd::Connected(reply) => {
                let _ = reply.send(a.connected);
            }
            LocalCmd::Queue(call) => a.to_hub(ToHub::Queued { call, at: now_ms() }),
            LocalCmd::Cached(who, query, reply) => {
                let _ = reply.send(a.cached(&who, &query));
            }
            LocalCmd::OfflineInject(who, what, reply) => {
                let _ = reply.send(a.offline_inject(&who, &what));
            }
            LocalCmd::CatchUp(session, reply) => {
                let _ = reply.send(a.offline.remove(&session));
            }
            LocalCmd::Injected(session) => {
                if a.connected {
                    a.to_hub(ToHub::Injected { session });
                }
            }
            LocalCmd::ChannelOpen(pid, conn, tx) => {
                a.channels.insert(pid, (conn, tx));
                a.to_hub(ToHub::ChannelOpen { pid });
            }
            LocalCmd::ChannelClosed(pid, conn) => {
                if a.channels.get(&pid).is_none_or(|(c, _)| *c != conn) {
                    continue;
                }
                a.channels.remove(&pid);
                a.to_hub(ToHub::ChannelClosed { pid });
            }
            LocalCmd::Ask(envelope, shown, decide) => {
                let id = util::random_hex(8).unwrap_or_else(|_| now_ms().to_string());
                let _ = shown.send(id.clone());
                if !a.connected {
                    continue;
                }
                a.to_hub(ToHub::Ask {
                    id: id.clone(),
                    envelope: (*envelope).clone(),
                });
                a.held.insert(id, (*envelope, decide));
            }
            LocalCmd::AskGone(id) => {
                if a.held.remove(&id).is_some() {
                    a.to_hub(ToHub::AskGone { id });
                }
            }
            LocalCmd::LinkUp => {
                a.connected = true;
                a.link_since = now_ms();
                // NOTE: every session that lived through the outage catches up at its next prompt
                if let Some(down) = a.down_at.take() {
                    let live: Vec<String> = a
                        .sessions
                        .values()
                        .filter(|s| s.activity != crate::model::Activity::Gone)
                        .map(|s| s.id.clone())
                        .collect();
                    for id in live {
                        a.offline.entry(id).or_insert(down);
                    }
                }
                a.resync();
                a.kick_sync(SyncCmd::Changed);
                a.kick_workspace(WsCmd::Connected(true));
            }
            LocalCmd::LinkDown => {
                a.link_down();
                a.kick_workspace(WsCmd::Connected(false));
            }
            LocalCmd::FromHub(msg) => a.take(*msg),
        }
    }
}
