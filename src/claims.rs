//! Claims: what a session says it is working on, as a short `what` and
//! the paths it covers, so other sessions keep clear. One per session; it
//! ends on `release`, when `until` passes, or with the session. Kept in
//! the hub's state and, to survive a restart, in the database.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::Claim;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS claims (
    session TEXT PRIMARY KEY,
    what TEXT NOT NULL,
    paths TEXT NOT NULL,
    since INTEGER NOT NULL,
    until INTEGER
);
";

const MAX_PATHS: usize = 20;
const MAX_WHAT: usize = 200;
const MAX_PATH: usize = 500;
/// A claim lapses after at most a day, whatever `until` asked for.
pub const MAX_UNTIL_MIN: i64 = 24 * 60;
const MINUTE_MS: i64 = 60_000;

/// A claim made at `now` by a session running in `cwd`: `paths` resolve
/// against `cwd` and must stay under `root` (the project, else the
/// workspace); `until_min` is minutes from now.
pub fn new(
    what: &str,
    paths: &[String],
    until_min: Option<i64>,
    cwd: &str,
    root: &str,
    now: i64,
) -> Result<Claim, String> {
    let what = what.split_whitespace().collect::<Vec<_>>().join(" ");
    if what.is_empty() {
        return Err("say what you are working on".into());
    }
    if what.chars().count() > MAX_WHAT {
        return Err(format!("`what` is at most {MAX_WHAT} characters"));
    }
    if paths.len() > MAX_PATHS {
        return Err(format!(
            "at most {MAX_PATHS} paths; claim a directory instead"
        ));
    }
    let mut absolute: Vec<String> = Vec::new();
    for path in paths {
        let path = normalize(path, cwd)?;
        if !within(&path, root) {
            return Err(format!("{path} is outside {root}"));
        }
        if !absolute.contains(&path) {
            absolute.push(path);
        }
    }
    let until = match until_min {
        Some(min) if min <= 0 => return Err("`until` is minutes from now".into()),
        Some(min) => Some(now + min.min(MAX_UNTIL_MIN) * MINUTE_MS),
        None => None,
    };
    Ok(Claim {
        what,
        paths: absolute,
        since: now,
        until,
    })
}

/// `path` made absolute against `cwd`, without `.` parts, repeated or
/// trailing slashes; `..` is refused.
pub fn normalize(path: &str, cwd: &str) -> Result<String, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("a path is empty".into());
    }
    if path.len() > MAX_PATH {
        return Err("a path is too long".into());
    }
    let base = if path.starts_with('/') { "" } else { cwd };
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(format!("{path}: `..` is not allowed")),
            part => parts.push(part),
        }
    }
    Ok(format!("/{}", parts.join("/")))
}

/// Whether `path` is `root` or below it.
pub fn within(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether the claim covers `path` (absolute).
pub fn covers(claim: &Claim, path: &str) -> bool {
    claim.paths.iter().any(|p| within(path, p))
}

/// `path` relative to `base` when it lies under it; unchanged for an
/// empty base.
pub fn relative<'a>(path: &'a str, base: &str) -> &'a str {
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return path;
    }
    path.strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path)
}

/// `"what" (a, b; lapses in 42 min)`, paths relative to `base`.
pub fn describe(claim: &Claim, base: &str, now: i64) -> String {
    let mut details: Vec<String> = Vec::new();
    if !claim.paths.is_empty() {
        let paths: Vec<&str> = claim.paths.iter().map(|p| relative(p, base)).collect();
        details.push(paths.join(", "));
    }
    if let Some(until) = claim.until {
        let left = (until - now).max(0) / MINUTE_MS;
        details.push(format!("lapses in {left} min"));
    }
    if details.is_empty() {
        format!("\"{}\"", claim.what)
    } else {
        format!("\"{}\" ({})", claim.what, details.join("; "))
    }
}

/// Stores the claim of `session`, or removes it.
pub fn save(conn: &Connection, session: &str, claim: Option<&Claim>) -> Result<()> {
    match claim {
        Some(c) => {
            conn.execute(
                "INSERT INTO claims (session, what, paths, since, until) VALUES (?1, ?2, ?3, ?4, ?5)
                    ON CONFLICT (session) DO UPDATE SET what = excluded.what,
                    paths = excluded.paths, since = excluded.since, until = excluded.until",
                params![
                    session,
                    c.what,
                    serde_json::to_string(&c.paths)?,
                    c.since,
                    c.until
                ],
            )?;
        }
        None => {
            conn.execute("DELETE FROM claims WHERE session = ?1", [session])?;
        }
    }
    Ok(())
}

/// The stored claim of `session`, unless it has lapsed.
pub fn load(conn: &Connection, session: &str, now: i64) -> Result<Option<Claim>> {
    let claim = conn
        .query_row(
            "SELECT what, paths, since, until FROM claims WHERE session = ?1",
            [session],
            |r| {
                Ok(Claim {
                    what: r.get(0)?,
                    paths: serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or_default(),
                    since: r.get(2)?,
                    until: r.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(claim.filter(|c| c.until.is_none_or(|u| u > now)))
}

/// Drops lapsed claims and those of sessions that ended.
pub fn prune(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM claims WHERE until < ?1
            OR session IN (SELECT id FROM sessions WHERE ended_at IS NOT NULL)",
        [now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_against_cwd_and_stay_under_root() {
        let root = "/ws/repo";
        let claim = new(
            "  the hub's  reply path ",
            &[
                "src/hub/".into(),
                "./src/protocol.rs".into(),
                "/ws/repo/web".into(),
            ],
            Some(30),
            "/ws/repo",
            root,
            1_000,
        )
        .unwrap();
        assert_eq!(claim.what, "the hub's reply path");
        assert_eq!(
            claim.paths,
            [
                "/ws/repo/src/hub",
                "/ws/repo/src/protocol.rs",
                "/ws/repo/web"
            ]
        );
        assert_eq!(claim.until, Some(1_000 + 30 * MINUTE_MS));
        assert!(covers(&claim, "/ws/repo/src/hub/core.rs"));
        assert!(covers(&claim, "/ws/repo/src/protocol.rs"));
        assert!(!covers(&claim, "/ws/repo/src/hubs/x.rs"));
        assert!(!covers(&claim, "/ws/repo/src/protocol.rs.bak"));
        assert_eq!(
            describe(&claim, root, 1_000 + 5 * MINUTE_MS),
            "\"the hub's reply path\" (src/hub, src/protocol.rs, web; lapses in 25 min)"
        );

        assert!(new("x", &["../other".into()], None, "/ws/repo", root, 0).is_err());
        assert!(new("x", &["/ws/other".into()], None, "/ws/repo", root, 0).is_err());
        assert!(new("", &[], None, "/ws/repo", root, 0).is_err());
        assert!(new("x", &[], Some(0), "/ws/repo", root, 0).is_err());
        let bare = new("just thinking", &[], None, "/ws/repo/sub", root, 0).unwrap();
        assert_eq!(describe(&bare, root, 0), "\"just thinking\"");
        assert_eq!(relative("/ws/repo/web", ""), "/ws/repo/web");
        assert_eq!(relative("/ws/repo", "/ws/repo/"), "/ws/repo");
        let capped = new("x", &[], Some(10_000), "/ws/repo", root, 0).unwrap();
        assert_eq!(capped.until, Some(MAX_UNTIL_MIN * MINUTE_MS));
    }

    #[test]
    fn stored_claims_lapse_and_prune() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let claim = Claim {
            what: "x".into(),
            paths: vec!["/a".into()],
            since: 1,
            until: Some(100),
        };
        save(&conn, "s", Some(&claim)).unwrap();
        assert_eq!(load(&conn, "s", 50).unwrap(), Some(claim.clone()));
        assert_eq!(load(&conn, "s", 100).unwrap(), None);
        save(
            &conn,
            "s",
            Some(&Claim {
                until: None,
                ..claim
            }),
        )
        .unwrap();
        assert!(load(&conn, "s", 1_000).unwrap().is_some());
        conn.execute(
            "INSERT INTO sessions VALUES ('s', 'h', '/a', NULL, NULL, 'interactive', 0, 5)",
            [],
        )
        .unwrap();
        prune(&conn, 1_000).unwrap();
        assert!(load(&conn, "s", 1_000).unwrap().is_none());
        save(&conn, "s", None).unwrap();
    }
}
