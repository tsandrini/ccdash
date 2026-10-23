//! The task that owns the hub's state: one reducer per host, the store,
//! the collector links, open asks and notifications. Everything else
//! talks to it through `Cmd`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Workspace name → the directories under it on one host.
pub type Dirs = BTreeMap<String, Vec<String>>;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{info, warn};

use crate::{
    ask::{self, Answer},
    claims,
    config::Knowledge as KnowledgeCfg,
    knowledge::{self, Query},
    messages,
    model::{
        Activity, Ask, HostStatus, Launch, LaunchState, Message, RateLimits, ScopedLimit, Session,
        Snapshot, Update,
    },
    notify::{Composer, Note},
    paths,
    protocol::{Call, Injection, Reply, TelemetryRecord, ToCollector, Who},
    state::{AgentEntry, Effects, NiriEvent, PREVIEW_LEN, ProcPoll, State, StoreMsg},
    store::{self, Store},
    sync::{SyncHost, SyncStatus},
    util::{self, now_ms},
    wire::Envelope,
    workspace::Workspaces,
};

const MAX_TITLE: usize = 100;
const MAX_REPLY: usize = 20_000;
const GONE: &str = "that prompt is no longer open";
const MINUTE_MS: i64 = 60_000;
/// A launch the session has not reported for within this fails.
const LAUNCH_WAIT_MS: i64 = 2 * MINUTE_MS;
/// Launches are kept this long for `GET /launches/{id}`.
const LAUNCH_KEEP_MS: i64 = 15 * MINUTE_MS;

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
    Tick(i64),
    Snapshot(oneshot::Sender<Snapshot>),
    Resolve(String, oneshot::Sender<Option<Session>>),
    /// Renames a session; replies with the updated session or why not.
    Rename(String, String, oneshot::Sender<Result<Session, String>>),
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
    /// Asks `host`'s collector to compare everything with the hub again.
    SyncResync {
        host: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
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
    pub resume: Option<String>,
}

/// A launch and what the hub needs to finish it.
struct Slot {
    launch: Launch,
    cwd: String,
    /// The id `claude --bg` printed, until the session shows up.
    short: Option<String>,
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
    pub fn spawn(
        host: String,
        public_url: String,
        workspaces: Workspaces,
        store: Store,
        notes: Option<Notes>,
        knowledge: KnowledgeCfg,
        tool_output_bytes: usize,
    ) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let (updates, _) = broadcast::channel(256);
        let reader = store
            .reader()
            .inspect_err(|e| warn!("store reader: {e:#}"))
            .ok();
        let core = Core {
            host,
            public_url,
            workspaces,
            states: HashMap::new(),
            links: HashMap::new(),
            hosts: BTreeMap::new(),
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
            dirs: HashMap::new(),
        };
        tokio::spawn(run(core, rx));
        Self { tx, updates }
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

    pub async fn launch_of(&self, id: &str) -> Result<Option<Launch>> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::LaunchOf(id.to_owned(), tx)).await?;
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
    /// The project directories each host reported.
    dirs: HashMap<String, Dirs>,
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
                match input {
                    Input::Envelope(env) => {
                        state.apply(&env, &mut fx);
                        if let (Some(launch), Some(id)) =
                            (&env.launch, env.payload["session_id"].as_str())
                        {
                            nonce = Some((launch.clone(), id.to_owned()));
                        }
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
                core.link_printed(&host);
            }
            Cmd::Tick(now) => {
                core.expire_launches(now);
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
            Cmd::Reply(key, text, reply) => {
                let _ = reply.send(core.reply(&key, &text));
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
                tx,
            } => core.collector_open(host, conn, terminals, tx),
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
                    Call::Reply { session, text } => core.reply(&session, &text),
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
            Cmd::Forget { key, reply } => {
                let _ = reply.send(core.forget(&key));
            }
            Cmd::Open { key, reply } => {
                let _ = reply.send(core.open(&key));
            }
            Cmd::Dirs { host, dirs } => {
                core.dirs.insert(host, dirs);
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
            Cmd::SyncResync { host, reply } => {
                let _ = reply.send(core.sync_resync(&host));
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
        tx: mpsc::Sender<ToCollector>,
    ) {
        let now = now_ms();
        info!("collector {host} connected");
        self.states.entry(host.clone()).or_insert_with(|| {
            State::new(host.clone(), terminals, self.workspaces.clone())
                .with_tool_output_bytes(self.tool_output_bytes)
        });
        self.links.insert(
            host.clone(),
            Link {
                conn,
                tx: tx.clone(),
            },
        );
        self.hosts.insert(
            host.clone(),
            HostStatus {
                name: host.clone(),
                connected: true,
                since: now,
            },
        );
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
        self.push_notes(vec![tx], None);
        self.hosts_changed();
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
        let (workspace, project) = self
            .workspaces
            .locate(cwd)
            .ok_or_else(|| format!("{cwd} is outside every ccdash workspace"))?;
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

    /// The project directory holding `cwd`, else its workspace's, else
    /// `cwd` itself: the base claims are shown and checked against.
    fn root_of(&self, cwd: &str) -> String {
        match self.workspaces.locate(cwd) {
            Some((ws, project)) => {
                let dir = self.workspaces.dir(&ws).unwrap_or(cwd);
                match project {
                    Some(p) => format!("{dir}/{p}"),
                    None => dir.to_owned(),
                }
            }
            None => cwd.to_owned(),
        }
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
                tokio::spawn(async move {
                    let result = store
                        .exec(move |conn| {
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
                tokio::spawn(async move {
                    let session = c.session.clone();
                    let result = store
                        .exec(move |conn| inject(conn, &c, &what, &around, at))
                        .await;
                    let delivered = matches!(&result, Ok(Ok((_, true))));
                    answer(match result {
                        Ok(Ok((text, _))) => Ok(Reply::Context(text)),
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
                ..
            } => {
                let c = caller.expect("located above");
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
                    return answer(Err(format!("{dir} is outside every ccdash workspace")));
                };
                tokio::spawn(async move {
                    let ws = workspace.clone();
                    let result = store
                        .exec(move |conn| {
                            let tx = conn.unchecked_transaction()?;
                            let (mut added, mut updated, mut unchanged) = (0, 0, 0);
                            for up in &notes {
                                let w = knowledge::upsert(&tx, &ws, up, Some("import"), at)
                                    .with_context(|| format!("importing {}", up.slug))?;
                                match (w.created, w.changed) {
                                    (true, _) => added += 1,
                                    (false, true) => updated += 1,
                                    (false, false) => unchanged += 1,
                                }
                            }
                            tx.commit()?;
                            Ok::<_, anyhow::Error>(Reply::Imported {
                                added,
                                updated,
                                unchanged,
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
                let project = self.workspaces.locate(&s.cwd).and_then(|(_, p)| p);
                if project != c.project {
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
                            claims::describe(claim, &self.root_of(&s.cwd), now)
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
                    claims::describe(claim, &self.root_of(&s.cwd), now)
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
            None if replayed => None,
            None => return answer(Err(format!("no session matches {to:?}"))),
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
                "SELECT id, host, cwd FROM sessions WHERE id = ?1 OR id LIKE ?1 || '%' LIMIT 2",
            )?;
            let rows: Vec<(String, String, String)> = stmt
                .query_map([&to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let (id, host, cwd) = match rows.as_slice() {
                [one] => one.clone(),
                [] => bail!("no session matches {to:?}"),
                _ => bail!("{to:?} matches more than one session"),
            };
            if workspaces.of(&cwd).as_deref() != Some(workspace.as_str()) {
                bail!("that session works in another workspace");
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
                let root = self.root_of(&c.cwd);
                let claim = claims::new(
                    spec.what, spec.paths, spec.until, spec.issue, &c.cwd, &root, at,
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
                resume: req.resume,
            },
        );
        self.launches.insert(
            id,
            Slot {
                launch: launch.clone(),
                cwd: req.cwd,
                short: None,
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
        let mut fx = Effects::default();
        let known = self
            .states
            .get_mut(host)
            .is_some_and(|st| st.set_launched(session, &mut fx));
        self.flush(host, fx);
        if known {
            self.settle(
                launch,
                LaunchState::Started {
                    session: session.to_owned(),
                },
            );
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
        slot.launch.state = state;
        let _ = self.updates.send(Update::Launch {
            launch: slot.launch.clone(),
        });
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

    fn stopped(&mut self, host: &str, session: &str, error: Option<String>) {
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
        if let Some(status) = self.hosts.get_mut(host) {
            status.connected = false;
            status.since = now_ms();
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
    }

    fn hosts_changed(&self) {
        let _ = self.updates.send(Update::Hosts {
            hosts: self.hosts.values().cloned().collect(),
        });
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
    /// channel and shows it as queued until Claude takes it.
    fn reply(&mut self, key: &str, text: &str) -> Result<Session, String> {
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
        state.sent_reply(&id, text, now_ms(), &mut fx);
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

    /// Restores, stores and broadcasts what an input changed on `host`;
    /// lets go of asks the state closed and refreshes the notification
    /// candidates.
    fn flush(&mut self, host: &str, fx: Effects) {
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
        if let Some(notes) = &self.notes {
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
            next.sort_by(|a, b| a.seq.cmp(&b.seq));
            notes.tx.send_if_modified(|current| {
                let changed = *current != next;
                if changed {
                    *current = next;
                }
                changed
            });
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
            branch: Some("main".into()),
            payload,
        }
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
        );
        let mut rxs = HashMap::new();
        for (conn, host) in ["navi", "flat"].into_iter().enumerate() {
            let (tx, rx) = mpsc::channel(256);
            hub.send(Cmd::CollectorOpen {
                host: host.into(),
                conn: conn as u64,
                terminals: Vec::new(),
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

    #[tokio::test]
    async fn launches_link_their_session_and_stops_answer() {
        let dir = std::env::temp_dir().join(format!("ccdash-launch-{}", std::process::id()));
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
        );
        let req = |resume: Option<&str>| LaunchReq {
            host: "navi".into(),
            workspace: "w".into(),
            project: Some("p".into()),
            cwd: "/w/p".into(),
            prompt: "go".into(),
            model: None,
            permission_mode: Some("plan".into()),
            resume: resume.map(str::to_owned),
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
            if launch == &first.id && cwd == "/w/p")
        );

        // the session's hooks carry the launch id
        let sid = "9c4719e5-9508-461b-b1f4-e7b5a4c26f9d";
        let start = envelope(
            Source::Hook,
            Some(&first.id),
            json!({"session_id": sid, "cwd": "/w/p", "hook_event_name": "SessionStart"}),
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
            cwd: Some("/w/p".into()),
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
        assert_eq!(waiting.await.unwrap().unwrap().id, bg.id);

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
}
