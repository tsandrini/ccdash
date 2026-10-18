//! The task that owns the hub's state: one reducer per host, the store,
//! the collector links, open asks and notifications. Everything else
//! talks to it through `Cmd`.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{info, warn};

use crate::{
    ask::{self, Answer},
    config::Knowledge as KnowledgeCfg,
    knowledge::{self, Query},
    model::{Activity, Ask, HostStatus, RateLimits, ScopedLimit, Session, Snapshot, Update},
    notify::{Composer, Note},
    paths,
    protocol::{Call, Injection, Reply, TelemetryRecord, ToCollector, Who},
    state::{AgentEntry, Effects, NiriEvent, ProcPoll, State, StoreMsg},
    store::{self, Store},
    util::{self, now_ms},
    wire::Envelope,
    workspace::Workspaces,
};

const MAX_TITLE: usize = 100;
const MAX_REPLY: usize = 20_000;
const GONE: &str = "that prompt is no longer open";
const MINUTE_MS: i64 = 60_000;

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
            pending: HashMap::new(),
            updates: updates.clone(),
            notes,
            knowledge,
            cmds: tx.clone(),
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
    published: HashMap<String, Session>,
    /// Open asks by id.
    pending: HashMap<String, Pending>,
    updates: broadcast::Sender<Update>,
    notes: Option<Notes>,
    knowledge: KnowledgeCfg,
    /// For tasks that finish a knowledge call and report back.
    cmds: mpsc::Sender<Cmd>,
}

/// The session behind a knowledge call, placed in its workspace.
#[derive(Debug, Clone)]
struct Caller {
    host: String,
    workspace: String,
    project: Option<String>,
    session: Option<String>,
    title: Option<String>,
}

async fn run(mut core: Core, mut rx: mpsc::Receiver<Cmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Input(host, input) => {
                let mut fx = Effects::default();
                let Some(state) = core.states.get_mut(&host) else {
                    continue;
                };
                match input {
                    Input::Envelope(env) => state.apply(&env, &mut fx),
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
            }
            Cmd::Tick(now) => {
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
        }
    }
}

impl Core {
    fn snapshot(&self, now: i64) -> Snapshot {
        let mut sessions: Vec<Session> = self
            .states
            .values()
            .flat_map(|s| s.sessions().cloned())
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
    /// id prefix.
    fn resolve(&self, key: &str) -> Option<(&str, &Session)> {
        let mut hits = self
            .states
            .iter()
            .filter_map(|(host, state)| Some((host.as_str(), state.resolve(key)?)));
        match (hits.next(), hits.next()) {
            (Some(hit), None) => Some(hit),
            (Some(hit), Some(_)) if hit.1.id == key => Some(hit),
            _ => None,
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
        self.states
            .entry(host.clone())
            .or_insert_with(|| State::new(host.clone(), terminals, self.workspaces.clone()));
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
        })
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
            | Call::Tell { who, .. } => Some(who),
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
                let others = match &what {
                    Injection::Prompt { .. } => self.others_active(&c, at),
                    _ => Vec::new(),
                };
                tokio::spawn(async move {
                    let result = store
                        .exec(move |conn| inject(conn, &c, &what, others, at))
                        .await;
                    answer(flatten(result));
                });
            }
            Call::Tell { to, text, .. } => {
                let c = caller.expect("located above");
                answer(self.tell_session(&c, &to, &text));
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
            Call::Rename { .. } | Call::Reply { .. } | Call::Answer { .. } => {
                answer(Err("not a knowledge call".into()));
            }
        }
    }

    /// Live sessions of the caller's project, on any host, as lines for
    /// the prompt injection.
    fn others_active(&self, c: &Caller, now: i64) -> Vec<String> {
        let mut lines = Vec::new();
        for (host, state) in &self.states {
            for s in state.sessions() {
                if Some(&s.id) == c.session.as_ref()
                    || matches!(s.activity, Activity::Gone | Activity::Parked)
                    || s.workspace.as_deref() != Some(c.workspace.as_str())
                {
                    continue;
                }
                let project = self.workspaces.locate(&s.cwd).and_then(|(_, p)| p);
                if project != c.project {
                    continue;
                }
                let label = s
                    .title
                    .clone()
                    .or_else(|| s.name.clone())
                    .unwrap_or_else(|| s.id.chars().take(8).collect());
                let doing = match s.activity {
                    Activity::Working => "working",
                    Activity::Permission => "waiting for a permission",
                    Activity::YourTurn => "waiting for the user",
                    Activity::Idle => "idle",
                    Activity::Gone | Activity::Parked => continue,
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
                lines.push(format!(
                    "- \"{label}\" on {host} ({}): {doing} for {since} min{files}",
                    s.id.chars().take(8).collect::<String>()
                ));
            }
        }
        lines.sort();
        lines
    }

    /// Passes `text` from the caller to another session of its workspace.
    fn tell_session(&mut self, c: &Caller, to: &str, text: &str) -> Result<Reply, String> {
        let (_, target) = self
            .resolve(to)
            .ok_or_else(|| format!("no session matches {to:?}"))?;
        if target.workspace.as_deref() != Some(c.workspace.as_str()) {
            return Err("that session works in another workspace".into());
        }
        if Some(&target.id) == c.session.as_ref() {
            return Err("that is this session".into());
        }
        let from = c
            .title
            .clone()
            .or_else(|| c.session.as_ref().map(|s| s.chars().take(8).collect()))
            .unwrap_or_else(|| "another session".into());
        let message = format!(
            "Message from the Claude Code session \"{from}\" on {} (not from the user):\n{text}",
            c.host
        );
        let key = target.id.clone();
        self.reply(&key, &message).map(|_| Reply::Sent)
    }

    fn collector_closed(&mut self, host: &str, conn: u64) {
        if self.links.get(host).is_none_or(|l| l.conn != conn) {
            return;
        }
        info!("collector {host} disconnected");
        self.links.remove(host);
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
        for id in fx.touched {
            let Some(session) = state.session(&id) else {
                continue;
            };
            if self.published.get(&id) != Some(session) {
                self.published.insert(id, session.clone());
                let update = Update::Session {
                    session: Box::new(session.clone()),
                };
                down.push(ToCollector::Update {
                    update: Box::new(update.clone()),
                });
                let _ = self.updates.send(update);
            }
        }
        for id in fx.removed {
            self.published.remove(&id);
            let update = Update::Removed {
                host: host.to_owned(),
                id,
            };
            down.push(ToCollector::Update {
                update: Box::new(update.clone()),
            });
            let _ = self.updates.send(update);
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

/// The text a hook injects.
fn inject(
    conn: &Connection,
    c: &Caller,
    what: &Injection,
    others: Vec<String>,
    now: i64,
) -> Result<Reply> {
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
            if !others.is_empty() {
                let scope = c.project.clone().unwrap_or_else(|| c.workspace.clone());
                out.push_str(&format!("Also active in {scope} right now (ccdash):\n"));
                for line in &others {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            (!out.is_empty()).then_some(out)
        }
        Injection::Edit { path } => {
            let Some(session) = c.session.as_deref() else {
                return Ok(Reply::Context(None));
            };
            let edits = knowledge::recent_edits(conn, session, path, now)?;
            (!edits.is_empty()).then(|| {
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
                format!(
                    "ccdash: another session edited this file recently ({}). Re-read it before \
                        changing it, and consider `tell_session` if you both work on it.",
                    who.join("; ")
                )
            })
        }
    };
    Ok(Reply::Context(text))
}
