//! SQLite history: events, statusline samples, and one row per session,
//! every row tagged with the host it came from. The hub is the only writer
//! (one thread); readers open their own read-only connections.

use std::{
    fs::{self, OpenOptions, Permissions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;
use tokio::sync::oneshot;
use tracing::warn;

use crate::{
    agent_message, claims, knowledge, messages,
    model::{Activity, Kind, Session, Stamped, Turn},
    paths, push,
    state::{PREVIEW_LEN, Restored, StoreMsg, apply_task},
    task_notification, tokens,
    util::{now_ms, truncate},
    workspace::Workspaces,
};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
    ts INTEGER NOT NULL,
    host TEXT NOT NULL,
    session TEXT NOT NULL,
    kind TEXT NOT NULL,
    data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_session_ts ON events (session, ts);
CREATE INDEX IF NOT EXISTS events_ts ON events (ts);
CREATE TABLE IF NOT EXISTS samples (
    ts INTEGER NOT NULL,
    host TEXT NOT NULL,
    session TEXT NOT NULL,
    context_pct REAL,
    cost_usd REAL,
    five_hour REAL,
    seven_day REAL,
    model TEXT,
    lines_added INTEGER,
    lines_removed INTEGER
);
CREATE INDEX IF NOT EXISTS samples_ts ON samples (ts);
CREATE TABLE IF NOT EXISTS proc_samples (
    ts INTEGER NOT NULL,
    host TEXT NOT NULL,
    session TEXT NOT NULL,
    cpu_pct REAL NOT NULL,
    mem_bytes INTEGER NOT NULL,
    procs INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS proc_samples_ts ON proc_samples (ts);
CREATE INDEX IF NOT EXISTS proc_samples_session_ts ON proc_samples (session, ts);
CREATE TABLE IF NOT EXISTS telemetry (
    ts INTEGER NOT NULL,
    host TEXT NOT NULL,
    session TEXT NOT NULL,
    name TEXT NOT NULL,
    data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS telemetry_ts ON telemetry (ts);
CREATE TABLE IF NOT EXISTS limit_samples (
    ts INTEGER NOT NULL,
    label TEXT NOT NULL,
    used_pct REAL NOT NULL,
    resets_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS limit_samples_ts ON limit_samples (ts);
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    host TEXT NOT NULL,
    cwd TEXT NOT NULL,
    name TEXT,
    title TEXT,
    kind TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    branch TEXT,
    launched INTEGER
);
";

const INSERT_SAMPLE: &str = "
INSERT INTO samples (ts, host, session, context_pct, cost_usd, five_hour, seven_day, model,
    lines_added, lines_removed)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
";

const INSERT_PROC_SAMPLE: &str = "
INSERT INTO proc_samples (ts, host, session, cpu_pct, mem_bytes, procs)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
";

/// Columns added after the first release, appended in this order.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("samples", "model", "TEXT"),
    ("samples", "lines_added", "INTEGER"),
    ("samples", "lines_removed", "INTEGER"),
    ("claims", "issue", "TEXT"),
    ("push_subscriptions", "token_id", "INTEGER"),
    ("sessions", "branch", "TEXT"),
    ("sessions", "launched", "INTEGER"),
];

const UPSERT_SESSION: &str = "
INSERT INTO sessions (id, host, cwd, name, title, kind, started_at, ended_at, branch, launched)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
ON CONFLICT (id) DO UPDATE SET
    cwd = excluded.cwd,
    name = COALESCE(excluded.name, name),
    title = COALESCE(excluded.title, title),
    kind = excluded.kind,
    ended_at = excluded.ended_at,
    branch = COALESCE(excluded.branch, branch),
    launched = MAX(COALESCE(launched, 0), excluded.launched)
";

const PRUNE_EVERY_MS: i64 = 3_600_000;

/// Work for the writer thread.
enum Job {
    Msg(String, StoreMsg),
    /// Runs on the writer connection, between batches.
    Exec(Box<dyn FnOnce(&mut Connection) + Send>),
}

#[derive(Clone)]
pub struct Store {
    tx: mpsc::Sender<Job>,
    path: PathBuf,
}

impl Store {
    /// Opens (creating if needed) the database and starts the writer thread.
    pub fn open(path: &Path, retention_days: u32) -> Result<Self> {
        let conn = open_writer(path)?;
        let (tx, rx) = mpsc::channel();
        let retention_ms = i64::from(retention_days) * 86_400_000;
        thread::Builder::new()
            .name("store".into())
            .spawn(move || writer(conn, rx, retention_ms))?;
        Ok(Self {
            tx,
            path: path.to_owned(),
        })
    }

    /// Queues `msg`, which came from `host`.
    pub fn send(&self, host: &str, msg: StoreMsg) {
        let _ = self.tx.send(Job::Msg(host.to_owned(), msg));
    }

    /// Runs `f` on the writer connection (the one place that writes) and
    /// hands back its result; queued messages ahead of it land first.
    pub async fn exec<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> R + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job = Job::Exec(Box::new(move |conn| {
            let _ = tx.send(f(conn));
        }));
        self.tx.send(job).ok().context("store stopped")?;
        rx.await.context("store stopped")
    }

    pub fn reader(&self) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(2))?;
        Ok(conn)
    }
}

/// Opens the database for writing with the settings every writer needs:
/// WAL, full syncs (consumer flash has no power-loss protection), 8 KiB
/// pages and incremental vacuum for the retention churn. Page size and
/// vacuum mode only take effect on a database created here.
pub fn open_writer(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
        if dir == paths::state_dir() {
            fs::set_permissions(dir, Permissions::from_mode(0o700))?;
        }
    }
    make_private(path)?;
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.pragma_update(None, "page_size", 8192)?;
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(SCHEMA)?;
    conn.execute_batch(knowledge::SCHEMA)?;
    conn.execute_batch(push::SCHEMA)?;
    conn.execute_batch(tokens::SCHEMA)?;
    conn.execute_batch(messages::SCHEMA)?;
    conn.execute_batch(claims::SCHEMA)?;
    conn.execute_batch(crate::sync::hub::SCHEMA)?;
    migrate(&conn)?;
    rewrite(&conn)?;
    Ok(conn)
}

fn writer(mut conn: Connection, rx: mpsc::Receiver<Job>, retention_ms: i64) {
    let mut last_prune = 0;
    while let Ok(first) = rx.recv() {
        let mut batch: Vec<(String, StoreMsg)> = Vec::new();
        for job in std::iter::once(first).chain(rx.try_iter()) {
            match job {
                Job::Msg(host, msg) => batch.push((host, msg)),
                Job::Exec(f) => {
                    if let Err(e) = write_batch(&mut conn, &batch) {
                        warn!("store: {e:#}");
                    }
                    batch.clear();
                    f(&mut conn);
                }
            }
        }
        if let Err(e) = write_batch(&mut conn, &batch) {
            warn!("store: {e:#}");
        }
        let now = now_ms();
        if now - last_prune > PRUNE_EVERY_MS {
            last_prune = now;
            if let Err(e) = prune(&conn, now - retention_ms) {
                warn!("store prune: {e:#}");
            }
        }
    }
}

fn write_batch(conn: &mut Connection, batch: &[(String, StoreMsg)]) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    for (host, msg) in batch {
        match msg {
            StoreMsg::Event {
                ts,
                session,
                kind,
                data,
            } => {
                tx.execute(
                    "INSERT INTO events (ts, host, session, kind, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![ts, host, session, kind, data.to_string()],
                )?;
            }
            StoreMsg::Sample {
                ts,
                session,
                model,
                context_pct,
                cost_usd,
                lines_added,
                lines_removed,
                five_hour,
                seven_day,
            } => {
                tx.execute(
                    INSERT_SAMPLE,
                    params![
                        ts,
                        host,
                        session,
                        context_pct,
                        cost_usd,
                        five_hour,
                        seven_day,
                        model,
                        lines_added.map(|n| n as i64),
                        lines_removed.map(|n| n as i64),
                    ],
                )?;
            }
            StoreMsg::ProcSample {
                ts,
                session,
                cpu_pct,
                mem_bytes,
                procs,
            } => {
                tx.execute(
                    INSERT_PROC_SAMPLE,
                    params![ts, host, session, cpu_pct, *mem_bytes as i64, procs],
                )?;
            }
            StoreMsg::Limit { ts, limit } => {
                tx.execute(
                    "INSERT INTO limit_samples (ts, label, used_pct, resets_at) VALUES (?1, ?2, ?3, ?4)",
                    params![ts, limit.label, limit.used_pct, limit.resets_at],
                )?;
            }
            StoreMsg::Telemetry {
                ts,
                session,
                name,
                data,
            } => {
                tx.execute(
                    "INSERT INTO telemetry (ts, host, session, name, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![ts, host, session, name, data.to_string()],
                )?;
            }
            StoreMsg::Session {
                id,
                cwd,
                name,
                title,
                kind,
                started_at,
                ended_at,
                branch,
                launched,
            } => {
                tx.execute(
                    UPSERT_SESSION,
                    params![
                        id,
                        host,
                        cwd,
                        name,
                        title,
                        kind_name(*kind),
                        started_at,
                        ended_at,
                        branch,
                        launched
                    ],
                )?;
            }
            StoreMsg::Claim { session, claim } => claims::save(&tx, session, claim.as_ref())?,
        }
    }
    tx.commit()?;
    Ok(())
}

fn prune(conn: &Connection, cutoff: i64) -> Result<()> {
    conn.execute("DELETE FROM events WHERE ts < ?1", [cutoff])?;
    conn.execute("DELETE FROM samples WHERE ts < ?1", [cutoff])?;
    conn.execute("DELETE FROM proc_samples WHERE ts < ?1", [cutoff])?;
    conn.execute("DELETE FROM telemetry WHERE ts < ?1", [cutoff])?;
    conn.execute("DELETE FROM limit_samples WHERE ts < ?1", [cutoff])?;
    messages::prune(conn, cutoff)?;
    claims::prune(conn, now_ms())?;
    tokens::prune(conn, now_ms())?;
    conn.execute(
        "DELETE FROM sessions WHERE started_at < ?1
            AND (ended_at < ?1 OR (ended_at IS NULL AND id NOT IN (SELECT session FROM events)))",
        [cutoff],
    )?;
    conn.execute_batch("PRAGMA incremental_vacuum")?;
    Ok(())
}

/// The database holds prompts and replies: owner-only. SQLite gives the
/// `-wal`/`-shm` files it creates the database file's mode.
fn make_private(path: &Path) -> Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    for suffix in ["", "-wal", "-shm"] {
        let file = PathBuf::from(format!("{}{suffix}", path.display()));
        if file.exists() {
            fs::set_permissions(&file, Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    for (table, column, kind) in ADDED_COLUMNS.iter().chain(crate::sync::hub::ADDED_COLUMNS) {
        let exists: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
            [table, column],
            |r| r.get(0),
        )?;
        if !exists {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
        }
    }
    Ok(())
}

/// Rewrites of stored data, each run once: `user_version` counts the done.
fn rewrite(conn: &Connection) -> Result<()> {
    let done: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if done < 1 {
        let tx = conn.unchecked_transaction()?;
        // Task notifications came in as prompts before they had a kind.
        tx.execute(
            "UPDATE events SET kind = ?1 WHERE kind = 'prompt'
                AND ltrim(json_extract(data, '$.text'), char(9, 10, 13, 32)) LIKE '<task-notification>%'",
            [task_notification::KIND],
        )?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
    }
    if done < 2 {
        let tx = conn.unchecked_transaction()?;
        // Subagents' messages came in as prompts too.
        tx.execute(
            "UPDATE events SET kind = ?1 WHERE kind = 'prompt'
                AND ltrim(json_extract(data, '$.text'), char(9, 10, 13, 32)) LIKE '<agent-message from=%'",
            [agent_message::KIND],
        )?;
        name_agent_messages(&tx)?;
        // And Claude's text kept a notification that arrived while it wrote.
        let glued: Vec<(i64, String)> = tx
            .prepare(
                "SELECT rowid, data FROM events WHERE kind IN ('message', 'reply')
                    AND data LIKE '%</task-notification>%'",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (rowid, data) in glued {
            let Ok(mut data) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            let text = data["text"].as_str().unwrap_or_default();
            let own = task_notification::strip_trailing(text);
            if own.len() < text.len() {
                data["text"] = own.into();
                tx.execute(
                    "UPDATE events SET data = ?1 WHERE rowid = ?2",
                    params![data.to_string(), rowid],
                )?;
            }
        }
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
    }
    Ok(())
}

/// Names the subagent messages recorded before the hub named them on
/// arrival: by a task notification carrying the sender's id, else (a
/// report) by the one subagent stop recorded within 50 ms after it.
fn name_agent_messages(tx: &rusqlite::Transaction) -> Result<()> {
    let unnamed: Vec<(i64, i64, String, String)> = tx
        .prepare(
            "SELECT rowid, ts, session, data FROM events
                WHERE kind = 'agent_message' AND json_extract(data, '$.name') IS NULL",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (rowid, ts, session, data) in unnamed {
        let Ok(mut data) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let text = data["text"].as_str().unwrap_or_default();
        let Some(message) = agent_message::parse(text, None, None) else {
            continue;
        };
        let noted: Option<String> = tx
            .query_row(
                "SELECT json_extract(data, '$.text') FROM events
                    WHERE session = ?1 AND kind = 'task_notification'
                        AND instr(data, '<task-id>' || ?2 || '</task-id>') > 0 LIMIT 1",
                params![session, message.from],
                |r| r.get(0),
            )
            .optional()?;
        let mut found = noted
            .as_deref()
            .and_then(task_notification::parse)
            .filter(|n| n.source == "agent")
            .and_then(|n| n.name)
            .map(|name| (name, None));
        if found.is_none() && message.hand_back {
            let stops: Vec<(Option<String>, Option<String>)> = tx
                .prepare_cached(
                    "SELECT json_extract(data, '$.description'), json_extract(data, '$.agent_type')
                        FROM events WHERE session = ?1 AND kind = 'subagent'
                            AND ts BETWEEN ?2 AND ?2 + 50",
                )?
                .query_map(params![session, ts], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            if let [(Some(name), agent_type)] = stops.as_slice() {
                found = Some((name.clone(), agent_type.clone()));
            }
        }
        if let Some((name, agent_type)) = found {
            data["name"] = name.into();
            if agent_type.is_some() {
                data["agent_type"] = agent_type.into();
            }
            tx.execute(
                "UPDATE events SET data = ?1 WHERE rowid = ?2",
                params![data.to_string(), rowid],
            )?;
        }
    }
    Ok(())
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Interactive => "interactive",
        Kind::Background => "background",
    }
}

pub fn restored(conn: &Connection, session: &str) -> Result<Restored> {
    let conn = conn.unchecked_transaction()?;
    let last_text = |kind: &str| -> Result<Option<Stamped>> {
        let row = conn
            .prepare_cached(
                "SELECT ts, data FROM events WHERE session = ?1 AND kind = ?2 ORDER BY ts DESC LIMIT 1",
            )?
            .query_row(params![session, kind], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .optional()?;
        Ok(row.and_then(|(at, data)| {
            let data: Value = serde_json::from_str(&data).ok()?;
            Some(Stamped {
                at,
                text: truncate(data.get("text")?.as_str()?, PREVIEW_LEN),
            })
        }))
    };
    let (title, started_at, branch, launched) = conn
        .query_row(
            "SELECT title, started_at, branch, COALESCE(launched, 0) FROM sessions WHERE id = ?1",
            [session],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, bool>(3)?,
                ))
            },
        )
        .optional()?
        .unwrap_or((None, None, None, false));
    let hooked = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM events WHERE session = ?1 AND kind != 'activity')",
        [session],
        |r| r.get(0),
    )?;
    let mut tasks = Vec::new();
    for data in conn
        .prepare_cached(
            "SELECT data FROM events WHERE session = ?1 AND kind = 'task' ORDER BY ts, rowid",
        )?
        .query_map([session], |r| r.get::<_, String>(0))?
    {
        if let Ok(delta) = serde_json::from_str::<Value>(&data?) {
            apply_task(&mut tasks, &delta);
        }
    }
    let now = now_ms();
    let restored = Restored {
        prompt: last_text("prompt")?,
        reply: last_text("reply")?,
        title,
        started_at,
        hooked,
        turn: last_turn(&conn, session)?,
        tasks,
        inbox: messages::previews(&messages::pending(&conn, session)?, PREVIEW_LEN),
        claim: claims::load(&conn, session, now)?,
        branch,
        launched,
    };
    conn.finish()?;
    Ok(restored)
}

/// A session the live state no longer holds, rebuilt from its row (by id
/// or unique id prefix): ended, with its last prompt, reply and turn.
pub fn ended_session(
    conn: &Connection,
    key: &str,
    workspaces: &Workspaces,
) -> Result<Option<Session>> {
    type Row = (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        i64,
    );
    const SELECT: &str = "SELECT id, host, cwd, name, title, kind, started_at FROM sessions";
    let row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Row> {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
        ))
    };
    let exact = conn
        .prepare_cached(&format!("{SELECT} WHERE id = ?1"))?
        .query_row([key], row)
        .optional()?;
    let found = match exact {
        Some(found) => found,
        None => {
            let rows: Vec<Row> = conn
                .prepare_cached(&format!(
                    "{SELECT} WHERE substr(id, 1, length(?1)) = ?1 LIMIT 2"
                ))?
                .query_map([key], row)?
                .collect::<rusqlite::Result<_>>()?;
            match <[Row; 1]>::try_from(rows) {
                Ok([one]) => one,
                Err(_) => return Ok(None),
            }
        }
    };
    let (id, host, cwd, name, title, kind, started_at) = found;
    let (ended_at, last_event_at): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT (SELECT ended_at FROM sessions WHERE id = ?1),
                (SELECT max(ts) FROM events WHERE session = ?1)",
        [&id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let saved = restored(conn, &id)?;
    let last = last_event_at.unwrap_or(started_at).max(started_at);
    let (workspace, project) = match workspaces.locate(&cwd) {
        Some((ws, project)) => (Some(ws), project),
        None => (None, None),
    };
    let mut session = Session::stub(&host, &id, &cwd);
    session.kind = if kind == "background" {
        Kind::Background
    } else {
        Kind::Interactive
    };
    session.workspace = workspace;
    session.project = project;
    session.name = name;
    session.title = title.or(saved.title);
    session.activity = Activity::Gone;
    session.activity_since = ended_at.unwrap_or(last);
    session.hooked = saved.hooked;
    session.prompt = saved.prompt;
    session.reply = saved.reply;
    session.turn = saved.turn;
    session.tasks = saved.tasks;
    session.branch = saved.branch;
    session.launched = saved.launched;
    session.started_at = started_at;
    session.last_event_at = last;
    Ok(Some(session))
}

/// Rebuilds the progress of a session's last turn from its events.
fn last_turn(conn: &Connection, session: &str) -> Result<Option<Turn>> {
    let started_at: Option<i64> = conn.query_row(
        "SELECT max(ts) FROM events
            WHERE session = ?1 AND kind IN ('prompt', 'task_notification', 'agent_message')",
        [session],
        |r| r.get(0),
    )?;
    let Some(started_at) = started_at else {
        return Ok(None);
    };
    let mut turn = Turn {
        started_at,
        ..Turn::default()
    };
    let mut stmt = conn.prepare_cached(
        "SELECT ts, kind, data FROM events
            WHERE session = ?1 AND ts >= ?2 AND kind IN ('tool', 'subagent', 'reply', 'error')
            ORDER BY ts, rowid",
    )?;
    let rows = stmt.query_map(params![session, started_at], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (ts, kind, data) = row?;
        let data: Value = serde_json::from_str(&data).unwrap_or_default();
        match kind.as_str() {
            "tool" => turn.note_tool(
                data["name"].as_str().unwrap_or_default(),
                data["summary"].as_str().unwrap_or_default(),
                data["ok"].as_bool().unwrap_or(true),
            ),
            "subagent" => {
                let tools = data["tools"].as_u64().unwrap_or(0);
                turn.subagent_tools += u32::try_from(tools).unwrap_or(u32::MAX);
            }
            _ => turn.ended_at = Some(ts),
        }
    }
    Ok(Some(turn))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TaskStatus;
    use serde_json::json;

    #[test]
    fn notifications_and_subagent_messages_stored_as_prompts_get_their_kind_once() {
        let dir = std::env::temp_dir().join(format!("ccdash-rewrite-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("t.db");
        let conn = open_writer(&path).unwrap();
        for text in [
            "\n<task-notification>\n<summary>x</summary>",
            "fix <task-notification> parsing",
            "<agent-message from=\"a1\">\nW1 request\n</agent-message>",
            "<agent-message from=\"a2\">\n[Subagent hand-back] The report follows:\n  done\n</agent-message>",
        ] {
            conn.execute(
                "INSERT INTO events VALUES (1, 'h', 's', 'prompt', ?1)",
                [json!({ "text": text }).to_string()],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO events VALUES (1, 'h', 's', 'subagent', ?1)",
            [json!({ "description": "Map it", "agent_type": "Explore" }).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events VALUES (1, 'h', 's', 'task_notification', ?1)",
            [json!({ "text": "<task-notification>\n<task-id>a1</task-id>\n<summary>Agent \"Port W1\" finished</summary>\n</task-notification>" }).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events VALUES (2, 'h', 's', 'reply', ?1)",
            [
                json!({ "text": "Running.\n\n<task-notification>\n</task-notification>" })
                    .to_string(),
            ],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 0).unwrap();
        drop(conn);

        let conn = open_writer(&path).unwrap();
        let kinds: Vec<String> = conn
            .prepare("SELECT kind FROM events ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            kinds,
            [
                task_notification::KIND,
                "prompt",
                agent_message::KIND,
                agent_message::KIND,
                "subagent",
                task_notification::KIND,
                "reply"
            ]
        );
        let names: Vec<(Option<String>, Option<String>)> = conn
            .prepare(
                "SELECT json_extract(data, '$.name'), json_extract(data, '$.agent_type') FROM events
                    WHERE kind = 'agent_message' ORDER BY rowid",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            names,
            [
                (Some("Port W1".into()), None),
                (Some("Map it".into()), Some("Explore".into()))
            ]
        );
        let reply: String = conn
            .query_row(
                "SELECT json_extract(data, '$.text') FROM events WHERE kind = 'reply'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reply, "Running.");
        let done: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(done, 2);
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_and_restores() {
        let dir = std::env::temp_dir().join(format!("ccdash-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("t.db"), 30).unwrap();
        let t = now_ms();
        store.send(
            "testhost",
            StoreMsg::Session {
                id: "s1".into(),
                cwd: "/w".into(),
                name: None,
                title: Some("Fix it".into()),
                kind: Kind::Interactive,
                started_at: t,
                ended_at: None,
                branch: Some("main".into()),
                launched: true,
            },
        );
        store.send(
            "testhost",
            StoreMsg::Session {
                id: "s1".into(),
                cwd: "/w/p".into(),
                name: None,
                title: None,
                kind: Kind::Background,
                started_at: t,
                ended_at: Some(t + 9),
                branch: None,
                launched: false,
            },
        );
        for (dt, kind, data) in [
            (1, "prompt", json!({ "text": "first" })),
            (2, "reply", json!({ "text": "done" })),
            (3, "prompt", json!({ "text": "second" })),
            (
                4,
                "tool",
                json!({ "name": "Edit", "summary": "a.rs", "ok": true }),
            ),
            (
                5,
                "task",
                json!({ "id": "1", "subject": "Ship", "status": "pending" }),
            ),
            (
                6,
                "task",
                json!({ "id": "2", "subject": "Test", "status": "pending" }),
            ),
            (
                7,
                "task",
                json!({ "id": "1", "subject": "Ship", "status": "completed" }),
            ),
            (
                8,
                "task",
                json!({ "id": "2", "subject": "Test", "status": "deleted" }),
            ),
        ] {
            store.send(
                "testhost",
                StoreMsg::Event {
                    ts: t + dt,
                    session: "s1".into(),
                    kind,
                    data,
                },
            );
        }

        let mode = fs::metadata(dir.join("t.db")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let reader = store.reader().unwrap();
        let page_size: i64 = reader
            .pragma_query_value(None, "page_size", |r| r.get(0))
            .unwrap();
        assert_eq!(page_size, 8192);
        let host: String = {
            let mut host = String::new();
            for _ in 0..100 {
                if let Ok(h) =
                    reader.query_row("SELECT host FROM sessions WHERE id = 's1'", [], |r| {
                        r.get(0)
                    })
                {
                    host = h;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            host
        };
        assert_eq!(host, "testhost");
        let mut saved = Restored::default();
        for _ in 0..100 {
            saved = restored(&reader, "s1").unwrap();
            if saved.title.is_some()
                && saved.tasks.len() == 1
                && saved.tasks[0].status == TaskStatus::Completed
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            saved.prompt,
            Some(Stamped {
                at: t + 3,
                text: "second".into()
            })
        );
        assert_eq!(saved.reply.as_ref().unwrap().text, "done");
        assert_eq!(saved.title.as_deref(), Some("Fix it"));
        assert!(saved.hooked);
        let turn = saved.turn.unwrap();
        assert_eq!(
            (turn.started_at, turn.tools, turn.ended_at),
            (t + 3, 1, None)
        );
        assert_eq!(turn.files, ["a.rs"]);
        assert_eq!(saved.tasks[0].subject, "Ship");
        assert_eq!(saved.branch.as_deref(), Some("main"));
        assert!(saved.launched);

        let workspaces = Workspaces::new(&["/w".into()], "/home");
        let ended = ended_session(&reader, "s", &workspaces).unwrap().unwrap();
        assert_eq!(ended.activity, Activity::Gone);
        assert_eq!(
            (ended.kind, ended.activity_since),
            (Kind::Background, t + 9)
        );
        assert_eq!(ended.title.as_deref(), Some("Fix it"));
        assert_eq!(ended.project.as_deref(), Some("p"));
        assert_eq!(ended.prompt.unwrap().text, "second");
        assert!(ended.launched);
        assert!(ended_session(&reader, "x", &workspaces).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
