//! niri IPC: window/focus event stream and the focus action.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};
use tracing::{info, warn};

use crate::{
    hub::{Cmd, Input},
    state::{NiriEvent, Window},
};

fn socket() -> Option<String> {
    std::env::var("NIRI_SOCKET").ok().filter(|s| !s.is_empty())
}

pub async fn run(tx: mpsc::Sender<Cmd>) {
    let Some(path) = socket() else {
        info!("NIRI_SOCKET unset, window tracking disabled");
        return;
    };
    loop {
        match stream(&path, &tx).await {
            Ok(()) => return,
            Err(e) => warn!("niri event stream: {e:#}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn stream(path: &str, tx: &mpsc::Sender<Cmd>) -> Result<()> {
    let socket = UnixStream::connect(path).await.context("connecting")?;
    let (read, mut write) = socket.into_split();
    write.write_all(b"\"EventStream\"\n").await?;
    let mut lines = BufReader::new(read).lines();
    let reply = lines.next_line().await?.context("no reply")?;
    if !reply.contains("\"Ok\"") {
        bail!("refused: {reply}");
    }
    info!("following niri events");
    while let Some(line) = lines.next_line().await? {
        if let Some(event) = parse_event(&line)
            && tx.send(Cmd::Input(Input::Niri(event))).await.is_err()
        {
            return Ok(());
        }
    }
    bail!("stream closed")
}

pub fn parse_event(line: &str) -> Option<NiriEvent> {
    let value: Value = serde_json::from_str(line).ok()?;
    let (name, body) = value.as_object()?.iter().next()?;
    match name.as_str() {
        "WindowsChanged" => Some(NiriEvent::Windows(
            body.get("windows")?
                .as_array()?
                .iter()
                .filter_map(parse_window)
                .collect(),
        )),
        "WindowOpenedOrChanged" => parse_window(body.get("window")?).map(NiriEvent::WindowChanged),
        "WindowClosed" => body.get("id")?.as_u64().map(NiriEvent::WindowClosed),
        "WindowFocusChanged" => Some(NiriEvent::Focus(body.get("id")?.as_u64())),
        _ => None,
    }
}

fn parse_window(v: &Value) -> Option<Window> {
    Some(Window {
        id: v.get("id")?.as_u64()?,
        app_id: v.get("app_id")?.as_str().unwrap_or_default().to_owned(),
        title: v.get("title")?.as_str().unwrap_or_default().to_owned(),
        focused: v.get("is_focused")?.as_bool().unwrap_or(false),
    })
}

pub async fn focus(window_id: u64) -> Result<()> {
    let path = socket().context("NIRI_SOCKET unset")?;
    let socket = UnixStream::connect(path).await?;
    let (read, mut write) = socket.into_split();
    let request = json!({ "Action": { "FocusWindow": { "id": window_id } } });
    write.write_all(format!("{request}\n").as_bytes()).await?;
    let reply = BufReader::new(read)
        .lines()
        .next_line()
        .await?
        .context("no reply")?;
    if !reply.contains("\"Ok\"") {
        bail!("niri refused focus: {reply}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events() {
        let windows = r#"{"WindowsChanged":{"windows":[{"id":7,"title":"✳ Fix","app_id":"com.mitchellh.ghostty","pid":1,"workspace_id":1,"is_focused":true,"is_floating":false}]}}"#;
        let Some(NiriEvent::Windows(list)) = parse_event(windows) else {
            panic!()
        };
        assert_eq!((list[0].id, list[0].focused), (7, true));
        assert!(matches!(
            parse_event(r#"{"WindowFocusChanged":{"id":null}}"#),
            Some(NiriEvent::Focus(None))
        ));
        assert!(matches!(
            parse_event(r#"{"WindowClosed":{"id":3}}"#),
            Some(NiriEvent::WindowClosed(3))
        ));
        assert!(parse_event(r#"{"OverviewOpenedOrClosed":{"is_open":false}}"#).is_none());
    }
}
