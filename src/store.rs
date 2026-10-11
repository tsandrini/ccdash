//! SQLite history: events, statusline samples, and one row per session.

use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use tracing::warn;

use crate::{
    model::{Kind, Stamped},
    state::{Restored, StoreMsg},
    util::now_ms,
};

const SCHEMA: &str = "
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
    seven_day REAL
);
CREATE INDEX IF NOT EXISTS samples_ts ON samples (ts);
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
INSERT INTO samples (ts, host, session, context_pct, cost_usd, five_hour, seven_day)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
";

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

#[derive(Debug, Serialize)]
pub struct EventRow {
    pub ts: i64,
    pub kind: String,
    pub data: Value,
}

impl Store {
    /// Opens (creating if needed) the database and starts the writer thread.
    pub fn open(path: &Path, host: String, retention_days: u32) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
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
                context_pct,
                cost_usd,
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
                        seven_day
                    ],
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
    conn.execute(
        "DELETE FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?1",
        [cutoff],
    )?;
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
                text: data.get("text")?.as_str()?.to_owned(),
            })
        }))
    };
    let title = conn
        .query_row("SELECT title FROM sessions WHERE id = ?1", [session], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten();
    let restored = Restored {
        prompt: last_text("prompt")?,
        reply: last_text("reply")?,
        title,
    };
    conn.finish()?;
    Ok(restored)
}

/// The newest `limit` events of a session, oldest first.
pub fn events(conn: &Connection, session: &str, limit: u32) -> Result<Vec<EventRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ts, kind, data FROM events WHERE session = ?1 ORDER BY ts DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![session, limit], |r| {
        Ok(EventRow {
            ts: r.get(0)?,
            kind: r.get(1)?,
            data: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or(Value::Null),
        })
    })?;
    let mut events = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    events.reverse();
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        for (dt, kind, text) in [
            (1, "prompt", "first"),
            (2, "reply", "done"),
            (3, "prompt", "second"),
        ] {
            store.send(StoreMsg::Event {
                ts: t + dt,
                session: "s1".into(),
                kind,
                data: json!({ "text": text }),
            });
        }

        let reader = store.reader().unwrap();
        let mut saved = Restored::default();
        for _ in 0..100 {
            saved = restored(&reader, "s1").unwrap();
            if saved.title.is_some() && saved.prompt.as_ref().is_some_and(|p| p.text == "second") {
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

        let kinds: Vec<String> = events(&reader, "s1", 2)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, ["reply", "prompt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
