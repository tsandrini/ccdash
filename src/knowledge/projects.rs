//! What the Projects pages show: a workspace's live notes grouped by
//! project, each open issue with its derived state (in progress when a
//! live session's claim names it, or it got a progress entry or a comment
//! in the last day) and the session working on it; and the note graph.
//! The views are observational: nothing here is set by hand.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::{COLUMNS, Card, LinkKind, Note, NoteKind, Status, row_note};
use crate::model::{Activity, Session};

const DAY_MS: i64 = 86_400_000;
/// Closed issues stay on the board this long.
const DONE_MS: i64 = 7 * DAY_MS;
/// A document is hot when it was written or recalled within this.
const HOT_MS: i64 = 7 * DAY_MS;
const PROGRESS_PER_PROJECT: usize = 60;

/// `GET /projects`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Projects {
    pub workspaces: Vec<WorkspaceProjects>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct WorkspaceProjects {
    pub name: String,
    /// By project name; the workspace-wide notes come first (`project` None).
    pub projects: Vec<Project>,
    /// Notes the lifecycle pass flagged stale, any project.
    pub flagged: Vec<Card>,
    /// Live notes in the workspace.
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Project {
    pub project: Option<String>,
    /// Open issues, highest priority first.
    pub issues: Vec<Issue>,
    /// Issues closed in the last 7 days, newest first.
    pub closed: Vec<Issue>,
    /// Hot documents first, then by update.
    pub docs: Vec<Doc>,
    /// Newest first.
    pub progress: Vec<Card>,
    /// Active memories first, then the flagged ones; by recalls within.
    pub memories: Vec<Card>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Issue {
    #[serde(flatten)]
    pub card: Card,
    pub in_progress: bool,
    /// The live session working on it: by claim, else the one that last
    /// logged progress or commented today.
    pub working: Option<Working>,
    pub last_progress: Option<Card>,
    pub closed_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Working {
    pub session: String,
    pub host: String,
    pub label: String,
    /// The session's claim names the issue (else it is inferred from
    /// what it wrote).
    pub claimed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Doc {
    #[serde(flatten)]
    pub card: Card,
    pub hot: bool,
}

/// The projects of a workspace's live notes and recently closed issues,
/// the workspace-wide notes (None) first.
fn project_names(live: &[Note], closed: &[Note]) -> Vec<Option<String>> {
    let mut names: Vec<Option<String>> = live
        .iter()
        .chain(closed)
        .map(|n| n.project.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The projects `GET /projects` lists for `workspace` (not the
/// workspace-wide entry).
pub fn names(conn: &Connection, workspace: &str, now: i64) -> Result<Vec<String>> {
    let live = notes(conn, workspace, &[Status::Active, Status::Stale])?;
    let closed: Vec<Note> = notes(conn, workspace, &[Status::Archived])?
        .into_iter()
        .filter(|n| n.kind == NoteKind::Issue && closed_at(n) >= now - DONE_MS)
        .collect();
    Ok(project_names(&live, &closed)
        .into_iter()
        .flatten()
        .collect())
}

/// The workspace's Projects view; `sessions` are the live ones the hub
/// sees (any workspace: they are filtered here).
pub fn build(
    conn: &Connection,
    workspace: &str,
    sessions: &[Session],
    now: i64,
) -> Result<WorkspaceProjects> {
    let live = notes(conn, workspace, &[Status::Active, Status::Stale])?;
    let closed: Vec<Note> = notes(conn, workspace, &[Status::Archived])?
        .into_iter()
        .filter(|n| n.kind == NoteKind::Issue && closed_at(n) >= now - DONE_MS)
        .collect();
    let sessions: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.activity != Activity::Gone && s.workspace.as_deref() == Some(workspace))
        .collect();
    let by_id: HashMap<&str, &Session> = sessions.iter().map(|s| (s.id.as_str(), *s)).collect();
    let claimed: HashMap<&str, &Session> = sessions
        .iter()
        .filter_map(|s| Some((s.claim.as_ref()?.issue.as_deref()?, *s)))
        .collect();
    let mut progress_of: HashMap<&str, Vec<&Note>> = HashMap::new();
    for n in live.iter().filter(|n| n.kind == NoteKind::Progress) {
        if let Some(issue) = n.extra["issue"].as_str() {
            progress_of.entry(issue).or_default().push(n);
        }
    }
    for list in progress_of.values_mut() {
        list.sort_by_key(|n| std::cmp::Reverse(n.created_at));
    }

    let issue = |n: &Note| -> Issue {
        let last = progress_of
            .get(n.slug.as_str())
            .and_then(|list| list.first().copied());
        let claim = claimed.get(n.slug.as_str()).copied();
        let recent_progress = last.filter(|p| p.created_at >= now - DAY_MS);
        let commented = n.version > 1
            && n.updated_at >= now - DAY_MS
            && by_session(n.updated_by.as_deref()).is_some();
        let open = n.status != Status::Archived;
        let in_progress = open && (claim.is_some() || recent_progress.is_some() || commented);
        let working = if !open {
            None
        } else if let Some(s) = claim {
            Some(working(s, true))
        } else {
            recent_progress
                .and_then(|p| by_session(p.created_by.as_deref()))
                .or_else(|| {
                    commented
                        .then(|| by_session(n.updated_by.as_deref()))
                        .flatten()
                })
                .and_then(|id| by_id.get(id))
                .map(|s| working(s, false))
        };
        Issue {
            card: n.card(),
            in_progress,
            working,
            last_progress: last.map(Note::card),
            closed_at: (!open).then(|| closed_at(n)),
        }
    };

    let flagged: Vec<Card> = live
        .iter()
        .filter(|n| n.status == Status::Stale)
        .map(Note::card)
        .collect();
    let projects = project_names(&live, &closed)
        .into_iter()
        .map(|project| {
            let of = |kind: NoteKind| -> Vec<&Note> {
                live.iter()
                    .filter(|n| n.project == project && n.kind == kind)
                    .collect()
            };
            let mut issues: Vec<Issue> = of(NoteKind::Issue)
                .into_iter()
                .filter(|n| n.status == Status::Active)
                .map(issue)
                .collect();
            issues.sort_by(|a, b| {
                b.card
                    .priority
                    .cmp(&a.card.priority)
                    .then(b.card.updated_at.cmp(&a.card.updated_at))
            });
            let mut done: Vec<Issue> = closed
                .iter()
                .filter(|n| n.project == project)
                .map(issue)
                .collect();
            done.sort_by_key(|i| std::cmp::Reverse(i.closed_at));
            let mut docs: Vec<Doc> = of(NoteKind::Doc)
                .into_iter()
                .map(|n| Doc {
                    hot: n.updated_at >= now - HOT_MS
                        || n.recalled_at.is_some_and(|t| t >= now - HOT_MS),
                    card: n.card(),
                })
                .collect();
            docs.sort_by(|a, b| {
                b.hot
                    .cmp(&a.hot)
                    .then(b.card.updated_at.cmp(&a.card.updated_at))
            });
            let mut progress: Vec<&Note> = of(NoteKind::Progress);
            progress.sort_by_key(|n| std::cmp::Reverse(n.created_at));
            let mut memories: Vec<&Note> = of(NoteKind::Memory);
            memories.sort_by(|a, b| {
                (a.status == Status::Stale)
                    .cmp(&(b.status == Status::Stale))
                    .then(b.recalls.cmp(&a.recalls))
                    .then(b.updated_at.cmp(&a.updated_at))
            });
            Project {
                project,
                issues,
                closed: done,
                docs,
                progress: progress
                    .into_iter()
                    .take(PROGRESS_PER_PROJECT)
                    .map(Note::card)
                    .collect(),
                memories: memories.into_iter().map(Note::card).collect(),
            }
        })
        .collect();
    Ok(WorkspaceProjects {
        name: workspace.to_owned(),
        projects,
        flagged,
        total: live.len(),
    })
}

/// Who signed a write, unless it was the owner (`panel`) or an import.
fn by_session(by: Option<&str>) -> Option<&str> {
    by.filter(|b| !matches!(*b, "panel" | "import"))
}

fn working(s: &Session, claimed: bool) -> Working {
    Working {
        session: s.id.clone(),
        host: s.host.clone(),
        label: s
            .title
            .clone()
            .or_else(|| s.name.clone())
            .unwrap_or_else(|| s.id.chars().take(8).collect()),
        claimed,
    }
}

/// When an issue was closed: `issue_update` records it; an archive from
/// the editor leaves the update time.
fn closed_at(n: &Note) -> i64 {
    n.extra["closed_at"].as_i64().unwrap_or(n.updated_at)
}

fn notes(conn: &Connection, workspace: &str, statuses: &[Status]) -> Result<Vec<Note>> {
    let statuses = serde_json::to_string(&statuses.iter().map(|s| s.name()).collect::<Vec<_>>())?;
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM notes WHERE workspace = ?1
                AND status IN (SELECT value FROM json_each(?2)) ORDER BY updated_at DESC"
        ))?
        .query_map(params![workspace, statuses], row_note)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

// --- graph -------------------------------------------------------------------

/// `GET /graph`: the workspace's notes and the links between them
/// (backlinks are the same edges seen from the other end).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct GraphNode {
    pub id: i64,
    pub slug: String,
    pub kind: NoteKind,
    pub title: String,
    pub project: Option<String>,
    pub status: Status,
    pub created_at: i64,
    pub updated_at: i64,
    pub recalls: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct GraphEdge {
    pub src: i64,
    pub dst: i64,
    pub kind: LinkKind,
}

/// The graph over the notes in `statuses`; links to notes outside it or
/// to slugs no note has are left out.
pub fn graph(conn: &Connection, workspace: &str, statuses: &[Status]) -> Result<Graph> {
    let statuses = serde_json::to_string(&statuses.iter().map(|s| s.name()).collect::<Vec<_>>())?;
    let nodes: Vec<GraphNode> = conn
        .prepare_cached(
            "SELECT id, slug, kind, title, project, status, created_at, updated_at, recalls
                FROM notes WHERE workspace = ?1
                AND status IN (SELECT value FROM json_each(?2)) ORDER BY created_at",
        )?
        .query_map(params![workspace, statuses], |r| {
            let kind: String = r.get(2)?;
            let status: String = r.get(5)?;
            Ok(GraphNode {
                id: r.get(0)?,
                slug: r.get(1)?,
                kind: NoteKind::parse(&kind).unwrap_or(NoteKind::Memory),
                title: r.get(3)?,
                project: r.get(4)?,
                status: Status::parse(&status).unwrap_or(Status::Active),
                created_at: r.get(6)?,
                updated_at: r.get(7)?,
                recalls: r.get::<_, i64>(8)? as u32,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let ids: std::collections::HashSet<i64> = nodes.iter().map(|n| n.id).collect();
    let edges: Vec<GraphEdge> = conn
        .prepare_cached(
            "SELECT l.src, n.id, l.kind FROM links l
                JOIN notes s ON s.id = l.src
                JOIN notes n ON n.workspace = s.workspace AND n.slug = l.dst
                WHERE s.workspace = ?1 ORDER BY l.src, l.pos",
        )?
        .query_map([workspace], |r| {
            let kind: String = r.get(2)?;
            Ok(GraphEdge {
                src: r.get(0)?,
                dst: r.get(1)?,
                kind: LinkKind::parse(&kind),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|e| ids.contains(&e.src) && ids.contains(&e.dst))
        .collect();
    Ok(Graph { nodes, edges })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        knowledge::{NoteOp, Priority, apply},
        model::Claim,
    };

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();
        conn.execute_batch(super::super::SCHEMA).unwrap();
        conn
    }

    fn session(id: &str, claim: Option<&str>) -> Session {
        let mut s = Session::stub("navi", id, "/ws/repo");
        s.workspace = Some("ws".into());
        s.project = Some("repo".into());
        s.title = Some(format!("session {id}"));
        s.activity = Activity::Working;
        s.claim = claim.map(|issue| Claim {
            what: "x".into(),
            paths: Vec::new(),
            issue: Some(issue.into()),
            since: 0,
            until: None,
        });
        s
    }

    fn add_issue(conn: &Connection, title: &str, by: &str, at: i64) -> String {
        apply(
            conn,
            "ws",
            Some("repo"),
            Some(by),
            &NoteOp::IssueAdd {
                title: title.into(),
                body: None,
                project: None,
                priority: Some(Priority::Normal),
            },
            at,
        )
        .unwrap()
        .slug
    }

    #[test]
    fn issues_derive_their_state_from_what_sessions_do() {
        let conn = db();
        let now = 100 * DAY_MS;
        let claimed = add_issue(&conn, "claimed", "s1", now - 10 * DAY_MS);
        let progressed = add_issue(&conn, "progressed", "s1", now - 10 * DAY_MS);
        let commented = add_issue(&conn, "commented", "s1", now - 10 * DAY_MS);
        let fresh = add_issue(&conn, "fresh", "s3", now - 1000);
        let stale = add_issue(&conn, "stale progress", "s1", now - 10 * DAY_MS);
        let panel = add_issue(&conn, "panel edit", "s1", now - 10 * DAY_MS);
        let done = add_issue(&conn, "done", "s1", now - 10 * DAY_MS);
        let old_done = add_issue(&conn, "old done", "s1", now - 20 * DAY_MS);
        let log = |issue: &str, by: &str, at: i64| {
            apply(
                &conn,
                "ws",
                Some("repo"),
                Some(by),
                &NoteOp::Progress {
                    text: format!("worked on {issue}"),
                    issue: Some(issue.into()),
                    project: None,
                },
                at,
            )
            .unwrap();
        };
        log(&progressed, "s2", now - 2 * 3_600_000);
        log(&stale, "s2", now - 3 * DAY_MS);
        let comment = |issue: &str, by: Option<&str>, at: i64, status: Option<&str>| {
            apply(
                &conn,
                "ws",
                Some("repo"),
                by,
                &NoteOp::IssueUpdate {
                    slug: issue.into(),
                    status: status.map(str::to_owned),
                    title: None,
                    body: None,
                    priority: None,
                    comment: Some("a note".into()),
                },
                at,
            )
            .unwrap();
        };
        comment(&commented, Some("s9"), now - 3_600_000, None);
        comment(&panel, Some("panel"), now - 3_600_000, None);
        comment(&done, Some("s1"), now - DAY_MS, Some("closed"));
        comment(&old_done, Some("s1"), now - 8 * DAY_MS, Some("closed"));
        apply(
            &conn,
            "ws",
            None,
            Some("s1"),
            &NoteOp::DocWrite {
                slug: "plan".into(),
                title: None,
                body: "# Plan".into(),
                base_version: None,
            },
            now - 30 * DAY_MS,
        )
        .unwrap();
        conn.execute(
            "UPDATE notes SET recalled_at = ?1 WHERE slug = 'plan'",
            [now - DAY_MS],
        )
        .unwrap();

        let sessions = [session("s1", Some(&claimed)), session("s2", None)];
        let view = build(&conn, "ws", &sessions, now).unwrap();
        assert_eq!(view.total, 9, "6 open issues, 2 progress entries, 1 doc");
        assert_eq!(view.projects.len(), 2);
        assert_eq!(view.projects[0].project, None);
        assert!(view.projects[0].docs[0].hot);
        let repo = &view.projects[1];
        assert_eq!(repo.project.as_deref(), Some("repo"));
        let by_slug: HashMap<&str, &Issue> = repo
            .issues
            .iter()
            .map(|i| (i.card.slug.as_str(), i))
            .collect();
        assert_eq!(by_slug.len(), 6);
        let claimed = by_slug[claimed.as_str()];
        assert!(claimed.in_progress);
        assert_eq!(
            claimed.working,
            Some(Working {
                session: "s1".into(),
                host: "navi".into(),
                label: "session s1".into(),
                claimed: true
            })
        );
        let progressed = by_slug[progressed.as_str()];
        assert!(progressed.in_progress);
        assert_eq!(progressed.working.as_ref().unwrap().session, "s2");
        assert!(!progressed.working.as_ref().unwrap().claimed);
        assert_eq!(
            progressed.last_progress.as_ref().unwrap().issue.as_deref(),
            Some(progressed.card.slug.as_str())
        );
        let commented = by_slug[commented.as_str()];
        assert!(commented.in_progress);
        assert_eq!(commented.working, None, "s9 is not live");
        assert!(
            !by_slug[fresh.as_str()].in_progress,
            "filing is not working"
        );
        assert!(!by_slug[stale.as_str()].in_progress);
        assert!(by_slug[stale.as_str()].last_progress.is_some());
        assert!(
            !by_slug[panel.as_str()].in_progress,
            "the owner's edits do not count"
        );
        assert_eq!(repo.closed.len(), 1);
        assert_eq!(repo.closed[0].card.slug, done);
        assert_eq!(repo.closed[0].closed_at, Some(now - DAY_MS));
        assert!(!repo.closed[0].in_progress);
        assert_eq!(repo.progress.len(), 2);
        assert_eq!(
            repo.progress[0].issue.as_deref(),
            Some(progressed.card.slug.as_str())
        );

        let graph = graph(&conn, "ws", &[Status::Active, Status::Stale]).unwrap();
        assert_eq!(graph.nodes.len(), 9);
        assert_eq!(graph.edges.len(), 2, "progress relates to its issue");
        assert!(graph.edges.iter().all(|e| e.kind == LinkKind::Relates));
        let all = graph_all(&conn);
        assert_eq!(all.nodes.len(), 11);
        assert_eq!(all.nodes[0].slug, "plan", "oldest first");
        assert!(all.nodes.iter().any(|n| n.slug == old_done));
    }

    fn graph_all(conn: &Connection) -> Graph {
        graph(
            conn,
            "ws",
            &[
                Status::Active,
                Status::Stale,
                Status::Superseded,
                Status::Archived,
            ],
        )
        .unwrap()
    }

    #[test]
    fn graph_drops_dangling_links() {
        let conn = db();
        apply(
            &conn,
            "ws",
            None,
            None,
            &NoteOp::Remember {
                title: "a".into(),
                body: "see [[b]] and [[nothing]]".into(),
                slug: Some("a".into()),
                project: None,
                summary: None,
                supersedes: None,
                status: None,
            },
            1,
        )
        .unwrap();
        apply(
            &conn,
            "ws",
            None,
            None,
            &NoteOp::Remember {
                title: "b".into(),
                body: "back to [[a]]".into(),
                slug: Some("b".into()),
                project: None,
                summary: None,
                supersedes: None,
                status: None,
            },
            2,
        )
        .unwrap();
        let g = graph_all(&conn);
        assert_eq!(g.nodes.len(), 2);
        let (a, b) = (g.nodes[0].id, g.nodes[1].id);
        assert_eq!(
            g.edges,
            [
                GraphEdge {
                    src: a,
                    dst: b,
                    kind: LinkKind::Mentions
                },
                GraphEdge {
                    src: b,
                    dst: a,
                    kind: LinkKind::Mentions
                }
            ]
        );
        let _ = json!({});
    }
}
