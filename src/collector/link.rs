//! The collector's link to the hub: one outbound WebSocket, reconnecting
//! with backoff. Hook events wait in the on-disk queue while it is down;
//! polls are dropped (the next one replaces them).

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::header::AUTHORIZATION},
};
use tracing::{info, warn};

use crate::{
    collector::LocalCmd,
    config::HubLink,
    protocol::{ToCollector, ToHub, VERSION},
    util::expand_env,
};

use super::queue::Queue;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_secs(30);
/// Three missed pings.
const IDLE: Duration = Duration::from_secs(90);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct Link {
    pub cfg: HubLink,
    pub host: String,
    pub terminals: Vec<String>,
    pub queue: Queue,
}

/// Runs until `rx` closes.
pub async fn run(
    mut link: Link,
    mut rx: mpsc::UnboundedReceiver<ToHub>,
    local: mpsc::Sender<LocalCmd>,
) {
    let mut backoff = Duration::ZERO;
    let mut last_error = None;
    loop {
        let wait = tokio::time::sleep(backoff);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => link.queue.park(&msg),
                    None => return,
                },
                () = &mut wait => break,
            }
        }
        backoff = (backoff * 2).clamp(Duration::from_secs(1), MAX_BACKOFF);
        let socket = match connect(&link.cfg).await {
            Ok(socket) => socket,
            Err(e) => {
                let message = format!("{e:#}");
                if last_error.as_ref() != Some(&message) {
                    warn!("hub {}: {message}", link.cfg.url);
                    last_error = Some(message);
                }
                continue;
            }
        };
        last_error = None;
        match session(socket, &mut link, &mut rx, &local).await {
            Ok(()) => info!("hub closed the link"),
            Err(e) => warn!("hub link: {e:#}"),
        }
        if local.send(LocalCmd::LinkDown).await.is_err() {
            return;
        }
        backoff = Duration::from_secs(1);
    }
}

async fn connect(cfg: &HubLink) -> Result<Socket> {
    if cfg.url.is_empty() {
        bail!("hub.url is not set");
    }
    let path = cfg
        .token_file
        .as_ref()
        .context("hub.token_file is not set")?;
    let path = PathBuf::from(expand_env(&path.to_string_lossy()));
    let token =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let token = token.trim();
    if token.is_empty() {
        bail!("{} is empty", path.display());
    }
    let mut request = cfg.url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert(AUTHORIZATION, format!("Bearer {token}").parse()?);
    let (socket, _) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request))
        .await
        .context("connecting timed out")?
        .context("connecting")?;
    Ok(socket)
}

/// One connected stretch: hello, replay, then relay until something breaks.
async fn session(
    mut ws: Socket,
    link: &mut Link,
    rx: &mut mpsc::UnboundedReceiver<ToHub>,
    local: &mpsc::Sender<LocalCmd>,
) -> Result<()> {
    let hello = ToHub::Hello {
        v: VERSION,
        host: link.host.clone(),
        terminals: link.terminals.clone(),
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await?;
    let first = tokio::time::timeout(WELCOME_TIMEOUT, ws.next())
        .await
        .context("no welcome")?
        .context("closed before the welcome")??;
    match parse(&first)? {
        Some(ToCollector::Welcome { v, .. }) if v != VERSION => {
            bail!("the hub speaks protocol {v}, this collector {VERSION}")
        }
        Some(welcome @ ToCollector::Welcome { .. }) => {
            info!("connected to the hub at {}", link.cfg.url);
            local.send(LocalCmd::FromHub(Box::new(welcome))).await?;
        }
        Some(_) => bail!("the hub did not start with a welcome"),
        None => bail!("the hub answered the hello with {first:?}"),
    }
    let parked = link.queue.take();
    for (i, line) in parked.iter().enumerate() {
        if let Err(e) = ws.send(Message::Text(line.clone().into())).await {
            link.queue.restore(&parked[i..]);
            return Err(e).context("replaying the outbox");
        }
    }
    if !parked.is_empty() {
        info!("replayed {} queued events", parked.len());
    }
    local.send(LocalCmd::LinkUp).await?;
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { return Ok(()) };
                let text = serde_json::to_string(&msg)?;
                if let Err(e) = ws.send(Message::Text(text.into())).await {
                    link.queue.park(&msg);
                    return Err(e.into());
                }
            }
            frame = tokio::time::timeout(IDLE, ws.next()) => {
                let frame = frame.context("no frame from the hub for 90 s")?
                    .context("the hub closed the link")??;
                if let Some(msg) = parse(&frame)? {
                    local.send(LocalCmd::FromHub(Box::new(msg))).await?;
                }
            }
            _ = ping.tick() => ws.send(Message::Ping(Vec::new().into())).await?,
        }
    }
}

fn parse(msg: &Message) -> Result<Option<ToCollector>> {
    match msg {
        Message::Text(text) => Ok(Some(serde_json::from_str(text).context("bad message")?)),
        Message::Binary(bytes) => Ok(Some(serde_json::from_slice(bytes).context("bad message")?)),
        Message::Close(frame) => bail!("closed: {frame:?}"),
        _ => Ok(None),
    }
}
