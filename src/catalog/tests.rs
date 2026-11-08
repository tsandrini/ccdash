//! The catalog end to end: transcripts written through the sync index, a
//! pass, the list, an overview, a conversation; appends from another
//! machine and a replaced transcript.

use std::{collections::HashSet, fs, path::PathBuf};

use rusqlite::{Connection, params};
use serde_json::json;

use super::{Catalog, Query, Sort, conversation, detail, find, index, list};
use crate::{
    sync::hub::{Put, Tree, put},
    workspace::Workspaces,
};

const A: &str = "1ee78b00-59c7-4bf5-b462-ef39161f7d8d";
const B: &str = "2ee78b00-59c7-4bf5-b462-ef39161f7d8d";

struct Fixture {
    dir: PathBuf,
    store: Connection,
    tree: Tree,
    catalog: Catalog,
    workspaces: Workspaces,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("ccdash-catalog-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = crate::store::open_writer(&dir.join("ccdash.db")).unwrap();
        let tree = Tree::open(&dir.join("claude")).unwrap();
        let catalog = Catalog::open(&dir.join("catalog.db"), &dir.join("ccdash.db")).unwrap();
        Self {
            store,
            tree,
            catalog,
            workspaces: Workspaces::new(&["/home/me/w".into()], "/home/me"),
            dir,
        }
    }

    /// Appends `lines` to the transcript of `id` under `dir`, as `host`'s
    /// collector would; `replace` writes it anew instead.
    fn write(
        &mut self,
        host: &str,
        dir: &str,
        id: &str,
        lines: &[serde_json::Value],
        replace: bool,
    ) {
        let path = format!("projects/{dir}/{id}.jsonl");
        let row: Option<(i64, i64)> = self
            .store
            .query_row(
                "SELECT size, version FROM sync_files WHERE path = ?1",
                [&path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let (offset, expect) = match row {
            Some((size, version)) if !replace => (size as u64, version),
            Some((_, version)) => (0, version),
            None => (0, 0),
        };
        let mut data = Vec::new();
        for line in lines {
            data.extend(serde_json::to_vec(line).unwrap());
            data.push(b'\n');
        }
        let done = put(
            &mut self.store,
            &self.tree,
            host,
            &path,
            offset,
            expect,
            &data,
            None,
            1,
        )
        .unwrap();
        assert!(matches!(done, Put::Done(_)));
    }

    fn pass(&self) -> usize {
        index::pass_now(&self.catalog, &self.dir.join("claude"))
    }

    fn list(&self, q: Query) -> super::Page {
        let conn = self.catalog.reader().unwrap();
        list(
            &conn,
            &self.workspaces,
            &HashSet::new(),
            &q,
            ms("2026-10-23T00:00:00Z"),
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn ms(at: &str) -> i64 {
    crate::util::parse_rfc3339(at).unwrap()
}

fn prompt(at: &str, cwd: &str, text: &str) -> serde_json::Value {
    json!({"type": "user", "timestamp": at, "cwd": cwd, "gitBranch": "main", "version": "2.1.312",
        "entrypoint": "cli", "promptSource": "typed", "message": {"role": "user", "content": text}})
}

fn reply(at: &str, text: &str) -> serde_json::Value {
    json!({"type": "assistant", "timestamp": at, "message": {"model": "claude-opus-5-5",
        "content": [{"type": "text", "text": text},
            {"type": "tool_use", "name": "Edit", "input": {"file_path": "/home/me/w/proj/src/main.rs"}}]}})
}

fn q(text: &str) -> Query {
    Query {
        text: (!text.is_empty()).then(|| text.to_owned()),
        limit: 50,
        ..Query::default()
    }
}

fn seed(f: &mut Fixture) {
    f.write(
        "navi",
        "-home-me-w-proj",
        A,
        &[
            prompt("2026-10-20T09:59:00Z", "/home/me/w/proj", "/model"),
            prompt("2026-10-20T10:00:00Z", "/home/me/w/proj", "Find me a webcam on alza"),
            reply("2026-10-20T10:01:00Z", "Here are three **webcams** worth a look."),
            json!({"type": "ai-title", "aiTitle": "Webcam shopping"}),
            json!({"type": "cost-state", "totalCostUSD": 0.5, "totalLinesAdded": 3, "totalLinesRemoved": 1}),
        ],
        false,
    );
    f.store
        .execute(
            "INSERT INTO sessions (id, host, cwd, kind, started_at, project) VALUES (?1, 'flat', '/home/me/w', 'interactive', 0, 'ccdash')",
            params![B],
        )
        .unwrap();
    f.write(
        "navi",
        "-home-me-w",
        B,
        &[
            prompt(
                "2026-10-21T09:00:00Z",
                "/home/me/w",
                "Fix the nix flake check",
            ),
            reply("2026-10-21T09:05:00Z", "The flake check passes now."),
        ],
        false,
    );
}

#[test]
fn builds_lists_and_searches() {
    let mut f = Fixture::new("list");
    seed(&mut f);
    assert_eq!(f.pass(), 2);
    assert_eq!(f.pass(), 0, "nothing changed");

    let page = f.list(q(""));
    assert_eq!(page.total, 2);
    assert_eq!(page.matched, None);
    let ids: Vec<&str> = page.sessions.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, [B, A], "last active first");
    let a = &page.sessions[1];
    assert_eq!(a.title.as_deref(), Some("Webcam shopping"));
    assert_eq!(a.workspace.as_deref(), Some("w"));
    assert_eq!(a.project.as_deref(), Some("proj"));
    assert_eq!(a.host, "navi");
    assert_eq!((a.prompts, a.replies, a.tools), (2, 1, 1));
    assert_eq!(a.first_prompt.as_deref(), Some("Find me a webcam on alza"));
    assert_eq!(a.cost_usd, Some(0.5));
    assert_eq!(a.model.as_deref(), Some("claude-opus-5-5"));
    assert!(!a.known);
    // NOTE: the store first saw B on flat; the hub took its transcript from navi
    let b = &page.sessions[0];
    assert_eq!(
        (b.host.as_str(), b.project.as_deref(), b.known),
        ("flat", Some("ccdash"), true)
    );
    let hosts: Vec<(&str, usize)> = page
        .facets
        .hosts
        .iter()
        .map(|h| (h.host.as_str(), h.count))
        .collect();
    assert_eq!(hosts, [("flat", 1), ("navi", 1)]);
    let keys: Vec<&str> = page
        .facets
        .projects
        .iter()
        .map(|p| p.key.as_str())
        .collect();
    assert_eq!(keys, ["w/ccdash", "w/proj"]);

    let page = f.list(q("webcam"));
    assert_eq!(page.matched.as_deref(), Some("all"));
    assert_eq!(page.total, 1);
    let snippet = &page.sessions[0].snippets[0];
    assert!(snippet.snippet.contains("\u{2}webcam\u{3}"), "{snippet:?}");
    assert_eq!(snippet.role, "you");

    let page = f.list(q("webcam flake"));
    assert_eq!(page.matched.as_deref(), Some("any"));
    assert_eq!(page.total, 2);

    let page = f.list(q("shopping"));
    assert_eq!(page.total, 1, "titles are searched");
    assert_eq!(page.sessions[0].snippets[0].role, "title");

    let page = f.list(Query {
        hosts: vec!["flat".into()],
        ..q("")
    });
    assert_eq!(page.total, 1);
    assert_eq!(page.sessions[0].id, B);
    assert_eq!(
        page.facets.hosts.len(),
        2,
        "the machine facet ignores its own filter"
    );

    let page = f.list(Query {
        project: Some("w/proj".into()),
        sort: Sort::Cost,
        ..q("")
    });
    assert_eq!(page.total, 1);
    assert_eq!(page.sessions[0].id, A);

    let page = f.list(Query {
        since: Some(ms("2026-10-21T00:00:00Z")),
        ..q("")
    });
    assert_eq!(page.total, 1);

    let page = f.list(Query {
        workspaces: vec!["elsewhere".into()],
        ..q("")
    });
    assert_eq!(page.total, 0);
}

#[test]
fn follows_appends_and_replacements() {
    let mut f = Fixture::new("follow");
    seed(&mut f);
    f.pass();
    // resumed on flat: same transcript, appended there
    f.write(
        "flat",
        "-home-me-w-proj",
        A,
        &[
            prompt(
                "2026-10-22T08:00:00Z",
                "/home/me/w/proj",
                "Now compare their prices",
            ),
            reply("2026-10-22T08:01:00Z", "The cheapest is the second one."),
            json!({"type": "custom-title", "customTitle": "Webcams, priced"}),
        ],
        false,
    );
    assert_eq!(f.pass(), 1);
    let conn = f.catalog.reader().unwrap();
    let a = find(&conn, &f.workspaces, &HashSet::new(), A)
        .unwrap()
        .unwrap();
    assert_eq!(a.host, "flat");
    assert_eq!(a.hosts, ["flat", "navi"]);
    assert_eq!(a.prompts, 3);
    assert_eq!(a.title.as_deref(), Some("Webcams, priced"));
    assert_eq!(a.first_prompt.as_deref(), Some("Find me a webcam on alza"));
    assert_eq!(a.last_prompt.as_deref(), Some("Now compare their prices"));
    assert_eq!(
        a.last_reply.as_deref(),
        Some("The cheapest is the second one.")
    );

    let d = detail(&conn, &f.workspaces, &HashSet::new(), A, Some("cheapest"))
        .unwrap()
        .unwrap();
    assert_eq!(d.prompts.len(), 3);
    assert_eq!(d.first_prompt.unwrap().text, "Find me a webcam on alza");
    assert_eq!(d.last_prompt.unwrap().text, "Now compare their prices");
    assert!(d.last_reply_html.unwrap().contains("cheapest"));
    assert_eq!(d.files, [("src/main.rs".to_owned(), 2)]);
    assert_eq!(d.matches.len(), 1);
    assert_eq!(d.matches[0].role, "claude");

    let c = conversation(&conn, A, None, None, 50).unwrap().unwrap();
    assert_eq!(c.total, 5);
    let roles: Vec<&str> = c.items.iter().map(|i| i.role.as_str()).collect();
    assert_eq!(roles, ["you", "you", "claude", "you", "claude"]);
    assert!(c.items[2].html.contains("<strong>webcams</strong>"));
    let around = conversation(&conn, A, None, Some(c.items[4].id), 2)
        .unwrap()
        .unwrap();
    assert_eq!(around.offset, 1, "three texts before it");

    // replaced (a conflict resolved the other way): read again from the start
    f.write(
        "navi",
        "-home-me-w-proj",
        A,
        &[prompt(
            "2026-10-20T10:00:00Z",
            "/home/me/w/proj",
            "Only this prompt",
        )],
        true,
    );
    assert_eq!(f.pass(), 1);
    let a = find(&conn, &f.workspaces, &HashSet::new(), A)
        .unwrap()
        .unwrap();
    assert_eq!((a.prompts, a.replies), (1, 0));
    assert_eq!(a.title, None);
    assert_eq!(f.list(q("webcam")).total, 0, "old texts left the index");
    assert_eq!(f.list(q("only")).total, 1);
}

#[test]
fn skips_subagents_and_empty_sessions_and_partial_lines() {
    let mut f = Fixture::new("skip");
    f.write(
        "navi",
        &format!("-home-me-w/{A}/subagents"),
        "agent-1",
        &[prompt("2026-10-20T10:00:00Z", "/home/me/w", "sub")],
        false,
    );
    f.write(
        "navi",
        "-home-me-w",
        B,
        &[json!({"type": "ai-title", "aiTitle": "nothing said"})],
        false,
    );
    f.pass();
    assert_eq!(f.list(q("")).total, 0);

    // a line cut by the sync: read once whole
    let path = format!("projects/-home-me-w/{A}.jsonl");
    let line = serde_json::to_vec(&prompt("2026-10-20T10:00:00Z", "/home/me/w", "halves")).unwrap();
    let (head, tail) = line.split_at(20);
    put(&mut f.store, &f.tree, "navi", &path, 0, 0, head, None, 1).unwrap();
    f.pass();
    assert_eq!(f.list(q("")).total, 0);
    let version: i64 = f
        .store
        .query_row(
            "SELECT version FROM sync_files WHERE path = ?1",
            [&path],
            |r| r.get(0),
        )
        .unwrap();
    let mut rest = tail.to_vec();
    rest.push(b'\n');
    put(
        &mut f.store,
        &f.tree,
        "navi",
        &path,
        head.len() as u64,
        version,
        &rest,
        None,
        1,
    )
    .unwrap();
    f.pass();
    let page = f.list(q("halves"));
    assert_eq!(page.total, 1);
    assert_eq!(page.sessions[0].prompts, 1);
}

#[test]
fn a_new_parser_rebuilds() {
    let mut f = Fixture::new("rebuild");
    seed(&mut f);
    f.pass();
    let writer = Connection::open(f.catalog.path()).unwrap();
    writer
        .execute("UPDATE catalog_meta SET value = 0 WHERE key = 'parser'", [])
        .unwrap();
    drop(writer);
    let catalog = Catalog::open(f.catalog.path(), &f.dir.join("ccdash.db")).unwrap();
    let conn = catalog.reader().unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM catalog", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(index::pass_now(&catalog, &f.dir.join("claude")), 2);
}

#[cfg(unix)]
#[test]
fn the_catalog_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new("private");
    seed(&mut f);
    f.pass();
    let mode = fs::metadata(f.catalog.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_copy_under_another_directory_is_one_session() {
    let mut f = Fixture::new("copy");
    seed(&mut f);
    f.write(
        "navi",
        "-home-me-w-other",
        A,
        &[prompt(
            "2026-10-19T10:00:00Z",
            "/home/me/w/other",
            "Find me a webcam on alza",
        )],
        false,
    );
    f.pass();
    let page = f.list(q(""));
    assert_eq!(page.total, 2);
    let a = page.sessions.iter().find(|s| s.id == A).unwrap();
    assert_eq!(a.cwd, "/home/me/w/proj", "the newest copy");
}
