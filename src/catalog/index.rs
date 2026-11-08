//! The indexer: a thread of its own (niced, so a first build on the Pi
//! never competes with the hub's work) that compares the sync index with
//! the catalog and reads what changed. Woken by the sync's change counter,
//! at most every `SETTLE`, and every `SAFETY` regardless.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{atomic::Ordering, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use super::{
    Catalog,
    parse::{self, Chunk, MAX_PREVIEW},
    writer,
};
use crate::util::truncate;

/// Changes are let settle this long before a pass.
const SETTLE: Duration = Duration::from_secs(5);
/// A pass runs at least this often, changes or not.
const SAFETY: Duration = Duration::from_secs(600);
/// Bytes read from a transcript at a time.
const BLOCK: usize = 8 << 20;
/// A line longer than this is skipped (no transcript line Claude Code
/// writes for a prompt or a reply comes near it).
const MAX_LINE: usize = 64 << 20;
/// Files kept per session (the most edited).
const MAX_FILES: usize = 50;

/// Starts the indexer; it runs until the process ends.
///
/// `tree` is the sync's file tree (`sync.dir`), `changed` its counter of
/// accepted writes.
pub fn spawn(catalog: Catalog, tree: PathBuf, mut changed: watch::Receiver<u64>) {
    let (tx, rx) = mpsc::channel::<()>();
    tokio::spawn(async move {
        while changed.changed().await.is_ok() {
            if tx.send(()).is_err() {
                return;
            }
        }
    });
    let started = thread::Builder::new()
        .name("catalog".into())
        .spawn(move || run(&catalog, &tree, &rx));
    if let Err(e) = started {
        warn!("catalog: cannot start the indexer: {e}");
    }
}

fn run(catalog: &Catalog, tree: &Path, rx: &mpsc::Receiver<()>) {
    // SAFETY: setpriority on this thread's own id, no memory involved
    #[cfg(unix)]
    unsafe {
        let tid = libc::gettid();
        libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, 10);
    }
    let mut conn = match writer(catalog.path()) {
        Ok(conn) => conn,
        Err(e) => {
            warn!("catalog: {e:#}");
            return;
        }
    };
    let mut failed = HashSet::new();
    loop {
        let start = Instant::now();
        match pass(&mut conn, catalog, tree, &mut failed) {
            Ok(0) => {}
            Ok(n) => info!(
                "catalog: read {n} transcripts in {:.1} s",
                start.elapsed().as_secs_f32()
            ),
            Err(e) => warn!("catalog: {e:#}"),
        }
        catalog.progress.passes.fetch_add(1, Ordering::Relaxed);
        match rx.recv_timeout(SAFETY) {
            Ok(()) => {
                thread::sleep(SETTLE);
                while rx.try_recv().is_ok() {}
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// A transcript as the sync index has it.
#[derive(Debug)]
struct Synced {
    path: String,
    size: u64,
    base: i64,
    host: String,
}

/// What failed to read, as the sync index had it: not tried again until
/// the file changes.
type Failed = HashSet<(String, i64, u64)>;

/// Reads every transcript the catalog is behind on; returns how many.
fn pass(
    conn: &mut Connection,
    catalog: &Catalog,
    tree: &Path,
    failed: &mut Failed,
) -> Result<usize> {
    let store = Connection::open_with_flags(
        catalog.store_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("opening the store")?;
    store.busy_timeout(Duration::from_secs(5))?;
    let synced = transcripts(&store)?;
    let known: HashMap<String, (i64, u64)> = conn
        .prepare("SELECT path, base, size FROM catalog")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get(1)?, r.get::<_, i64>(2)?.try_into().unwrap_or(0)),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let behind: Vec<&Synced> = synced
        .iter()
        .filter(|s| known.get(&s.path) != Some(&(s.base, s.size)))
        .filter(|s| !failed.contains(&(s.path.clone(), s.base, s.size)))
        .collect();
    catalog
        .progress
        .pending
        .store(behind.len() as u64, Ordering::Relaxed);
    let mut read = 0;
    for s in behind {
        match index_file(conn, &store, tree, s) {
            Ok(()) => read += 1,
            Err(e) => {
                warn!("catalog: {}: {e:#}", s.path);
                failed.insert((s.path.clone(), s.base, s.size));
            }
        }
        catalog.progress.pending.fetch_sub(1, Ordering::Relaxed);
    }
    Ok(read)
}

/// The main transcripts the hub holds: `projects/<dir>/<uuid>.jsonl`
/// (subagents' files sit deeper).
fn transcripts(store: &Connection) -> Result<Vec<Synced>> {
    let mut stmt = store.prepare(
        "SELECT path, size, base, host FROM sync_files
            WHERE path LIKE 'projects/%.jsonl' AND session IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Synced {
            path: r.get(0)?,
            size: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
            base: r.get(2)?,
            host: r.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        if is_main(&row.path) {
            out.push(row);
        }
    }
    Ok(out)
}

fn is_main(path: &str) -> bool {
    path.split('/').count() == 3
        && crate::sync::session_of(path).is_some_and(|id| path.ends_with(&format!("/{id}.jsonl")))
}

/// What the catalog keeps of a session, as loaded for an update.
#[derive(Debug, Default)]
struct Entry {
    n: Option<i64>,
    offset: u64,
    cwd: Option<String>,
    first_at: Option<i64>,
    last_at: Option<i64>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    summary: Option<String>,
    recap: Option<String>,
    first_prompt: Option<(i64, String)>,
    last_prompt: Option<(i64, String)>,
    last_reply: Option<(i64, String)>,
    prompts: i64,
    replies: i64,
    tools: i64,
    active_ms: i64,
    compactions: i64,
    models: BTreeMap<String, i64>,
    files: BTreeMap<String, i64>,
    branch: Option<String>,
    version: Option<String>,
    entrypoint: Option<String>,
    cost: Option<parse::Cost>,
    continued_in: Option<String>,
    host: Option<String>,
}

impl Entry {
    fn load(tx: &Transaction<'_>, path: &str) -> Result<Option<(Self, i64)>> {
        Ok(tx
            .query_row(
                "SELECT n, offset, cwd, first_at, last_at, custom_title, ai_title, summary, recap,
                    first_prompt_at, first_prompt, last_prompt_at, last_prompt, last_reply_at,
                    last_reply, prompts, replies, tools, active_ms, compactions, models, files,
                    branch, version, entrypoint, cost_usd, lines_added, lines_removed,
                    continued_in, host, base
                FROM catalog WHERE path = ?1",
                [path],
                |r| {
                    let pair = |at: usize, text: usize| -> rusqlite::Result<_> {
                        Ok(match (r.get::<_, Option<i64>>(at)?, r.get(text)?) {
                            (Some(at), Some(text)) => Some((at, text)),
                            _ => None,
                        })
                    };
                    let json = |i: usize| -> rusqlite::Result<BTreeMap<String, i64>> {
                        Ok(serde_json::from_str(&r.get::<_, String>(i)?).unwrap_or_default())
                    };
                    Ok((
                        Self {
                            n: Some(r.get(0)?),
                            offset: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
                            cwd: r.get(2)?,
                            first_at: r.get(3)?,
                            last_at: r.get(4)?,
                            custom_title: r.get(5)?,
                            ai_title: r.get(6)?,
                            summary: r.get(7)?,
                            recap: r.get(8)?,
                            first_prompt: pair(9, 10)?,
                            last_prompt: pair(11, 12)?,
                            last_reply: pair(13, 14)?,
                            prompts: r.get(15)?,
                            replies: r.get(16)?,
                            tools: r.get(17)?,
                            active_ms: r.get(18)?,
                            compactions: r.get(19)?,
                            models: json(20)?,
                            files: json(21)?,
                            branch: r.get(22)?,
                            version: r.get(23)?,
                            entrypoint: r.get(24)?,
                            cost: r.get::<_, Option<f64>>(25)?.map(|usd| parse::Cost {
                                usd,
                                lines_added: r
                                    .get::<_, Option<i64>>(26)
                                    .ok()
                                    .flatten()
                                    .unwrap_or(0),
                                lines_removed: r
                                    .get::<_, Option<i64>>(27)
                                    .ok()
                                    .flatten()
                                    .unwrap_or(0),
                            }),
                            continued_in: r.get(28)?,
                            host: r.get(29)?,
                        },
                        r.get(30)?,
                    ))
                },
            )
            .optional()?)
    }

    /// Adds what `c` says.
    fn merge(&mut self, c: Chunk) {
        let preview = |t: &parse::Text| (t.at, truncate(&t.text, MAX_PREVIEW));
        if self.first_prompt.is_none() {
            self.first_prompt = c
                .prompts()
                .find(|t| !parse::bare_command(&t.text))
                .map(preview);
        }
        if let Some(last) = c.prompts().last() {
            self.last_prompt = Some(preview(last));
        }
        if let Some(last) = c.replies().last() {
            self.last_reply = Some(preview(last));
        }
        self.prompts += c.prompts().count() as i64;
        self.replies += c.replies().count() as i64;
        if self.cwd.is_none() {
            self.cwd = c.cwd;
        }
        if self.first_at.is_none() {
            self.first_at = c.first_at;
        }
        self.last_at = self.last_at.max(c.last_at);
        keep(&mut self.custom_title, c.custom_title);
        keep(&mut self.ai_title, c.ai_title);
        keep(&mut self.summary, c.summary);
        keep(&mut self.recap, c.recap);
        self.tools += c.tools;
        self.active_ms += c.active_ms;
        self.compactions += c.compactions;
        for (model, n) in c.models {
            *self.models.entry(model).or_default() += n;
        }
        for (file, n) in c.files {
            *self.files.entry(file).or_default() += n;
        }
        if self.files.len() > MAX_FILES {
            let mut by_count: Vec<_> = std::mem::take(&mut self.files).into_iter().collect();
            by_count.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            by_count.truncate(MAX_FILES);
            self.files = by_count.into_iter().collect();
        }
        keep(&mut self.branch, c.branch);
        keep(&mut self.version, c.version);
        if self.entrypoint.is_none() {
            self.entrypoint = c.entrypoint;
        }
        if c.cost.is_some() {
            self.cost = c.cost;
        }
        keep(&mut self.continued_in, c.continued_in);
    }

    fn title(&self) -> Option<&str> {
        self.custom_title
            .as_deref()
            .or(self.ai_title.as_deref())
            .or(self.summary.as_deref())
    }
}

fn keep(slot: &mut Option<String>, value: Option<String>) {
    if value.is_some() {
        *slot = value;
    }
}

/// Reads what `s` holds past the catalog's offset (all of it when the
/// file is new to the catalog, was replaced or shrank) and stores it.
fn index_file(conn: &mut Connection, store: &Connection, tree: &Path, s: &Synced) -> Result<()> {
    let id = crate::sync::session_of(&s.path)
        .context("not a session's transcript")?
        .to_owned();
    let tx = conn.transaction()?;
    let (mut entry, fresh) = match Entry::load(&tx, &s.path)? {
        Some((entry, base)) if base == s.base && s.size >= entry.offset => (entry, false),
        Some((old, _)) => {
            let n = old.n;
            tx.execute("DELETE FROM catalog_texts WHERE session = ?1", [n])?;
            tx.execute("DELETE FROM catalog_hosts WHERE session = ?1", [n])?;
            (
                Entry {
                    n,
                    ..Entry::default()
                },
                true,
            )
        }
        None => (Entry::default(), true),
    };
    // NOTE: the first chunk ran where the store first saw the session, when it did; the hub took a backfilled copy from whichever machine came first
    let host = if fresh {
        store
            .query_row("SELECT host FROM sessions WHERE id = ?1", [&id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .unwrap_or_else(|| s.host.clone())
    } else {
        s.host.clone()
    };
    let mut texts = Vec::new();
    let consumed = read_from(&tree.join(&s.path), entry.offset, s.size, |chunk| {
        texts.extend(chunk.texts.iter().cloned());
        entry.merge(chunk);
    })?;
    entry.offset += consumed;
    if !texts.is_empty() || entry.host.is_none() {
        entry.host = Some(host.clone());
    }
    let n = save(&tx, &entry, &s.path, &id, s)?;
    if !texts.is_empty() {
        tx.execute(
            "INSERT OR IGNORE INTO catalog_hosts (session, host) VALUES (?1, ?2)",
            params![n, host],
        )?;
    }
    let mut insert = tx.prepare_cached(
        "INSERT INTO catalog_texts (session, at, role, text) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for t in &texts {
        insert.execute(params![n, t.at, t.role.as_str(), t.text])?;
    }
    drop(insert);
    tx.execute("DELETE FROM catalog_titles WHERE rowid = ?1", [n])?;
    if let Some(title) = entry.title() {
        tx.execute(
            "INSERT INTO catalog_titles (rowid, title) VALUES (?1, ?2)",
            params![n, title],
        )?;
    }
    tx.commit()?;
    debug!(
        "catalog: {} +{} texts, at {} of {}",
        s.path,
        texts.len(),
        entry.offset,
        s.size
    );
    Ok(())
}

fn save(tx: &Transaction<'_>, e: &Entry, path: &str, id: &str, s: &Synced) -> Result<i64> {
    let json = |m: &BTreeMap<String, i64>| serde_json::to_string(m).unwrap_or_else(|_| "{}".into());
    let split = |p: &Option<(i64, String)>| match p {
        Some((at, text)) => (Some(*at), Some(text.clone())),
        None => (None, None),
    };
    let (first_prompt_at, first_prompt) = split(&e.first_prompt);
    let (last_prompt_at, last_prompt) = split(&e.last_prompt);
    let (last_reply_at, last_reply) = split(&e.last_reply);
    Ok(tx
        .prepare_cached(
            "INSERT INTO catalog (n, path, id, base, size, offset, cwd, first_at, last_at,
                custom_title, ai_title, summary, recap, first_prompt, first_prompt_at,
                last_prompt, last_prompt_at, last_reply, last_reply_at, prompts, replies, tools,
                active_ms, compactions, models, files, branch, version, entrypoint, cost_usd,
                lines_added, lines_removed, continued_in, host)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33,
                ?34)
            ON CONFLICT (path) DO UPDATE SET id = excluded.id, base = excluded.base,
                size = excluded.size, offset = excluded.offset, cwd = excluded.cwd,
                first_at = excluded.first_at, last_at = excluded.last_at,
                custom_title = excluded.custom_title, ai_title = excluded.ai_title,
                summary = excluded.summary, recap = excluded.recap,
                first_prompt = excluded.first_prompt, first_prompt_at = excluded.first_prompt_at,
                last_prompt = excluded.last_prompt, last_prompt_at = excluded.last_prompt_at,
                last_reply = excluded.last_reply, last_reply_at = excluded.last_reply_at,
                prompts = excluded.prompts, replies = excluded.replies, tools = excluded.tools,
                active_ms = excluded.active_ms, compactions = excluded.compactions,
                models = excluded.models, files = excluded.files, branch = excluded.branch,
                version = excluded.version, entrypoint = excluded.entrypoint,
                cost_usd = excluded.cost_usd, lines_added = excluded.lines_added,
                lines_removed = excluded.lines_removed, continued_in = excluded.continued_in,
                host = excluded.host
            RETURNING n",
        )?
        .query_row(
            params![
                e.n,
                path,
                id,
                s.base,
                i64::try_from(s.size)?,
                i64::try_from(e.offset)?,
                e.cwd,
                e.first_at,
                e.last_at,
                e.custom_title,
                e.ai_title,
                e.summary,
                e.recap,
                first_prompt,
                first_prompt_at,
                last_prompt,
                last_prompt_at,
                last_reply,
                last_reply_at,
                e.prompts,
                e.replies,
                e.tools,
                e.active_ms,
                e.compactions,
                json(&e.models),
                json(&e.files),
                e.branch,
                e.version,
                e.entrypoint,
                e.cost.map(|c| c.usd),
                e.cost.map(|c| c.lines_added),
                e.cost.map(|c| c.lines_removed),
                e.continued_in,
                e.host,
            ],
            |r| r.get(0),
        )?)
}

/// Feeds the complete lines of `file` between `from` and `to` to `f`,
/// a block at a time; returns the bytes they took.
fn read_from(file: &Path, from: u64, to: u64, mut f: impl FnMut(Chunk)) -> Result<u64> {
    if to <= from {
        return Ok(0);
    }
    let mut file = File::open(file).with_context(|| format!("opening {}", file.display()))?;
    file.seek(SeekFrom::Start(from))?;
    let mut left = file.take(to - from);
    let mut buf: Vec<u8> = Vec::with_capacity(BLOCK.min(usize::try_from(to - from)?));
    let mut consumed = 0u64;
    // NOTE: a line past MAX_LINE is dropped whole, up to its newline
    let mut skipping = false;
    loop {
        let start = buf.len();
        buf.resize(start + BLOCK, 0);
        let got = left.read(&mut buf[start..])?;
        buf.truncate(start + got);
        if skipping {
            match buf.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    buf.drain(..=i);
                    consumed += i as u64 + 1;
                    skipping = false;
                }
                None => {
                    consumed += buf.len() as u64;
                    buf.clear();
                }
            }
        }
        if !skipping {
            let (chunk, used) = parse::chunk(&buf);
            if used > 0 {
                f(chunk);
                buf.drain(..used);
                consumed += used as u64;
            } else if buf.len() > MAX_LINE {
                consumed += buf.len() as u64;
                buf.clear();
                skipping = true;
            }
        }
        if got == 0 {
            return Ok(consumed);
        }
    }
}

/// One pass, as the indexer thread runs it.
#[cfg(test)]
pub(super) fn pass_now(catalog: &Catalog, tree: &Path) -> usize {
    let mut conn = writer(catalog.path()).unwrap();
    pass(&mut conn, catalog, tree, &mut HashSet::new()).unwrap()
}
