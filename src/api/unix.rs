//! Local API on `api.sock`: newline-delimited JSON, one request per line.
//!
//! Requests: `{"cmd":"snapshot"}`, `{"cmd":"subscribe"}`, `{"cmd":"open","id":"…"}`,
//! `{"cmd":"rename","id":"…","title":"…"}`, `{"cmd":"reply","id":"…","text":"…"}`,
//! `{"cmd":"permission","id":"…","request_id":"…","behavior":"allow|deny"}`.
//! Replies carry a `type`: `snapshot`, `session`, `removed`, `rate_limits`, `ok`, `error`.
//!
//! `{"cmd":"channel","pid":N}` turns the connection into the link of a
//! `ccdash channel` (see `crate::channel`).

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
    sync::{broadcast::error::RecvError, mpsc},
};
use tracing::debug;

use crate::{
    actions,
    channel::{Behavior, FromChannel},
    config::Config,
    hub::{Cmd, Hub},
};

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
    Permission {
        id: String,
        request_id: String,
        behavior: Behavior,
    },
    Channel {
        pid: u32,
    },
}

pub async fn serve(listener: UnixListener, hub: Hub, cfg: Arc<Config>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let (hub, cfg) = (hub.clone(), cfg.clone());
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, hub, cfg).await {
                        debug!("api client: {e:#}");
                    }
                });
            }
            Err(e) => debug!("api accept: {e}"),
        }
    }
}

async fn handle(stream: UnixStream, hub: Hub, cfg: Arc<Config>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let reply = match serde_json::from_str::<Request>(&line) {
            Err(e) => error(format!("bad request: {e}")),
            Ok(Request::Snapshot) => tagged("snapshot", &hub.snapshot().await?),
            Ok(Request::Subscribe) => return subscribe(write, hub).await,
            Ok(Request::Open { id }) => match actions::open(&hub, &cfg.attach.command, &id).await {
                Ok(message) => json!({ "type": "ok", "message": message }),
                Err(e) => error(format!("{e:#}")),
            },
            Ok(Request::Rename { id, title }) => match hub.rename(&id, &title).await? {
                Ok(_) => json!({
                    "type": "ok",
                    "message": "renamed; Claude Code adopts it at the next prompt",
                }),
                Err(e) => error(e),
            },
            Ok(Request::Reply { id, text }) => match hub.reply(&id, &text).await? {
                Ok(_) => json!({ "type": "ok", "message": "sent" }),
                Err(e) => error(e),
            },
            Ok(Request::Permission {
                id,
                request_id,
                behavior,
            }) => match hub.answer(&id, &request_id, behavior).await? {
                Ok(_) => json!({ "type": "ok", "message": "answered" }),
                Err(e) => error(e),
            },
            Ok(Request::Channel { pid }) => return channel(lines, write, hub, pid).await,
        };
        send(&mut write, &reply).await?;
    }
    Ok(())
}

/// Relays between the hub and one `ccdash channel` until either side leaves.
async fn channel(
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    mut write: OwnedWriteHalf,
    hub: Hub,
    pid: u32,
) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let conn = NEXT.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::channel(32);
    hub.send(Cmd::ChannelOpen(pid, conn, tx)).await?;
    let result = async {
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { return Ok(()) };
                    send(&mut write, &serde_json::to_value(&msg)?).await?;
                }
                line = lines.next_line() => {
                    let Some(line) = line? else { return Ok(()) };
                    match serde_json::from_str::<FromChannel>(&line) {
                        Ok(msg) => hub.send(Cmd::FromChannel(pid, msg)).await?,
                        Err(e) => debug!("channel {pid}: {e}"),
                    }
                }
            }
        }
    }
    .await;
    hub.send(Cmd::ChannelClosed(pid, conn)).await?;
    result
}

async fn subscribe(mut write: OwnedWriteHalf, hub: Hub) -> Result<()> {
    let mut updates = hub.subscribe();
    send(&mut write, &tagged("snapshot", &hub.snapshot().await?)).await?;
    loop {
        match updates.recv().await {
            Ok(update) => send(&mut write, &serde_json::to_value(&update)?).await?,
            Err(RecvError::Lagged(_)) => {
                send(&mut write, &tagged("snapshot", &hub.snapshot().await?)).await?;
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
