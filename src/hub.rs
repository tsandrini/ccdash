//! The task that owns the session state. Everything else talks to it through `Cmd`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::warn;

use crate::{
    channel::{Behavior, FromChannel, ToChannel},
    model::{Activity, PermissionAsk, Session, Snapshot, Update},
    paths,
    state::{AgentEntry, Effects, NiriEvent, ProcPoll, State},
    store::{self, Store},
    util::now_ms,
    wire::Envelope,
};

const MAX_TITLE: usize = 100;
const MAX_REPLY: usize = 20_000;

pub enum Input {
    Envelope(Envelope),
    Agents(Vec<AgentEntry>, i64),
    Niri(NiriEvent),
    Procs(Vec<ProcPoll>, i64),
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
    FromChannel(u32, FromChannel),
    /// Sends a message to a session through its channel.
    Reply(String, String, oneshot::Sender<Result<Session, String>>),
    /// Answers a session's open permission prompt (by request id).
    Answer(
        String,
        String,
        Behavior,
        oneshot::Sender<Result<Session, String>>,
    ),
}

type Channels = HashMap<u32, (u64, mpsc::Sender<ToChannel>)>;

#[derive(Clone)]
pub struct Hub {
    tx: mpsc::Sender<Cmd>,
    updates: broadcast::Sender<Update>,
}

impl Hub {
    pub fn spawn(state: State, store: Store) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let (updates, _) = broadcast::channel(256);
        let reader = store
            .reader()
            .inspect_err(|e| warn!("store reader: {e:#}"))
            .ok();
        tokio::spawn(run(state, store, reader, rx, updates.clone()));
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
        self.tx
            .send(Cmd::Snapshot(tx))
            .await
            .ok()
            .context("hub stopped")?;
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
        request_id: &str,
        behavior: Behavior,
    ) -> Result<Result<Session, String>> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Answer(
            key.to_owned(),
            request_id.to_owned(),
            behavior,
            tx,
        ))
        .await?;
        rx.await.context("hub stopped")
    }

    pub async fn send(&self, cmd: Cmd) -> Result<()> {
        self.tx.send(cmd).await.ok().context("hub stopped")
    }

    /// `(session id, pid)` of live sessions.
    pub async fn pids(&self) -> Result<Vec<(String, u32)>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Pids(tx))
            .await
            .ok()
            .context("hub stopped")?;
        rx.await.context("hub stopped")
    }

    pub async fn resolve(&self, key: &str) -> Result<Option<Session>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Resolve(key.to_owned(), tx))
            .await
            .ok()
            .context("hub stopped")?;
        rx.await.context("hub stopped")
    }
}

async fn run(
    mut state: State,
    store: Store,
    reader: Option<Connection>,
    mut rx: mpsc::Receiver<Cmd>,
    updates: broadcast::Sender<Update>,
) {
    let mut published: HashMap<String, Session> = HashMap::new();
    let mut channels = Channels::new();
    while let Some(cmd) = rx.recv().await {
        let input = match cmd {
            Cmd::Input(input) => input,
            Cmd::Snapshot(reply) => {
                let _ = reply.send(state.snapshot(now_ms()));
                continue;
            }
            Cmd::Resolve(key, reply) => {
                let _ = reply.send(state.resolve(&key).cloned());
                continue;
            }
            Cmd::Pids(reply) => {
                let _ = reply.send(state.pids());
                continue;
            }
            Cmd::Rename(key, title, reply) => {
                let mut fx = Effects::default();
                let result = rename(&mut state, &key, &title, &mut fx);
                let _ = reply.send(result);
                flush(
                    &mut state,
                    &store,
                    reader.as_ref(),
                    fx,
                    &mut published,
                    &updates,
                );
                continue;
            }
            Cmd::ChannelOpen(pid, conn, tx) => {
                channels.insert(pid, (conn, tx));
                let mut fx = Effects::default();
                state.channel(pid, true, &mut fx);
                flush(
                    &mut state,
                    &store,
                    reader.as_ref(),
                    fx,
                    &mut published,
                    &updates,
                );
                continue;
            }
            Cmd::ChannelClosed(pid, conn) => {
                if channels.get(&pid).is_none_or(|(c, _)| *c != conn) {
                    continue;
                }
                channels.remove(&pid);
                let mut fx = Effects::default();
                state.channel(pid, false, &mut fx);
                flush(
                    &mut state,
                    &store,
                    reader.as_ref(),
                    fx,
                    &mut published,
                    &updates,
                );
                continue;
            }
            Cmd::FromChannel(
                pid,
                FromChannel::PermissionRequest {
                    request_id,
                    tool_name,
                    description,
                    input_preview,
                },
            ) => {
                let ask = PermissionAsk {
                    request_id,
                    tool: tool_name,
                    description,
                    preview: input_preview,
                    at: now_ms(),
                };
                let mut fx = Effects::default();
                state.permission_ask(pid, ask, &mut fx);
                flush(
                    &mut state,
                    &store,
                    reader.as_ref(),
                    fx,
                    &mut published,
                    &updates,
                );
                continue;
            }
            Cmd::Reply(key, text, reply) => {
                let mut fx = Effects::default();
                let result = send_reply(&mut state, &channels, &key, &text, &mut fx);
                let _ = reply.send(result);
                flush(
                    &mut state,
                    &store,
                    reader.as_ref(),
                    fx,
                    &mut published,
                    &updates,
                );
                continue;
            }
            Cmd::Answer(key, request_id, behavior, reply) => {
                let mut fx = Effects::default();
                let result = answer(&mut state, &channels, &key, &request_id, behavior, &mut fx);
                let _ = reply.send(result);
                flush(
                    &mut state,
                    &store,
                    reader.as_ref(),
                    fx,
                    &mut published,
                    &updates,
                );
                continue;
            }
        };

        let mut fx = Effects::default();
        match input {
            Input::Envelope(env) => state.apply(&env, &mut fx),
            Input::Agents(list, now) => state.agents(&list, now, &mut fx),
            Input::Niri(event) => state.niri(event, &mut fx),
            Input::Procs(polls, now) => state.resources(polls, now, &mut fx),
            Input::Tick(now) => state.tick(now, &mut fx),
        }
        flush(
            &mut state,
            &store,
            reader.as_ref(),
            fx,
            &mut published,
            &updates,
        );
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

/// The live session `key` names and the channel of its process.
fn channel_of<'a>(
    state: &'a State,
    channels: &'a Channels,
    key: &str,
) -> Result<(&'a Session, &'a mpsc::Sender<ToChannel>), String> {
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
    Ok((s, tx))
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
    let (s, tx) = channel_of(state, channels, key)?;
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

/// Answers the session's open permission prompt `request_id`.
fn answer(
    state: &mut State,
    channels: &Channels,
    key: &str,
    request_id: &str,
    behavior: Behavior,
    fx: &mut Effects,
) -> Result<Session, String> {
    let (s, tx) = channel_of(state, channels, key)?;
    if s.permission_ask
        .as_ref()
        .is_none_or(|a| a.request_id != request_id)
    {
        return Err("that permission prompt is no longer open".into());
    }
    let id = s.id.clone();
    tx.try_send(ToChannel::Verdict {
        request_id: request_id.to_owned(),
        behavior,
    })
    .map_err(|_| "the session's channel is not responding")?;
    state.answered(&id, request_id, fx);
    state
        .session(&id)
        .cloned()
        .ok_or_else(|| "session vanished".into())
}

/// Restores, stores and broadcasts what an input changed.
fn flush(
    state: &mut State,
    store: &Store,
    reader: Option<&Connection>,
    mut fx: Effects,
    published: &mut HashMap<String, Session>,
    updates: &broadcast::Sender<Update>,
) {
    if let Some(conn) = reader {
        for id in fx.created.clone() {
            match store::restored(conn, &id) {
                Ok(saved) => state.restore(&id, saved),
                Err(e) => warn!("restoring {id}: {e:#}"),
            }
            let queued = paths::pending_title(&id).and_then(|p| std::fs::read_to_string(p).ok());
            if let Some(title) = queued.filter(|t| !t.trim().is_empty()) {
                state.mark_pending(&id, title.trim(), &mut fx);
            }
        }
    }
    for msg in fx.store {
        store.send(msg);
    }
    for id in fx.touched {
        let Some(session) = state.session(&id) else {
            continue;
        };
        if published.get(&id) != Some(session) {
            published.insert(id, session.clone());
            let _ = updates.send(Update::Session {
                session: Box::new(session.clone()),
            });
        }
    }
    for id in fx.removed {
        published.remove(&id);
        let _ = updates.send(Update::Removed { id });
    }
    if fx.rate_limits
        && let Some(limits) = state.rate_limits()
    {
        let _ = updates.send(Update::RateLimits {
            rate_limits: limits.clone(),
        });
    }
}
