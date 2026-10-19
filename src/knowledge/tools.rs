//! The knowledge tools a session calls through `ccdash channel`: their MCP
//! definitions, the turning of arguments into hub calls, and the text a
//! tool answers with.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{Hit, NoteKind, NoteOp, NoteView, Priority, Query, Status, Written, ymd};
use crate::{
    protocol::{Call, Injection, Reply, Who},
    util::truncate,
};

/// Tool names, in the order `tools/list` shows them.
#[cfg(test)]
pub const NAMES: [&str; 10] = [
    "recall",
    "remember",
    "verify",
    "issue_add",
    "issue_update",
    "progress",
    "doc_write",
    "tell_session",
    "claim",
    "release",
];

/// The `tools/list` entries.
pub fn definitions() -> Vec<Value> {
    let string = |desc: &str| json!({ "type": "string", "description": desc });
    let slug = |desc: &str| json!({ "type": "string", "description": format!("{desc} Lowercase words joined with '-'."), "pattern": "^[a-z0-9][a-z0-9._-]*$" });
    vec![
        json!({
            "name": "recall",
            "description": "Search this workspace's knowledge in ccdash (memories, issues, progress, docs), \
                or read one note in full by slug (with its links and backlinks). Use it before asking \
                the user about past decisions, quirks or ongoing work. Reads count as recalls, which \
                keep a memory alive.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": string("Free text to search for (title, summary and body; prefix matching)."),
                    "slug": slug("Read this note in full instead of searching."),
                    "kinds": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["memory", "issue", "progress", "doc"] },
                        "description": "Restrict a search to these kinds."
                    },
                    "project": string("Restrict a search to notes of this project (repo directory name)."),
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Hits to return (default 10)." }
                }
            }
        }),
        json!({
            "name": "remember",
            "description": "Store a durable, non-obvious fact for future sessions in this workspace: dev-env \
                quirks, debugging discoveries, decisions and their reasons, user preferences. Not for \
                anything derivable from the code or specific to this conversation. Writing to an \
                existing slug replaces that memory (last write wins).",
            "inputSchema": {
                "type": "object",
                "required": ["title", "body"],
                "properties": {
                    "title": string("Short title (what it is about)."),
                    "body": string("Markdown. Say what is true, why it matters and how to apply it. Link other notes as [[slug]]."),
                    "summary": string("One line used in indexes and session-start injections; when to recall this."),
                    "slug": slug("Identifier; derived from the title when omitted. Convention: <scope>--<topic>."),
                    "project": string("Repo directory name this applies to; omit for workspace-wide facts (default: the current project)."),
                    "supersedes": slug("A memory this one replaces; it is retired with a supersedes link."),
                    "status": { "type": "string", "enum": ["active", "archived"], "description": "Use archived to retire a memory that turned out wrong." }
                }
            }
        }),
        json!({
            "name": "verify",
            "description": "Confirm that a memory is still true (it was checked against reality this session). \
                Resets its stale flag and the 90-day verification clock. Call it when a recalled or \
                injected memory proved right.",
            "inputSchema": {
                "type": "object",
                "required": ["slug"],
                "properties": { "slug": slug("The note.") }
            }
        }),
        json!({
            "name": "issue_add",
            "description": "File an issue for this workspace: a bug, a TODO the user agreed on, a follow-up, \
                a question to settle. Open issues are injected into every new session of the project.",
            "inputSchema": {
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": string("One line."),
                    "body": string("Markdown details: what, where, how to reproduce or decide. Link notes as [[slug]]."),
                    "project": string("Repo directory name (default: the current project)."),
                    "priority": { "type": "string", "enum": ["low", "normal", "high"] }
                }
            }
        }),
        json!({
            "name": "issue_update",
            "description": "Update an issue: close or reopen it, change its title, body or priority, or add a dated comment.",
            "inputSchema": {
                "type": "object",
                "required": ["slug"],
                "properties": {
                    "slug": slug("The issue."),
                    "status": { "type": "string", "enum": ["open", "closed"] },
                    "title": string("New title."),
                    "body": string("Replacement body (markdown)."),
                    "priority": { "type": "string", "enum": ["low", "normal", "high"] },
                    "comment": string("Appended to the body with today's date, e.g. what was done or found.")
                }
            }
        }),
        json!({
            "name": "progress",
            "description": "Log progress for this workspace: what landed, what was decided, what is next. \
                A dated entry other sessions and the user see; link it to an issue when it is about one.",
            "inputSchema": {
                "type": "object",
                "required": ["text"],
                "properties": {
                    "text": string("Markdown; the first line becomes the title."),
                    "issue": slug("The issue this progress is on."),
                    "project": string("Repo directory name (default: the current project, or the issue's).")
                }
            }
        }),
        json!({
            "name": "doc_write",
            "description": "Create or update a document (a plan, a spec, a runbook) shared by sessions. Updating \
                needs base_version, the version you read (recall returns it); a stale version is refused \
                so nobody's edit is lost silently: re-read, merge, retry.",
            "inputSchema": {
                "type": "object",
                "required": ["slug", "body"],
                "properties": {
                    "slug": slug("The document."),
                    "title": string("Title (default: the first # heading, or the slug)."),
                    "body": string("The whole document, markdown."),
                    "base_version": { "type": "integer", "minimum": 1, "description": "The version this edit is based on; omit only when creating." }
                }
            }
        }),
        json!({
            "name": "tell_session",
            "description": "Send a message to another running Claude Code session of this workspace. It is \
                handed to that session as context with its next prompt (it never starts a turn there), \
                so use it to hand over a finding, warn about a file you are about to change, or answer \
                a message you got. Name the session by its id (the prefix shown next to it).",
            "inputSchema": {
                "type": "object",
                "required": ["session", "text"],
                "properties": {
                    "session": string("Session id (or unique prefix) of the recipient."),
                    "text": string("The message.")
                }
            }
        }),
        json!({
            "name": "claim",
            "description": "Say what this session is working on so other sessions of the project keep clear: \
                a short description and, when it helps, the files or directories it covers. The other \
                sessions see it with their prompts and get a warning before editing a claimed file. \
                Calling it again replaces the claim; it ends with release, after `until`, or when the \
                session ends.",
            "inputSchema": {
                "type": "object",
                "required": ["what"],
                "properties": {
                    "what": string("What you are working on, in a few words."),
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Files or directories the work touches, relative to the project (a directory covers everything under it); at most 20."
                    },
                    "until": { "type": "integer", "minimum": 1, "maximum": 1440, "description": "Minutes until the claim lapses by itself (default: until released or the session ends)." },
                    "issue": slug("The open issue this work is on (as `recall` lists them); the Projects board shows it in progress with this session.")
                }
            }
        }),
        json!({
            "name": "release",
            "description": "Drop this session's claim once the work it covered is done or handed over.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

/// The hub call for a tool invocation.
pub fn call(name: &str, args: &Value, who: Who) -> Result<Call> {
    let text = |key: &str| -> Option<String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let need = |key: &str| text(key).ok_or_else(|| anyhow::anyhow!("{key} is required"));
    let priority = |key: &str| -> Result<Option<Priority>> {
        match text(key) {
            None => Ok(None),
            Some(p) => Priority::parse(&p)
                .map(Some)
                .ok_or_else(|| anyhow::anyhow!("priority is low, normal or high, not {p:?}")),
        }
    };
    let op = match name {
        "recall" => {
            let kinds = args["kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|k| NoteKind::parse(k).ok_or_else(|| anyhow::anyhow!("unknown kind {k:?}")))
                .collect::<Result<Vec<_>>>()?;
            let query = Query {
                slug: text("slug"),
                text: text("query"),
                kinds,
                project: text("project"),
                limit: args["limit"].as_u64().map(|l| l as usize),
            };
            if query.slug.is_none() && query.text.is_none() && query.kinds.is_empty() {
                bail!("give a query, a slug or kinds");
            }
            return Ok(Call::Recall { who, query });
        }
        "remember" => NoteOp::Remember {
            title: need("title")?,
            body: need("body")?,
            slug: text("slug"),
            project: text("project"),
            summary: text("summary"),
            supersedes: text("supersedes"),
            status: match text("status") {
                None => None,
                Some(s) => Some(
                    Status::parse(&s)
                        .filter(|s| matches!(s, Status::Active | Status::Archived))
                        .ok_or_else(|| anyhow::anyhow!("status is active or archived"))?,
                ),
            },
        },
        "verify" => NoteOp::Verify {
            slug: need("slug")?,
        },
        "issue_add" => NoteOp::IssueAdd {
            title: need("title")?,
            body: text("body"),
            project: text("project"),
            priority: priority("priority")?,
        },
        "issue_update" => NoteOp::IssueUpdate {
            slug: need("slug")?,
            status: text("status"),
            title: text("title"),
            body: text("body"),
            priority: priority("priority")?,
            comment: text("comment"),
        },
        "progress" => NoteOp::Progress {
            text: need("text")?,
            issue: text("issue"),
            project: text("project"),
        },
        "doc_write" => NoteOp::DocWrite {
            slug: need("slug")?,
            title: text("title"),
            body: need("body")?,
            base_version: args["base_version"].as_u64().map(|v| v as u32),
        },
        "tell_session" => {
            return Ok(Call::Tell {
                who,
                to: need("session")?,
                text: need("text")?,
            });
        }
        "claim" => {
            let paths = args["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| {
                    p.as_str()
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("paths are non-empty strings"))
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok(Call::Claim {
                who,
                what: need("what")?,
                paths,
                until: args["until"].as_i64(),
                issue: text("issue"),
            });
        }
        "release" => return Ok(Call::Release { who }),
        _ => bail!("unknown tool {name}"),
    };
    Ok(Call::Note { who, op })
}

/// Whether a call changes something (and so is queued while the hub is
/// unreachable) rather than reads.
pub fn is_write(call: &Call) -> bool {
    matches!(
        call,
        Call::Note { .. } | Call::Tell { .. } | Call::Claim { .. } | Call::Release { .. }
    )
}

/// What the tool answers for a hub reply.
pub fn text_of(reply: &Reply) -> String {
    match reply {
        Reply::Written(w) => written(w),
        Reply::Hits(hits) => hits_text(hits, "the hub"),
        Reply::Note(view) => note_text(view),
        Reply::Sent => "sent; the session gets it as context with its next prompt".into(),
        Reply::Claimed(Some(c)) => format!(
            "claimed: {}; other sessions of the project see it with their prompts",
            crate::claims::describe(c, "", crate::util::now_ms())
        ),
        Reply::Claimed(None) => "released".into(),
        Reply::Imported {
            added,
            updated,
            unchanged,
        } => format!("imported: {added} added, {updated} updated, {unchanged} unchanged"),
        Reply::Context(Some(text)) => text.clone(),
        Reply::Context(None) => "nothing to add".into(),
        Reply::Session(_) => "ok".into(),
    }
}

fn written(w: &Written) -> String {
    let what = match (w.created, w.changed) {
        (true, _) => "created",
        (false, true) => "updated",
        (false, false) => "unchanged (already stored)",
    };
    let status = match w.status {
        Status::Active if w.kind == NoteKind::Issue => "open",
        Status::Archived if w.kind == NoteKind::Issue => "closed",
        other => other.name(),
    };
    format!(
        "{what}: {} `{}` (version {}, {status})",
        w.kind, w.slug, w.version
    )
}

/// Search hits as a markdown list; `source` says where they came from.
pub fn hits_text(hits: &[Hit], source: &str) -> String {
    if hits.is_empty() {
        return format!("no matching notes in this workspace (searched {source})");
    }
    let mut out = format!(
        "{} notes (from {source}); `recall` a slug for the full text:\n",
        hits.len()
    );
    for h in hits {
        let c = &h.card;
        let status = match c.status {
            Status::Active => String::new(),
            other => format!(" [{}]", other.name()),
        };
        let project = c
            .project
            .as_deref()
            .map_or(String::new(), |p| format!(" ({p})"));
        out.push_str(&format!(
            "- **{}** `{}` {}{status}{project}, {}: {}\n",
            c.title,
            c.slug,
            c.kind,
            ymd(c.updated_at),
            truncate(&h.snippet, 300)
        ));
    }
    out
}

/// A full note as markdown with its metadata and neighbours.
pub fn note_text(v: &NoteView) -> String {
    let n = &v.note;
    let mut out = format!("# {} (`{}`)\n", n.title, n.slug);
    let mut meta = vec![
        n.kind.to_string(),
        format!("status {}", n.status.name()),
        format!("version {}", n.version),
        format!("updated {}", ymd(n.updated_at)),
    ];
    if let Some(p) = &n.project {
        meta.push(format!("project {p}"));
    }
    if let Some(r) = &n.stale_reason {
        meta.push(format!("stale: {r}"));
    }
    if let Some(v) = n.verified_at {
        meta.push(format!("verified {}", ymd(v)));
    }
    if n.kind == NoteKind::Issue {
        meta.push(format!("priority {}", n.priority().name()));
    }
    out.push_str(&format!("{}\n", meta.join(" · ")));
    if let Some(s) = &n.summary {
        out.push_str(&format!("\n> {s}\n"));
    }
    out.push_str(&format!("\n{}\n", n.body.trim_end()));
    let refs = |title: &str, list: &[super::LinkRef]| -> String {
        if list.is_empty() {
            return String::new();
        }
        let items: Vec<String> = list
            .iter()
            .map(|l| match &l.title {
                Some(t) => format!("`{}` ({t}, {})", l.slug, l.kind.name()),
                None => format!("`{}` (no such note yet)", l.slug),
            })
            .collect();
        format!("\n{title}: {}\n", items.join(", "))
    };
    out.push_str(&refs("Links", &v.links));
    out.push_str(&refs("Backlinks", &v.backlinks));
    out
}

/// The injection for a hook, or None when there is nothing to say.
pub fn injection_of(payload: &Value) -> Option<Injection> {
    let event = payload.get("hook_event_name")?.as_str()?;
    match event {
        "SessionStart" => Some(Injection::SessionStart),
        "UserPromptSubmit" => Some(Injection::Prompt { since: None }),
        "PreToolUse" => {
            let tool = payload.get("tool_name")?.as_str()?;
            if !matches!(tool, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
                return None;
            }
            let input = payload.get("tool_input")?;
            let path = input
                .get("file_path")
                .or_else(|| input.get("notebook_path"))?
                .as_str()?;
            Some(Injection::Edit {
                path: path.to_owned(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who() -> Who {
        Who {
            session: Some("s".into()),
            pid: Some(1),
            cwd: Some("/w".into()),
        }
    }

    #[test]
    fn definitions_cover_every_tool_with_schemas() {
        let defs = definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, NAMES);
        for d in &defs {
            assert_eq!(d["inputSchema"]["type"], "object", "{}", d["name"]);
            assert!(d["description"].as_str().unwrap().len() > 40);
        }
    }

    #[test]
    fn arguments_become_calls() {
        let made = call(
            "remember",
            &json!({"title": " T ", "body": "b", "priority": "ignored"}),
            who(),
        )
        .unwrap();
        assert!(
            matches!(made, Call::Note { op: NoteOp::Remember { ref title, .. }, .. } if title == "T")
        );
        assert!(is_write(&made));
        assert!(
            call("remember", &json!({"title": "T"}), who()).is_err(),
            "body required"
        );
        let recall = call(
            "recall",
            &json!({"query": "x", "kinds": ["issue"], "limit": 3}),
            who(),
        )
        .unwrap();
        assert!(!is_write(&recall));
        assert!(
            matches!(recall, Call::Recall { query: Query { kinds, limit: Some(3), .. }, .. } if kinds == [NoteKind::Issue])
        );
        assert!(call("recall", &json!({}), who()).is_err());
        assert!(call("recall", &json!({"kinds": ["nope"]}), who()).is_err());
        assert!(
            call(
                "issue_add",
                &json!({"title": "x", "priority": "urgent"}),
                who()
            )
            .is_err()
        );
        let doc = call(
            "doc_write",
            &json!({"slug": "p", "body": "b", "base_version": 2}),
            who(),
        )
        .unwrap();
        assert!(matches!(
            doc,
            Call::Note {
                op: NoteOp::DocWrite {
                    base_version: Some(2),
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            call(
                "tell_session",
                &json!({"session": "abc", "text": "hi"}),
                who()
            )
            .unwrap(),
            Call::Tell { .. }
        ));
        let claim = call(
            "claim",
            &json!({"what": "the hub", "paths": [" src/hub ", "web/app.js"], "until": 30, "issue": "hub--x"}),
            who(),
        )
        .unwrap();
        assert!(is_write(&claim));
        assert!(matches!(
            claim,
            Call::Claim { ref paths, until: Some(30), issue: Some(ref i), .. }
                if paths == &["src/hub", "web/app.js"] && i == "hub--x"
        ));
        assert!(call("claim", &json!({"paths": ["x"]}), who()).is_err());
        assert!(call("claim", &json!({"what": "x", "paths": [1]}), who()).is_err());
        assert!(matches!(
            call("release", &json!({}), who()).unwrap(),
            Call::Release { .. }
        ));
        assert!(call("nope", &json!({}), who()).is_err());
    }

    #[test]
    fn hooks_map_to_injections() {
        assert_eq!(
            injection_of(&json!({"hook_event_name": "SessionStart", "source": "startup"})),
            Some(Injection::SessionStart)
        );
        assert_eq!(
            injection_of(
                &json!({"hook_event_name": "PreToolUse", "tool_name": "Write", "tool_input": {"file_path": "/a"}})
            ),
            Some(Injection::Edit { path: "/a".into() })
        );
        assert_eq!(
            injection_of(
                &json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "ls"}})
            ),
            None
        );
        assert_eq!(injection_of(&json!({"hook_event_name": "Stop"})), None);
    }

    #[test]
    fn replies_read_well() {
        let w = Written {
            slug: "a".into(),
            kind: NoteKind::Issue,
            title: "A".into(),
            status: Status::Archived,
            version: 3,
            created: false,
            changed: true,
        };
        assert_eq!(
            text_of(&Reply::Written(w)),
            "updated: issue `a` (version 3, closed)"
        );
        assert!(text_of(&Reply::Hits(vec![])).starts_with("no matching"));
        assert_eq!(text_of(&Reply::Context(None)), "nothing to add");
    }
}
