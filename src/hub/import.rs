//! `ccdash import`: `db` copies the history of a single-host ccdash
//! database (what the daemon kept before the hub) into the hub's store,
//! rows keeping their `host`; `memory` and `plan` turn markdown files into
//! notes for the hub (sent through the local collector). All of it is
//! safe to rerun.

use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::{
    knowledge::{self, LinkKind, NoteKind, Status, Upsert},
    store,
    util::truncate,
};

/// `(table, columns)` copied with duplicate rows skipped.
const TABLES: &[(&str, &str)] = &[
    ("events", "ts, host, session, kind, data"),
    (
        "samples",
        "ts, host, session, context_pct, cost_usd, five_hour, seven_day, model, lines_added, lines_removed",
    ),
    (
        "proc_samples",
        "ts, host, session, cpu_pct, mem_bytes, procs",
    ),
    ("telemetry", "ts, host, session, name, data"),
    ("limit_samples", "ts, label, used_pct, resets_at"),
];

/// Imports `source` into the hub database at `dest`; returns the rows
/// added per table.
pub fn import(source: &Path, dest: &Path) -> Result<Vec<(String, usize)>> {
    if !source.is_file() {
        bail!("{} is not a file", source.display());
    }
    // NOTE: a copy made by another user is often 0600; SQLite only says "error code 14"
    std::fs::File::open(source)
        .with_context(|| format!("{} is not readable by this user", source.display()))?;
    let mut conn = store::open_writer(dest)?;
    conn.execute(
        "ATTACH DATABASE ?1 AS src",
        [source.to_string_lossy().as_ref()],
    )
    .with_context(|| format!("attaching {}", source.display()))?;
    let tx = conn.transaction()?;
    let mut added = Vec::new();
    let sessions = tx.execute(
        "INSERT OR IGNORE INTO sessions (id, host, cwd, name, title, kind, started_at, ended_at)
            SELECT id, host, cwd, name, title, kind, started_at, ended_at FROM src.sessions",
        [],
    )?;
    added.push(("sessions".to_owned(), sessions));
    store::fill_cwd_keys(&tx)?;
    for (table, columns) in TABLES {
        let on = columns
            .split(", ")
            .map(|c| format!("t.{c} IS s.{c}"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let n = tx.execute(
            &format!(
                "INSERT INTO {table} ({columns}) SELECT {columns} FROM src.{table} s
                    WHERE NOT EXISTS (SELECT 1 FROM main.{table} t WHERE {on})"
            ),
            [],
        )?;
        added.push(((*table).to_owned(), n));
    }
    tx.commit()?;
    conn.execute_batch("DETACH DATABASE src")?;
    Ok(added)
}

/// Reads the source's schema version in passing: an old database lacks
/// columns added later, which `open_writer` adds to the destination only.
pub fn check(source: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(
        source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", source.display()))?;
    for (table, columns) in TABLES {
        for column in columns.split(", ") {
            let exists: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
                [table, column],
                |r| r.get(0),
            )?;
            if !exists {
                bail!("{}: {table} has no column {column}", source.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    #[test]
    fn imports_once() {
        let dir = std::env::temp_dir().join(format!("ccdash-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (src, dst) = (dir.join("old.db"), dir.join("hub.db"));
        {
            let conn = store::open_writer(&src).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, host, cwd, kind, started_at) VALUES ('s1', 'navi', '/w', 'interactive', 10)",
                [],
            )
            .unwrap();
            conn.execute(
                r"INSERT INTO sessions (id, host, cwd, kind, started_at) VALUES ('s2', 'win', 'C:\Users\U\w', 'interactive', 10)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (ts, host, session, kind, data) VALUES (11, 'navi', 's1', 'prompt', '{}')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO samples (ts, host, session, context_pct) VALUES (12, 'navi', 's1', 3.5)",
                [],
            )
            .unwrap();
        }
        check(&src).unwrap();
        let first = import(&src, &dst).unwrap();
        // NOTE: the copied sessions get their keys, so scoped queries find them
        let key: String = rusqlite::Connection::open(&dst)
            .unwrap()
            .query_row("SELECT cwd_key FROM sessions WHERE id = 's2'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(key, "c:/users/u/w");
        assert_eq!(
            first,
            [
                ("sessions".to_owned(), 2),
                ("events".to_owned(), 1),
                ("samples".to_owned(), 1),
                ("proc_samples".to_owned(), 0),
                ("telemetry".to_owned(), 0),
                ("limit_samples".to_owned(), 0),
            ]
        );
        let again = import(&src, &dst).unwrap();
        assert!(again.iter().all(|(_, n)| *n == 0), "{again:?}");
        let conn = Connection::open(&dst).unwrap();
        let host: String = conn
            .query_row("SELECT host FROM events WHERE session = 's1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(host, "navi");
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM samples WHERE context_pct = ?1",
                params![3.5],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}

// --- knowledge: memory files and plans ----------------------------------

/// `ccdash import memory <dir>`: every `<name>.md` of a workspace's
/// `.claude/memory` becomes a memory note keyed by its `name`. The title is
/// the index line's (`MEMORY.md` or `INDEX.md`, `- [Title](file.md) — hook`),
/// the summary the frontmatter `description`, or the index hook when that
/// says more. Subdirectories are walked, each with its own index; a note
/// from one carries `memory_dir` in `extra`. A `<scope>--<topic>` name
/// whose scope is a directory of the workspace gets that project. The
/// workspace is the parent of the `.claude` ancestor, so a subdirectory
/// imports on its own too; `status: archived` in the frontmatter retires the
/// note. Safe to rerun: unchanged files change nothing,
/// and the hub keeps a note it changed after the file.
pub fn memory_dir(dir: &Path) -> Result<Vec<Upsert>> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let workspace_dir = workspace_of(dir);
    let root = memory_root(dir);
    let mut notes = Vec::new();
    memory_tree(root, dir, workspace_dir, &mut notes)?;
    Ok(notes)
}

const INDEX_FILES: [&str; 2] = ["MEMORY.md", "INDEX.md"];

/// The workspace holding a memory directory: the parent of its `.claude`
/// ancestor, else two levels up.
fn workspace_of(dir: &Path) -> Option<&Path> {
    dir.ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == ".claude"))
        .and_then(Path::parent)
        .or_else(|| dir.parent().and_then(Path::parent))
}

/// The `.claude/memory` directory `dir` lies in (itself when none does),
/// against which a note's `memory_dir` is named.
fn memory_root(dir: &Path) -> &Path {
    dir.ancestors()
        .find(|a| {
            a.file_name().is_some_and(|n| n == "memory")
                && a.parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == ".claude")
        })
        .unwrap_or(dir)
}

/// The notes of one directory, then of its subdirectories (hidden ones
/// skipped).
fn memory_tree(
    root: &Path,
    dir: &Path,
    workspace_dir: Option<&Path>,
    notes: &mut Vec<Upsert>,
) -> Result<()> {
    let mut index = HashMap::new();
    for name in INDEX_FILES {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            index.extend(index_entries(&text));
        }
    }
    let subdir = dir
        .strip_prefix(root)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned());
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    let mut dirs = Vec::new();
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            if !name.starts_with('.') {
                dirs.push(path);
            }
            continue;
        }
        if !path.extension().is_some_and(|e| e == "md") || INDEX_FILES.contains(&name.as_str()) {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        if let Some(note) = memory_file(
            &text,
            &stem,
            &index,
            workspace_dir,
            mtime,
            subdir.as_deref(),
        ) {
            notes.push(note);
        } else {
            eprintln!("skipping {}: no usable content", path.display());
        }
    }
    for sub in dirs {
        memory_tree(root, &sub, workspace_dir, notes)?;
    }
    Ok(())
}

/// The directory of `workspace_dir` named like `scope`, case-insensitively
/// (`radar-bite` → `radar-BITE`), as the note's project.
fn project_dir(workspace_dir: &Path, scope: &str) -> Option<String> {
    // NOTE: listed, not `join(scope).is_dir()`: a case-insensitive file
    // system (Windows) finds `radar-bite` and the real name is lost
    let names: Vec<String> = std::fs::read_dir(workspace_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let wanted = scope.to_lowercase();
    names
        .iter()
        .find(|name| *name == scope)
        .or_else(|| names.iter().find(|name| name.to_lowercase() == wanted))
        .cloned()
}

/// One `- [Title](file.md) — hook` line of an index.
struct IndexLine {
    title: String,
    /// The recall cue after the dash; empty when the line has none.
    hook: String,
}

/// The index lines of a `MEMORY.md` or `INDEX.md`, by file stem.
fn index_entries(text: &str) -> HashMap<String, IndexLine> {
    let mut lines = HashMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- [") else {
            continue;
        };
        let Some((title, rest)) = rest.split_once("](") else {
            continue;
        };
        let Some((file, rest)) = rest.split_once(')') else {
            continue;
        };
        let stem = file.trim().trim_end_matches(".md");
        let hook = rest.trim().trim_start_matches(['—', '–', '-']).trim();
        lines.insert(
            stem.to_owned(),
            IndexLine {
                title: title.trim().to_owned(),
                hook: hook.to_owned(),
            },
        );
    }
    lines
}

/// One memory file as a note; None when it has no body.
fn memory_file(
    text: &str,
    stem: &str,
    index: &HashMap<String, IndexLine>,
    workspace_dir: Option<&Path>,
    mtime: Option<i64>,
    memory_dir: Option<&str>,
) -> Option<Upsert> {
    let (front, body) = frontmatter(text);
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let name = front
        .get("name")
        .cloned()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| stem.to_owned());
    let slug = if knowledge::valid_slug(&name) {
        name.clone()
    } else {
        knowledge::slugify(&name)
    };
    let line = index.get(stem).or_else(|| index.get(&name));
    let title = line
        .map(|l| l.title.clone())
        .or_else(|| knowledge::heading(body))
        .unwrap_or_else(|| name.replace("--", ": ").replace(['-', '_'], " "));
    let description = front.get("description").cloned().filter(|d| !d.is_empty());
    // NOTE: the index hook is the curated "when to recall" line; it wins when it says more
    let summary = match line.map(|l| l.hook.as_str()).filter(|h| !h.is_empty()) {
        Some(hook) if description.as_ref().is_none_or(|d| hook.len() > d.len()) => {
            Some(hook.to_owned())
        }
        _ => description,
    };
    let project = name
        .split_once("--")
        .map(|(scope, _)| scope)
        .and_then(|scope| workspace_dir.and_then(|dir| project_dir(dir, scope)));
    let modified = front
        .get("metadata.modified")
        .and_then(|m| crate::util::parse_rfc3339(m))
        .or(mtime);
    let mut extra = serde_json::Map::new();
    extra.insert("source".into(), json!("memory"));
    if let Some(kind) = front.get("metadata.type").or_else(|| front.get("type")) {
        extra.insert("memory_type".into(), json!(kind));
    }
    if let Some(origin) = front.get("metadata.originSessionId") {
        extra.insert("origin_session".into(), json!(origin));
    }
    if let Some(sub) = memory_dir {
        extra.insert("memory_dir".into(), json!(sub));
    }
    // NOTE: `status: archived` round-trips what `ccdash export` writes
    let status = match front.get("status").map(String::as_str) {
        Some("archived") => Status::Archived,
        _ => Status::Active,
    };
    Some(Upsert {
        slug,
        kind: NoteKind::Memory,
        project,
        title,
        summary,
        status,
        extra: Value::Object(extra),
        body: body.to_owned(),
        links: Vec::new(),
        created_at: modified,
        updated_at: modified,
        verified: true,
    })
}

/// A `---` frontmatter block as flat `key` / `parent.key` pairs, and the
/// body after it. Values are unquoted; nesting is one level (what the
/// memory files use).
pub fn frontmatter(text: &str) -> (HashMap<String, String>, &str) {
    let mut map = HashMap::new();
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (map, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (map, text);
    };
    let block = &rest[..end];
    let body = rest[end + 4..]
        .trim_start_matches(['-'])
        .trim_start_matches(['\r', '\n']);
    let mut parent: Option<String> = None;
    for line in block.lines() {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let value = unquote(value.trim());
        let key = key.trim();
        if indented && let Some(p) = &parent {
            map.insert(format!("{p}.{key}"), value);
        } else if value.is_empty() {
            parent = Some(key.to_owned());
        } else {
            parent = None;
            map.insert(key.to_owned(), value);
        }
    }
    (map, body)
}

fn unquote(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        serde_json::from_str::<String>(value)
            .unwrap_or_else(|_| value[1..value.len() - 1].to_owned())
    } else {
        value.to_owned()
    }
}

/// `ccdash import plan <file>`: the file becomes a document; unless
/// `doc_only`, every `- [ ]`/`- [x]` item an open or closed issue linked to
/// it (keyed by a hash of its text, so a rerun updates its status) and every
/// `## Log` entry (`- **YYYY-MM-DD (label):** …`) a progress note. The
/// document slug is `slug`, else the file stem, prefixed with its directory
/// when that is not `plans`.
pub fn plan_file(
    path: &Path,
    project: Option<&str>,
    slug: Option<&str>,
    doc_only: bool,
) -> Result<Vec<Upsert>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .context("no file name")?;
    let parent = path
        .parent()
        .and_then(Path::file_name)
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|p| p != "plans" && !p.is_empty());
    let slug = match (slug, parent) {
        (Some(s), _) if knowledge::valid_slug(s) => s.to_owned(),
        (Some(s), _) => knowledge::slugify(s),
        (None, Some(p)) => format!("{}--{}", knowledge::slugify(&p), knowledge::slugify(&stem)),
        (None, None) => knowledge::slugify(&stem),
    };
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    Ok(plan_text(&text, &slug, project, mtime, doc_only))
}

pub fn plan_text(
    text: &str,
    slug: &str,
    project: Option<&str>,
    mtime: Option<i64>,
    doc_only: bool,
) -> Vec<Upsert> {
    let title = knowledge::heading(text).unwrap_or_else(|| slug.to_owned());
    let mut notes = vec![Upsert {
        slug: slug.to_owned(),
        kind: NoteKind::Doc,
        project: project.map(str::to_owned),
        title: title.clone(),
        summary: None,
        status: Status::Active,
        extra: json!({ "source": if doc_only { "file" } else { "plan" } }),
        body: text.trim_end().to_owned(),
        links: Vec::new(),
        created_at: None,
        updated_at: mtime,
        verified: false,
    }];
    if doc_only {
        return notes;
    }
    let mut section = String::new();
    let mut in_log = false;
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(h) = line
            .strip_prefix("## ")
            .or_else(|| line.strip_prefix("### "))
        {
            section = h.trim().to_owned();
            in_log = h.trim().eq_ignore_ascii_case("log");
            i += 1;
            continue;
        }
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let is_item = trimmed.starts_with("- ") || trimmed.starts_with("* ");
        if !is_item {
            i += 1;
            continue;
        }
        // The item with its continuation lines (indented deeper).
        let mut item = trimmed[2..].trim().to_owned();
        let mut j = i + 1;
        while j < lines.len() {
            let next = lines[j];
            let next_trim = next.trim_start();
            let next_indent = next.len() - next_trim.len();
            if next_trim.is_empty() || next_indent <= indent || next.starts_with('#') {
                break;
            }
            if next_trim.starts_with("- ") || next_trim.starts_with("* ") {
                if next_indent <= indent + 2 {
                    break;
                }
                item.push('\n');
                item.push_str(&" ".repeat(next_indent - indent - 2));
                item.push_str(next_trim);
            } else {
                item.push(' ');
                item.push_str(next_trim);
            }
            j += 1;
        }
        if in_log {
            if let Some(note) = log_entry(&item, slug, project) {
                notes.push(note);
            }
        } else if let Some(rest) = item.strip_prefix("[ ] ").map(|r| (false, r)).or_else(|| {
            item.strip_prefix("[x] ")
                .or_else(|| item.strip_prefix("[X] "))
                .map(|r| (true, r))
        }) {
            let (checked, body) = rest;
            notes.push(checkbox(
                body, checked, slug, &section, &title, project, mtime,
            ));
        }
        i = j;
    }
    notes
}

fn checkbox(
    text: &str,
    checked: bool,
    doc: &str,
    section: &str,
    doc_title: &str,
    project: Option<&str>,
    mtime: Option<i64>,
) -> Upsert {
    let first = text.lines().next().unwrap_or_default().trim();
    let plain = first.replace("**", "");
    let head = truncate(&plain, 100);
    let title = if section.is_empty() {
        head
    } else {
        format!("{}: {head}", truncate(section, 40))
    };
    let mut extra = json!({ "source": "plan", "doc": doc, "checked": checked });
    if !section.is_empty() {
        extra["section"] = json!(section);
    }
    Upsert {
        // NOTE: the same line under two headings is two issues
        slug: format!("{doc}--{}", fnv(&normalize(&format!("{section}\n{text}")))),
        kind: NoteKind::Issue,
        project: project.map(str::to_owned),
        title,
        summary: None,
        status: if checked {
            Status::Archived
        } else {
            Status::Active
        },
        extra,
        body: format!(
            "{text}\n\nFrom [[{doc}]] ({doc_title}{}).",
            if section.is_empty() {
                String::new()
            } else {
                format!(", {section}")
            }
        ),
        links: vec![(doc.to_owned(), LinkKind::Relates)],
        created_at: None,
        updated_at: mtime,
        verified: false,
    }
}

/// `**YYYY-MM-DD (label):** text` or `**YYYY-MM-DD:** text`.
fn log_entry(item: &str, doc: &str, project: Option<&str>) -> Option<Upsert> {
    let rest = item.strip_prefix("**")?;
    let (head, text) = rest.split_once(":**")?;
    let (date, label) = match head.split_once(" (") {
        Some((d, l)) => (d.trim(), Some(l.trim_end_matches(')').trim())),
        None => (head.trim(), None),
    };
    if date.len() != 10 || !date.as_bytes()[4] == b'-' {
        return None;
    }
    let at = crate::util::parse_rfc3339(&format!("{date}T12:00:00Z"))?;
    let text = text.trim();
    let title = match label {
        Some(l) if !l.is_empty() => format!("{date} {l}"),
        _ => format!(
            "{date} {}",
            truncate(text.lines().next().unwrap_or_default(), 60)
        ),
    };
    Some(Upsert {
        slug: format!("{doc}--log-{date}-{}", &fnv(&normalize(text))[..6]),
        kind: NoteKind::Progress,
        project: project.map(str::to_owned),
        title,
        summary: None,
        status: Status::Active,
        extra: json!({ "source": "plan", "doc": doc }),
        body: format!("{text}\n\nFrom the log of [[{doc}]]."),
        links: vec![(doc.to_owned(), LinkKind::Relates)],
        created_at: Some(at),
        updated_at: Some(at),
        verified: false,
    })
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .replace("[x]", "[ ]")
}

/// 32-bit FNV-1a, hex: a stable short key for an item's text.
fn fnv(text: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in text.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

#[cfg(test)]
mod knowledge_tests {
    use super::*;

    #[test]
    fn memory_files_become_notes() {
        let text = "---\nname: ccdash--project\ndescription: ccdash — the dashboard\nmetadata:\n  node_type: memory\n  type: project\n  modified: 2026-10-10T23:38:34.851Z\n---\n\n`ccdash/` is a dashboard. See [[workspace--new-repo-scaffold]].\n";
        let (front, body) = frontmatter(text);
        assert_eq!(front["name"], "ccdash--project");
        assert_eq!(front["metadata.type"], "project");
        assert!(body.starts_with("`ccdash/`"));
        let dir = std::env::temp_dir().join(format!("ccdash-memimport-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mem = dir.join(".claude/memory");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::create_dir_all(dir.join("ccdash")).unwrap();
        std::fs::create_dir_all(dir.join("radar-BITE")).unwrap();
        std::fs::write(mem.join("ccdash--project.md"), text).unwrap();
        std::fs::write(
            mem.join("radar-bite--x.md"),
            "---\nname: radar-bite--x\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(mem.join("loose.md"), "no frontmatter, just text\n").unwrap();
        std::fs::write(mem.join("empty.md"), "---\nname: empty\n---\n").unwrap();
        std::fs::write(
            mem.join("MEMORY.md"),
            "# Index\n\n- [ccdash](ccdash--project.md) — the hook\n",
        )
        .unwrap();
        // a store of its own in a subdirectory, indexed by INDEX.md
        let so = mem.join("sys-operativa");
        std::fs::create_dir_all(so.join(".cache")).unwrap();
        std::fs::write(
            so.join("INDEX.md"),
            "# SO\n\n## Entries\n\n- [uzb2 disks](uzb2--disks.md) — NVMe dies recurrently; reboot revives; death tell = ext4 -5 spam\n- [BITE y](radar-bite--y.md) - short\n",
        )
        .unwrap();
        std::fs::write(
            so.join("uzb2--disks.md"),
            "---\nname: uzb2--disks\ndescription: uzb2 disk failure\nmetadata:\n  type: project\n---\n\nSO-106.\n",
        )
        .unwrap();
        std::fs::write(
            so.join("radar-bite--y.md"),
            "---\nname: radar-bite--y\ndescription: a description longer than the hook\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(so.join(".cache/z.md"), "---\nname: z\n---\nhidden\n").unwrap();
        std::fs::write(
            so.join("old--gone.md"),
            "---\nname: old--gone\nstatus: archived\n---\nsuperseded by uzb2--disks\n",
        )
        .unwrap();
        let notes = memory_dir(&mem).unwrap();
        assert_eq!(notes.len(), 6);
        let gone = notes.iter().find(|n| n.slug == "old--gone").unwrap();
        assert_eq!(gone.status, Status::Archived);
        assert!(
            notes
                .iter()
                .filter(|n| n.slug != "old--gone")
                .all(|n| n.status == Status::Active)
        );
        let bite = notes.iter().find(|n| n.slug == "radar-bite--x").unwrap();
        assert_eq!(bite.project.as_deref(), Some("radar-BITE"));
        let n = notes.iter().find(|n| n.slug == "ccdash--project").unwrap();
        assert_eq!(n.title, "ccdash");
        assert_eq!(n.project.as_deref(), Some("ccdash"));
        assert_eq!(n.summary.as_deref(), Some("ccdash — the dashboard"));
        assert_eq!(n.extra["memory_type"], "project");
        assert!(n.extra.get("memory_dir").is_none());
        assert_eq!(n.created_at, Some(1_791_675_514_851));
        assert!(n.verified);
        let loose = notes.iter().find(|n| n.slug == "loose").unwrap();
        assert_eq!(loose.title, "loose");
        assert!(loose.project.is_none());
        let uzb2 = notes.iter().find(|n| n.slug == "uzb2--disks").unwrap();
        assert_eq!(uzb2.title, "uzb2 disks");
        assert_eq!(
            uzb2.summary.as_deref(),
            Some("NVMe dies recurrently; reboot revives; death tell = ext4 -5 spam"),
            "the longer index hook is the summary"
        );
        assert_eq!(uzb2.extra["memory_dir"], "sys-operativa");
        assert!(uzb2.project.is_none());
        let y = notes.iter().find(|n| n.slug == "radar-bite--y").unwrap();
        assert_eq!(y.title, "BITE y");
        assert_eq!(
            y.summary.as_deref(),
            Some("a description longer than the hook")
        );
        assert_eq!(y.project.as_deref(), Some("radar-BITE"));
        // the subdirectory on its own: same workspace, same memory_dir
        let alone = memory_dir(&so).unwrap();
        assert_eq!(alone.len(), 3);
        let y = alone.iter().find(|n| n.slug == "radar-bite--y").unwrap();
        assert_eq!(y.project.as_deref(), Some("radar-BITE"));
        assert_eq!(y.extra["memory_dir"], "sys-operativa");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn index_lines_carry_title_and_hook() {
        let lines = index_entries(
            "- [A](a.md) — the hook\n- [B](b.md) - dash hook\n- [C](c.md)\n- plain\n",
        );
        assert_eq!(
            (lines["a"].title.as_str(), lines["a"].hook.as_str()),
            ("A", "the hook")
        );
        assert_eq!(lines["b"].hook, "dash hook");
        assert_eq!(
            (lines["c"].title.as_str(), lines["c"].hook.as_str()),
            ("C", "")
        );
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn plans_split_into_doc_issues_and_progress() {
        let text = "# ccdash: live dashboard\n\nIntro.\n\n## Milestones\n\n### M1: collector\n\n- [x] crate modules: model, ingest\n      and more\n- [ ] **STOP → owner:** commit + push\n- plain bullet\n\n## Log\n\n- **2026-10-18 (M3.8 live):** deployed; watch answers\n  verified.\n- **2026-10-11:** brainstorm and M0.\n- not an entry\n";
        let notes = plan_text(text, "ccdash", Some("ccdash"), Some(5), false);
        assert_eq!(notes[0].kind, NoteKind::Doc);
        assert_eq!(notes[0].title, "ccdash: live dashboard");
        assert_eq!(notes[0].extra["source"], "plan");
        let only = plan_text(text, "ccdash", Some("ccdash"), Some(5), true);
        assert_eq!(only.len(), 1, "--doc: the document alone");
        assert_eq!(only[0].extra["source"], "file");
        let issues: Vec<&Upsert> = notes.iter().filter(|n| n.kind == NoteKind::Issue).collect();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].status, Status::Archived);
        assert_eq!(
            issues[0].title,
            "M1: collector: crate modules: model, ingest and more"
        );
        assert!(
            issues[0]
                .body
                .contains("From [[ccdash]] (ccdash: live dashboard, M1: collector).")
        );
        assert_eq!(issues[1].status, Status::Active);
        assert_eq!(
            issues[1].title,
            "M1: collector: STOP → owner: commit + push"
        );
        assert!(issues[1].slug.starts_with("ccdash--"));
        let again = plan_text(
            &text.replace("- [ ] **STOP", "- [x] **STOP"),
            "ccdash",
            Some("ccdash"),
            Some(6),
            false,
        );
        let closed = again
            .iter()
            .filter(|n| n.kind == NoteKind::Issue)
            .nth(1)
            .unwrap();
        assert_eq!(
            (closed.slug.as_str(), closed.status),
            (issues[1].slug.as_str(), Status::Archived),
            "same key, new status"
        );
        let progress: Vec<&Upsert> = notes
            .iter()
            .filter(|n| n.kind == NoteKind::Progress)
            .collect();
        assert_eq!(progress.len(), 2);
        assert_eq!(progress[0].title, "2026-10-18 M3.8 live");
        assert!(
            progress[0]
                .body
                .starts_with("deployed; watch answers verified.")
        );
        assert_eq!(progress[0].created_at, Some(1_792_324_800_000));
        assert_eq!(progress[1].title, "2026-10-11 brainstorm and M0.");
        assert!(progress[1].slug.starts_with("ccdash--log-2026-10-11-"));
        assert!(
            notes.iter().all(|n| n.kind == NoteKind::Doc
                || n.links == [("ccdash".to_owned(), LinkKind::Relates)])
        );
    }
}
