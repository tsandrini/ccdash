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
//!
//! Knowledge: `{"cmd":"tool","name":"…","args":{…},"pid":N,"session":"…",
//! "cwd":"…"}` runs a knowledge tool (reply `{"type":"tool","ok":bool,
//! "text":"…"}`); `{"cmd":"inject","envelope":{…}}` asks for a hook's
//! context (`{"type":"context","text":"…"}` or `{"type":"none"}`);
//! `{"cmd":"import","dir":"…","notes":[…]}` loads notes into the hub.
//! Writes are queued while the hub is unreachable, reads fall back to the
//! notes cache.
//!
//! `{"cmd":"workspace","op":"status"|"resync"|"init"|"adopt","unit":"…"}`
//! drives the workspace sync (`ccdash workspace …`).

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
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

use super::{Local, LocalCmd, NOT_CONNECTED, TIMED_OUT};
use crate::{
    actions,
    ask::Answer,
    config::CollectorConfig,
    knowledge::{Upsert, tools},
    protocol::{Call, Injection, Reply, Who},
    sync::collector::Handle as SyncHandle,
    wire::Envelope,
    workspace::collector::{Cmd as WsCmd, Handle as WsHandle},
};

/// How long a tool waits for the hub before it answers from the cache or
/// reports the write unconfirmed; the channel gives up at 5 s.
const TOOL_WAIT: Duration = Duration::from_millis(4000);
/// Hooks run with a 5 s timeout; `ccdash emit` waits a little longer than this.
const START_WAIT: Duration = Duration::from_millis(2500);
const HOOK_WAIT: Duration = Duration::from_millis(1200);
/// The guard asks the hub alongside the injection, within the same time.
const GUARD_WAIT: Duration = Duration::from_millis(1000);

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
    Tool {
        name: String,
        #[serde(default)]
        args: Value,
        #[serde(flatten)]
        who: Who,
    },
    Inject {
        envelope: Box<Envelope>,
    },
    Import {
        dir: String,
        notes: Vec<Upsert>,
    },
    Workspace {
        op: String,
        #[serde(default)]
        unit: Option<String>,
    },
}

pub async fn serve(
    listener: UnixListener,
    local: Local,
    cfg: Arc<CollectorConfig>,
    sync: Option<SyncHandle>,
    workspace: Option<WsHandle>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let (local, cfg, sync, workspace) =
                    (local.clone(), cfg.clone(), sync.clone(), workspace.clone());
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, local, cfg, sync, workspace).await {
                        debug!("api client: {e:#}");
                    }
                });
            }
            Err(e) => debug!("api accept: {e}"),
        }
    }
}

async fn handle(
    stream: UnixStream,
    local: Local,
    cfg: Arc<CollectorConfig>,
    sync: Option<SyncHandle>,
    workspace: Option<WsHandle>,
) -> Result<()> {
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
                done(
                    local.call(Call::Rename { session, title }).await?,
                    "renamed; Claude Code adopts it at the next prompt",
                )
            }
            Ok(Request::Reply { id, text }) => {
                let session = resolve(&local, &id).await?;
                done(local.call(Call::Reply { session, text }).await?, "sent")
            }
            Ok(Request::Answer { id, ask, answer }) => {
                let session = resolve(&local, &id).await?;
                done(
                    local
                        .call(Call::Answer {
                            session,
                            ask,
                            answer,
                        })
                        .await?,
                    "answered",
                )
            }
            Ok(Request::Channel { pid }) => return channel(lines, write, local, pid).await,
            Ok(Request::Ask { envelope }) => return ask(lines, write, local, envelope).await,
            Ok(Request::Tool { name, args, who }) => tool(&local, &name, &args, who).await?,
            Ok(Request::Inject { envelope }) => {
                let (warning, injected) = tokio::join!(
                    guard(&local, sync.as_ref(), &envelope),
                    inject(&local, &cfg, &envelope)
                );
                let (mut reply, mut ack) = injected?;
                if let Some(text) = warning? {
                    if envelope.payload["hook_event_name"] == "UserPromptSubmit" {
                        reply = json!({ "type": "block", "text": text });
                        ack = None;
                    } else {
                        reply["warn"] = json!(text);
                    }
                }
                send(&mut write, &reply).await?;
                // NOTE: the hook has the context once the write went through
                if let Some(session) = ack {
                    local.send(LocalCmd::Injected(session)).await?;
                }
                continue;
            }
            Ok(Request::Import { dir, notes }) => {
                match local
                    .call_within(Call::Import { dir, notes }, Duration::from_secs(60))
                    .await?
                {
                    Ok(reply) => json!({ "type": "ok", "message": tools::text_of(&reply) }),
                    Err(e) => error(e),
                }
            }
            Ok(Request::Workspace { op, unit }) => match &workspace {
                None => {
                    error("the workspace sync is off on this machine ([workspace] enable)".into())
                }
                Some(ws) => match op.as_str() {
                    "status" => {
                        let mut status = ws.status().await?;
                        status["type"] = json!("workspace");
                        status
                    }
                    "resync" => {
                        ws.send(WsCmd::Resync(unit));
                        json!({ "type": "ok", "message": "fetching and snapshotting again" })
                    }
                    "init" => {
                        ws.send(WsCmd::Init);
                        json!({ "type": "ok", "message": "creating every unit the hub has and this machine lacks; `ccdash workspace status` follows it" })
                    }
                    "adopt" => {
                        ws.send(WsCmd::Adopt(unit));
                        json!({ "type": "ok", "message": "taking the first snapshot of the units that exist here and on the hub; `ccdash workspace status` follows it" })
                    }
                    other => error(format!("unknown workspace op {other:?}")),
                },
            },
        };
        send(&mut write, &reply).await?;
    }
    Ok(())
}

fn done(result: Result<Reply, String>, message: &str) -> Value {
    match result {
        Ok(_) => json!({ "type": "ok", "message": message }),
        Err(e) => error(e),
    }
}

/// Runs a knowledge tool: on the hub when the link is up, else writes are
/// queued and reads come from the cache.
async fn tool(local: &Local, name: &str, args: &Value, who: Who) -> Result<Value> {
    let call = match tools::call(name, args, who.clone()) {
        Ok(call) => call,
        Err(e) => return Ok(tool_reply(false, format!("{e:#}"))),
    };
    let write = tools::is_write(&call);
    let connected = local.connected().await?;
    if !connected && write {
        local.send(LocalCmd::Queue(call)).await?;
        return Ok(tool_reply(
            true,
            "queued: the ccdash hub is offline; the write lands when the link returns",
        ));
    }
    if !connected {
        return cached(local, &who, call).await;
    }
    match local.call_within(call.clone(), TOOL_WAIT).await? {
        Ok(reply) => Ok(tool_reply(true, tools::text_of(&reply))),
        Err(e) if write && e == NOT_CONNECTED => {
            local.send(LocalCmd::Queue(call)).await?;
            Ok(tool_reply(
                true,
                "queued: the hub link dropped; the write lands when it returns",
            ))
        }
        Err(e) if write && e == TIMED_OUT => Ok(tool_reply(
            false,
            "the hub did not confirm in time; the write may have landed, recall it before retrying",
        )),
        Err(e) if e == NOT_CONNECTED || e == TIMED_OUT => Ok(cached(local, &who, call).await?),
        Err(e) => Ok(tool_reply(false, e)),
    }
}

async fn cached(local: &Local, who: &Who, call: Call) -> Result<Value> {
    let Call::Recall { query, .. } = call else {
        return Ok(tool_reply(false, "the hub is offline"));
    };
    Ok(match local.cached(who.clone(), query).await? {
        Ok(Reply::Hits(hits)) => tool_reply(true, tools::hits_text(&hits, "the offline cache")),
        Ok(reply) => tool_reply(
            true,
            format!(
                "{}
(from the offline cache; links not shown)",
                tools::text_of(&reply)
            ),
        ),
        Err(e) => tool_reply(false, e),
    })
}

fn tool_reply(ok: bool, text: impl Into<String>) -> Value {
    json!({ "type": "tool", "ok": ok, "text": text.into() })
}

/// The context a hook injects, from the hub or (session starts) the cache,
/// and the session to acknowledge once a prompt hook has the hub's text.
async fn inject(
    local: &Local,
    cfg: &CollectorConfig,
    env: &Envelope,
) -> Result<(Value, Option<String>)> {
    let none = (json!({ "type": "none" }), None);
    let Some(what) = tools::injection_of(&env.payload) else {
        return Ok(none);
    };
    let wanted = match what {
        Injection::SessionStart => cfg.inject.session_start,
        Injection::Prompt { .. } => cfg.inject.prompt,
        Injection::Edit { .. } => cfg.inject.edits,
    };
    if !wanted {
        return Ok(none);
    }
    let who = Who {
        session: env.payload["session_id"].as_str().map(str::to_owned),
        pid: env.pid,
        cwd: env.payload["cwd"].as_str().map(str::to_owned),
    };
    let mut ack = None;
    let text = if local.connected().await? {
        let what = match (&what, &who.session) {
            (Injection::Prompt { .. }, Some(session)) => Injection::Prompt {
                since: local.catch_up(session).await?,
            },
            _ => what.clone(),
        };
        let wait = if what == Injection::SessionStart {
            START_WAIT
        } else {
            HOOK_WAIT
        };
        match local
            .call_within(
                Call::Inject {
                    who: who.clone(),
                    what: what.clone(),
                },
                wait,
            )
            .await?
        {
            Ok(Reply::Context(text)) => {
                debug!(
                    "inject {what:?}: {} chars",
                    text.as_ref().map_or(0, String::len)
                );
                if text.is_some() && matches!(what, Injection::Prompt { .. }) {
                    ack = who.session.clone();
                }
                text
            }
            Ok(_) => None,
            Err(e) => {
                debug!("inject {what:?}: {e}; falling back to the cache");
                local.offline_inject(who, what).await?
            }
        }
    } else {
        local.offline_inject(who, what).await?
    };
    Ok(match text {
        Some(text) => (json!({ "type": "context", "text": text }), ack),
        None => (json!({ "type": "none" }), None),
    })
}

/// Why a prompt must not run in the hook's session here, or why its resume
/// is risky: the hub sees it open on another machine, or this machine's
/// copy of its transcript is behind the hub's. Quiet without the hub.
async fn guard(local: &Local, sync: Option<&SyncHandle>, env: &Envelope) -> Result<Option<String>> {
    let p = &env.payload;
    let guarded = match p["hook_event_name"].as_str() {
        Some("UserPromptSubmit") => true,
        Some("SessionStart") => p["source"] == "resume",
        _ => false,
    };
    let Some(session) = p["session_id"].as_str().filter(|_| guarded) else {
        return Ok(None);
    };
    if local.connected().await? {
        let who = Who {
            session: Some(session.to_owned()),
            pid: env.pid,
            cwd: p["cwd"].as_str().map(str::to_owned),
        };
        if let Ok(Reply::Guard(Some(reason))) =
            local.call_within(Call::Guard { who }, GUARD_WAIT).await?
        {
            return Ok(Some(reason));
        }
    }
    Ok(sync.filter(|s| s.behind(session)).map(|_| {
        format!(
            "Another machine added to this session after it was opened here, so this copy \
            is out of date. Quit it and resume it again (`claude --resume {session}`) to go \
            on with the whole conversation."
        )
    }))
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
