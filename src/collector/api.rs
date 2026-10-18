//! Local API on `api.sock`: newline-delimited JSON, one request per line.
//!
//! Requests: `{"cmd":"snapshot"}`, `{"cmd":"subscribe"}`, `{"cmd":"open","id":"…"}`,
//! `{"cmd":"rename","id":"…","title":"…"}`, `{"cmd":"reply","id":"…","text":"…"}`,
//! `{"cmd":"answer","id":"…","ask":"…","answer":{"action":"allow"}}` (see
//! `crate::ask::Answer`). Replies carry a `type`: `snapshot`, `session`,
//! `removed`, `rate_limits`, `ok`, `error`. Reads come from the mirror of
//! this host's sessions; rename, reply and answer go to the hub.
//!
//! `{"cmd":"channel","pid":N}` turns the connection into the link of a
//! `ccdash channel` (see `crate::channel`); `{"cmd":"ask","envelope":{…}}`
//! into a prompt held by `ccdash ask`, answered with one
//! `{"type":"decision","decision":{…}}` line or closed without one.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{broadcast::error::RecvError, mpsc, oneshot},
};
use tracing::debug;

use super::{Local, LocalCmd};
use crate::{actions, ask::Answer, config::CollectorConfig, protocol::Call, wire::Envelope};

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    Snapshot,
    Subscribe,
    Open {
        id: String,
    },
    Rename {
        id: String,
        title: String,
    },
    Reply {
        id: String,
        text: String,
    },
    Answer {
        id: String,
        ask: String,
        answer: Answer,
    },
    Channel {
        pid: u32,
    },
    Ask {
        envelope: Box<Envelope>,
    },
}

pub async fn serve(listener: UnixListener, local: Local, cfg: Arc<CollectorConfig>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let (local, cfg) = (local.clone(), cfg.clone());
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, local, cfg).await {
                        debug!("api client: {e:#}");
                    }
                });
            }
            Err(e) => debug!("api accept: {e}"),
        }
    }
}

async fn handle(stream: UnixStream, local: Local, cfg: Arc<CollectorConfig>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let reply = match serde_json::from_str::<Request>(&line) {
            Err(e) => error(format!("bad request: {e}")),
            Ok(Request::Snapshot) => tagged("snapshot", &local.snapshot().await?),
            Ok(Request::Subscribe) => return subscribe(write, local).await,
            Ok(Request::Open { id }) => {
                match actions::open(&local, &cfg.attach.command, &id).await {
                    Ok(message) => json!({ "type": "ok", "message": message }),
                    Err(e) => error(format!("{e:#}")),
                }
            }
            Ok(Request::Rename { id, title }) => {
                let session = resolve(&local, &id).await?;
                match local.call(Call::Rename { session, title }).await? {
                    Ok(_) => json!({
                        "type": "ok",
                        "message": "renamed; Claude Code adopts it at the next prompt",
                    }),
                    Err(e) => error(e),
                }
            }
            Ok(Request::Reply { id, text }) => {
                let session = resolve(&local, &id).await?;
                match local.call(Call::Reply { session, text }).await? {
                    Ok(_) => json!({ "type": "ok", "message": "sent" }),
                    Err(e) => error(e),
                }
            }
            Ok(Request::Answer { id, ask, answer }) => {
                let session = resolve(&local, &id).await?;
                match local
                    .call(Call::Answer {
                        session,
                        ask,
                        answer,
                    })
                    .await?
                {
                    Ok(_) => json!({ "type": "ok", "message": "answered" }),
                    Err(e) => error(e),
                }
            }
            Ok(Request::Channel { pid }) => return channel(lines, write, local, pid).await,
            Ok(Request::Ask { envelope }) => return ask(lines, write, local, envelope).await,
        };
        send(&mut write, &reply).await?;
    }
    Ok(())
}

/// The full id behind a key the mirror knows; unknown keys pass through
/// for the hub to judge.
async fn resolve(local: &Local, key: &str) -> Result<String> {
    Ok(local
        .resolve(key)
        .await?
        .map_or_else(|| key.to_owned(), |s| s.id))
}

/// Relays between the actor and one `ccdash channel` until either side leaves.
async fn channel(
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    mut write: OwnedWriteHalf,
    local: Local,
    pid: u32,
) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let conn = NEXT.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::channel(32);
    local.send(LocalCmd::ChannelOpen(pid, conn, tx)).await?;
    let result = async {
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { return Ok(()) };
                    send(&mut write, &serde_json::to_value(&msg)?).await?;
                }
                line = lines.next_line() => {
                    if line?.is_none() {
                        return Ok(());
                    }
                }
            }
        }
    }
    .await;
    local.send(LocalCmd::ChannelClosed(pid, conn)).await?;
    result
}

/// Holds a prompt for `ccdash ask` until the hub answers it, lets go of
/// it, or the hook goes away.
async fn ask(
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    mut write: OwnedWriteHalf,
    local: Local,
    envelope: Box<Envelope>,
) -> Result<()> {
    let (shown, id) = oneshot::channel();
    let (decide, decision) = oneshot::channel();
    local.send(LocalCmd::Ask(envelope, shown, decide)).await?;
    let Ok(id) = id.await else {
        return Ok(());
    };
    tokio::select! {
        decision = decision => {
            if let Ok(decision) = decision {
                send(&mut write, &json!({ "type": "decision", "decision": decision })).await?;
            }
        }
        _ = lines.next_line() => local.send(LocalCmd::AskGone(id)).await?,
    }
    Ok(())
}

async fn subscribe(mut write: OwnedWriteHalf, local: Local) -> Result<()> {
    let mut updates = local.subscribe();
    send(&mut write, &tagged("snapshot", &local.snapshot().await?)).await?;
    loop {
        match updates.recv().await {
            Ok(update) => send(&mut write, &update).await?,
            Err(RecvError::Lagged(_)) => {
                send(&mut write, &tagged("snapshot", &local.snapshot().await?)).await?;
            }
            Err(RecvError::Closed) => return Ok(()),
        }
    }
}

fn tagged(kind: &str, value: &impl Serialize) -> Value {
    let mut value = serde_json::to_value(value).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.insert("type".into(), kind.into());
    }
    value
}

fn error(message: String) -> Value {
    json!({ "type": "error", "message": message })
}

async fn send(write: &mut OwnedWriteHalf, value: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    Ok(())
}
