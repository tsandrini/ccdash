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
    model::{Activity, Ask, HostStatus, RateLimits, ScopedLimit, Session, Snapshot, Update},
    notify::{Composer, Note},
    paths,
    protocol::{Call, TelemetryRecord, ToCollector},
    state::{AgentEntry, Effects, NiriEvent, ProcPoll, State, StoreMsg},
    store::{self, Store},
    util::{self, now_ms},
    wire::Envelope,
    workspace::Workspaces,
};

const MAX_TITLE: usize = 100;
const MAX_REPLY: usize = 20_000;
const GONE: &str = "that prompt is no longer open";

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
        };
        tokio::spawn(run(core, rx));
        Self { tx, updates }
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
                };
                core.tell(
                    &host,
                    ToCollector::Result {
                        id,
                        result: result.map(Box::new),
                    },
                );
            }
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
        self.links.insert(host.clone(), Link { conn, tx });
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
            workspaces: self.workspaces.names(),
        };
        let snapshot = ToCollector::Snapshot {
            sessions,
            rate_limits: self.rate_limits(),
        };
        self.tell(&host, welcome);
        self.tell(&host, snapshot);
        self.hosts_changed();
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
