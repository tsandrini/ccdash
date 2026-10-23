//! The hub's side: the file tree under `sync.dir`, its index in the hub
//! database, and the `/sync/` routes collectors call with their own
//! tokens. Every read and write runs on the store's writer thread, so a
//! reader never sees a file between its write and its index row.

use std::{
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
    BODY_LIMIT, CHUNK, Changes, ConflictReport, HistoryPage, Probe, Probed, Refused, Row,
    SyncConflict, fp, line_digest, session_of, valid_path,
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
    dismissed_at INTEGER
);
";

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
/// appends, at 0 it replaces; anything else is refused.
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
    Ok(Put::Done(new))
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
        "INSERT INTO sync_conflicts (path, host, size, copy, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![c.path, host, i64::try_from(c.size)?, c.copy, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Conflicts not dismissed yet, newest first.
///
/// # Errors
///
/// The index cannot be read.
pub fn conflicts(conn: &Connection) -> Result<Vec<SyncConflict>> {
    Ok(conn
        .prepare_cached(
            "SELECT id, path, host, size, copy, at FROM sync_conflicts
            WHERE dismissed_at IS NULL ORDER BY at DESC LIMIT 200",
        )?
        .query_map([], |r| {
            Ok(SyncConflict {
                id: r.get(0)?,
                path: r.get(1)?,
                host: r.get(2)?,
                size: r.get::<_, i64>(3)?.try_into().unwrap_or(0),
                copy: r.get(4)?,
                at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Hides a conflict from the panel; false when there is no such one open.
///
/// # Errors
///
/// The index cannot be written.
pub fn dismiss(conn: &Connection, id: i64, now: i64) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE sync_conflicts SET dismissed_at = ?2 WHERE id = ?1 AND dismissed_at IS NULL",
        params![id, now],
    )? > 0)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let a = done(put(&mut conn, &tree, "navi", path, 0, 0, b"one\n", 1).unwrap());
        assert_eq!((a.size, a.version, a.base), (4, 1, 1));
        // a second creator lost the race
        assert_eq!(
            put(&mut conn, &tree, "flat", path, 0, 0, b"x\n", 2).unwrap(),
            Put::Refused(Some(a.clone()))
        );
        let b = done(put(&mut conn, &tree, "navi", path, 4, 1, b"two\n", 3).unwrap());
        assert_eq!((b.size, b.version, b.base), (8, 2, 1));
        // an append that missed one
        assert!(matches!(
            put(&mut conn, &tree, "flat", path, 4, 1, b"three\n", 4).unwrap(),
            Put::Refused(Some(r)) if r.version == 2
        ));
        // a torn write is cut back before the next append
        OpenOptions::new()
            .append(true)
            .open(tree.file(path))
            .unwrap()
            .write_all(b"garbage")
            .unwrap();
        let c = done(put(&mut conn, &tree, "flat", path, 8, 2, b"three\n", 5).unwrap());
        assert_eq!(fs::read(tree.file(path)).unwrap(), b"one\ntwo\nthree\n");
        assert_eq!((c.base, c.host.as_str()), (1, "flat"));
        // replacing moves the base
        let d = done(put(&mut conn, &tree, "flat", path, 0, c.version, b"new\n", 6).unwrap());
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
        done(put(&mut conn, &tree, "navi", "plans/p.md", 0, 0, b"abcdef", 1).unwrap());
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
        let report = ConflictReport {
            path: "plans/p.md".into(),
            size: 3,
            copy: "ccdash-conflicts/1/plans/p.md".into(),
        };
        let id = add_conflict(&conn, "flat", &report, 5).unwrap();
        assert_eq!(conflicts(&conn).unwrap().len(), 1);
        assert!(dismiss(&conn, id, 6).unwrap());
        assert!(!dismiss(&conn, id, 7).unwrap());
        assert!(conflicts(&conn).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
}
