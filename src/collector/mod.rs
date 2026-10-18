//! `ccdash collector`: the per-machine daemon. It takes what Claude Code
//! and the desktop report (hooks, statusline, `claude agents`, niri,
//! `/proc`, `/usage`, OpenTelemetry), forwards it to the hub, and carries
//! out what the hub sends back: channel replies, ask decisions, queued
//! titles. The local API keeps serving a mirror of this host's sessions for
//! the DMS plugin and the CLI.

mod api;
mod link;
mod queue;

use std::{
    collections::HashMap,
    fs::Permissions,
    os::unix::{fs::PermissionsExt, net::UnixStream as StdUnixStream},
    path::Path,
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
    agents,
    channel::ToChannel,
    config::CollectorConfig,
    model::{HostStatus, RateLimits, ScopedLimit, Session, Snapshot, Update},
    niri, otlp, paths, procs,
    protocol::{Call, TelemetryRecord, ToCollector, ToHub},
    state::{AgentEntry, NiriEvent, ProcPoll, StoreMsg, Window},
    usage,
    util::{self, now_ms, pid_alive},
    wire::{self, Envelope},
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
    Call(Call, oneshot::Sender<Result<Session, String>>),
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

    pub async fn call(&self, call: Call) -> Result<Result<Session, String>> {
        let (tx, rx) = oneshot::channel();
        self.send(LocalCmd::Call(call, tx)).await?;
        match tokio::time::timeout(Duration::from_secs(15), rx).await {
            Ok(result) => result.context("collector stopped"),
            Err(_) => Ok(Err("the hub did not answer in time".into())),
        }
    }
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
    tokio::spawn(run_actor(
        Actor::new(host.clone(), hub_tx, local.updates.clone()),
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
    tokio::spawn(api::serve(api, local.clone(), cfg.clone()));
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

impl Local {
    fn spawn() -> (Self, mpsc::Receiver<LocalCmd>) {
        let (tx, rx) = mpsc::channel(1024);
        let (updates, _) = broadcast::channel(256);
        (Self { tx, updates }, rx)
    }
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
    workspaces: Vec<String>,
    sessions: HashMap<String, Session>,
    rate_limits: Option<RateLimits>,
    channels: HashMap<u32, (u64, mpsc::Sender<ToChannel>)>,
    /// Held prompts by ask id: the envelope (for a resend) and the hook's
    /// decision slot.
    held: HashMap<String, (Envelope, oneshot::Sender<Value>)>,
    calls: HashMap<u64, oneshot::Sender<Result<Session, String>>>,
    next_call: u64,
    windows: HashMap<u64, Window>,
    focused: Option<u64>,
    niri_seen: bool,
    updates: broadcast::Sender<Value>,
}

impl Actor {
    fn new(
        host: String,
        hub: mpsc::UnboundedSender<ToHub>,
        updates: broadcast::Sender<Value>,
    ) -> Self {
        Self {
            host,
            hub,
            connected: false,
            link_since: now_ms(),
            hub_host: String::new(),
            web_url: String::new(),
            workspaces: Vec::new(),
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
        }
    }

    fn to_hub(&self, msg: ToHub) {
        let _ = self.hub.send(msg);
    }

    fn snapshot(&self, now: i64) -> Snapshot {
        let mut sessions: Vec<Session> = self.sessions.values().cloned().collect();
        sessions.sort_by_key(|s| s.started_at);
        Snapshot {
            host: self.host.clone(),
            now,
            workspaces: self.workspaces.clone(),
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
                self.workspaces = workspaces;
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
                    Update::Hosts { .. } => return,
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
                    let _ = reply.send(result.map(|s| *s));
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
        // NOTE: a held hook would otherwise wait for a decision nobody can give
        self.held.clear();
        for (_, reply) in self.calls.drain() {
            let _ = reply.send(Err("the hub is not connected".into()));
        }
        self.publish(self.tagged("snapshot", &self.snapshot(now_ms())));
    }
}

async fn run_actor(mut a: Actor, mut rx: mpsc::Receiver<LocalCmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            LocalCmd::Envelope(envelope) => a.to_hub(ToHub::Envelope { envelope }),
            LocalCmd::Agents(list, at) => {
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
                    let _ = reply.send(Err("the hub is not connected".into()));
                    continue;
                }
                let id = a.next_call;
                a.next_call += 1;
                a.calls.insert(id, reply);
                a.to_hub(ToHub::Call { id, call });
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
                a.resync();
            }
            LocalCmd::LinkDown => a.link_down(),
            LocalCmd::FromHub(msg) => a.take(*msg),
        }
    }
}
