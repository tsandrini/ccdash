//! The `/collect` WebSocket: one link per collector, opened with that
//! machine's bearer token. Frames are JSON `ToHub` / `ToCollector`
//! messages (see `crate::protocol`).

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::{
    hub::core::{Cmd, Hub, Input},
    protocol::{ToCollector, ToHub, VERSION},
    util::{constant_time_eq, expand_env},
};

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// A link with no frame for this long is dropped (collectors ping every 30 s).
const IDLE: Duration = Duration::from_secs(90);

/// Collector tokens by host name, read at every connection so a rotated
/// token needs no restart.
#[derive(Clone, Default)]
pub struct Tokens {
    files: BTreeMap<String, PathBuf>,
}

impl Tokens {
    pub fn new(files: BTreeMap<String, PathBuf>) -> Self {
        Self { files }
    }

    /// The host whose token `given` is.
    pub(crate) fn host_of(&self, given: &str) -> Option<String> {
        let mut hit = None;
        for (host, path) in &self.files {
            let path = PathBuf::from(expand_env(&path.to_string_lossy()));
            let token = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    debug!("collector token {}: {e}", path.display());
                    continue;
                }
            };
            let token = token.trim();
            if !token.is_empty() && constant_time_eq(token.as_bytes(), given.as_bytes()) {
                hit = Some(host.clone());
            }
        }
        hit
    }
}

pub async fn collect(
    State((hub, tokens, changed)): State<(Hub, Tokens, watch::Receiver<u64>)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(host) = given.and_then(|t| tokens.host_of(t)) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = link(socket, host.clone(), hub, changed).await {
            debug!("collector {host}: {e:#}");
        }
    })
}

async fn link(
    socket: WebSocket,
    host: String,
    hub: Hub,
    mut changed: watch::Receiver<u64>,
) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let conn = NEXT.fetch_add(1, Ordering::Relaxed);
    let (mut sink, mut stream) = socket.split();
    let hello = tokio::time::timeout(HELLO_TIMEOUT, stream.next())
        .await
        .context("no hello")?
        .context("closed before hello")??;
    let terminals = match parse(&hello)? {
        Some(ToHub::Hello {
            v,
            host: claimed,
            terminals,
        }) => {
            if v != VERSION {
                let _ = sink
                    .send(Message::Text(
                        format!("the hub speaks protocol {VERSION}, not {v}").into(),
                    ))
                    .await;
                bail!("protocol {v}");
            }
            if claimed != host {
                bail!("token of {host} used by {claimed}");
            }
            terminals
        }
        _ => bail!("first message was not a hello"),
    };
    let (tx, mut rx) = mpsc::channel(256);
    hub.send(Cmd::CollectorOpen {
        host: host.clone(),
        conn,
        terminals,
        tx,
    })
    .await?;
    changed.mark_unchanged();
    let result = async {
        loop {
            tokio::select! {
                gone = changed.changed() => {
                    gone?;
                    let seq = *changed.borrow_and_update();
                    let text = serde_json::to_string(&ToCollector::SyncChanged { seq })?;
                    sink.send(Message::Text(text.into())).await?;
                }
                msg = tokio::time::timeout(IDLE, stream.next()) => {
                    let Some(msg) = msg.context("idle")? else { return Ok(()) };
                    if let Some(cmd) = parse(&msg?)?.and_then(|m| command(&host, m)) {
                        hub.send(cmd).await?;
                    }
                }
                out = rx.recv() => {
                    let Some(out) = out else { return Ok(()) };
                    match serde_json::to_string(&out) {
                        Ok(text) => sink.send(Message::Text(text.into())).await?,
                        Err(e) => warn!("collector {host}: unserializable message: {e}"),
                    }
                }
            }
        }
    }
    .await;
    hub.send(Cmd::CollectorClosed { host, conn }).await?;
    result
}

fn parse(msg: &Message) -> Result<Option<ToHub>> {
    match msg {
        Message::Text(text) => Ok(Some(serde_json::from_str(text).context("bad message")?)),
        Message::Binary(bytes) => Ok(Some(serde_json::from_slice(bytes).context("bad message")?)),
        Message::Close(_) => bail!("closed"),
        _ => Ok(None),
    }
}

/// The hub command for a collector message; None for a stray hello.
fn command(host: &str, msg: ToHub) -> Option<Cmd> {
    let host = host.to_owned();
    Some(match msg {
        ToHub::Hello { .. } => {
            warn!("collector {host}: repeated hello");
            return None;
        }
        ToHub::Envelope { envelope } => Cmd::Input(host, Input::Envelope(envelope)),
        ToHub::Agents { list, at, dead } => Cmd::Input(host, Input::Agents(list, at, dead)),
        ToHub::Niri { event } => Cmd::Input(host, Input::Niri(event)),
        ToHub::Procs { polls, at } => Cmd::Input(host, Input::Procs(polls, at)),
        ToHub::Usage { limits, at } => Cmd::Input(host, Input::Usage(limits, at)),
        ToHub::Telemetry { records } => Cmd::Input(host, Input::Telemetry(records)),
        ToHub::ChannelOpen { pid } => Cmd::ChannelOpen { host, pid },
        ToHub::ChannelClosed { pid } => Cmd::ChannelClosed { host, pid },
        ToHub::Ask { id, envelope } => Cmd::Ask {
            host,
            id,
            envelope: Box::new(envelope),
        },
        ToHub::AskGone { id } => Cmd::AskGone { host, id },
        ToHub::Call { id, call } => Cmd::Call { host, id, call },
        ToHub::Queued { call, at } => Cmd::Queued { host, call, at },
        ToHub::Injected { session } => Cmd::Injected { host, session },
        ToHub::Launched {
            launch,
            session,
            error,
        } => Cmd::Launched {
            host,
            launch,
            session,
            error,
        },
        ToHub::Stopped { session, error } => Cmd::Stopped {
            host,
            session,
            error,
        },
        ToHub::Dirs { dirs } => Cmd::Dirs { host, dirs },
        ToHub::Sync { status } => Cmd::SyncStatus { host, status },
        ToHub::SyncResolved { id, error, copy } => Cmd::SyncResolved {
            host,
            id,
            error,
            copy,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_name_their_host() {
        let dir = std::env::temp_dir().join(format!("ccdash-tokens-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), "tok-a\n").unwrap();
        std::fs::write(dir.join("b"), "tok-b").unwrap();
        let tokens = Tokens::new(BTreeMap::from([
            ("alpha".to_owned(), dir.join("a")),
            ("beta".to_owned(), dir.join("b")),
            ("gone".to_owned(), dir.join("missing")),
        ]));
        assert_eq!(tokens.host_of("tok-a").as_deref(), Some("alpha"));
        assert_eq!(tokens.host_of("tok-b").as_deref(), Some("beta"));
        assert_eq!(tokens.host_of("tok-c"), None);
        assert_eq!(tokens.host_of(""), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
