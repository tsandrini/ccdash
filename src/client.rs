//! CLI commands that talk to a running daemon over `api.sock`.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    model::{Activity, Session, Snapshot},
    paths,
    util::{now_ms, truncate},
};

fn request(request: &Value) -> Result<Value> {
    let path = paths::api_socket();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("connecting to {} (is the daemon running?)", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    writeln!(stream, "{request}")?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let reply: Value = serde_json::from_str(&line).context("parsing reply")?;
    if reply["type"] == "error" {
        bail!("{}", reply["message"].as_str().unwrap_or("unknown error"));
    }
    Ok(reply)
}

pub fn status(as_json: bool) -> Result<()> {
    let reply = request(&json!({ "cmd": "snapshot" }))?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&reply)?);
        return Ok(());
    }
    let mut snapshot: Snapshot = serde_json::from_value(reply)?;
    snapshot
        .sessions
        .sort_by_key(|s| (attention_rank(s.activity), s.activity_since));
    let now = now_ms();
    for s in &snapshot.sessions {
        println!(
            "{:<10} {:>6}  {:<40}  {}",
            activity_label(s.activity),
            age(now - s.activity_since),
            truncate(&label(s), 40),
            truncate(&detail(s), 70),
        );
    }
    if snapshot.sessions.is_empty() {
        println!("no sessions");
    }
    Ok(())
}

pub fn open(id: &str) -> Result<()> {
    let reply = request(&json!({ "cmd": "open", "id": id }))?;
    println!("{}", reply["message"].as_str().unwrap_or("ok"));
    Ok(())
}

/// Renames a session; Claude Code adopts the title at its next prompt.
pub fn rename(id: &str, title: &str) -> Result<()> {
    let reply = request(&json!({ "cmd": "rename", "id": id, "title": title }))?;
    println!("{}", reply["message"].as_str().unwrap_or("ok"));
    Ok(())
}

/// Sends `text` to a session through its ccdash channel.
pub fn reply(id: &str, text: &str) -> Result<()> {
    let reply = request(&json!({ "cmd": "reply", "id": id, "text": text }))?;
    println!("{}", reply["message"].as_str().unwrap_or("ok"));
    Ok(())
}

fn attention_rank(activity: Activity) -> u8 {
    match activity {
        Activity::Permission => 0,
        Activity::YourTurn => 1,
        Activity::Working => 2,
        Activity::Idle => 3,
        Activity::Gone => 4,
    }
}

fn activity_label(activity: Activity) -> &'static str {
    match activity {
        Activity::Permission => "PERMISSION",
        Activity::YourTurn => "your turn",
        Activity::Working => "working",
        Activity::Idle => "idle",
        Activity::Gone => "gone",
    }
}

fn label(s: &Session) -> String {
    s.title
        .clone()
        .or_else(|| s.name.clone())
        .unwrap_or_else(|| s.id.chars().take(8).collect())
}

fn detail(s: &Session) -> String {
    if let Some(waiting) = &s.waiting_for {
        return waiting.clone();
    }
    if let Some(tool) = &s.tool {
        return format!("{}: {}", tool.name, tool.summary);
    }
    s.cwd.clone()
}

fn age(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}
