//! The task that owns the session state. Everything else talks to it through `Cmd`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::warn;

use crate::{
    model::{Session, Snapshot, Update},
    paths,
    state::{AgentEntry, Effects, NiriEvent, ProcPoll, State},
    store::{self, Store},
    util::now_ms,
    wire::Envelope,
};

const MAX_TITLE: usize = 100;

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
}

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
        self.tx
            .send(Cmd::Rename(key.to_owned(), title.to_owned(), tx))
            .await
            .ok()
            .context("hub stopped")?;
        rx.await.context("hub stopped")
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
