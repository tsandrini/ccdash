//! SQLite history: events, statusline samples, and one row per session.

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
use tracing::warn;

use crate::{
    model::{Kind, Stamped, Turn},
    paths,
    state::{PREVIEW_LEN, Restored, StoreMsg, apply_task},
    util::{now_ms, truncate},
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
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    host TEXT NOT NULL,
    cwd TEXT NOT NULL,
    name TEXT,
    title TEXT,
    kind TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER
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
];

const UPSERT_SESSION: &str = "
INSERT INTO sessions (id, host, cwd, name, title, kind, started_at, ended_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
ON CONFLICT (id) DO UPDATE SET
    cwd = excluded.cwd,
    name = COALESCE(excluded.name, name),
    title = COALESCE(excluded.title, title),
    kind = excluded.kind,
    ended_at = excluded.ended_at
";

const PRUNE_EVERY_MS: i64 = 3_600_000;

#[derive(Clone)]
pub struct Store {
    tx: mpsc::Sender<StoreMsg>,
    path: PathBuf,
}

impl Store {
    /// Opens (creating if needed) the database and starts the writer thread.
    pub fn open(path: &Path, host: String, retention_days: u32) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
            if dir == paths::state_dir() {
                fs::set_permissions(dir, Permissions::from_mode(0o700))?;
            }
        }
        make_private(path)?;
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        let (tx, rx) = mpsc::channel();
        let retention_ms = i64::from(retention_days) * 86_400_000;
        thread::Builder::new()
            .name("store".into())
            .spawn(move || writer(conn, rx, host, retention_ms))?;
        Ok(Self {
            tx,
            path: path.to_owned(),
        })
    }

    pub fn send(&self, msg: StoreMsg) {
        let _ = self.tx.send(msg);
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

fn writer(mut conn: Connection, rx: mpsc::Receiver<StoreMsg>, host: String, retention_ms: i64) {
    let mut last_prune = 0;
    while let Ok(first) = rx.recv() {
        let batch: Vec<StoreMsg> = std::iter::once(first).chain(rx.try_iter()).collect();
        if let Err(e) = write_batch(&mut conn, &host, &batch) {
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

fn write_batch(conn: &mut Connection, host: &str, batch: &[StoreMsg]) -> Result<()> {
    let tx = conn.transaction()?;
    for msg in batch {
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
            StoreMsg::Session {
                id,
                cwd,
                name,
                title,
                kind,
                started_at,
                ended_at,
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
                        ended_at
                    ],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

fn prune(conn: &Connection, cutoff: i64) -> Result<()> {
    conn.execute("DELETE FROM events WHERE ts < ?1", [cutoff])?;
    conn.execute("DELETE FROM samples WHERE ts < ?1", [cutoff])?;
    conn.execute("DELETE FROM proc_samples WHERE ts < ?1", [cutoff])?;
    conn.execute(
        "DELETE FROM sessions WHERE started_at < ?1
            AND (ended_at < ?1 OR (ended_at IS NULL AND id NOT IN (SELECT session FROM events)))",
        [cutoff],
    )?;
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
    for (table, column, kind) in ADDED_COLUMNS {
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
    let title = conn
        .query_row("SELECT title FROM sessions WHERE id = ?1", [session], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten();
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
    let restored = Restored {
        prompt: last_text("prompt")?,
        reply: last_text("reply")?,
        title,
        hooked,
        turn: last_turn(&conn, session)?,
        tasks,
    };
    conn.finish()?;
    Ok(restored)
}

/// Rebuilds the progress of a session's last turn from its events.
fn last_turn(conn: &Connection, session: &str) -> Result<Option<Turn>> {
    let started_at: Option<i64> = conn.query_row(
        "SELECT max(ts) FROM events WHERE session = ?1 AND kind = 'prompt'",
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
    fn writes_and_restores() {
        let dir = std::env::temp_dir().join(format!("ccdash-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("t.db"), "testhost".into(), 30).unwrap();
        let t = now_ms();
        store.send(StoreMsg::Session {
            id: "s1".into(),
            cwd: "/w".into(),
            name: None,
            title: Some("Fix it".into()),
            kind: Kind::Interactive,
            started_at: t,
            ended_at: None,
        });
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
            store.send(StoreMsg::Event {
                ts: t + dt,
                session: "s1".into(),
                kind,
                data,
            });
        }

        let mode = fs::metadata(dir.join("t.db")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let reader = store.reader().unwrap();
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
