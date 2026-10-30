//! CLI commands that talk to a running daemon over `api.sock`.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    knowledge::Upsert,
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

/// Sends `notes` to the hub for the workspace holding `source`.
pub fn import(source: &std::path::Path, notes: Vec<Upsert>) -> Result<()> {
    if notes.is_empty() {
        bail!("nothing to import from {}", source.display());
    }
    let n = notes.len();
    let reply = request(&json!({ "cmd": "import", "dir": source, "notes": notes }))?;
    println!(
        "{n} notes from {}: {}",
        source.display(),
        reply["message"].as_str().unwrap_or("done")
    );
    Ok(())
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
            crate::util::age(now - s.activity_since),
            truncate(&label(s), 40),
            truncate(&detail(s), 70),
        );
    }
    if snapshot.sessions.is_empty() {
        println!("no sessions");
    }
    Ok(())
}

/// `ccdash workspace <op>`: the state of every unit, or a resync, init
/// or adopt handed to the collector.
pub fn workspace(op: &str, unit: Option<&str>, as_json: bool) -> Result<()> {
    let reply = request(&json!({ "cmd": "workspace", "op": op, "unit": unit }))?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&reply)?);
        return Ok(());
    }
    if op != "status" {
        println!("{}", reply["message"].as_str().unwrap_or("ok"));
        return Ok(());
    }
    let units = reply["units"].as_array().cloned().unwrap_or_default();
    let shown: Vec<&Value> = units
        .iter()
        .filter(|u| unit.is_none_or(|w| u["unit"].as_str() == Some(w)))
        .collect();
    for u in &shown {
        let state = u["state"].as_str().unwrap_or("?");
        let detail = u["detail"].as_str().unwrap_or("");
        let conflicts = u["conflicts"].as_array().map_or(0, Vec::len);
        let extra = if conflicts > 0 {
            format!("{conflicts} conflicts")
        } else {
            String::new()
        };
        println!(
            "{:<10} {:<40} {}",
            state,
            truncate(u["unit"].as_str().unwrap_or(""), 40),
            truncate(format!("{extra} {detail}").trim(), 80)
        );
    }
    if shown.is_empty() {
        println!("no units");
    }
    let status = &reply["status"];
    println!(
        "{} units, {} in step, {} pending; setup v{}; {}",
        status["units"],
        status["in_step"],
        status["pending"],
        status["setup_version"],
        if reply["connected"].as_bool().unwrap_or(false) {
            "hub connected"
        } else {
            "hub not connected"
        }
    );
    if let Some(new) = reply["new"].as_array().filter(|n| !n.is_empty()) {
        println!("not included ({}):", new.len());
        for n in new {
            println!(
                "  {:<40} {} {}",
                truncate(n["unit"].as_str().unwrap_or(""), 40),
                n["kind"].as_str().unwrap_or(""),
                n["size"]
                    .as_u64()
                    .map_or(String::new(), |b| format!("{} MB", b >> 20))
            );
        }
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
        Activity::Parked => 3,
        Activity::Idle => 4,
        Activity::Gone => 5,
    }
}

fn activity_label(activity: Activity) -> &'static str {
    match activity {
        Activity::Permission => "PERMISSION",
        Activity::YourTurn => "your turn",
        Activity::Working => "working",
        Activity::Parked => "parked",
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
