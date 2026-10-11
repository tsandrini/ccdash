//! The task that owns the session state. Everything else talks to it through `Cmd`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::warn;

use crate::{
    model::{Session, Snapshot, Update},
    state::{AgentEntry, Effects, NiriEvent, State},
    store::{self, Store},
    util::now_ms,
    wire::Envelope,
};

pub enum Input {
    Envelope(Envelope),
    Agents(Vec<AgentEntry>, i64),
    Niri(NiriEvent),
    Tick(i64),
}

pub enum Cmd {
    Input(Input),
    Snapshot(oneshot::Sender<Snapshot>),
    Resolve(String, oneshot::Sender<Option<Session>>),
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
        };

        let mut fx = Effects::default();
        match input {
            Input::Envelope(env) => state.apply(&env, &mut fx),
            Input::Agents(list, now) => state.agents(&list, now, &mut fx),
            Input::Niri(event) => state.niri(event, &mut fx),
            Input::Tick(now) => state.tick(now, &mut fx),
        }
        if let Some(conn) = &reader {
            for id in &fx.created {
                match store::restored(conn, id) {
                    Ok(saved) => state.restore(id, saved),
                    Err(e) => warn!("restoring {id}: {e:#}"),
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
}
