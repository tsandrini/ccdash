//! The hub's side: the file tree under `sync.dir`, its index in the hub
//! database, and the `/sync/` routes collectors call with their own
//! tokens. Every read and write runs on the store's writer thread, so a
//! reader never sees a file between its write and its index row.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;
use tracing::warn;

use super::{
    BODY_LIMIT, CHUNK, Changes, ConflictReport, ConflictState, HistoryPage, Keep, Probe, Probed,
    Refused, Row, StatGroup, StatSession, StatSize, SyncConflict, SyncStats, fp, line_digest,
    session_of, valid_path,
};
use crate::{
    hub::{collect::Tokens, core::Hub},
    store::Store,
    util::now_ms,
};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sync_files (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    version INTEGER NOT NULL,
    base INTEGER NOT NULL,
    host TEXT NOT NULL,
    session TEXT,
    at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sync_files_version ON sync_files (version);
CREATE INDEX IF NOT EXISTS sync_files_session ON sync_files (session);
CREATE TABLE IF NOT EXISTS sync_history (
    id INTEGER PRIMARY KEY,
    host TEXT NOT NULL,
    digest BLOB NOT NULL UNIQUE,
    line TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_conflicts (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL,
    host TEXT NOT NULL,
    size INTEGER NOT NULL,
    copy TEXT NOT NULL,
    at INTEGER NOT NULL,
    dismissed_at INTEGER,
    details TEXT,
    former_hub INTEGER,
    pending TEXT,
    error TEXT,
    resolved_at INTEGER,
    resolution TEXT
);
CREATE TABLE IF NOT EXISTS sync_growth (
    day TEXT NOT NULL,
    grp TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    files INTEGER NOT NULL,
    PRIMARY KEY (day, grp)
);
";

/// Columns `sync_conflicts` gained after its first release (see
/// `store::ADDED_COLUMNS`).
pub const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("sync_conflicts", "details", "TEXT"),
    ("sync_conflicts", "former_hub", "INTEGER"),
    ("sync_conflicts", "pending", "TEXT"),
    ("sync_conflicts", "error", "TEXT"),
    ("sync_conflicts", "resolved_at", "INTEGER"),
    ("sync_conflicts", "resolution", "TEXT"),
];

/// Rows one `changes` page holds at most.
const MAX_ROWS: usize = 2000;
/// Probes one `compare` call takes at most.
const MAX_PROBES: usize = 2000;
/// Bytes of prompt history one page holds at most (one line may exceed it).
const HISTORY_PAGE_BYTES: usize = 4 << 20;
/// Suffix of a replacement being written, renamed over the file when whole.
const PART: &str = ".ccdash-part";

/// Where the hub keeps the files.
#[derive(Debug, Clone)]
pub struct Tree {
    root: PathBuf,
}

impl Tree {
    /// Creates the directory, private to the hub's user.
    ///
    /// # Errors
    ///
    /// The directory cannot be created.
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            root: root.to_owned(),
        })
    }

    fn file(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

/// What a write came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Put {
    Done(Row),
    /// The expected version or offset did not match; the hub's row.
    Refused(Option<Row>),
}

fn row_of(conn: &Connection, path: &str) -> Result<Option<Row>> {
    Ok(conn
        .prepare_cached(
            "SELECT path, size, version, base, host, at FROM sync_files WHERE path = ?1",
        )?
        .query_row([path], read_row)
        .optional()?)
}

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        path: r.get(0)?,
        size: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
        version: r.get(2)?,
        base: r.get(3)?,
        host: r.get(4)?,
        at: r.get(5)?,
    })
}

/// Writes `data` at `offset` of `path` for `host`, if `expect` is the
/// file's version (0 for a file the hub lacks): at the current size it
/// appends, at 0 it replaces; anything else is refused. The size change
/// counts as growth on the day of `mtime` (when the collector's copy was
/// written; a backfilled old file lands on its own day), else of `now`.
///
/// # Errors
///
/// The file or the index cannot be written, or the hub's copy is shorter
/// than its index says.
#[allow(clippy::too_many_arguments)]
pub fn put(
    conn: &mut Connection,
    tree: &Tree,
    host: &str,
    path: &str,
    offset: u64,
    expect: i64,
    data: &[u8],
    mtime: Option<i64>,
    now: i64,
) -> Result<Put> {
    let row = row_of(conn, path)?;
    if row.as_ref().map_or(0, |r| r.version) != expect {
        return Ok(Put::Refused(row));
    }
    let file = tree.file(path);
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let (size, base) = match &row {
        Some(r) if offset == r.size && offset > 0 && data.is_empty() => {
            return Ok(Put::Done(r.clone()));
        }
        Some(r) if offset == r.size && offset > 0 => {
            let mut f = OpenOptions::new().write(true).open(&file)?;
            let len = f.metadata()?.len();
            if len < r.size {
                bail!(
                    "{path}: the hub's copy has {len} bytes, its index {}",
                    r.size
                );
            }
            // NOTE: a write that died before its index row
            f.set_len(r.size)?;
            f.seek(SeekFrom::Start(r.size))?;
            f.write_all(data)?;
            f.sync_data()?;
            (r.size + data.len() as u64, Some(r.base))
        }
        _ if offset == 0 => {
            let part = PathBuf::from(format!("{}{PART}", file.display()));
            let mut f = File::create(&part)?;
            f.write_all(data)?;
            f.sync_data()?;
            fs::rename(&part, &file)?;
            (data.len() as u64, None)
        }
        _ => return Ok(Put::Refused(row)),
    };
    let version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM sync_files",
        [],
        |r| r.get(0),
    )?;
    let new = Row {
        path: path.to_owned(),
        size,
        version,
        base: base.unwrap_or(version),
        host: host.to_owned(),
        at: now,
    };
    conn.prepare_cached(
        "INSERT INTO sync_files (path, size, version, base, host, session, at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ON CONFLICT (path) DO UPDATE SET size = ?2, version = ?3, base = ?4, host = ?5,
            session = ?6, at = ?7",
    )?
    .execute(params![
        new.path,
        i64::try_from(new.size)?,
        new.version,
        new.base,
        new.host,
        session_of(path),
        new.at
    ])?;
    let delta = i64::try_from(new.size)? - row.as_ref().map_or(Ok(0), |r| i64::try_from(r.size))?;
    if delta != 0 || row.is_none() {
        conn.prepare_cached(
            "INSERT INTO sync_growth (day, grp, bytes, files)
            VALUES (date(?1 / 1000, 'unixepoch', 'localtime'), ?2, ?3, ?4)
            ON CONFLICT (day, grp) DO UPDATE SET bytes = bytes + ?3, files = files + ?4",
        )?
        .execute(params![
            mtime.unwrap_or(now),
            group_of(path),
            delta,
            i64::from(row.is_none())
        ])?;
    }
    Ok(Put::Done(new))
}

/// What a path's growth is counted under: `projects/<dir>` for sessions,
/// else its top-level directory.
fn group_of(path: &str) -> &str {
    let mut cut = path.match_indices('/').map(|(i, _)| i);
    let first = cut.next().unwrap_or(path.len());
    if &path[..first] == "projects" {
        &path[..cut.next().unwrap_or(path.len())]
    } else {
        &path[..first]
    }
}

/// Up to `CHUNK` bytes of `path` from `from`, stopping at the indexed
/// size, with the row they belong to.
///
/// # Errors
///
/// The file cannot be read or is shorter than its index.
pub fn read(
    conn: &Connection,
    tree: &Tree,
    path: &str,
    from: u64,
) -> Result<Option<(Row, Vec<u8>)>> {
    let Some(row) = row_of(conn, path)? else {
        return Ok(None);
    };
    let len = row.size.saturating_sub(from).min(CHUNK as u64);
    let mut data = Vec::with_capacity(len as usize);
    if len > 0 {
        let mut f = File::open(tree.file(path))?;
        f.seek(SeekFrom::Start(from))?;
        f.take(len).read_to_end(&mut data)?;
        if data.len() as u64 != len {
            bail!("{path}: the hub's copy is shorter than its index");
        }
    }
    Ok(Some((row, data)))
}

/// Rows written after `since`, oldest first.
///
/// # Errors
///
/// The index cannot be read.
pub fn changes(conn: &Connection, since: i64, limit: usize) -> Result<Changes> {
    let limit = limit.clamp(1, MAX_ROWS);
    let mut rows = conn
        .prepare_cached(
            "SELECT path, size, version, base, host, at FROM sync_files
            WHERE version > ?1 ORDER BY version LIMIT ?2",
        )?
        .query_map(params![since, limit as i64 + 1], read_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    Ok(Changes { rows, more })
}

/// The hub's row and fingerprint for each probe, at the smaller size.
///
/// # Errors
///
/// The index or a file cannot be read.
pub fn compare(conn: &Connection, tree: &Tree, probes: &[Probe]) -> Result<Vec<Probed>> {
    probes
        .iter()
        .map(|p| {
            let row = row_of(conn, &p.path)?;
            let fp = match &row {
                Some(r) => Some(fp(
                    &mut File::open(tree.file(&p.path))?,
                    r.size.min(p.size),
                )?),
                None => None,
            };
            Ok(Probed {
                path: p.path.clone(),
                row,
                fp,
            })
        })
        .collect()
}

/// Adds prompt history lines from `host`; returns how many were new.
///
/// # Errors
///
/// The index cannot be written.
pub fn add_history(conn: &mut Connection, host: &str, lines: &[String], now: i64) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut added = 0;
    {
        let mut insert = tx.prepare_cached(
            "INSERT OR IGNORE INTO sync_history (host, digest, line, at) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for line in lines {
            added += insert.execute(params![host, line_digest(line), line, now])?;
        }
    }
    tx.commit()?;
    Ok(added)
}

/// Prompt history lines after `since`, oldest first.
///
/// # Errors
///
/// The index cannot be read.
pub fn history(conn: &Connection, since: i64, limit: usize) -> Result<HistoryPage> {
    let limit = limit.clamp(1, 10_000);
    let mut stmt = conn
        .prepare_cached("SELECT id, line FROM sync_history WHERE id > ?1 ORDER BY id LIMIT ?2")?;
    let mut rows = stmt.query(params![since, limit as i64 + 1])?;
    let (mut lines, mut bytes, mut more) = (Vec::new(), 0, false);
    while let Some(r) = rows.next()? {
        if lines.len() == limit || (bytes >= HISTORY_PAGE_BYTES && !lines.is_empty()) {
            more = true;
            break;
        }
        let line: String = r.get(1)?;
        bytes += line.len();
        lines.push((r.get(0)?, line));
    }
    Ok(HistoryPage { lines, more })
}

/// Records a conflict a collector set aside.
///
/// # Errors
///
/// The index cannot be written.
pub fn add_conflict(conn: &Connection, host: &str, c: &ConflictReport, now: i64) -> Result<i64> {
    conn.execute(
        "INSERT INTO sync_conflicts (path, host, size, copy, at, details, former_hub)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            c.path,
            host,
            i64::try_from(c.size)?,
            c.copy,
            now,
            c.details.as_ref().map(serde_json::to_string).transpose()?,
            c.former_hub
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

const CONFLICT_COLUMNS: &str =
    "id, path, host, size, copy, at, details, COALESCE(former_hub, 0), pending, error";

fn conflict_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SyncConflict> {
    let path: String = r.get(1)?;
    Ok(SyncConflict {
        id: r.get(0)?,
        session: session_of(&path).map(str::to_owned),
        title: None,
        path,
        host: r.get(2)?,
        size: r.get::<_, i64>(3)?.try_into().unwrap_or(0),
        copy: r.get(4)?,
        at: r.get(5)?,
        details: r
            .get::<_, Option<String>>(6)?
            .and_then(|d| serde_json::from_str(&d).ok()),
        former_hub: r.get(7)?,
        pending: r
            .get::<_, Option<String>>(8)?
            .and_then(|k| serde_json::from_value(serde_json::Value::String(k)).ok()),
        error: r.get(9)?,
    })
}

/// Conflicts still open (not resolved, not hidden), newest first, with
/// their sessions' titles.
///
/// # Errors
///
/// The index cannot be read.
pub fn conflicts(conn: &Connection) -> Result<Vec<SyncConflict>> {
    let mut open = conn
        .prepare_cached(&format!(
            "SELECT {CONFLICT_COLUMNS} FROM sync_conflicts
            WHERE dismissed_at IS NULL AND resolved_at IS NULL ORDER BY at DESC LIMIT 200"
        ))?
        .query_map([], conflict_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_sessions: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'sessions')",
        [],
        |r| r.get(0),
    )?;
    if has_sessions {
        let mut title =
            conn.prepare_cached("SELECT COALESCE(title, name) FROM sessions WHERE id = ?1")?;
        for c in &mut open {
            if let Some(session) = &c.session {
                c.title = title
                    .query_row([session], |r| r.get(0))
                    .optional()?
                    .flatten();
            }
        }
    }
    Ok(open)
}

/// Follows what `host` holds under `ccdash-conflicts/`: open conflicts
/// whose copy is gone are resolved, the others get the copy's details, and
/// a copy the hub has no record of gets one.
///
/// # Errors
///
/// The index cannot be written.
pub fn conflict_state(
    conn: &mut Connection,
    host: &str,
    state: &ConflictState,
    now: i64,
) -> Result<()> {
    let tx = conn.transaction()?;
    let open: Vec<(i64, String)> = tx
        .prepare_cached(
            "SELECT id, copy FROM sync_conflicts
            WHERE host = ?1 AND dismissed_at IS NULL AND resolved_at IS NULL",
        )?
        .query_map([host], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, copy) in &open {
        match state.copies.iter().find(|c| &c.copy == copy) {
            Some(c) => {
                tx.execute(
                    "UPDATE sync_conflicts SET size = ?2, details = ?3, former_hub = ?4
                    WHERE id = ?1",
                    params![
                        id,
                        i64::try_from(c.size)?,
                        c.details.as_ref().map(serde_json::to_string).transpose()?,
                        c.former_hub
                    ],
                )?;
            }
            None => {
                tx.execute(
                    "UPDATE sync_conflicts SET resolved_at = ?2, pending = NULL,
                        resolution = COALESCE(resolution, 'the copy was deleted')
                    WHERE id = ?1",
                    params![id, now],
                )?;
            }
        }
    }
    for c in &state.copies {
        let known: bool = tx
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM sync_conflicts WHERE host = ?1 AND copy = ?2)",
            )?
            .query_row(params![host, c.copy], |r| r.get(0))?;
        if !known {
            add_conflict(&tx, host, c, now)?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Marks a resolution as sent; the conflict with its host, or why not.
///
/// # Errors
///
/// The index cannot be written.
pub fn request(conn: &Connection, id: i64, keep: Keep) -> Result<Result<SyncConflict, String>> {
    let Some(c) = conn
        .prepare_cached(&format!(
            "SELECT {CONFLICT_COLUMNS} FROM sync_conflicts
            WHERE id = ?1 AND dismissed_at IS NULL AND resolved_at IS NULL"
        ))?
        .query_row([id], conflict_row)
        .optional()?
    else {
        return Ok(Err(format!("no open conflict {id}")));
    };
    if c.pending.is_some() {
        return Ok(Err("a resolution is already on its way".into()));
    }
    let keep = serde_json::to_value(keep)?;
    conn.execute(
        "UPDATE sync_conflicts SET pending = ?2, error = NULL WHERE id = ?1",
        params![id, keep.as_str()],
    )?;
    Ok(Ok(c))
}

/// What `host` made of a resolution: with `error`, it stays open and says
/// why; keeping the hub's version resolves it, using the copy leaves it
/// open with the replaced version as the copy, now at `copy`.
///
/// # Errors
///
/// The index cannot be written.
pub fn resolved(
    conn: &Connection,
    host: &str,
    id: i64,
    error: Option<&str>,
    copy: Option<&str>,
    now: i64,
) -> Result<()> {
    if error.is_none()
        && let Some(copy) = copy
    {
        conn.execute(
            "UPDATE sync_conflicts SET copy = ?3 WHERE id = ?1 AND host = ?2",
            params![id, host, copy],
        )?;
    }
    match error {
        Some(e) => conn.execute(
            "UPDATE sync_conflicts SET pending = NULL, error = ?3 WHERE id = ?1 AND host = ?2",
            params![id, host, e],
        )?,
        None => conn.execute(
            "UPDATE sync_conflicts SET
                resolved_at = CASE pending WHEN 'hub' THEN ?3 ELSE resolved_at END,
                resolution = CASE pending WHEN 'hub' THEN 'kept the hub''s version'
                    ELSE resolution END,
                former_hub = CASE pending WHEN 'copy' THEN 1 ELSE former_hub END,
                pending = NULL, error = NULL
            WHERE id = ?1 AND host = ?2",
            params![id, host, now],
        )?,
    };
    Ok(())
}

/// Hides a conflict from the panel (the copy stays on its machine); false
/// when there is no such one open.
///
/// # Errors
///
/// The index cannot be written.
pub fn dismiss(conn: &Connection, id: i64, now: i64) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE sync_conflicts SET dismissed_at = ?2
        WHERE id = ?1 AND dismissed_at IS NULL AND resolved_at IS NULL",
        params![id, now],
    )? > 0)
}

/// Sizes by kind, project and session, and growth, for the panel;
/// `labels` names project directories (`projects/<dir>` → `ws/project`),
/// and a directory it lacks is named after the workspace it lies in
/// (`workspaces`: a workspace's `dir_name` and its name).
///
/// # Errors
///
/// The index cannot be read.
pub fn stats(
    conn: &Connection,
    labels: &HashMap<String, String>,
    workspaces: &[(String, String)],
) -> Result<SyncStats> {
    let label_of = |key: &str| -> String {
        if let Some(label) = labels.get(key) {
            return label.clone();
        }
        let dir = key.trim_start_matches("projects/");
        workspaces
            .iter()
            .filter(|(prefix, _)| dir.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map_or_else(
                || dir.trim_start_matches('-').to_owned(),
                |(prefix, ws)| match dir[prefix.len()..].strip_prefix('-') {
                    Some(rest) if !rest.is_empty() => format!("{ws}/{rest}"),
                    _ => ws.clone(),
                },
            )
    };
    let mut kinds: BTreeMap<String, StatGroup> = BTreeMap::new();
    let mut projects: HashMap<String, (StatGroup, HashSet<String>)> = HashMap::new();
    let mut total = StatSize::default();
    {
        let mut stmt = conn.prepare("SELECT path, size, session FROM sync_files")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let path: String = r.get(0)?;
            let size: u64 = r.get::<_, i64>(1)?.try_into().unwrap_or(0);
            let session: Option<String> = r.get(2)?;
            total.files += 1;
            total.bytes += size;
            let kind = path.split('/').next().unwrap_or_default().to_owned();
            let k = kinds.entry(kind.clone()).or_insert_with(|| StatGroup {
                key: kind.clone(),
                label: kind.clone(),
                ..StatGroup::default()
            });
            k.size.files += 1;
            k.size.bytes += size;
            if kind == "projects" {
                let key = group_of(&path).to_owned();
                let (g, sessions) = projects.entry(key.clone()).or_insert_with(|| {
                    let label = label_of(&key);
                    (
                        StatGroup {
                            key,
                            label,
                            ..StatGroup::default()
                        },
                        HashSet::new(),
                    )
                });
                g.size.files += 1;
                g.size.bytes += size;
                if let Some(s) = session {
                    sessions.insert(s);
                }
            }
        }
    }
    let mut growth = conn.prepare(
        "SELECT grp,
            SUM(CASE WHEN day > date('now', 'localtime', '-1 day') THEN bytes ELSE 0 END),
            SUM(CASE WHEN day > date('now', 'localtime', '-7 days') THEN bytes ELSE 0 END),
            SUM(CASE WHEN day > date('now', 'localtime', '-30 days') THEN bytes ELSE 0 END)
        FROM sync_growth GROUP BY grp",
    )?;
    let mut rows = growth.query([])?;
    while let Some(r) = rows.next()? {
        let grp: String = r.get(0)?;
        let (day, week, month): (i64, i64, i64) = (r.get(1)?, r.get(2)?, r.get(3)?);
        let kind = grp.split('/').next().unwrap_or_default();
        if let Some(k) = kinds.get_mut(kind) {
            k.day += day;
            k.week += week;
            k.month += month;
        }
        if let Some((g, _)) = projects.get_mut(&grp) {
            g.day += day;
            g.week += week;
            g.month += month;
        }
    }
    // NOTE: a project's subdirectories have directories of their own
    let mut merged: HashMap<String, (StatGroup, HashSet<String>)> = HashMap::new();
    for (g, sessions) in projects.into_values() {
        match merged.get_mut(&g.label) {
            Some((m, all)) => {
                m.size.files += g.size.files;
                m.size.bytes += g.size.bytes;
                m.day += g.day;
                m.week += g.week;
                m.month += g.month;
                all.extend(sessions);
            }
            None => {
                merged.insert(g.label.clone(), (g, sessions));
            }
        }
    }
    let mut projects: Vec<StatGroup> = merged
        .into_values()
        .map(|(mut g, sessions)| {
            g.sessions = sessions.len() as u64;
            g
        })
        .collect();
    projects.sort_by_key(|g| std::cmp::Reverse(g.size.bytes));
    let mut kinds: Vec<StatGroup> = kinds.into_values().collect();
    kinds.sort_by_key(|g| std::cmp::Reverse(g.size.bytes));
    let sessions = conn
        .prepare(
            "SELECT f.session, SUM(f.size), COUNT(*), MAX(f.at),
                COALESCE(s.title, s.name), s.cwd, s.host
            FROM sync_files f LEFT JOIN sessions s ON s.id = f.session
            WHERE f.session IS NOT NULL
            GROUP BY f.session ORDER BY 2 DESC LIMIT 25",
        )?
        .query_map([], |r| {
            let cwd: Option<String> = r.get(5)?;
            Ok(StatSession {
                session: r.get(0)?,
                size: StatSize {
                    bytes: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
                    files: r.get::<_, i64>(2)?.try_into().unwrap_or(0),
                },
                last_write: r.get(3)?,
                title: r.get(4)?,
                label: cwd.map(|c| label_of(&format!("projects/{}", dir_name(&c)))),
                host: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let days = conn
        .prepare(
            "SELECT day, SUM(bytes) FROM sync_growth
            WHERE day > date('now', 'localtime', '-30 days') GROUP BY day ORDER BY day",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let since: Option<String> =
        conn.query_row("SELECT MIN(day) FROM sync_growth", [], |r| r.get(0))?;
    let history = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(CAST(line AS BLOB))), 0) FROM sync_history",
        [],
        |r| {
            Ok(StatSize {
                files: r.get::<_, i64>(0)?.try_into().unwrap_or(0),
                bytes: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
            })
        },
    )?;
    Ok(SyncStats {
        total,
        kinds,
        projects,
        sessions,
        days,
        since,
        history,
    })
}

/// The directory name Claude Code files a working directory's sessions
/// under: every character but ASCII letters and digits becomes `-`.
pub fn dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Whether the hub holds the main transcript of `session`.
///
/// # Errors
///
/// The index cannot be read.
pub fn holds_transcript(conn: &Connection, session: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT 1 FROM sync_files WHERE session = ?1 AND path LIKE 'projects/%/' || ?1 || '.jsonl'",
        )?
        .exists([session])?)
}

/// Files and bytes the hub holds.
///
/// # Errors
///
/// The index cannot be read.
pub fn totals(conn: &Connection) -> Result<(u64, u64)> {
    Ok(conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM sync_files",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?.try_into().unwrap_or(0),
                r.get::<_, i64>(1)?.try_into().unwrap_or(0),
            ))
        },
    )?)
}

/// The hub's copy of Claude Code's state: what the `/sync/` routes share.
#[derive(Clone)]
pub struct Mirror {
    pub store: Store,
    pub tree: Arc<Tree>,
    /// Counts accepted writes (files and history); every collector link
    /// forwards it as `SyncChanged`.
    pub changed: watch::Sender<u64>,
    /// Who holds a session (`Hub::holder`), so a machine where it is open
    /// second cannot write its files.
    hub: Hub,
}

impl Mirror {
    /// Opens the tree.
    ///
    /// # Errors
    ///
    /// The directory cannot be created.
    pub fn open(store: Store, root: &Path, hub: Hub) -> Result<Self> {
        Ok(Self {
            store,
            tree: Arc::new(Tree::open(root)?),
            changed: watch::channel(0).0,
            hub,
        })
    }

    fn bump(&self) {
        self.changed.send_modify(|seq| *seq += 1);
    }
}

/// The host a collector token names, on every `/sync/` request.
#[derive(Clone)]
struct Host(String);

/// The `/sync/` routes, each behind a collector token.
pub fn routes<S>(sync: Mirror, tokens: Tokens) -> Router<S>
where
    S: Clone + Send + std::marker::Sync + 'static,
{
    Router::new()
        .route("/changes", get(changes_route))
        .route("/compare", post(compare_route))
        .route("/file", get(get_file).put(put_file))
        .route("/history", get(history_route).post(add_history_route))
        .route("/conflicts", post(conflict_route))
        .route("/conflicts/state", post(conflict_state_route))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(tokens, collector_auth))
        .with_state(sync)
}

async fn collector_auth(
    State(tokens): State<Tokens>,
    mut request: Request,
    next: Next,
) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    let Some(host) = given.and_then(|t| tokens.host_of(&t)) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    request.extensions_mut().insert(Host(host));
    next.run(request).await
}

struct SyncError(StatusCode, String);

impl From<anyhow::Error> for SyncError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for SyncError {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            warn!("sync {}: {}", self.0, self.1);
        }
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn checked(path: &str) -> Result<(), SyncError> {
    if valid_path(path) {
        Ok(())
    } else {
        Err(SyncError(
            StatusCode::BAD_REQUEST,
            format!("bad path {path:?}"),
        ))
    }
}

#[derive(Deserialize)]
struct Since {
    since: i64,
    limit: Option<usize>,
}

async fn changes_route(
    State(sync): State<Mirror>,
    Query(q): Query<Since>,
) -> Result<Json<Changes>, SyncError> {
    let page = sync
        .store
        .exec(move |conn| changes(conn, q.since, q.limit.unwrap_or(MAX_ROWS)))
        .await??;
    Ok(Json(page))
}

async fn compare_route(
    State(sync): State<Mirror>,
    Json(probes): Json<Vec<Probe>>,
) -> Result<Json<Vec<Probed>>, SyncError> {
    if probes.len() > MAX_PROBES {
        return Err(SyncError(
            StatusCode::BAD_REQUEST,
            format!("at most {MAX_PROBES} probes per call"),
        ));
    }
    for p in &probes {
        checked(&p.path)?;
    }
    let tree = sync.tree.clone();
    Ok(Json(
        sync.store
            .exec(move |conn| compare(conn, &tree, &probes))
            .await??,
    ))
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
    #[serde(default)]
    from: u64,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    expect: i64,
    /// When the collector's copy was last written (ms), for the growth.
    #[serde(default)]
    mtime: Option<i64>,
}

/// The row of a file, as response headers.
fn row_headers(row: &Row) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("x-sync-size", row.size.to_string()),
        ("x-sync-version", row.version.to_string()),
        ("x-sync-base", row.base.to_string()),
        ("x-sync-at", row.at.to_string()),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    if let Ok(host) = HeaderValue::from_str(&row.host) {
        headers.insert("x-sync-host", host);
    }
    headers
}

async fn get_file(
    State(sync): State<Mirror>,
    Query(q): Query<FileQuery>,
) -> Result<Response, SyncError> {
    checked(&q.path)?;
    let tree = sync.tree.clone();
    let path = q.path.clone();
    match sync
        .store
        .exec(move |conn| read(conn, &tree, &path, q.from))
        .await??
    {
        Some((row, data)) => Ok((row_headers(&row), data).into_response()),
        None => Err(SyncError(StatusCode::NOT_FOUND, format!("no {}", q.path))),
    }
}

async fn put_file(
    State(sync): State<Mirror>,
    Extension(Host(host)): Extension<Host>,
    Query(q): Query<FileQuery>,
    body: Bytes,
) -> Result<Response, SyncError> {
    checked(&q.path)?;
    // NOTE: a session open on a second machine writes even when its prompts
    // are blocked (queue and cost lines); the holder's copy must win
    if let Some(session) = session_of(&q.path)
        && let Some(holder) = sync.hub.holder(&host, session).await?
    {
        let (store, path) = (sync.store.clone(), q.path.clone());
        let row = store.exec(move |conn| row_of(conn, &path)).await??;
        let refused = Refused {
            row,
            held_by: Some(holder),
        };
        return Ok((StatusCode::CONFLICT, Json(refused)).into_response());
    }
    let tree = sync.tree.clone();
    let done = sync
        .store
        .exec(move |conn| {
            put(
                conn,
                &tree,
                &host,
                &q.path,
                q.offset,
                q.expect,
                &body,
                q.mtime,
                now_ms(),
            )
        })
        .await??;
    Ok(match done {
        Put::Done(row) => {
            sync.bump();
            Json(row).into_response()
        }
        Put::Refused(row) => {
            (StatusCode::CONFLICT, Json(Refused { row, held_by: None })).into_response()
        }
    })
}

async fn history_route(
    State(sync): State<Mirror>,
    Query(q): Query<Since>,
) -> Result<Json<HistoryPage>, SyncError> {
    let page = sync
        .store
        .exec(move |conn| history(conn, q.since, q.limit.unwrap_or(5000)))
        .await??;
    Ok(Json(page))
}

#[derive(Deserialize)]
struct Lines {
    lines: Vec<String>,
}

async fn add_history_route(
    State(sync): State<Mirror>,
    Extension(Host(host)): Extension<Host>,
    Json(body): Json<Lines>,
) -> Result<Json<serde_json::Value>, SyncError> {
    let added = sync
        .store
        .exec(move |conn| add_history(conn, &host, &body.lines, now_ms()))
        .await??;
    if added > 0 {
        sync.bump();
    }
    Ok(Json(json!({ "added": added })))
}

async fn conflict_route(
    State(sync): State<Mirror>,
    Extension(Host(host)): Extension<Host>,
    Json(report): Json<ConflictReport>,
) -> Result<Json<serde_json::Value>, SyncError> {
    checked(&report.path)?;
    checked(&report.copy)?;
    let id = sync
        .store
        .exec(move |conn| add_conflict(conn, &host, &report, now_ms()))
        .await??;
    Ok(Json(json!({ "id": id })))
}

async fn conflict_state_route(
    State(sync): State<Mirror>,
    Extension(Host(host)): Extension<Host>,
    Json(state): Json<ConflictState>,
) -> Result<Json<serde_json::Value>, SyncError> {
    for c in &state.copies {
        checked(&c.path)?;
        checked(&c.copy)?;
    }
    sync.store
        .exec(move |conn| conflict_state(conn, &host, &state, now_ms()))
        .await??;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::ConflictDetails;

    fn setup(name: &str) -> (PathBuf, Connection, Tree) {
        let dir = std::env::temp_dir().join(format!("ccdash-sync-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("db")).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let tree = Tree::open(&dir.join("claude")).unwrap();
        (dir, conn, tree)
    }

    fn done(p: Put) -> Row {
        match p {
            Put::Done(row) => row,
            Put::Refused(row) => panic!("refused, hub has {row:?}"),
        }
    }

    #[test]
    fn appends_replaces_and_refusals() {
        let (dir, mut conn, tree) = setup("put");
        let path = "projects/-w/1ee78b00-59c7-4bf5-b462-ef39161f7d8d.jsonl";
        let a = done(put(&mut conn, &tree, "navi", path, 0, 0, b"one\n", None, 1).unwrap());
        assert_eq!((a.size, a.version, a.base), (4, 1, 1));
        // a second creator lost the race
        assert_eq!(
            put(&mut conn, &tree, "flat", path, 0, 0, b"x\n", None, 2).unwrap(),
            Put::Refused(Some(a.clone()))
        );
        let b = done(put(&mut conn, &tree, "navi", path, 4, 1, b"two\n", None, 3).unwrap());
        assert_eq!((b.size, b.version, b.base), (8, 2, 1));
        // an append that missed one
        assert!(matches!(
            put(&mut conn, &tree, "flat", path, 4, 1, b"three\n", None, 4).unwrap(),
            Put::Refused(Some(r)) if r.version == 2
        ));
        // a torn write is cut back before the next append
        OpenOptions::new()
            .append(true)
            .open(tree.file(path))
            .unwrap()
            .write_all(b"garbage")
            .unwrap();
        let c = done(put(&mut conn, &tree, "flat", path, 8, 2, b"three\n", None, 5).unwrap());
        assert_eq!(fs::read(tree.file(path)).unwrap(), b"one\ntwo\nthree\n");
        assert_eq!((c.base, c.host.as_str()), (1, "flat"));
        // replacing moves the base
        let d = done(
            put(
                &mut conn, &tree, "flat", path, 0, c.version, b"new\n", None, 6,
            )
            .unwrap(),
        );
        assert_eq!((d.size, d.base), (4, d.version));
        assert_eq!(fs::read(tree.file(path)).unwrap(), b"new\n");
        let (row, data) = read(&conn, &tree, path, 1).unwrap().unwrap();
        assert_eq!((row, data.as_slice()), (d.clone(), &b"ew\n"[..]));
        let page = changes(&conn, 0, 10).unwrap();
        assert_eq!(page.rows, vec![d]);
        assert!(changes(&conn, 4, 10).unwrap().rows.is_empty());
        let session: Option<String> = conn
            .query_row("SELECT session FROM sync_files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            session.as_deref(),
            Some("1ee78b00-59c7-4bf5-b462-ef39161f7d8d")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn compare_answers_at_the_smaller_size() {
        let (dir, mut conn, tree) = setup("compare");
        done(
            put(
                &mut conn,
                &tree,
                "navi",
                "plans/p.md",
                0,
                0,
                b"abcdef",
                None,
                1,
            )
            .unwrap(),
        );
        let local = dir.join("local");
        fs::write(&local, b"abc").unwrap();
        let probes = vec![
            Probe {
                path: "plans/p.md".into(),
                size: 3,
                fp: crate::sync::fp_of(&local, 3).unwrap(),
            },
            Probe {
                path: "plans/missing.md".into(),
                size: 1,
                fp: String::new(),
            },
        ];
        let out = compare(&conn, &tree, &probes).unwrap();
        assert_eq!(out[0].fp.as_deref(), Some(probes[0].fp.as_str()));
        assert_eq!(out[0].row.as_ref().map(|r| r.size), Some(6));
        assert!(out[1].row.is_none() && out[1].fp.is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_lines_merge_once() {
        let (dir, mut conn, _) = setup("history");
        let lines = vec!["{\"a\":1}".to_owned(), "{\"b\":2}".to_owned()];
        assert_eq!(add_history(&mut conn, "navi", &lines, 1).unwrap(), 2);
        assert_eq!(add_history(&mut conn, "flat", &lines, 2).unwrap(), 0);
        let page = history(&conn, 0, 1).unwrap();
        assert_eq!(page.lines, vec![(1, lines[0].clone())]);
        assert!(page.more);
        assert_eq!(history(&conn, 1, 10).unwrap().lines.len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    fn report(path: &str, copy: &str, size: u64) -> ConflictReport {
        ConflictReport {
            path: path.into(),
            size,
            copy: copy.into(),
            details: None,
            former_hub: false,
        }
    }

    #[test]
    fn conflicts_follow_the_machine_and_resolve() {
        let (dir, mut conn, _) = setup("conflicts");
        let sid = "1ee78b00-59c7-4bf5-b462-ef39161f7d8d";
        let t = format!("projects/-w/{sid}.jsonl");
        let a = report(&t, &format!("ccdash-conflicts/1/{t}"), 30);
        let id = add_conflict(&conn, "flat", &a, 5).unwrap();
        let open = conflicts(&conn).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].session.as_deref(), Some(sid));
        // hiding keeps it out of the list
        let b = report("plans/p.md", "ccdash-conflicts/1/plans/p.md", 3);
        let hidden = add_conflict(&conn, "flat", &b, 5).unwrap();
        assert!(dismiss(&conn, hidden, 6).unwrap());
        assert!(!dismiss(&conn, hidden, 7).unwrap());
        assert_eq!(conflicts(&conn).unwrap().len(), 1);

        // the machine reports what it holds: details land, an unknown copy
        // gets a record, the hidden one stays hidden
        let mut with = a.clone();
        with.details = Some(ConflictDetails {
            shared: 10,
            copy_lines: 2,
            ..ConflictDetails::default()
        });
        let c = report("tasks/t.json", "ccdash-conflicts/2/tasks/t.json", 1);
        let state = ConflictState {
            copies: vec![with, b.clone(), c],
        };
        conflict_state(&mut conn, "flat", &state, 8).unwrap();
        let open = conflicts(&conn).unwrap();
        assert_eq!(open.len(), 2, "{open:?}");
        let first = open.iter().find(|x| x.id == id).unwrap();
        assert_eq!(first.details.as_ref().map(|d| d.shared), Some(10));

        // using the copy: pending until the machine answers, then the copy is
        // the hub's former version and the conflict stays open
        assert!(request(&conn, id, Keep::Copy).unwrap().is_ok());
        assert!(
            request(&conn, id, Keep::Hub).unwrap().is_err(),
            "one at a time"
        );
        assert_eq!(conflicts(&conn).unwrap()[1].pending, Some(Keep::Copy));
        resolved(&conn, "flat", id, None, Some("ccdash-conflicts/3-hub/x"), 9).unwrap();
        let first = conflicts(&conn)
            .unwrap()
            .into_iter()
            .find(|x| x.id == id)
            .unwrap();
        assert!(first.former_hub && first.pending.is_none());
        assert_eq!(first.copy, "ccdash-conflicts/3-hub/x");
        // keeping the hub's version resolves it; a failure says why and stays
        assert!(request(&conn, id, Keep::Hub).unwrap().is_ok());
        resolved(&conn, "flat", id, Some("disk full"), None, 10).unwrap();
        let first = conflicts(&conn)
            .unwrap()
            .into_iter()
            .find(|x| x.id == id)
            .unwrap();
        assert_eq!(first.error.as_deref(), Some("disk full"));
        assert!(request(&conn, id, Keep::Hub).unwrap().is_ok());
        resolved(&conn, "flat", id, None, None, 11).unwrap();
        assert_eq!(conflicts(&conn).unwrap().len(), 1);
        // a copy deleted by hand resolves its conflict at the next report
        conflict_state(&mut conn, "flat", &ConflictState { copies: vec![] }, 12).unwrap();
        assert!(conflicts(&conn).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn growth_is_counted_per_project_and_day() {
        let (dir, mut conn, tree) = setup("growth");
        let t = "projects/-home-me-w-p/1ee78b00-59c7-4bf5-b462-ef39161f7d8d.jsonl";
        let day = 1_790_000_000_000; // a past day
        done(
            put(
                &mut conn,
                &tree,
                "navi",
                t,
                0,
                0,
                b"0123456789",
                Some(day),
                1,
            )
            .unwrap(),
        );
        let r = done(put(&mut conn, &tree, "navi", t, 10, 1, b"abc", None, 2).unwrap());
        done(
            put(
                &mut conn,
                &tree,
                "navi",
                "plans/p.md",
                0,
                0,
                b"plan",
                None,
                3,
            )
            .unwrap(),
        );
        // a rewrite that shrank counts negative
        done(put(&mut conn, &tree, "navi", t, 0, r.version, b"short", None, 4).unwrap());
        let rows: Vec<(String, String, i64, i64)> = conn
            .prepare("SELECT day, grp, bytes, files FROM sync_growth ORDER BY day, grp")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let today: String = conn
            .query_row(
                "SELECT date(?1 / 1000, 'unixepoch', 'localtime')",
                [2],
                |r| r.get(0),
            )
            .unwrap();
        let past: String = conn
            .query_row(
                "SELECT date(?1 / 1000, 'unixepoch', 'localtime')",
                [day],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            rows.contains(&(past, "projects/-home-me-w-p".into(), 10, 1)),
            "{rows:?}"
        );
        assert!(
            rows.contains(&(today.clone(), "projects/-home-me-w-p".into(), 3 - 8, 0)),
            "{rows:?}"
        );
        assert!(rows.contains(&(today, "plans".into(), 4, 1)), "{rows:?}");

        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, host TEXT, cwd TEXT, name TEXT, title TEXT);
            INSERT INTO sessions VALUES ('1ee78b00-59c7-4bf5-b462-ef39161f7d8d', 'navi', '/home/me/w/p', NULL, 'Work');",
        )
        .unwrap();
        done(
            put(
                &mut conn,
                &tree,
                "navi",
                "projects/-home-me-w-other/m.md",
                0,
                0,
                b"x",
                None,
                5,
            )
            .unwrap(),
        );
        let labels = HashMap::from([
            (
                format!("projects/{}", dir_name("/home/me/w/p")),
                "w/p".to_owned(),
            ),
            (
                format!("projects/{}", dir_name("/home/me/w/p/sub")),
                "w/p".to_owned(),
            ),
        ]);
        done(
            put(
                &mut conn,
                &tree,
                "navi",
                "projects/-home-me-w-p-sub/n.md",
                0,
                0,
                b"yy",
                None,
                6,
            )
            .unwrap(),
        );
        let stats = stats(&conn, &labels, &[(dir_name("/home/me/w"), "w".into())]).unwrap();
        // a directory no known session ran in is named after its workspace
        assert!(
            stats.projects.iter().any(|p| p.label == "w/other"),
            "{:?}",
            stats.projects
        );
        // a project's subdirectory counts as the project
        assert_eq!(
            stats.projects.iter().filter(|p| p.label == "w/p").count(),
            1
        );
        assert_eq!((stats.total.files, stats.total.bytes), (4, 12));
        assert_eq!(stats.projects[0].label, "w/p");
        assert_eq!(stats.projects[0].sessions, 1);
        assert_eq!(stats.sessions[0].title.as_deref(), Some("Work"));
        assert_eq!(stats.sessions[0].label.as_deref(), Some("w/p"));
        assert_eq!(
            stats
                .kinds
                .iter()
                .map(|k| k.key.as_str())
                .collect::<Vec<_>>(),
            ["projects", "plans"]
        );
        assert_eq!(group_of("file-history/s/a@v1"), "file-history");
        assert_eq!(group_of("projects/-w/s/subagents/a.jsonl"), "projects/-w");
        assert_eq!(
            dir_name("/home/me/Project Bundle/x.y"),
            "-home-me-Project-Bundle-x-y"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
