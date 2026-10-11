//! Local API on `api.sock`: newline-delimited JSON, one request per line.
//!
//! Requests: `{"cmd":"snapshot"}`, `{"cmd":"subscribe"}`, `{"cmd":"open","id":"…"}`.
//! Replies carry a `type`: `snapshot`, `session`, `removed`, `rate_limits`, `ok`, `error`.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream, unix::OwnedWriteHalf},
    sync::broadcast::error::RecvError,
};
use tracing::debug;

use crate::{actions, config::Config, hub::Hub};

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    Snapshot,
    Subscribe,
    Open { id: String },
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
        };
        send(&mut write, &reply).await?;
    }
    Ok(())
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
