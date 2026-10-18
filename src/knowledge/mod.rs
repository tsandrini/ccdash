//! The knowledge layer: memories, issues, progress and documents as notes
//! in the hub's database, linked org-roam style (`[[slug]]`), searched
//! with FTS5 and scoped by workspace. Everything here runs on a database
//! connection; the hub calls it on the writer thread, the panel on a
//! reader. `tools` turns tool calls into operations and results into text.

pub mod tools;

use std::{collections::BTreeMap, fmt, path::Path};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::util::truncate;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS notes (
    id INTEGER PRIMARY KEY,
    workspace TEXT NOT NULL,
    slug TEXT NOT NULL,
    kind TEXT NOT NULL,
    project TEXT,
    title TEXT NOT NULL,
    summary TEXT,
    status TEXT NOT NULL DEFAULT 'active',
    stale_reason TEXT,
    version INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    created_by TEXT,
    updated_by TEXT,
    verified_at INTEGER,
    expires_at INTEGER,
    recalls INTEGER NOT NULL DEFAULT 0,
    recalled_at INTEGER,
    extra BLOB,
    body TEXT NOT NULL,
    UNIQUE (workspace, slug)
);
CREATE INDEX IF NOT EXISTS notes_ws_kind_status ON notes (workspace, kind, status, updated_at);
CREATE TABLE IF NOT EXISTS links (
    src INTEGER NOT NULL REFERENCES notes (id) ON DELETE CASCADE,
    dst TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'mentions',
    pos INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (src, dst, kind)
);
CREATE INDEX IF NOT EXISTS links_dst ON links (dst, kind);
CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
    title, summary, body,
    content='notes', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER IF NOT EXISTS notes_ai AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts (rowid, title, summary, body)
        VALUES (new.id, new.title, new.summary, new.body);
END;
CREATE TRIGGER IF NOT EXISTS notes_ad AFTER DELETE ON notes BEGIN
    INSERT INTO notes_fts (notes_fts, rowid, title, summary, body)
        VALUES ('delete', old.id, old.title, old.summary, old.body);
END;
CREATE TRIGGER IF NOT EXISTS notes_au AFTER UPDATE ON notes BEGIN
    INSERT INTO notes_fts (notes_fts, rowid, title, summary, body)
        VALUES ('delete', old.id, old.title, old.summary, old.body);
    INSERT INTO notes_fts (rowid, title, summary, body)
        VALUES (new.id, new.title, new.summary, new.body);
END;
";

const COLUMNS: &str =
    "workspace, slug, kind, project, title, summary, status, stale_reason, version,
    created_at, updated_at, created_by, updated_by, verified_at, expires_at, recalls, recalled_at,
    json(extra), body";

/// `COLUMNS` qualified with a table alias, for joins.
fn columns(alias: &str) -> String {
    COLUMNS
        .split(',')
        .map(|c| match c.trim() {
            "json(extra)" => format!("json({alias}.extra)"),
            c => format!("{alias}.{c}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}
const MAX_SLUG: usize = 80;
const MAX_TITLE: usize = 200;
const MAX_BODY: usize = 256 * 1024;
const EXCERPT_LEN: usize = 240;
const DAY_MS: i64 = 86_400_000;
/// Claude Code caps `additionalContext` at 10,000 characters.
const MAX_CONTEXT: usize = 9_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    Memory,
    Issue,
    Progress,
    Doc,
}

impl NoteKind {
    pub const ALL: [Self; 4] = [Self::Memory, Self::Issue, Self::Progress, Self::Doc];

    pub fn name(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Issue => "issue",
            Self::Progress => "progress",
            Self::Doc => "doc",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == s)
    }
}

impl fmt::Display for NoteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Stale,
    Superseded,
    Archived,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Stale => "stale",
            Self::Superseded => "superseded",
            Self::Archived => "archived",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Self::Active, Self::Stale, Self::Superseded, Self::Archived]
            .into_iter()
            .find(|k| k.name() == s)
    }

    /// Open issue, live memory: shown to sessions.
    pub fn live(self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Mentions,
    Supersedes,
    Relates,
    Blocks,
}

impl LinkKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Mentions => "mentions",
            Self::Supersedes => "supersedes",
            Self::Relates => "relates",
            Self::Blocks => "blocks",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "supersedes" => Self::Supersedes,
            "relates" => Self::Relates,
            "blocks" => Self::Blocks,
            _ => Self::Mentions,
        }
    }
}

/// A note as stored, body included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub workspace: String,
    pub slug: String,
    pub kind: NoteKind,
    pub project: Option<String>,
    pub title: String,
    /// One line for indexes and injections.
    pub summary: Option<String>,
    pub status: Status,
    pub stale_reason: Option<String>,
    pub version: u32,
    pub created_at: i64,
    pub updated_at: i64,
    pub created_by: Option<String>,
    pub updated_by: Option<String>,
    pub verified_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub recalls: u32,
    pub recalled_at: Option<i64>,
    /// Kind-specific fields: issues carry `priority`, `closed_at`,
    /// `closed_by`; progress carries `issue`; imports carry `source`.
    pub extra: Value,
    pub body: String,
}

impl Note {
    pub fn card(&self) -> Card {
        Card {
            workspace: self.workspace.clone(),
            slug: self.slug.clone(),
            kind: self.kind,
            project: self.project.clone(),
            title: self.title.clone(),
            summary: self.summary.clone(),
            status: self.status,
            stale_reason: self.stale_reason.clone(),
            version: self.version,
            updated_at: self.updated_at,
            verified_at: self.verified_at,
            recalls: self.recalls,
            priority: priority_of(&self.extra),
            excerpt: excerpt(&self.body),
        }
    }

    pub fn priority(&self) -> Priority {
        priority_of(&self.extra)
    }
}

/// A note in a list: everything but the body, plus its first lines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub workspace: String,
    pub slug: String,
    pub kind: NoteKind,
    pub project: Option<String>,
    pub title: String,
    pub summary: Option<String>,
    pub status: Status,
    pub stale_reason: Option<String>,
    pub version: u32,
    pub updated_at: i64,
    pub verified_at: Option<i64>,
    pub recalls: u32,
    pub priority: Priority,
    pub excerpt: String,
}

impl Card {
    /// The one line shown in indexes and injections.
    pub fn line(&self) -> &str {
        self.summary
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.excerpt)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
}

impl Priority {
    pub fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Self::Low),
            "normal" | "" => Some(Self::Normal),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

fn priority_of(extra: &Value) -> Priority {
    extra["priority"]
        .as_str()
        .and_then(Priority::parse)
        .unwrap_or(Priority::Normal)
}

/// A link as the panel shows it: the other end, resolved when it exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkRef {
    pub slug: String,
    pub kind: LinkKind,
    /// None for a `[[slug]]` that names no note yet.
    pub title: Option<String>,
    pub note_kind: Option<NoteKind>,
    pub status: Option<Status>,
}

/// A note with its neighbourhood.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteView {
    #[serde(flatten)]
    pub note: Note,
    pub links: Vec<LinkRef>,
    pub backlinks: Vec<LinkRef>,
}

/// A search hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    #[serde(flatten)]
    pub card: Card,
    /// Matching passage with `[`…`]` around the terms (FTS) or the excerpt.
    pub snippet: String,
}

/// What a write did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Written {
    pub slug: String,
    pub kind: NoteKind,
    pub title: String,
    pub status: Status,
    pub version: u32,
    pub created: bool,
    /// False when the content was already there (an idempotent rerun).
    pub changed: bool,
}

/// A full write of one note, as the importer and the tools produce it;
/// `upsert` keeps or replaces what is stored under the slug.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upsert {
    pub slug: String,
    pub kind: NoteKind,
    #[serde(default)]
    pub project: Option<String>,
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default = "default_status")]
    pub status: Status,
    #[serde(default = "empty_object")]
    pub extra: Value,
    pub body: String,
    /// Explicit links besides the `[[slug]]`s in the body.
    #[serde(default)]
    pub links: Vec<(String, LinkKind)>,
    /// Timestamps for imported notes; now when None.
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub updated_at: Option<i64>,
    /// Imported notes count as verified at import time.
    #[serde(default)]
    pub verified: bool,
}

fn default_status() -> Status {
    Status::Active
}

fn empty_object() -> Value {
    json!({})
}

/// What a session asks of the knowledge layer; the write half is durable
/// (queued while the hub is unreachable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum NoteOp {
    Remember {
        title: String,
        body: String,
        #[serde(default)]
        slug: Option<String>,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        summary: Option<String>,
        /// Retires that note with a `supersedes` link.
        #[serde(default)]
        supersedes: Option<String>,
        #[serde(default)]
        status: Option<Status>,
    },
    Verify {
        slug: String,
    },
    IssueAdd {
        title: String,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        priority: Option<Priority>,
    },
    IssueUpdate {
        slug: String,
        /// `open` or `closed`.
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        priority: Option<Priority>,
        /// Appended to the body with a date.
        #[serde(default)]
        comment: Option<String>,
    },
    Progress {
        text: String,
        #[serde(default)]
        issue: Option<String>,
        #[serde(default)]
        project: Option<String>,
    },
    DocWrite {
        slug: String,
        #[serde(default)]
        title: Option<String>,
        body: String,
        /// The version the edit was based on; required for an existing
        /// document, refused when it moved on.
        #[serde(default)]
        base_version: Option<u32>,
    },
}

/// A read: by slug, or a search.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Query {
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub kinds: Vec<NoteKind>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Lowercase ASCII words joined with `-`, at most `MAX_SLUG` bytes.
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut dash = true;
    for c in text.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && (c.is_whitespace() || c.is_ascii_punctuation() || !c.is_ascii()) {
            out.push('-');
            dash = true;
        }
        if out.len() >= MAX_SLUG {
            break;
        }
    }
    out.trim_end_matches('-').to_owned()
}

pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= MAX_SLUG
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && !slug.starts_with('.')
}

/// `[[slug]]` and `[[slug|label]]` targets in `body`, with their offsets.
pub fn wiki_links(body: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut rest = body;
    let mut base = 0;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };
        let inner = &after[..end];
        let target = inner.split('|').next().unwrap_or_default().trim();
        if !inner.contains('\n') && valid_slug(target) {
            out.push((target.to_owned(), base + start));
        }
        rest = &after[end + 2..];
        base += start + 2 + end + 2;
    }
    out
}

fn excerpt(body: &str) -> String {
    let text: String = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join(" ");
    truncate(&text, EXCERPT_LEN)
}

/// `YYYY-MM-DD` of a Unix millisecond timestamp (UTC).
pub fn ymd(ms: i64) -> String {
    let (y, m, d) = civil(ms.div_euclid(DAY_MS));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYYMMDD-HHMMSS` (UTC), for slugs.
fn stamp(ms: i64) -> String {
    let days = ms.div_euclid(DAY_MS);
    let secs = ms.rem_euclid(DAY_MS) / 1000;
    let (y, m, d) = civil(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

// NOTE: civil_from_days, http://howardhinnant.github.io/date_algorithms.html
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn check(up: &Upsert) -> Result<()> {
    if !valid_slug(&up.slug) {
        bail!(
            "{:?} is not a slug (lowercase words joined with '-')",
            up.slug
        );
    }
    if up.title.trim().is_empty() {
        bail!("the title is empty");
    }
    if up.title.chars().count() > MAX_TITLE {
        bail!("titles are at most {MAX_TITLE} characters");
    }
    if up.body.len() > MAX_BODY {
        bail!("bodies are at most {} KB", MAX_BODY / 1024);
    }
    if !up.extra.is_object() {
        bail!("extra must be an object");
    }
    Ok(())
}

fn row_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    let kind: String = r.get(2)?;
    let status: String = r.get(6)?;
    let extra: Option<String> = r.get(17)?;
    Ok(Note {
        workspace: r.get(0)?,
        slug: r.get(1)?,
        kind: NoteKind::parse(&kind).unwrap_or(NoteKind::Memory),
        project: r.get(3)?,
        title: r.get(4)?,
        summary: r.get(5)?,
        status: Status::parse(&status).unwrap_or(Status::Active),
        stale_reason: r.get(7)?,
        version: r.get::<_, i64>(8)? as u32,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
        created_by: r.get(11)?,
        updated_by: r.get(12)?,
        verified_at: r.get(13)?,
        expires_at: r.get(14)?,
        recalls: r.get::<_, i64>(15)? as u32,
        recalled_at: r.get(16)?,
        extra: extra
            .and_then(|e| serde_json::from_str(&e).ok())
            .unwrap_or_else(|| json!({})),
        body: r.get(18)?,
    })
}

pub fn get(conn: &Connection, workspace: &str, slug: &str) -> Result<Option<Note>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM notes WHERE workspace = ?1 AND slug = ?2"
        ))?
        .query_row(params![workspace, slug], row_note)
        .optional()?)
}

fn id_of(conn: &Connection, workspace: &str, slug: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT id FROM notes WHERE workspace = ?1 AND slug = ?2")?
        .query_row(params![workspace, slug], |r| r.get(0))
        .optional()?)
}

/// Stores `up` under its slug in `workspace`: creates the note, or
/// replaces its content (bumping the version) when something differs.
pub fn upsert(
    conn: &Connection,
    workspace: &str,
    up: &Upsert,
    by: Option<&str>,
    now: i64,
) -> Result<Written> {
    check(up)?;
    let existing = get(conn, workspace, &up.slug)?;
    let extra = serde_json::to_string(&up.extra)?;
    let (version, created, changed) = match &existing {
        None => {
            conn.execute(
                "INSERT INTO notes (workspace, slug, kind, project, title, summary, status,
                    version, created_at, updated_at, created_by, updated_by, verified_at,
                    extra, body)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?10, ?10, ?11, jsonb(?12), ?13)",
                params![
                    workspace,
                    up.slug,
                    up.kind.name(),
                    up.project,
                    up.title,
                    up.summary,
                    up.status.name(),
                    up.created_at.unwrap_or(now),
                    up.updated_at.or(up.created_at).unwrap_or(now),
                    by,
                    up.verified.then_some(now),
                    extra,
                    up.body,
                ],
            )?;
            (1, true, true)
        }
        Some(old) => {
            if old.kind != up.kind {
                bail!(
                    "{} is a {}, not a {}; pick another slug",
                    up.slug,
                    old.kind,
                    up.kind
                );
            }
            let same = old.title == up.title
                && old.summary == up.summary
                && old.body == up.body
                && old.status == up.status
                && old.project == up.project
                && old.extra == up.extra;
            if same {
                (old.version, false, false)
            } else {
                let version = old.version + 1;
                let stale_reason = if up.status == Status::Stale {
                    old.stale_reason.clone()
                } else {
                    None
                };
                conn.execute(
                    "UPDATE notes SET project = ?3, title = ?4, summary = ?5, status = ?6,
                        stale_reason = ?7, version = ?8, updated_at = ?9, updated_by = ?10,
                        verified_at = COALESCE(?11, verified_at), extra = jsonb(?12), body = ?13
                        WHERE workspace = ?1 AND slug = ?2",
                    params![
                        workspace,
                        up.slug,
                        up.project,
                        up.title,
                        up.summary,
                        up.status.name(),
                        stale_reason,
                        i64::from(version),
                        up.updated_at.unwrap_or(now),
                        by,
                        up.verified.then_some(now),
                        extra,
                        up.body,
                    ],
                )?;
                (version, false, true)
            }
        }
    };
    if changed {
        relink(conn, workspace, &up.slug, &up.body, &up.links)?;
    }
    Ok(Written {
        slug: up.slug.clone(),
        kind: up.kind,
        title: up.title.clone(),
        status: up.status,
        version,
        created,
        changed,
    })
}

/// Rewrites the links out of `slug`: the `[[…]]`s in the body plus `extra`.
fn relink(
    conn: &Connection,
    workspace: &str,
    slug: &str,
    body: &str,
    extra: &[(String, LinkKind)],
) -> Result<()> {
    let id = id_of(conn, workspace, slug)?.context("note vanished")?;
    conn.execute("DELETE FROM links WHERE src = ?1", [id])?;
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO links (src, dst, kind, pos) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (dst, pos) in wiki_links(body) {
        if dst != slug {
            stmt.execute(params![id, dst, LinkKind::Mentions.name(), pos as i64])?;
        }
    }
    for (dst, kind) in extra {
        if dst != slug && valid_slug(dst) {
            stmt.execute(params![id, dst, kind.name(), 0])?;
        }
    }
    Ok(())
}

/// Applies a session's write; `by` names the session for `created_by`.
pub fn apply(
    conn: &Connection,
    workspace: &str,
    project: Option<&str>,
    by: Option<&str>,
    op: &NoteOp,
    now: i64,
) -> Result<Written> {
    match op {
        NoteOp::Remember {
            title,
            body,
            slug,
            project: given,
            summary,
            supersedes,
            status,
        } => {
            let slug = match slug {
                Some(s) if !s.trim().is_empty() => s.trim().to_owned(),
                _ => slugify(title),
            };
            let old = get(conn, workspace, &slug)?;
            let mut up = Upsert {
                slug,
                kind: NoteKind::Memory,
                project: given.clone().or_else(|| project.map(str::to_owned)),
                title: title.trim().to_owned(),
                summary: summary.clone().map(|s| s.trim().to_owned()),
                status: status.unwrap_or(Status::Active),
                extra: old.as_ref().map_or_else(|| json!({}), |o| o.extra.clone()),
                body: body.trim().to_owned(),
                links: Vec::new(),
                created_at: None,
                updated_at: None,
                verified: true,
            };
            if let Some(old) = &old {
                // NOTE: a rewrite keeps the project and summary unless given
                if given.is_none() {
                    up.project = old.project.clone();
                }
                if summary.is_none() {
                    up.summary = old.summary.clone();
                }
            }
            if let Some(target) = supersedes
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                if get(conn, workspace, target)?.is_none() {
                    bail!("no note {target:?} to supersede");
                }
                up.links.push((target.to_owned(), LinkKind::Supersedes));
            }
            let written = upsert(conn, workspace, &up, by, now)?;
            if let Some((target, _)) = up.links.first() {
                conn.execute(
                    "UPDATE notes SET status = 'superseded', updated_at = ?3, updated_by = ?4
                        WHERE workspace = ?1 AND slug = ?2 AND status IN ('active', 'stale')",
                    params![workspace, target, now, by],
                )?;
            }
            Ok(written)
        }
        NoteOp::Verify { slug } => {
            let note = get(conn, workspace, slug)?
                .with_context(|| format!("no note {slug:?} in this workspace"))?;
            let status = match note.status {
                Status::Stale => Status::Active,
                other => other,
            };
            conn.execute(
                "UPDATE notes SET verified_at = ?3, status = ?4, stale_reason = NULL,
                    updated_by = ?5
                    WHERE workspace = ?1 AND slug = ?2",
                params![workspace, slug, now, status.name(), by],
            )?;
            Ok(Written {
                slug: slug.clone(),
                kind: note.kind,
                title: note.title,
                status,
                version: note.version,
                created: false,
                changed: note.status != status,
            })
        }
        NoteOp::IssueAdd {
            title,
            body,
            project: given,
            priority,
        } => {
            let project = given.clone().or_else(|| project.map(str::to_owned));
            let base = match &project {
                Some(p) => format!("{}--{}", slugify(p), slugify(title)),
                None => slugify(title),
            };
            let slug = free_slug(conn, workspace, &base)?;
            let up = Upsert {
                slug,
                kind: NoteKind::Issue,
                project,
                title: title.trim().to_owned(),
                summary: None,
                status: Status::Active,
                extra: json!({
                    "priority": priority.unwrap_or(Priority::Normal).name(),
                    "opened_by": by,
                }),
                body: body.clone().unwrap_or_default().trim().to_owned(),
                links: Vec::new(),
                created_at: None,
                updated_at: None,
                verified: false,
            };
            upsert(conn, workspace, &up, by, now)
        }
        NoteOp::IssueUpdate {
            slug,
            status,
            title,
            body,
            priority,
            comment,
        } => {
            let old = get(conn, workspace, slug)?
                .with_context(|| format!("no issue {slug:?} in this workspace"))?;
            if old.kind != NoteKind::Issue {
                bail!("{slug} is a {}, not an issue", old.kind);
            }
            let mut extra = old.extra.clone();
            if let Some(p) = priority {
                extra["priority"] = json!(p.name());
            }
            let status = match status.as_deref().map(str::trim) {
                None | Some("") => old.status,
                Some("open") | Some("active") => {
                    extra["closed_at"] = Value::Null;
                    extra["closed_by"] = Value::Null;
                    Status::Active
                }
                Some("closed") | Some("done") | Some("archived") => {
                    extra["closed_at"] = json!(now);
                    extra["closed_by"] = json!(by);
                    Status::Archived
                }
                Some(other) => bail!("status is open or closed, not {other:?}"),
            };
            if let Value::Object(map) = &mut extra {
                map.retain(|_, v| !v.is_null());
            }
            let mut text = body.clone().unwrap_or(old.body.clone());
            if let Some(c) = comment.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
                let who = by.map_or(String::new(), |b| format!(" ({b})"));
                text = format!("{}\n\n— {}{who}: {c}", text.trim_end(), ymd(now));
            }
            let up = Upsert {
                slug: slug.clone(),
                kind: NoteKind::Issue,
                project: old.project.clone(),
                title: title.clone().unwrap_or(old.title.clone()),
                summary: old.summary.clone(),
                status,
                extra,
                body: text,
                links: Vec::new(),
                created_at: None,
                updated_at: None,
                verified: false,
            };
            upsert(conn, workspace, &up, by, now)
        }
        NoteOp::Progress {
            text,
            issue,
            project: given,
        } => {
            let text = text.trim();
            if text.is_empty() {
                bail!("the progress text is empty");
            }
            let title = truncate(
                text.lines()
                    .next()
                    .unwrap_or_default()
                    .trim_matches(['-', '*', ' ']),
                100,
            );
            let issue = issue.as_deref().map(str::trim).filter(|i| !i.is_empty());
            let mut project = given.clone().or_else(|| project.map(str::to_owned));
            let mut links = Vec::new();
            if let Some(issue) = issue {
                let target = get(conn, workspace, issue)?
                    .with_context(|| format!("no note {issue:?} to log progress on"))?;
                if given.is_none() && target.project.is_some() {
                    project = target.project.clone();
                }
                links.push((issue.to_owned(), LinkKind::Relates));
            }
            let slug = free_slug(conn, workspace, &format!("progress--{}", stamp(now)))?;
            let up = Upsert {
                slug,
                kind: NoteKind::Progress,
                project,
                title,
                summary: None,
                status: Status::Active,
                extra: json!({ "issue": issue }),
                body: text.to_owned(),
                links,
                created_at: None,
                updated_at: None,
                verified: false,
            };
            upsert(conn, workspace, &up, by, now)
        }
        NoteOp::DocWrite {
            slug,
            title,
            body,
            base_version,
        } => {
            let old = get(conn, workspace, slug)?;
            if let Some(old) = &old {
                if old.kind != NoteKind::Doc {
                    bail!("{slug} is a {}, not a document", old.kind);
                }
                match base_version {
                    Some(v) if *v == old.version => {}
                    Some(v) => bail!(
                        "conflict: {slug} is at version {} (you based the edit on {v}); \
                            re-read it and retry",
                        old.version
                    ),
                    None => bail!(
                        "{slug} exists at version {}; pass base_version to update it",
                        old.version
                    ),
                }
            }
            let title = title
                .clone()
                .or_else(|| old.as_ref().map(|o| o.title.clone()))
                .or_else(|| heading(body))
                .unwrap_or_else(|| slug.clone());
            let up = Upsert {
                slug: slug.clone(),
                kind: NoteKind::Doc,
                project: old
                    .as_ref()
                    .map_or_else(|| project.map(str::to_owned), |o| o.project.clone()),
                title,
                summary: old.as_ref().and_then(|o| o.summary.clone()),
                status: Status::Active,
                extra: old.as_ref().map_or_else(|| json!({}), |o| o.extra.clone()),
                body: body.trim_end().to_owned(),
                links: Vec::new(),
                created_at: None,
                updated_at: None,
                verified: false,
            };
            upsert(conn, workspace, &up, by, now)
        }
    }
}

/// The first `# heading` of a markdown body.
pub fn heading(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("# "))
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
}

/// `base`, or `base-2`, `base-3`… when taken.
fn free_slug(conn: &Connection, workspace: &str, base: &str) -> Result<String> {
    let base = if base.is_empty() { "note" } else { base };
    if id_of(conn, workspace, base)?.is_none() {
        return Ok(base.to_owned());
    }
    for n in 2..1000 {
        let candidate = format!("{base}-{n}");
        if id_of(conn, workspace, &candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    bail!("too many notes named {base}")
}

/// A note with its links and backlinks resolved.
pub fn view(conn: &Connection, workspace: &str, slug: &str) -> Result<Option<NoteView>> {
    let Some(note) = get(conn, workspace, slug)? else {
        return Ok(None);
    };
    let links = conn
        .prepare_cached(
            "SELECT l.dst, l.kind, n.title, n.kind, n.status
                FROM links l JOIN notes s ON s.id = l.src
                LEFT JOIN notes n ON n.workspace = s.workspace AND n.slug = l.dst
                WHERE s.workspace = ?1 AND s.slug = ?2 ORDER BY l.pos, l.dst",
        )?
        .query_map(params![workspace, slug], link_ref)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let backlinks = conn
        .prepare_cached(
            "SELECT s.slug, l.kind, s.title, s.kind, s.status
                FROM links l JOIN notes s ON s.id = l.src
                WHERE s.workspace = ?1 AND l.dst = ?2 ORDER BY s.updated_at DESC",
        )?
        .query_map(params![workspace, slug], link_ref)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(NoteView {
        note,
        links,
        backlinks,
    }))
}

fn link_ref(r: &rusqlite::Row<'_>) -> rusqlite::Result<LinkRef> {
    let kind: String = r.get(1)?;
    let note_kind: Option<String> = r.get(3)?;
    let status: Option<String> = r.get(4)?;
    Ok(LinkRef {
        slug: r.get(0)?,
        kind: LinkKind::parse(&kind),
        title: r.get(2)?,
        note_kind: note_kind.as_deref().and_then(NoteKind::parse),
        status: status.as_deref().and_then(Status::parse),
    })
}

/// Which notes a list covers.
#[derive(Debug, Clone, Default)]
pub struct Filter<'a> {
    pub kinds: &'a [NoteKind],
    pub statuses: &'a [Status],
    pub project: Option<&'a str>,
    pub limit: usize,
}

pub fn list(conn: &Connection, workspace: &str, filter: &Filter<'_>) -> Result<Vec<Card>> {
    let kinds = serde_json::to_string(&filter.kinds.iter().map(|k| k.name()).collect::<Vec<_>>())?;
    let statuses =
        serde_json::to_string(&filter.statuses.iter().map(|s| s.name()).collect::<Vec<_>>())?;
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM notes
            WHERE workspace = :ws
            AND (:kinds = '[]' OR kind IN (SELECT value FROM json_each(:kinds)))
            AND (:statuses = '[]' OR status IN (SELECT value FROM json_each(:statuses)))
            AND (:project IS NULL OR project IS :project)
            ORDER BY updated_at DESC LIMIT :limit"
    ))?;
    let notes = stmt
        .query_map(
            rusqlite::named_params! {
                ":ws": workspace,
                ":kinds": kinds,
                ":statuses": statuses,
                ":project": filter.project,
                ":limit": if filter.limit == 0 { 10_000 } else { filter.limit as i64 },
            },
            row_note,
        )?
        .collect::<rusqlite::Result<Vec<Note>>>()?;
    Ok(notes.iter().map(Note::card).collect())
}

/// The live notes of a workspace, bodies included: what collectors cache.
pub fn active(conn: &Connection, workspace: &str) -> Result<Vec<Note>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM notes WHERE workspace = ?1 AND status IN ('active', 'stale')
                ORDER BY updated_at DESC"
        ))?
        .query_map([workspace], row_note)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// An FTS5 query for free text: every word a quoted prefix term, ORed, so
/// a natural phrase finds notes with some of its words and bm25 ranks the
/// ones with more of them first.
fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

/// Full-text search in a workspace, best first; stale notes rank as they
/// match but say so, retired ones are left out.
pub fn search(conn: &Connection, workspace: &str, query: &Query) -> Result<Vec<Hit>> {
    let limit = query.limit.unwrap_or(10).clamp(1, 50) as i64;
    let kinds = serde_json::to_string(&query.kinds.iter().map(|k| k.name()).collect::<Vec<_>>())?;
    let Some(match_expr) = query.text.as_deref().and_then(fts_query) else {
        let cards = list(
            conn,
            workspace,
            &Filter {
                kinds: &query.kinds,
                statuses: &[Status::Active, Status::Stale],
                project: query.project.as_deref(),
                limit: limit as usize,
            },
        )?;
        return Ok(cards
            .into_iter()
            .map(|card| Hit {
                snippet: card.excerpt.clone(),
                card,
            })
            .collect());
    };
    let sql = format!(
        "SELECT {cols}, snippet(notes_fts, 2, '[', ']', '…', 20)
            FROM notes_fts JOIN notes ON notes.id = notes_fts.rowid
            WHERE notes_fts MATCH :q AND notes.workspace = :ws
            AND notes.status IN ('active', 'stale')
            AND (:kinds = '[]' OR notes.kind IN (SELECT value FROM json_each(:kinds)))
            AND (:project IS NULL OR notes.project IS :project)
            ORDER BY bm25(notes_fts, 5.0, 3.0, 1.0) LIMIT :limit",
        cols = columns("notes")
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(
        rusqlite::named_params! {
            ":q": match_expr,
            ":ws": workspace,
            ":kinds": kinds,
            ":project": query.project,
            ":limit": limit,
        },
        |r| Ok((row_note(r)?, r.get::<_, String>(19)?)),
    )?;
    let mut hits = Vec::new();
    for row in rows {
        let (note, snippet) = row?;
        let card = note.card();
        let snippet = if snippet.trim().is_empty() {
            card.excerpt.clone()
        } else {
            snippet.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        hits.push(Hit { card, snippet });
    }
    Ok(hits)
}

/// Counts a recall of the notes `slugs` name.
pub fn recalled(conn: &Connection, workspace: &str, slugs: &[String], now: i64) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "UPDATE notes SET recalls = recalls + 1, recalled_at = ?3
            WHERE workspace = ?1 AND slug = ?2",
    )?;
    for slug in slugs {
        stmt.execute(params![workspace, slug, now])?;
    }
    Ok(())
}

/// Scores a note against free text for the offline cache: words found in
/// the title weigh most, then the summary, then the body.
pub fn score(card: &Card, body: &str, text: &str) -> u32 {
    let words: Vec<String> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .filter(|w| w.len() > 1)
        .map(|w| w.to_lowercase())
        .collect();
    if words.is_empty() {
        return 0;
    }
    let (title, summary, body) = (
        card.title.to_lowercase(),
        card.summary.clone().unwrap_or_default().to_lowercase(),
        body.to_lowercase(),
    );
    let mut score = 0;
    for w in &words {
        if title.contains(w.as_str()) || card.slug.contains(w.as_str()) {
            score += 5;
        }
        if summary.contains(w.as_str()) {
            score += 3;
        }
        if body.contains(w.as_str()) {
            score += 1;
        }
    }
    score
}

/// Picks from the cached notes what `search` would find.
pub fn search_cached(notes: &[Note], query: &Query) -> Vec<Hit> {
    let limit = query.limit.unwrap_or(10).clamp(1, 50);
    let mut scored: Vec<(u32, &Note)> = notes
        .iter()
        .filter(|n| matches!(n.status, Status::Active | Status::Stale))
        .filter(|n| query.kinds.is_empty() || query.kinds.contains(&n.kind))
        .filter(|n| {
            query
                .project
                .as_deref()
                .is_none_or(|p| n.project.as_deref() == Some(p))
        })
        .filter_map(|n| match &query.text {
            Some(text) => {
                let s = score(&n.card(), &n.body, text);
                (s > 0).then_some((s, n))
            }
            None => Some((0, n)),
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.updated_at.cmp(&a.1.updated_at)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, n)| {
            let card = n.card();
            Hit {
                snippet: card.excerpt.clone(),
                card,
            }
        })
        .collect()
}

/// What a session gets at its start: open issues, the memories that fit
/// the project, documents and recent progress. None when the workspace
/// holds nothing live.
pub fn session_start(
    cards: &[Card],
    workspace: &str,
    project: Option<&str>,
    now: i64,
) -> Option<String> {
    let live: Vec<&Card> = cards.iter().filter(|c| c.status.live()).collect();
    if live.is_empty() {
        return None;
    }
    let fits = |c: &Card| c.project.is_none() || c.project.as_deref() == project;
    let mut issues: Vec<&Card> = live
        .iter()
        .copied()
        .filter(|c| c.kind == NoteKind::Issue && fits(c))
        .collect();
    issues.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then((b.project.is_some()).cmp(&a.project.is_some()))
            .then(b.updated_at.cmp(&a.updated_at))
    });
    let mut memories: Vec<&Card> = live
        .iter()
        .copied()
        .filter(|c| c.kind == NoteKind::Memory && fits(c))
        .collect();
    memories.sort_by(|a, b| {
        (b.project.is_some())
            .cmp(&a.project.is_some())
            .then(b.recalls.cmp(&a.recalls))
            .then(b.updated_at.cmp(&a.updated_at))
    });
    let mut docs: Vec<&Card> = live
        .iter()
        .copied()
        .filter(|c| c.kind == NoteKind::Doc && fits(c))
        .collect();
    docs.sort_by(|a, b| {
        (b.project.is_some())
            .cmp(&a.project.is_some())
            .then(b.updated_at.cmp(&a.updated_at))
    });
    let mut progress: Vec<&Card> = live
        .iter()
        .copied()
        .filter(|c| c.kind == NoteKind::Progress && fits(c) && now - c.updated_at < 14 * DAY_MS)
        .collect();
    progress.sort_by_key(|a| std::cmp::Reverse(a.updated_at));

    let scope = match project {
        Some(p) => format!("{workspace} / {p}"),
        None => workspace.to_owned(),
    };
    let mut out = format!(
        "## ccdash knowledge: {scope}\n\
            The hub holds this workspace's memories, issues, progress and docs. Tools: \
            `recall` (a slug for the full note, or a search), `remember`, `verify`, \
            `issue_add`, `issue_update`, `progress`, `doc_write`; between sessions \
            `claim` / `release` (what you are working on, so others keep clear) and \
            `tell_session` (a message handed to another session at its next prompt). \
            Memory files are frozen: remember through the tool, verify memories you \
            confirm, file issues and log progress as you go.\n"
    );
    let section = |title: &str, items: &[&Card], max: usize, out: &mut String| {
        if items.is_empty() {
            return;
        }
        let shown = items.len().min(max);
        let more = items.len() - shown;
        out.push_str(&format!("\n### {title} ({})\n", items.len()));
        for c in &items[..shown] {
            let tag = match c.kind {
                NoteKind::Issue if c.priority != Priority::Normal => {
                    format!("[{}] ", c.priority.name())
                }
                NoteKind::Progress => format!("{} ", ymd(c.updated_at)),
                _ => String::new(),
            };
            // NOTE: without a summary an issue's excerpt repeats its title
            let line = match c.kind {
                NoteKind::Memory | NoteKind::Doc => c.line(),
                _ => c.summary.as_deref().unwrap_or_default(),
            };
            let line = if line.is_empty() {
                String::new()
            } else {
                format!(" — {}", truncate(line, 200))
            };
            out.push_str(&format!("- {tag}{} (`{}`){line}\n", c.title, c.slug));
        }
        if more > 0 {
            out.push_str(&format!("- …and {more} more; `recall` lists them\n"));
        }
    };
    section("Open issues", &issues, 15, &mut out);
    section("Memories", &memories, 12, &mut out);
    section("Documents", &docs, 10, &mut out);
    section("Recent progress", &progress, 5, &mut out);
    Some(truncate(&out, MAX_CONTEXT))
}

/// Notes changed since `since`, for a session that missed them.
pub fn changed_since<'a>(cards: &'a [Card], project: Option<&str>, since: i64) -> Vec<&'a Card> {
    let mut changed: Vec<&Card> = cards
        .iter()
        .filter(|c| c.updated_at > since)
        .filter(|c| c.project.is_none() || c.project.as_deref() == project)
        .collect();
    changed.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
    changed
}

/// Someone else's recent edit of a file, for the PreToolUse injection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentEdit {
    pub session: String,
    pub host: String,
    pub title: Option<String>,
    pub at: i64,
}

/// Other sessions that edited `path` (absolute) in the last day, newest
/// first, from the stored tool events.
pub fn recent_edits(
    conn: &Connection,
    session: &str,
    path: &str,
    now: i64,
) -> Result<Vec<RecentEdit>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.session, s.host, s.title, max(e.ts)
            FROM events e JOIN sessions s ON s.id = e.session
            WHERE e.kind = 'tool' AND e.ts > ?2 AND e.session != ?1
            AND e.data ->> 'name' IN ('Edit', 'Write', 'MultiEdit', 'NotebookEdit')
            AND (CASE WHEN substr(e.data ->> 'summary', 1, 1) = '/' THEN e.data ->> 'summary'
                ELSE s.cwd || '/' || (e.data ->> 'summary') END) = ?3
            GROUP BY e.session ORDER BY max(e.ts) DESC LIMIT 5",
    )?;
    Ok(stmt
        .query_map(params![session, now - DAY_MS, path], |r| {
            Ok(RecentEdit {
                session: r.get(0)?,
                host: r.get(1)?,
                title: r.get(2)?,
                at: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The nightly lifecycle pass: memories unverified for `unverified_days`
/// or not recalled for `unrecalled_days` turn stale (still searchable,
/// no longer injected); notes an active note supersedes retire. Returns
/// what changed as `(workspace, slug, reason)`.
pub fn reflect(
    conn: &Connection,
    now: i64,
    unverified_days: u32,
    unrecalled_days: u32,
) -> Result<Vec<(String, String, String)>> {
    let mut flagged = Vec::new();
    let mut flag = |sql: &str, cutoff: i64, reason: &str| -> Result<()> {
        let rows: Vec<(String, String)> = conn
            .prepare(sql)?
            .query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (ws, slug) in rows {
            conn.execute(
                "UPDATE notes SET status = 'stale', stale_reason = ?3
                    WHERE workspace = ?1 AND slug = ?2 AND status = 'active'",
                params![ws, slug, reason],
            )?;
            flagged.push((ws, slug, reason.to_owned()));
        }
        Ok(())
    };
    if unverified_days > 0 {
        flag(
            "SELECT workspace, slug FROM notes WHERE kind = 'memory' AND status = 'active'
                AND COALESCE(verified_at, created_at) < ?1",
            now - i64::from(unverified_days) * DAY_MS,
            &format!("not verified in {unverified_days} days"),
        )?;
    }
    if unrecalled_days > 0 {
        flag(
            "SELECT workspace, slug FROM notes WHERE kind = 'memory' AND status = 'active'
                AND COALESCE(recalled_at, verified_at, created_at) < ?1",
            now - i64::from(unrecalled_days) * DAY_MS,
            &format!("not recalled in {unrecalled_days} days"),
        )?;
    }
    let superseded: Vec<(String, String)> = conn
        .prepare(
            "SELECT n.workspace, n.slug FROM links l
                JOIN notes s ON s.id = l.src AND s.status IN ('active', 'stale')
                JOIN notes n ON n.workspace = s.workspace AND n.slug = l.dst
                WHERE l.kind = 'supersedes' AND n.status IN ('active', 'stale')",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (ws, slug) in superseded {
        conn.execute(
            "UPDATE notes SET status = 'superseded', updated_at = ?3 WHERE workspace = ?1 AND slug = ?2",
            params![ws, slug, now],
        )?;
        flagged.push((ws, slug, "superseded".to_owned()));
    }
    conn.execute_batch("INSERT INTO notes_fts (notes_fts) VALUES ('integrity-check')")
        .or_else(|_| conn.execute_batch("INSERT INTO notes_fts (notes_fts) VALUES ('rebuild')"))?;
    Ok(flagged)
}

/// Markdown with `[[slug]]` links as `note:` URLs, for the panel.
pub fn render_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) if !after[..end].contains('\n') => {
                let inner = &after[..end];
                let (target, label) = match inner.split_once('|') {
                    Some((t, l)) => (t.trim(), l.trim()),
                    None => (inner.trim(), inner.trim()),
                };
                if valid_slug(target) {
                    out.push_str(&format!("[{label}](note:{target})"));
                } else {
                    out.push_str(&rest[start..start + 2 + end + 2]);
                }
                rest = &after[end + 2..];
            }
            _ => {
                out.push_str("[[");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    crate::markdown::render_notes(&out)
}

/// Writes every note as markdown with frontmatter under `dir`:
/// `<workspace>/<kind>/<slug>.md`. Returns how many files it wrote.
pub fn export(conn: &Connection, dir: &Path) -> Result<usize> {
    let notes = conn
        .prepare(&format!(
            "SELECT {COLUMNS} FROM notes ORDER BY workspace, kind, slug"
        ))?
        .query_map([], row_note)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut n = 0;
    for note in &notes {
        let folder = dir.join(&note.workspace).join(note.kind.name());
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join(format!("{}.md", note.slug)), frontmatter(note))?;
        n += 1;
    }
    Ok(n)
}

/// The note as a markdown file: YAML-ish frontmatter the importer reads
/// back, then the body.
pub fn frontmatter(note: &Note) -> String {
    let mut head = BTreeMap::new();
    head.insert("name", json!(note.slug));
    head.insert("title", json!(note.title));
    head.insert("kind", json!(note.kind.name()));
    head.insert("status", json!(note.status.name()));
    if let Some(p) = &note.project {
        head.insert("project", json!(p));
    }
    if let Some(s) = &note.summary {
        head.insert("description", json!(s));
    }
    head.insert("version", json!(note.version));
    head.insert("created", json!(ymd(note.created_at)));
    head.insert("updated", json!(ymd(note.updated_at)));
    if let Some(v) = note.verified_at {
        head.insert("verified", json!(ymd(v)));
    }
    let mut out = String::from("---\n");
    for (k, v) in head {
        let text = match v {
            Value::String(s) => yaml_string(&s),
            other => other.to_string(),
        };
        out.push_str(&format!("{k}: {text}\n"));
    }
    if note.extra.as_object().is_some_and(|m| !m.is_empty()) {
        out.push_str(&format!("extra: {}\n", note.extra));
    }
    out.push_str("---\n\n");
    out.push_str(&note.body);
    out.push('\n');
    out
}

fn yaml_string(s: &str) -> String {
    if s.is_empty()
        || s.contains([
            '#', ':', '"', '\'', '\n', '[', ']', '{', '}', '|', '>', '&', '*',
        ])
        || s.starts_with([' ', '-', '?', '!', '%', '@', '`'])
        || s.ends_with(' ')
    {
        serde_json::to_string(s).unwrap_or_default()
    } else {
        s.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn remember(title: &str, body: &str) -> NoteOp {
        NoteOp::Remember {
            title: title.into(),
            body: body.into(),
            slug: None,
            project: None,
            summary: None,
            supersedes: None,
            status: None,
        }
    }

    #[test]
    fn slugs_links_and_dates() {
        assert_eq!(slugify("Hello, Wörld! 42"), "hello-w-rld-42");
        assert_eq!(slugify("  --x--  "), "x");
        assert!(valid_slug("ccdash--project") && !valid_slug("Bad Slug"));
        assert_eq!(
            wiki_links("see [[a-b]] and [[c|label]] not [[Bad One]] or [[x\ny]]"),
            vec![("a-b".to_owned(), 4), ("c".to_owned(), 16)]
        );
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_792_025_999_823), "2026-10-15");
        assert_eq!(stamp(1_792_025_999_823), "20261015-005959");
    }

    #[test]
    fn remember_recall_verify_and_supersede() {
        let conn = db();
        let now = 1_792_000_000_000;
        let w = apply(
            &conn,
            "ws",
            Some("repo"),
            Some("s1"),
            &remember("Font pick", "Young Serif, see [[other-note]]"),
            now,
        )
        .unwrap();
        assert!(w.created && w.changed && w.version == 1);
        assert_eq!(w.slug, "font-pick");
        let again = apply(
            &conn,
            "ws",
            Some("repo"),
            Some("s1"),
            &remember("Font pick", "Young Serif, see [[other-note]]"),
            now + 1,
        )
        .unwrap();
        assert!(!again.created && !again.changed && again.version == 1);
        let edited = apply(
            &conn,
            "ws",
            None,
            Some("s2"),
            &remember("Font pick", "Newsreader"),
            now + 2,
        )
        .unwrap();
        assert_eq!(edited.version, 2);
        let note = get(&conn, "ws", "font-pick").unwrap().unwrap();
        assert_eq!(
            note.project.as_deref(),
            Some("repo"),
            "a rewrite keeps the project"
        );
        assert_eq!(note.updated_by.as_deref(), Some("s2"));

        let v = view(&conn, "ws", "font-pick").unwrap().unwrap();
        assert!(v.links.is_empty(), "the link went with the old body");
        apply(&conn, "ws", None, None, &remember("Other note", "x"), now).unwrap();
        apply(
            &conn,
            "ws",
            None,
            None,
            &remember("Third", "mentions [[font-pick]]"),
            now + 3,
        )
        .unwrap();
        let v = view(&conn, "ws", "font-pick").unwrap().unwrap();
        assert_eq!(v.backlinks.len(), 1);
        assert_eq!(v.backlinks[0].slug, "third");
        assert_eq!(v.backlinks[0].title.as_deref(), Some("Third"));
        let t = view(&conn, "ws", "third").unwrap().unwrap();
        assert_eq!(t.links[0].title.as_deref(), Some("Font pick"));

        assert!(
            get(&conn, "other-ws", "font-pick").unwrap().is_none(),
            "scoped"
        );

        let hits = search(
            &conn,
            "ws",
            &Query {
                text: Some("newsreader".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].card.slug, "font-pick");
        assert!(
            hits[0].snippet.contains("[Newsreader]"),
            "{}",
            hits[0].snippet
        );
        assert!(
            search(
                &conn,
                "other-ws",
                &Query {
                    text: Some("newsreader".into()),
                    ..Default::default()
                }
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            search(
                &conn,
                "ws",
                &Query {
                    text: Some("(((".into()),
                    ..Default::default()
                }
            )
            .unwrap()
            .len()
                == 3,
            "junk falls back to a list"
        );
        recalled(&conn, "ws", &["font-pick".into()], now + 4).unwrap();
        assert_eq!(get(&conn, "ws", "font-pick").unwrap().unwrap().recalls, 1);

        conn.execute(
            "UPDATE notes SET status = 'stale', stale_reason = 'old' WHERE slug = 'font-pick'",
            [],
        )
        .unwrap();
        let v = apply(
            &conn,
            "ws",
            None,
            Some("s3"),
            &NoteOp::Verify {
                slug: "font-pick".into(),
            },
            now + 5,
        )
        .unwrap();
        assert_eq!(v.status, Status::Active);
        let note = get(&conn, "ws", "font-pick").unwrap().unwrap();
        assert_eq!((note.verified_at, note.stale_reason), (Some(now + 5), None));
        assert!(
            apply(
                &conn,
                "ws",
                None,
                None,
                &NoteOp::Verify {
                    slug: "nope".into()
                },
                now
            )
            .is_err()
        );

        let op = NoteOp::Remember {
            title: "Font pick 2".into(),
            body: "Replaces".into(),
            slug: None,
            project: None,
            summary: Some("the new pick".into()),
            supersedes: Some("font-pick".into()),
            status: None,
        };
        apply(&conn, "ws", None, None, &op, now + 6).unwrap();
        assert_eq!(
            get(&conn, "ws", "font-pick").unwrap().unwrap().status,
            Status::Superseded
        );
        let v = view(&conn, "ws", "font-pick-2").unwrap().unwrap();
        assert_eq!(v.links[0].kind, LinkKind::Supersedes);
        assert_eq!(active(&conn, "ws").unwrap().len(), 3);
    }

    #[test]
    fn issues_progress_and_docs() {
        let conn = db();
        let now = 1_792_000_000_000;
        let add = NoteOp::IssueAdd {
            title: "Fix the thing".into(),
            body: Some("details".into()),
            project: None,
            priority: Some(Priority::High),
        };
        let w = apply(&conn, "ws", Some("ccdash"), Some("s1"), &add, now).unwrap();
        assert_eq!(w.slug, "ccdash--fix-the-thing");
        let w2 = apply(&conn, "ws", Some("ccdash"), Some("s1"), &add, now).unwrap();
        assert_eq!(w2.slug, "ccdash--fix-the-thing-2", "same title, new issue");
        let note = get(&conn, "ws", &w.slug).unwrap().unwrap();
        assert_eq!(note.priority(), Priority::High);
        assert_eq!(note.project.as_deref(), Some("ccdash"));

        let p = NoteOp::Progress {
            text: "- did half of it\nmore".into(),
            issue: Some(w.slug.clone()),
            project: None,
        };
        let pw = apply(&conn, "ws", None, Some("s1"), &p, now + 1000).unwrap();
        assert!(pw.slug.starts_with("progress--"));
        assert_eq!(pw.title, "did half of it");
        let v = view(&conn, "ws", &w.slug).unwrap().unwrap();
        assert_eq!(v.backlinks[0].kind, LinkKind::Relates);
        assert_eq!(
            get(&conn, "ws", &pw.slug)
                .unwrap()
                .unwrap()
                .project
                .as_deref(),
            Some("ccdash")
        );

        let close = NoteOp::IssueUpdate {
            slug: w.slug.clone(),
            status: Some("closed".into()),
            title: None,
            body: None,
            priority: None,
            comment: Some("done in abc123".into()),
        };
        let c = apply(&conn, "ws", None, Some("s2"), &close, now + 2000).unwrap();
        assert_eq!(c.status, Status::Archived);
        let note = get(&conn, "ws", &w.slug).unwrap().unwrap();
        assert!(note.body.ends_with("(s2): done in abc123"));
        assert_eq!(note.extra["closed_by"], "s2");
        let reopen = NoteOp::IssueUpdate {
            slug: w.slug.clone(),
            status: Some("open".into()),
            title: None,
            body: None,
            priority: Some(Priority::Low),
            comment: None,
        };
        apply(&conn, "ws", None, None, &reopen, now + 3000).unwrap();
        let note = get(&conn, "ws", &w.slug).unwrap().unwrap();
        assert_eq!(
            (note.status, note.priority()),
            (Status::Active, Priority::Low)
        );
        assert!(note.extra.get("closed_by").is_none());
        assert!(
            apply(
                &conn,
                "ws",
                None,
                None,
                &NoteOp::IssueUpdate {
                    slug: pw.slug.clone(),
                    status: None,
                    title: None,
                    body: None,
                    priority: None,
                    comment: None
                },
                now
            )
            .is_err(),
            "not an issue"
        );

        let doc = |body: &str, base: Option<u32>| NoteOp::DocWrite {
            slug: "plan".into(),
            title: None,
            body: body.into(),
            base_version: base,
        };
        let d = apply(
            &conn,
            "ws",
            Some("ccdash"),
            None,
            &doc("# The plan\n\nstep 1", None),
            now,
        )
        .unwrap();
        assert_eq!((d.version, d.title.as_str()), (1, "The plan"));
        assert!(
            apply(
                &conn,
                "ws",
                None,
                None,
                &doc("# The plan\n\nstep 2", None),
                now
            )
            .is_err(),
            "needs base_version"
        );
        let e = apply(
            &conn,
            "ws",
            None,
            None,
            &doc("# The plan\n\nstep 2", Some(0)),
            now,
        )
        .unwrap_err();
        assert!(e.to_string().contains("conflict"), "{e}");
        let d2 = apply(
            &conn,
            "ws",
            None,
            None,
            &doc("# The plan\n\nstep 2", Some(1)),
            now,
        )
        .unwrap();
        assert_eq!(d2.version, 2);
        let same = apply(
            &conn,
            "ws",
            None,
            None,
            &doc("# The plan\n\nstep 2", Some(2)),
            now,
        )
        .unwrap();
        assert!(!same.changed && same.version == 2);

        let cards = list(
            &conn,
            "ws",
            &Filter {
                kinds: &[NoteKind::Issue],
                statuses: &[Status::Active],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cards.len(), 2);
        let ctx = session_start(&cards_of(&conn, "ws"), "ws", Some("ccdash"), now + 4000).unwrap();
        assert!(
            ctx.contains("### Open issues (2)") && ctx.contains("[low]"),
            "{ctx}"
        );
        assert!(ctx.contains("### Documents (1)") && ctx.contains("### Recent progress (1)"));
        assert!(
            session_start(&cards_of(&conn, "ws"), "ws", Some("elsewhere"), now)
                .is_none_or(|c| !c.contains("Open issues"))
        );
        assert!(session_start(&[], "ws", None, now).is_none());
    }

    fn cards_of(conn: &Connection, ws: &str) -> Vec<Card> {
        active(conn, ws).unwrap().iter().map(Note::card).collect()
    }

    #[test]
    fn reflect_flags_old_memories_and_retires_superseded() {
        let conn = db();
        let day = DAY_MS;
        let now = 200 * day;
        apply(&conn, "ws", None, None, &remember("Fresh", "x"), now - day).unwrap();
        apply(
            &conn,
            "ws",
            None,
            None,
            &remember("Old", "x"),
            now - 100 * day,
        )
        .unwrap();
        apply(
            &conn,
            "ws",
            None,
            None,
            &remember("Forgotten", "x"),
            now - 70 * day,
        )
        .unwrap();
        conn.execute(
            "UPDATE notes SET verified_at = ?1 WHERE slug = 'forgotten'",
            [now - day],
        )
        .unwrap();
        conn.execute(
            "UPDATE notes SET recalled_at = ?1 WHERE slug = 'forgotten'",
            [now - 61 * day],
        )
        .unwrap();
        let flagged = reflect(&conn, now, 90, 60).unwrap();
        let mut slugs: Vec<&str> = flagged.iter().map(|(_, s, _)| s.as_str()).collect();
        slugs.sort_unstable();
        assert_eq!(slugs, ["forgotten", "old"]);
        assert_eq!(
            get(&conn, "ws", "fresh").unwrap().unwrap().status,
            Status::Active
        );
        let old = get(&conn, "ws", "old").unwrap().unwrap();
        assert_eq!(old.stale_reason.as_deref(), Some("not verified in 90 days"));
        assert!(
            reflect(&conn, now, 90, 60).unwrap().is_empty(),
            "idempotent"
        );
        let hits = search(
            &conn,
            "ws",
            &Query {
                text: Some("x".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 3, "stale notes stay searchable");
        let ctx = session_start(&cards_of(&conn, "ws"), "ws", None, now).unwrap();
        assert!(ctx.contains("`fresh`") && !ctx.contains("`old`"), "{ctx}");
    }

    #[test]
    fn upsert_is_idempotent_and_exports() {
        let conn = db();
        let up = Upsert {
            slug: "a--b".into(),
            kind: NoteKind::Memory,
            project: Some("a".into()),
            title: "A: b".into(),
            summary: Some("one line".into()),
            status: Status::Active,
            extra: json!({"source": "import"}),
            body: "body [[c]]".into(),
            links: vec![("d".into(), LinkKind::Relates)],
            created_at: Some(5),
            updated_at: Some(6),
            verified: true,
        };
        let w = upsert(&conn, "ws", &up, None, 100).unwrap();
        assert!(w.created);
        let w = upsert(&conn, "ws", &up, None, 101).unwrap();
        assert!(!w.created && !w.changed);
        let note = get(&conn, "ws", "a--b").unwrap().unwrap();
        assert_eq!(
            (note.created_at, note.updated_at, note.verified_at),
            (5, 6, Some(100))
        );
        let v = view(&conn, "ws", "a--b").unwrap().unwrap();
        assert_eq!(v.links.len(), 2);
        assert!(
            v.links
                .iter()
                .any(|l| l.slug == "d" && l.kind == LinkKind::Relates && l.title.is_none())
        );
        let bad = Upsert {
            slug: "Not Ok".into(),
            ..up.clone()
        };
        assert!(upsert(&conn, "ws", &bad, None, 1).is_err());
        let other = Upsert {
            kind: NoteKind::Doc,
            ..up.clone()
        };
        assert!(upsert(&conn, "ws", &other, None, 1).is_err(), "kind clash");

        let dir = std::env::temp_dir().join(format!("ccdash-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(export(&conn, &dir).unwrap(), 1);
        let text = std::fs::read_to_string(dir.join("ws/memory/a--b.md")).unwrap();
        assert!(text.starts_with("---\ncreated: 1970-01-01\n"), "{text}");
        assert!(text.contains("description: one line\n") && text.contains("title: \"A: b\"\n"));
        assert!(text.ends_with("---\n\nbody [[c]]\n"));
        std::fs::remove_dir_all(dir).unwrap();

        let html = render_body("see [[a--b|the note]] and [[nope!]] x");
        assert!(
            html.contains(r#"<a href="note:a--b">the note</a>"#),
            "{html}"
        );
        assert!(html.contains("[[nope!]]"));
    }

    #[test]
    fn cached_search_scores_titles_first() {
        let n = |slug: &str, title: &str, body: &str| Note {
            workspace: "ws".into(),
            slug: slug.into(),
            kind: NoteKind::Memory,
            project: None,
            title: title.into(),
            summary: None,
            status: Status::Active,
            stale_reason: None,
            version: 1,
            created_at: 0,
            updated_at: 0,
            created_by: None,
            updated_by: None,
            verified_at: None,
            expires_at: None,
            recalls: 0,
            recalled_at: None,
            extra: json!({}),
            body: body.into(),
        };
        let notes = [
            n("a", "Nginx vhost", "proxy"),
            n("b", "Backups", "nginx logs"),
            n("c", "Other", "x"),
        ];
        let hits = search_cached(
            &notes,
            &Query {
                text: Some("nginx".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            hits.iter()
                .map(|h| h.card.slug.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(search_cached(&notes, &Query::default()).len(), 3);
        let cards: Vec<Card> = notes.iter().map(Note::card).collect();
        assert!(changed_since(&cards, None, 0).is_empty());
    }

    #[test]
    fn recent_edits_match_absolute_paths() {
        let conn = db();
        conn.execute("INSERT INTO sessions (id, host, cwd, title, kind, started_at) VALUES ('s1', 'navi', '/w/repo', 'Other work', 'interactive', 0)", []).unwrap();
        conn.execute("INSERT INTO sessions (id, host, cwd, kind, started_at) VALUES ('s2', 'flat', '/elsewhere', 'interactive', 0)", []).unwrap();
        let now = 10 * DAY_MS;
        for (ts, session, summary) in [
            (now - 1000, "s1", "src/a.rs"),
            (now - 2000, "s2", "/w/repo/src/a.rs"),
            (now - 3000, "s1", "src/b.rs"),
            (now - 2 * DAY_MS, "s2", "/w/repo/src/a.rs"),
        ] {
            conn.execute(
                "INSERT INTO events (ts, host, session, kind, data) VALUES (?1, 'h', ?2, 'tool', ?3)",
                params![ts, session, json!({"name": "Edit", "summary": summary, "ok": true}).to_string()],
            ).unwrap();
        }
        let edits = recent_edits(&conn, "me", "/w/repo/src/a.rs", now).unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(
            (edits[0].session.as_str(), edits[0].title.as_deref()),
            ("s1", Some("Other work"))
        );
        assert_eq!(edits[1].host, "flat");
        assert!(
            recent_edits(&conn, "s1", "/w/repo/src/b.rs", now)
                .unwrap()
                .is_empty(),
            "own edits don't count"
        );
    }
}
