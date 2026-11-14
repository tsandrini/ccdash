//! Reading the catalog: the list of past sessions with its filters, facet
//! counts and full-text search; one session's overview; its conversation.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{markdown, util::truncate, workspace::Workspaces};

/// Bytes of the first prompt, the last prompt and the last reply on a card.
const CARD_TEXT: usize = 400;
/// Bytes of each prompt in the overview's outline.
const OUTLINE_TEXT: usize = 600;
/// Prompts the outline lists at most.
const MAX_OUTLINE: usize = 500;
/// Matches shown per card.
const SNIPPETS: usize = 2;
/// Matches listed in the overview.
const DETAIL_MATCHES: usize = 40;
/// Files listed in the overview.
const FILES: usize = 12;
/// Tokens around a match in a snippet.
const SNIPPET_TOKENS: i64 = 18;
/// Facet key of the sessions outside every workspace.
pub const OUTSIDE: &str = "-";

/// How the list is ordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "types.ts", rename = "PastSort")]
pub enum Sort {
    /// The best match first (only with a search; else as `Recent`).
    #[default]
    Best,
    /// Last active first.
    Recent,
    /// Started last first.
    Started,
    /// Most prompts first.
    Prompts,
    /// Largest transcript first.
    Size,
    /// Most expensive first.
    Cost,
}

/// What the list shows.
#[derive(Debug, Default)]
pub struct Query {
    pub text: Option<String>,
    /// Sessions any of these machines wrote to; empty is every machine.
    pub hosts: Vec<String>,
    /// The scope's workspaces; empty is every session, outside ones too.
    pub workspaces: Vec<String>,
    /// A [`ProjectCount::key`].
    pub project: Option<String>,
    /// Sessions active since then.
    pub since: Option<i64>,
    pub sort: Sort,
    pub offset: usize,
    pub limit: usize,
}

/// A past session as the list shows it.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct PastSession {
    pub id: String,
    pub cwd: String,
    pub workspace: Option<String>,
    pub project: Option<String>,
    /// The machine that wrote to it last: where it last ran.
    pub host: String,
    /// Every machine that wrote to it.
    pub hosts: Vec<String>,
    /// Its title: renamed, else Claude Code's, else an older summary.
    pub title: Option<String>,
    /// Claude Code's recap ("We're … Next: …"), when it wrote one.
    pub recap: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub last_reply: Option<String>,
    pub first_at: i64,
    pub last_at: i64,
    /// Bytes of the transcript.
    pub size: i64,
    pub prompts: i64,
    pub replies: i64,
    pub tools: i64,
    /// Time Claude worked on its turns, where Claude Code recorded it.
    pub active_ms: i64,
    pub compactions: i64,
    /// The model most of its replies came from.
    pub model: Option<String>,
    pub branch: Option<String>,
    pub version: Option<String>,
    /// `cli`, `sdk-cli` (headless), …
    pub entrypoint: Option<String>,
    pub cost_usd: Option<f64>,
    pub lines_added: Option<i64>,
    pub lines_removed: Option<i64>,
    /// The session it was continued in (a copy under a new id): Claude
    /// Code's mark in the transcript, else the link the hub made when one
    /// of its resumes came back as a copy.
    pub continued_in: Option<String>,
    /// It runs now: go to it rather than resume it.
    pub open: bool,
    /// The store has it, so its page has the whole chat.
    pub known: bool,
    /// Where the search matched, best first.
    pub snippets: Vec<Snippet>,
}

/// A match: the text around it with `\u{2}` before and `\u{3}` after each
/// matched word.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Snippet {
    /// The text's id, for [`conversation`]'s `around`.
    pub text: i64,
    pub at: i64,
    /// `you` or `claude`; `title` for the title.
    pub role: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct HostCount {
    pub host: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct ProjectCount {
    /// `workspace/project`, `workspace/` for the workspace itself, `-` for
    /// sessions outside every workspace.
    pub key: String,
    pub workspace: Option<String>,
    pub project: Option<String>,
    pub count: usize,
}

/// Counts for the filters, each under every other filter.
#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Facets {
    pub hosts: Vec<HostCount>,
    pub projects: Vec<ProjectCount>,
}

/// A page of the list.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts", rename = "PastPage")]
pub struct Page {
    pub sessions: Vec<PastSession>,
    /// Sessions matching, over every page.
    pub total: usize,
    /// `all` when every word matched, `any` when no session had all of
    /// them and some had some; none without a search.
    pub matched: Option<String>,
    pub facets: Facets,
    /// Sessions in the catalog.
    pub indexed: usize,
    /// Transcripts the indexer has still to read.
    pub pending: u64,
    /// The indexer finished a pass since the hub started.
    pub ready: bool,
}

/// A prompt or a reply.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct PastText {
    pub id: i64,
    pub at: i64,
    pub text: String,
}

/// One session's overview.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct PastDetail {
    pub session: PastSession,
    /// The first prompt in full.
    pub first_prompt: Option<PastText>,
    /// The last prompt in full, when it is not the first.
    pub last_prompt: Option<PastText>,
    /// Claude's last text, rendered.
    pub last_reply_html: Option<String>,
    pub last_reply_at: Option<i64>,
    /// Every prompt, cut to a few lines each.
    pub prompts: Vec<PastText>,
    /// Prompts left out of `prompts`.
    pub more_prompts: i64,
    /// The files it edited most, with how often.
    pub files: Vec<(String, i64)>,
    /// Models and how many of its replies each wrote.
    pub models: Vec<(String, i64)>,
    /// Where the search matched in this session.
    pub matches: Vec<Snippet>,
}

/// A page of a session's conversation: prompts and Claude's text in order.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts", rename = "PastConversation")]
pub struct Conversation {
    pub items: Vec<ConversationItem>,
    /// Texts in the whole conversation.
    pub total: i64,
    /// Position of `items[0]`.
    pub offset: i64,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts", rename = "PastItem")]
pub struct ConversationItem {
    pub id: i64,
    pub at: i64,
    pub role: String,
    pub html: String,
}

/// What a row of the list needs before it is filtered.
#[derive(Debug)]
struct Light {
    n: i64,
    id: String,
    first_at: i64,
    last_at: i64,
    size: i64,
    prompts: i64,
    cost: Option<f64>,
    hosts: Vec<String>,
    workspace: Option<String>,
    project: Option<String>,
    key: String,
}

fn key_of(workspace: Option<&str>, project: Option<&str>) -> String {
    match (workspace, project) {
        (Some(ws), Some(p)) => format!("{ws}/{p}"),
        (Some(ws), None) => format!("{ws}/"),
        (None, _) => OUTSIDE.to_owned(),
    }
}

/// The session's workspace and project: the store's label when it has
/// one, else the directory under the workspace.
fn place(
    workspaces: &Workspaces,
    cwd: &str,
    label: Option<String>,
) -> (Option<String>, Option<String>) {
    match workspaces.locate(cwd) {
        Some((ws, dir)) => (Some(ws), label.or(dir)),
        None => (None, None),
    }
}

fn lights(conn: &Connection, workspaces: &Workspaces) -> Result<Vec<Light>> {
    let mut stmt = conn.prepare_cached(
        "SELECT c.n, c.id, COALESCE(c.cwd, ''), COALESCE(c.first_at, 0), COALESCE(c.last_at, 0),
            c.size, c.prompts, c.cost_usd, c.host, s.project,
            (SELECT group_concat(h.host, char(31)) FROM catalog_hosts h WHERE h.session = c.n)
        FROM catalog c LEFT JOIN store.sessions s ON s.id = c.id
        WHERE c.prompts > 0
        ORDER BY c.last_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, Option<f64>>(7)?,
            r.get::<_, Option<String>>(8)?,
            r.get::<_, Option<String>>(9)?,
            r.get::<_, Option<String>>(10)?,
        ))
    })?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for row in rows {
        let (n, id, cwd, first_at, last_at, size, prompts, cost, host, label, hosts) = row?;
        // NOTE: a copy of a transcript in another directory (same id) is one session: the newest row wins (ordered by last_at)
        if !seen.insert(id.clone()) {
            continue;
        }
        let mut hosts: Vec<String> = hosts
            .map(|h| h.split('\u{1f}').map(str::to_owned).collect())
            .unwrap_or_default();
        if hosts.is_empty() {
            hosts.extend(host);
        }
        let (workspace, project) = place(workspaces, &cwd, label);
        let key = key_of(workspace.as_deref(), project.as_deref());
        out.push(Light {
            n,
            id,
            first_at,
            last_at,
            size,
            prompts,
            cost,
            hosts,
            workspace,
            project,
            key,
        });
    }
    Ok(out)
}

/// FTS5 terms of a search: quoted phrases as written, other words as
/// prefixes (a single character exactly). None when there is no word.
fn terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut parts = text.split('"');
    let mut quoted = false;
    for part in parts.by_ref() {
        if quoted {
            let words: Vec<&str> = words(part).collect();
            if !words.is_empty() {
                out.push(format!("\"{}\"", words.join(" ")));
            }
        } else {
            for word in words(part) {
                if word.chars().count() > 1 {
                    out.push(format!("\"{word}\"*"));
                } else {
                    out.push(format!("\"{word}\""));
                }
            }
        }
        quoted = !quoted;
    }
    out
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

/// A session's best match, its number of matches, whether the title matched.
#[derive(Debug, Default, Clone, Copy)]
struct Score {
    best: f64,
    hits: i64,
    title: bool,
}

fn scores(conn: &Connection, expr: &str) -> Result<HashMap<i64, Score>> {
    let mut out: HashMap<i64, Score> = HashMap::new();
    let mut stmt = conn.prepare_cached(
        "WITH hits AS MATERIALIZED (
            SELECT t.session AS session, bm25(catalog_fts) AS rank
            FROM catalog_fts JOIN catalog_texts t ON t.id = catalog_fts.rowid
            WHERE catalog_fts MATCH ?1
        )
        SELECT session, MIN(rank), COUNT(*) FROM hits GROUP BY session",
    )?;
    for row in stmt.query_map([expr], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (n, best, hits) = row?;
        out.insert(
            n,
            Score {
                best: -best,
                hits,
                title: false,
            },
        );
    }
    let mut stmt =
        conn.prepare_cached("SELECT rowid FROM catalog_titles WHERE catalog_titles MATCH ?1")?;
    for row in stmt.query_map([expr], |r| r.get::<_, i64>(0))? {
        out.entry(row?).or_default().title = true;
    }
    Ok(out)
}

/// How a match ranks: bm25 of its best text, more matches and a matched
/// title help, and so does being recent.
fn rank(score: Score, last_at: i64, now: i64) -> f64 {
    let days = (now - last_at).max(0) as f64 / 86_400_000.0;
    score.best
        + 0.5 * (score.hits as f64).ln_1p()
        + if score.title { 4.0 } else { 0.0 }
        + 1.5 * 0.5f64.powf(days / 14.0)
}

/// The list: filtered, counted per machine and project, searched, sorted
/// and cut to a page. `open` holds the ids of the sessions running now.
pub fn list(
    conn: &Connection,
    workspaces: &Workspaces,
    open: &HashSet<String>,
    q: &Query,
    now: i64,
) -> Result<Page> {
    let all = lights(conn, workspaces)?;
    let indexed = all.len();
    let terms = q.text.as_deref().map(terms).unwrap_or_default();
    let (scores, matched) = if terms.is_empty() {
        (None, None)
    } else {
        let every = scores(conn, &terms.join(" AND "))?;
        if every.is_empty() && terms.len() > 1 {
            (Some(scores(conn, &terms.join(" OR "))?), Some("any"))
        } else {
            (Some(every), Some("all"))
        }
    };
    let in_scope = |l: &Light| {
        (q.workspaces.is_empty()
            || l.workspace
                .as_ref()
                .is_some_and(|w| q.workspaces.contains(w)))
            && q.since.is_none_or(|since| l.last_at >= since)
            && scores.as_ref().is_none_or(|s| s.contains_key(&l.n))
    };
    let host_ok = |l: &Light| q.hosts.is_empty() || l.hosts.iter().any(|h| q.hosts.contains(h));
    let project_ok = |l: &Light| q.project.as_ref().is_none_or(|p| &l.key == p);
    let mut facets = Facets::default();
    let mut hosts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut projects: BTreeMap<&str, (&Light, usize)> = BTreeMap::new();
    let mut shown: Vec<&Light> = Vec::new();
    for l in all.iter().filter(|l| in_scope(l)) {
        let (h, p) = (host_ok(l), project_ok(l));
        if p {
            for host in &l.hosts {
                *hosts.entry(host).or_default() += 1;
            }
        }
        if h {
            projects.entry(&l.key).or_insert((l, 0)).1 += 1;
        }
        if h && p {
            shown.push(l);
        }
    }
    facets.hosts = hosts
        .into_iter()
        .map(|(host, count)| HostCount {
            host: host.to_owned(),
            count,
        })
        .collect();
    facets.projects = projects
        .into_iter()
        .map(|(key, (l, count))| ProjectCount {
            key: key.to_owned(),
            workspace: l.workspace.clone(),
            project: l.project.clone(),
            count,
        })
        .collect();
    let sort = match (q.sort, &scores) {
        (Sort::Best, None) => Sort::Recent,
        (sort, _) => sort,
    };
    let rank_of = |l: &Light| {
        scores
            .as_ref()
            .and_then(|s| s.get(&l.n))
            .map_or(0.0, |s| rank(*s, l.last_at, now))
    };
    shown.sort_by(|a, b| {
        let by = match sort {
            Sort::Best => rank_of(b).total_cmp(&rank_of(a)),
            Sort::Recent => b.last_at.cmp(&a.last_at),
            Sort::Started => b.first_at.cmp(&a.first_at),
            Sort::Prompts => b.prompts.cmp(&a.prompts),
            Sort::Size => b.size.cmp(&a.size),
            Sort::Cost => b.cost.unwrap_or(0.0).total_cmp(&a.cost.unwrap_or(0.0)),
        };
        by.then_with(|| b.last_at.cmp(&a.last_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    let total = shown.len();
    let page: Vec<&Light> = shown
        .into_iter()
        .skip(q.offset)
        .take(q.limit.clamp(1, 200))
        .collect();
    let expr = matched.map(|m| terms.join(if m == "any" { " OR " } else { " AND " }));
    let mut snippets = match &expr {
        Some(expr) => card_snippets(conn, expr, &page)?,
        None => HashMap::new(),
    };
    let mut sessions = Vec::with_capacity(page.len());
    for l in page {
        if let Some(mut s) = session(conn, workspaces, open, l.n)? {
            s.snippets = snippets.remove(&l.n).unwrap_or_default();
            sessions.push(s);
        }
    }
    Ok(Page {
        sessions,
        total,
        matched: matched.map(str::to_owned),
        facets,
        indexed,
        pending: 0,
        ready: true,
    })
}

/// Up to `SNIPPETS` matches per session of the page, best first.
fn card_snippets(
    conn: &Connection,
    expr: &str,
    page: &[&Light],
) -> Result<HashMap<i64, Vec<Snippet>>> {
    let ids: Vec<i64> = page.iter().map(|l| l.n).collect();
    let mut best: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut stmt = conn.prepare_cached(
        "SELECT t.session, t.id FROM catalog_fts JOIN catalog_texts t ON t.id = catalog_fts.rowid
        WHERE catalog_fts MATCH ?1 AND t.session IN (SELECT value FROM json_each(?2))
        ORDER BY bm25(catalog_fts)",
    )?;
    for row in stmt.query_map(params![expr, serde_json::to_string(&ids)?], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
    })? {
        let (n, text) = row?;
        let picked = best.entry(n).or_default();
        if picked.len() < SNIPPETS {
            picked.push(text);
        }
    }
    let texts: Vec<i64> = best.values().flatten().copied().collect();
    let mut found = snippets_of(conn, expr, &texts)?;
    let mut out: HashMap<i64, Vec<Snippet>> = HashMap::new();
    for (n, texts) in best {
        let list = out.entry(n).or_default();
        for text in texts {
            if let Some(s) = found.remove(&text) {
                list.push(s);
            }
        }
    }
    let mut stmt = conn.prepare_cached(
        "SELECT rowid, highlight(catalog_titles, 0, char(2), char(3)) FROM catalog_titles
        WHERE catalog_titles MATCH ?1 AND rowid IN (SELECT value FROM json_each(?2))",
    )?;
    for row in stmt.query_map(params![expr, serde_json::to_string(&ids)?], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })? {
        let (n, title) = row?;
        let list = out.entry(n).or_default();
        if list.is_empty() {
            list.push(Snippet {
                text: 0,
                at: 0,
                role: "title".into(),
                snippet: title,
            });
        }
    }
    Ok(out)
}

/// The snippets of texts `ids` for `expr`.
fn snippets_of(conn: &Connection, expr: &str, ids: &[i64]) -> Result<HashMap<i64, Snippet>> {
    let mut stmt = conn.prepare_cached(
        "SELECT t.id, t.at, t.role, snippet(catalog_fts, 0, char(2), char(3), '…', ?3)
        FROM catalog_fts JOIN catalog_texts t ON t.id = catalog_fts.rowid
        WHERE catalog_fts MATCH ?1 AND catalog_fts.rowid IN (SELECT value FROM json_each(?2))",
    )?;
    let rows = stmt.query_map(
        params![expr, serde_json::to_string(ids)?, SNIPPET_TOKENS],
        |r| {
            Ok(Snippet {
                text: r.get(0)?,
                at: r.get(1)?,
                role: r.get(2)?,
                snippet: r.get(3)?,
            })
        },
    )?;
    let mut out = HashMap::new();
    for row in rows {
        let s = row?;
        out.insert(s.text, s);
    }
    Ok(out)
}

const SESSION: &str = "SELECT c.n, c.id, COALESCE(c.cwd, ''), COALESCE(c.first_at, 0),
    COALESCE(c.last_at, 0), c.size, c.custom_title, c.ai_title, c.summary, c.recap,
    c.first_prompt, c.last_prompt, c.last_reply, c.prompts, c.replies, c.tools, c.active_ms,
    c.compactions, c.models, c.branch, c.version, c.entrypoint, c.cost_usd, c.lines_added,
    c.lines_removed,
    COALESCE(c.continued_in, (SELECT k.id FROM store.sessions k WHERE k.continues = c.id
        ORDER BY k.started_at DESC, k.id LIMIT 1)),
    COALESCE(c.host, ''), s.project, s.title, s.id IS NOT NULL,
    (SELECT group_concat(h.host, char(31)) FROM catalog_hosts h WHERE h.session = c.n)
    FROM catalog c LEFT JOIN store.sessions s ON s.id = c.id";

fn session(
    conn: &Connection,
    workspaces: &Workspaces,
    open: &HashSet<String>,
    n: i64,
) -> Result<Option<PastSession>> {
    Ok(conn
        .prepare_cached(&format!("{SESSION} WHERE c.n = ?1"))?
        .query_row([n], |r| {
            let cwd: String = r.get(2)?;
            let id: String = r.get(1)?;
            let models: BTreeMap<String, i64> =
                serde_json::from_str(&r.get::<_, String>(18)?).unwrap_or_default();
            let host: String = r.get(26)?;
            let mut hosts: Vec<String> = r
                .get::<_, Option<String>>(30)?
                .map(|h| h.split('\u{1f}').map(str::to_owned).collect())
                .unwrap_or_default();
            if hosts.is_empty() && !host.is_empty() {
                hosts.push(host.clone());
            }
            let (workspace, project) = place(workspaces, &cwd, r.get(27)?);
            let card = |i: usize| -> rusqlite::Result<Option<String>> {
                Ok(r.get::<_, Option<String>>(i)?
                    .map(|t| truncate(&t, CARD_TEXT)))
            };
            Ok(PastSession {
                open: open.contains(&id),
                id,
                workspace,
                project,
                host,
                hosts,
                title: r
                    .get::<_, Option<String>>(6)?
                    .or(r.get(7)?)
                    .or(r.get(28)?)
                    .or(r.get(8)?),
                recap: r.get(9)?,
                first_prompt: card(10)?,
                last_prompt: card(11)?,
                last_reply: card(12)?,
                first_at: r.get(3)?,
                last_at: r.get(4)?,
                size: r.get(5)?,
                prompts: r.get(13)?,
                replies: r.get(14)?,
                tools: r.get(15)?,
                active_ms: r.get(16)?,
                compactions: r.get(17)?,
                model: models
                    .into_iter()
                    .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
                    .map(|(m, _)| m),
                branch: r.get(19)?,
                version: r.get(20)?,
                entrypoint: r.get(21)?,
                cost_usd: r.get(22)?,
                lines_added: r.get(23)?,
                lines_removed: r.get(24)?,
                continued_in: r.get(25)?,
                known: r.get(29)?,
                snippets: Vec::new(),
                cwd,
            })
        })
        .optional()?)
}

/// The catalog's row for session `id`: the newest when two directories
/// hold a transcript of that id.
fn n_of(conn: &Connection, id: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT n FROM catalog WHERE id = ?1 ORDER BY last_at DESC LIMIT 1")?
        .query_row([id], |r| r.get(0))
        .optional()?)
}

/// Session `id` as the list shows it, without snippets.
///
/// # Errors
///
/// The catalog cannot be read.
pub fn find(
    conn: &Connection,
    workspaces: &Workspaces,
    open: &HashSet<String>,
    id: &str,
) -> Result<Option<PastSession>> {
    match n_of(conn, id)? {
        Some(n) => session(conn, workspaces, open, n),
        None => Ok(None),
    }
}

/// Session `id`'s overview; with `text`, where it matched.
///
/// # Errors
///
/// The catalog cannot be read.
pub fn detail(
    conn: &Connection,
    workspaces: &Workspaces,
    open: &HashSet<String>,
    id: &str,
    text: Option<&str>,
) -> Result<Option<PastDetail>> {
    let Some(n) = n_of(conn, id)? else {
        return Ok(None);
    };
    let Some(session) = session(conn, workspaces, open, n)? else {
        return Ok(None);
    };
    let row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<PastText> {
        Ok(PastText {
            id: r.get(0)?,
            at: r.get(1)?,
            text: r.get(2)?,
        })
    };
    let first_prompt = conn
        .prepare_cached(
            "SELECT id, at, text FROM catalog_texts WHERE session = ?1 AND role = 'you'
            ORDER BY id",
        )?
        .query_map([n], row)?
        .find(|t| {
            t.as_ref()
                .map_or(true, |t| !super::parse::bare_command(&t.text))
        })
        .transpose()?;
    let last_prompt = conn
        .prepare_cached(
            "SELECT id, at, text FROM catalog_texts WHERE session = ?1 AND role = 'you'
            ORDER BY id DESC LIMIT 1",
        )?
        .query_row([n], row)
        .optional()?
        .filter(|last| {
            first_prompt
                .as_ref()
                .is_none_or(|first| first.id != last.id)
        });
    let last_reply = conn
        .prepare_cached(
            "SELECT id, at, text FROM catalog_texts WHERE session = ?1 AND role = 'claude'
            ORDER BY id DESC LIMIT 1",
        )?
        .query_row([n], row)
        .optional()?;
    let mut prompts: Vec<PastText> = conn
        .prepare_cached(
            "SELECT id, at, text FROM catalog_texts WHERE session = ?1 AND role = 'you'
            ORDER BY id LIMIT ?2",
        )?
        .query_map(params![n, MAX_OUTLINE as i64], row)?
        .collect::<rusqlite::Result<_>>()?;
    for p in &mut prompts {
        p.text = truncate(&p.text, OUTLINE_TEXT);
    }
    let more_prompts = (session.prompts - prompts.len() as i64).max(0);
    let (files, models): (BTreeMap<String, i64>, BTreeMap<String, i64>) =
        conn.query_row("SELECT files, models FROM catalog WHERE n = ?1", [n], |r| {
            Ok((
                serde_json::from_str(&r.get::<_, String>(0)?).unwrap_or_default(),
                serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or_default(),
            ))
        })?;
    let by_count = |m: BTreeMap<String, i64>| {
        let mut v: Vec<(String, i64)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    };
    let mut files = by_count(files);
    files.truncate(FILES);
    for (path, _) in &mut files {
        if let Some(rest) = path
            .strip_prefix(&session.cwd)
            .and_then(|r| r.strip_prefix('/'))
        {
            *path = rest.to_owned();
        }
    }
    let matches = match text.map(terms).filter(|t| !t.is_empty()) {
        Some(terms) => {
            let mut found = matches_in(conn, n, &terms.join(" AND "))?;
            if found.is_empty() && terms.len() > 1 {
                found = matches_in(conn, n, &terms.join(" OR "))?;
            }
            found
        }
        None => Vec::new(),
    };
    Ok(Some(PastDetail {
        first_prompt,
        last_prompt,
        last_reply_html: last_reply.as_ref().map(|r| markdown::render(&r.text)),
        last_reply_at: last_reply.map(|r| r.at),
        prompts,
        more_prompts,
        files,
        models: by_count(models),
        matches,
        session,
    }))
}

/// Matches of `expr` in session `n`, in the order they were said.
fn matches_in(conn: &Connection, n: i64, expr: &str) -> Result<Vec<Snippet>> {
    let ids: Vec<i64> = conn
        .prepare_cached(
            "SELECT t.id FROM catalog_fts JOIN catalog_texts t ON t.id = catalog_fts.rowid
            WHERE catalog_fts MATCH ?1 AND t.session = ?2 ORDER BY t.id LIMIT ?3",
        )?
        .query_map(params![expr, n, DETAIL_MATCHES as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut found = snippets_of(conn, expr, &ids)?;
    Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
}

/// Texts of session `id` from `offset` (or a few before text `around`).
///
/// # Errors
///
/// The catalog cannot be read.
pub fn conversation(
    conn: &Connection,
    id: &str,
    offset: Option<i64>,
    around: Option<i64>,
    limit: i64,
) -> Result<Option<Conversation>> {
    let Some(n) = n_of(conn, id)? else {
        return Ok(None);
    };
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM catalog_texts WHERE session = ?1",
        [n],
        |r| r.get(0),
    )?;
    let offset = match (around, offset) {
        (Some(text), _) => {
            let before: i64 = conn.query_row(
                "SELECT COUNT(*) FROM catalog_texts WHERE session = ?1 AND id < ?2",
                params![n, text],
                |r| r.get(0),
            )?;
            (before - 3).max(0)
        }
        (None, Some(offset)) => offset.max(0),
        (None, None) => 0,
    };
    let items = conn
        .prepare_cached(
            "SELECT id, at, role, text FROM catalog_texts WHERE session = ?1
            ORDER BY id LIMIT ?2 OFFSET ?3",
        )?
        .query_map(params![n, limit.clamp(1, 200), offset], |r| {
            Ok(ConversationItem {
                id: r.get(0)?,
                at: r.get(1)?,
                role: r.get(2)?,
                html: markdown::render(&r.get::<_, String>(3)?),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(Conversation {
        items,
        total,
        offset,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_terms() {
        assert_eq!(terms("nix flake"), ["\"nix\"*", "\"flake\"*"]);
        assert_eq!(
            terms("a \"exact phrase\" x-y"),
            ["\"a\"", "\"exact phrase\"", "\"x\"", "\"y\""]
        );
        assert!(terms("  ** \"\" ").is_empty());
        assert_eq!(terms("žluťoučký"), ["\"žluťoučký\"*"]);
    }

    #[test]
    fn keys() {
        assert_eq!(key_of(Some("w"), Some("p")), "w/p");
        assert_eq!(key_of(Some("w"), None), "w/");
        assert_eq!(key_of(None, Some("p")), OUTSIDE);
    }
}
