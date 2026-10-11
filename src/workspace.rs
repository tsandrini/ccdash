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
    /// Workspace directory; sessions outside it are left out.
    pub dir: Option<String>,
}

impl Scope {
    pub fn contains(&self, cwd: &str) -> bool {
        self.dir.as_deref().is_none_or(|dir| within(cwd, dir))
    }
}

/// SQL condition on a `sessions` row aliased `x`, bound to `:dir`.
pub const IN_SCOPE: &str =
    "(:dir IS NULL OR x.cwd = :dir OR substr(x.cwd, 1, length(:dir) + 1) = :dir || '/')";

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
        let hit = |cwd: &str, dir: Option<&str>| -> bool {
            conn.query_row(
                &format!("SELECT {IN_SCOPE} FROM (SELECT :cwd AS cwd) x"),
                named_params! {":cwd": cwd, ":dir": dir},
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(hit("/a/b_c/d", Some("/a/b_c")));
        assert!(!hit("/a/bxc/d", Some("/a/b_c")));
        assert!(!hit("/a/b_cd", Some("/a/b_c")));
        assert!(hit("/anything", None));
    }
}
