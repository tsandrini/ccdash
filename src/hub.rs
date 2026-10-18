//! The task that owns the session state. Everything else talks to it through `Cmd`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::warn;

use crate::{
    ask::{self, Answer},
    channel::ToChannel,
    model::{Activity, Ask, ScopedLimit, Session, Snapshot, Update},
    notify::{Composer, Note},
    paths,
    state::{AgentEntry, Effects, NiriEvent, ProcPoll, State},
    store::{self, Store},
    util::{self, now_ms},
    wire::Envelope,
};

const MAX_TITLE: usize = 100;
const MAX_REPLY: usize = 20_000;
const GONE: &str = "that prompt is no longer open";

pub enum Input {
    Envelope(Envelope),
    Agents(Vec<AgentEntry>, i64),
    Niri(NiriEvent),
    Procs(Vec<ProcPoll>, i64),
    Usage(Vec<ScopedLimit>, i64),
    Tick(i64),
}

pub enum Cmd {
    Input(Input),
    Snapshot(oneshot::Sender<Snapshot>),
    Resolve(String, oneshot::Sender<Option<Session>>),
    Pids(oneshot::Sender<Vec<(String, u32)>>),
    /// Renames a session; replies with the updated session or why not.
    Rename(String, String, oneshot::Sender<Result<Session, String>>),
    /// A `ccdash channel` connected for the Claude process `pid`; `u64`
    /// tells its connections apart.
    ChannelOpen(u32, u64, mpsc::Sender<ToChannel>),
    ChannelClosed(u32, u64),
    /// Sends a message to a session through its channel.
    Reply(String, String, oneshot::Sender<Result<Session, String>>),
    /// `ccdash ask` holds a prompt open: the PermissionRequest event. The
    /// first sender gets the ask id when ccdash shows it, the second the
    /// decision; dropping the second lets the hook go.
    Ask(
        Box<Envelope>,
        oneshot::Sender<Option<String>>,
        oneshot::Sender<Value>,
    ),
    /// The hook holding ask `id` went away.
    AskGone(String),
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
}

type Channels = HashMap<u32, (u64, mpsc::Sender<ToChannel>)>;

/// What answering an open ask needs and the session doesn't show.
struct Pending {
    session: String,
    /// Secret in the notification buttons' links.
    key: String,
    input: Value,
    updates: Vec<Value>,
    decide: oneshot::Sender<Value>,
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
    pub fn spawn(state: State, store: Store, notes: Option<Notes>) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let (updates, _) = broadcast::channel(256);
        let reader = store
            .reader()
            .inspect_err(|e| warn!("store reader: {e:#}"))
            .ok();
        let core = Core {
            state,
            store,
            reader,
            published: HashMap::new(),
            channels: Channels::new(),
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

    /// `(session id, pid)` of live sessions.
    pub async fn pids(&self) -> Result<Vec<(String, u32)>> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Pids(tx)).await?;
        rx.await.context("hub stopped")
    }

    pub async fn resolve(&self, key: &str) -> Result<Option<Session>> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Resolve(key.to_owned(), tx)).await?;
        rx.await.context("hub stopped")
    }
}

struct Core {
    state: State,
    store: Store,
    reader: Option<Connection>,
    published: HashMap<String, Session>,
    channels: Channels,
    /// Open asks by id.
    pending: HashMap<String, Pending>,
    updates: broadcast::Sender<Update>,
    notes: Option<Notes>,
}

async fn run(mut core: Core, mut rx: mpsc::Receiver<Cmd>) {
    while let Some(cmd) = rx.recv().await {
        let mut fx = Effects::default();
        match cmd {
            Cmd::Input(input) => match input {
                Input::Envelope(env) => core.state.apply(&env, &mut fx),
                Input::Agents(list, now) => core.state.agents(&list, now, &mut fx),
                Input::Niri(event) => core.state.niri(event, &mut fx),
                Input::Procs(polls, now) => core.state.resources(polls, now, &mut fx),
                Input::Usage(limits, now) => core.state.usage(limits, now, &mut fx),
                Input::Tick(now) => core.state.tick(now, &mut fx),
            },
            Cmd::Snapshot(reply) => {
                let _ = reply.send(core.state.snapshot(now_ms()));
                continue;
            }
            Cmd::Resolve(key, reply) => {
                let _ = reply.send(core.state.resolve(&key).cloned());
                continue;
            }
            Cmd::Pids(reply) => {
                let _ = reply.send(core.state.pids());
                continue;
            }
            Cmd::Rename(key, title, reply) => {
                let _ = reply.send(rename(&mut core.state, &key, &title, &mut fx));
            }
            Cmd::ChannelOpen(pid, conn, tx) => {
                core.channels.insert(pid, (conn, tx));
                core.state.channel(pid, true, &mut fx);
            }
            Cmd::ChannelClosed(pid, conn) => {
                if core.channels.get(&pid).is_none_or(|(c, _)| *c != conn) {
                    continue;
                }
                core.channels.remove(&pid);
                core.state.channel(pid, false, &mut fx);
            }
            Cmd::Reply(key, text, reply) => {
                let result = send_reply(&mut core.state, &core.channels, &key, &text, &mut fx);
                let _ = reply.send(result);
            }
            Cmd::Ask(env, shown, decide) => {
                core.state.apply(&env, &mut fx);
                let _ = shown.send(core.open_ask(&env, decide, &mut fx));
            }
            Cmd::AskGone(id) => {
                if let Some(p) = core.pending.remove(&id) {
                    core.state.remove_ask(&p.session, &id, &mut fx);
                }
            }
            Cmd::Answer {
                key,
                ask,
                answer,
                reply,
            } => {
                let result = match core.state.resolve(&key).map(|s| s.id.clone()) {
                    None => Err(format!("no session matches {key:?}")),
                    Some(id) => core
                        .answer(&id, &ask, &answer, "web", &mut fx)
                        .and_then(|()| core.state.session(&id).cloned().ok_or(GONE.into())),
                };
                let _ = reply.send(result);
            }
            Cmd::Button { key, action, reply } => {
                let _ = reply.send(core.button(&key, &action, &mut fx));
            }
        }
        core.flush(fx);
    }
}

impl Core {
    /// Shows the prompt in `env` and keeps what answering it needs; the
    /// ask id, or None when ccdash doesn't show it.
    fn open_ask(
        &mut self,
        env: &Envelope,
        decide: oneshot::Sender<Value>,
        fx: &mut Effects,
    ) -> Option<String> {
        let session = env.payload["session_id"].as_str()?.to_owned();
        let (kind, input, updates) = ask::parse(&env.payload);
        let (id, key) = (util::random_hex(6).ok()?, util::random_hex(16).ok()?);
        let ask = Ask {
            id: id.clone(),
            agent_id: env.payload["agent_id"].as_str().map(str::to_owned),
            at: env.ts,
            kind,
        };
        if !self.state.add_ask(&session, ask, fx) {
            return None;
        }
        self.pending.insert(
            id.clone(),
            Pending {
                session,
                key,
                input,
                updates,
                decide,
            },
        );
        Some(id)
    }

    /// Hands the decision for `answer` to the hook holding ask `id`.
    fn answer(
        &mut self,
        session: &str,
        id: &str,
        answer: &Answer,
        via: &str,
        fx: &mut Effects,
    ) -> Result<(), String> {
        let (Some(p), Some(ask)) = (
            self.pending.get(id).filter(|p| p.session == session),
            self.state.ask(session, id),
        ) else {
            return Err(GONE.into());
        };
        let decision = ask::decision(&ask.kind, &p.input, &p.updates, answer)?;
        let p = self.pending.remove(id).expect("found above");
        p.decide.send(decision).map_err(|_| GONE.to_owned())?;
        self.state.answered(session, id, answer, via, now_ms(), fx);
        Ok(())
    }

    fn button(&mut self, key: &str, action: &str, fx: &mut Effects) -> Result<(), String> {
        let (id, session) = self
            .pending
            .iter()
            .find(|(_, p)| util::constant_time_eq(p.key.as_bytes(), key.as_bytes()))
            .map(|(id, p)| (id.clone(), p.session.clone()))
            .ok_or(GONE)?;
        let ask = self.state.ask(&session, &id).ok_or(GONE)?;
        let answer = Answer::from_button(&ask.kind, action).ok_or("unknown button")?;
        self.answer(&session, &id, &answer, "notification", fx)
    }

    /// Restores, stores and broadcasts what an input changed; lets go of
    /// asks the state closed and refreshes the notification candidates.
    fn flush(&mut self, mut fx: Effects) {
        if let Some(conn) = &self.reader {
            for id in fx.created.clone() {
                match store::restored(conn, &id) {
                    Ok(saved) => self.state.restore(&id, saved),
                    Err(e) => warn!("restoring {id}: {e:#}"),
                }
                let queued =
                    paths::pending_title(&id).and_then(|p| std::fs::read_to_string(p).ok());
                if let Some(title) = queued.filter(|t| !t.trim().is_empty()) {
                    self.state.mark_pending(&id, title.trim(), &mut fx);
                }
            }
        }
        for msg in fx.store {
            self.store.send(msg);
        }
        for id in fx.touched {
            let Some(session) = self.state.session(&id) else {
                continue;
            };
            if self.published.get(&id) != Some(session) {
                self.published.insert(id, session.clone());
                let _ = self.updates.send(Update::Session {
                    session: Box::new(session.clone()),
                });
            }
        }
        for id in fx.removed {
            self.published.remove(&id);
            let _ = self.updates.send(Update::Removed { id });
        }
        if fx.rate_limits
            && let Some(limits) = self.state.rate_limits()
        {
            let _ = self.updates.send(Update::RateLimits {
                rate_limits: limits.clone(),
            });
        }
        let state = &self.state;
        self.pending
            .retain(|id, p| state.ask(&p.session, id).is_some());
        if let Some(notes) = &self.notes {
            let keys: HashMap<&str, &str> = self
                .pending
                .iter()
                .map(|(id, p)| (id.as_str(), p.key.as_str()))
                .collect();
            let next = notes.composer.notes(state, &keys, now_ms());
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

/// Validates `title`, queues it for the session's next prompt and shows it.
fn rename(state: &mut State, key: &str, title: &str, fx: &mut Effects) -> Result<Session, String> {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return Err("the title is empty".into());
    }
    if title.chars().count() > MAX_TITLE {
        return Err(format!("titles are at most {MAX_TITLE} characters"));
    }
    let id = state
        .resolve(key)
        .map(|s| s.id.clone())
        .ok_or_else(|| format!("no session matches {key:?}"))?;
    let path = paths::pending_title(&id).ok_or("unusual session id")?;
    std::fs::create_dir_all(path.parent().expect("titles dir"))
        .and_then(|_| std::fs::write(&path, &title))
        .map_err(|e| format!("queueing the title: {e}"))?;
    state.rename(&id, &title, fx)?;
    state
        .session(&id)
        .cloned()
        .ok_or_else(|| "session vanished".into())
}

/// Passes `text` to the session and shows it as queued until Claude takes it.
fn send_reply(
    state: &mut State,
    channels: &Channels,
    key: &str,
    text: &str,
    fx: &mut Effects,
) -> Result<Session, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the message is empty".into());
    }
    if text.chars().count() > MAX_REPLY {
        return Err(format!("messages are at most {MAX_REPLY} characters"));
    }
    let s = state
        .resolve(key)
        .ok_or_else(|| format!("no session matches {key:?}"))?;
    if s.activity == Activity::Gone {
        return Err("the session has ended".into());
    }
    let tx = s
        .pid
        .filter(|_| s.channel)
        .and_then(|pid| channels.get(&pid))
        .map(|(_, tx)| tx)
        .ok_or("the session runs without the ccdash channel; restart it with `claude --resume`")?;
    let id = s.id.clone();
    tx.try_send(ToChannel::Say {
        text: text.to_owned(),
    })
    .map_err(|_| "the session's channel is not responding")?;
    state.sent_reply(&id, text, now_ms(), fx);
    state
        .session(&id)
        .cloned()
        .ok_or_else(|| "session vanished".into())
}
