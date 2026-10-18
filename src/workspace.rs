//! Workspaces (configured top-level directories) and the scope of history
//! and analytics queries.

use std::path::Path;

/// Configured workspace directories, named by their last path component.
#[derive(Debug, Clone, Default)]
pub struct Workspaces {
    dirs: Vec<(String, String)>,
}

impl Workspaces {
    /// `~/` expands to `home`; trailing slashes are dropped.
    pub fn new(dirs: &[String], home: &str) -> Self {
        let dirs = dirs
            .iter()
            .map(|dir| {
                let dir = match dir.strip_prefix("~/") {
                    Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
                    None => dir.clone(),
                };
                let dir = dir.trim_end_matches('/').to_owned();
                let name = Path::new(&dir)
                    .file_name()
                    .map_or_else(|| dir.clone(), |n| n.to_string_lossy().into_owned());
                (name, dir)
            })
            .collect();
        Self { dirs }
    }

    pub fn names(&self) -> Vec<String> {
        self.dirs.iter().map(|(name, _)| name.clone()).collect()
    }

    pub fn dir(&self, name: &str) -> Option<&str> {
        self.dirs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, dir)| dir.as_str())
    }

    /// The workspace holding `cwd` (the longest matching directory).
    pub fn of(&self, cwd: &str) -> Option<String> {
        self.dirs
            .iter()
            .filter(|(_, dir)| within(cwd, dir))
            .max_by_key(|(_, dir)| dir.len())
            .map(|(name, _)| name.clone())
    }
}

fn within(cwd: &str, dir: &str) -> bool {
    cwd == dir
        || cwd
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// What a history or analytics query covers.
#[derive(Debug, Clone)]
pub struct Scope {
    pub since: i64,
    pub until: i64,
    pub now: i64,
    /// Workspace directories; sessions outside all of them are left out.
    /// None covers everything, sessions outside every workspace included.
    pub dirs: Option<Vec<String>>,
    /// One machine's sessions only; None covers every host.
    pub host: Option<String>,
}

impl Scope {
    /// Whether a session on `host` running in `cwd` is in scope.
    pub fn covers(&self, host: &str, cwd: &str) -> bool {
        self.host.as_deref().is_none_or(|h| h == host)
            && self
                .dirs
                .as_ref()
                .is_none_or(|dirs| dirs.iter().any(|dir| within(cwd, dir)))
    }

    /// The directories as a JSON array for [`IN_SCOPE`]'s `:dirs`.
    pub fn dirs_param(&self) -> Option<String> {
        self.dirs
            .as_ref()
            .map(|dirs| serde_json::to_string(dirs).unwrap_or_default())
    }
}

/// SQL condition on a `sessions` row aliased `x`, bound to `:dirs`
/// ([`Scope::dirs_param`]) and `:host` ([`Scope::host`]).
pub const IN_SCOPE: &str = "((:host IS NULL OR x.host = :host)
    AND (:dirs IS NULL OR EXISTS (SELECT 1 FROM json_each(:dirs) d
    WHERE x.cwd = d.value OR substr(x.cwd, 1, length(d.value) + 1) = d.value || '/')))";

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, named_params};

    #[test]
    fn names_and_lookup() {
        let ws = Workspaces::new(
            &["~/PB/meteopress".into(), "/srv/PB/tsandrini/".into()],
            "/home/u",
        );
        assert_eq!(ws.names(), ["meteopress", "tsandrini"]);
        assert_eq!(ws.dir("meteopress"), Some("/home/u/PB/meteopress"));
        assert_eq!(
            ws.of("/home/u/PB/meteopress/radar"),
            Some("meteopress".into())
        );
        assert_eq!(ws.of("/home/u/PB/meteopress-old"), None);
        assert_eq!(ws.of("/srv/PB/tsandrini"), Some("tsandrini".into()));
    }

    #[test]
    fn sql_scope_matches_rust() {
        let conn = Connection::open_in_memory().unwrap();
        let hit = |cwd: &str, dirs: Option<&[&str]>| -> bool {
            let scope = Scope {
                since: 0,
                until: 0,
                now: 0,
                dirs: dirs.map(|d| d.iter().map(|d| (*d).to_owned()).collect()),
                host: None,
            };
            let sql: bool = conn
                .query_row(
                    &format!("SELECT {IN_SCOPE} FROM (SELECT :cwd AS cwd, 'h' AS host) x"),
                    named_params! {":cwd": cwd, ":dirs": scope.dirs_param(), ":host": scope.host},
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(sql, scope.covers("h", cwd), "{cwd} in {dirs:?}");
            sql
        };
        assert!(hit("/a/b_c/d", Some(&["/a/b_c"])));
        assert!(!hit("/a/bxc/d", Some(&["/a/b_c"])));
        assert!(!hit("/a/b_cd", Some(&["/a/b_c"])));
        assert!(hit("/z/y", Some(&["/a/b_c", "/z"])));
        assert!(!hit("/q", Some(&["/a/b_c", "/z"])));
        assert!(!hit("/q", Some(&[])));
        assert!(hit("/anything", None));

        let one = Scope {
            since: 0,
            until: 0,
            now: 0,
            dirs: None,
            host: Some("h".into()),
        };
        let on = |host: &str| -> bool {
            conn.query_row(
                &format!("SELECT {IN_SCOPE} FROM (SELECT '/w' AS cwd, :h AS host) x"),
                named_params! {":h": host, ":dirs": one.dirs_param(), ":host": one.host},
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(on("h") && one.covers("h", "/w"));
        assert!(!on("other") && !one.covers("other", "/w"));
    }
}
