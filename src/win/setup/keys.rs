//! Keys added to the collector's `config.toml` as text: the file is the
//! user's to edit, with their comments and their order, so it is not
//! parsed and written back. Enough for the flat tables of ccdash's config.

use anyhow::{Result, bail};

/// `s` as a TOML literal string (`'…'`): backslashes stay as they are,
/// which paths on Windows are full of.
///
/// # Errors
///
/// `s` holds what a literal string cannot: a `'` or a line break.
pub fn literal(s: &str) -> Result<String> {
    if s.contains(['\'', '\n', '\r']) {
        bail!("{s:?} cannot be written into config.toml as it is: put it there by hand");
    }
    Ok(format!("'{s}'"))
}

/// A list of literal strings: `['a', 'b']`.
///
/// # Errors
///
/// As [`literal`].
pub fn list(items: &[&str]) -> Result<String> {
    let items: Vec<String> = items.iter().map(|s| literal(s)).collect::<Result<_>>()?;
    Ok(format!("[{}]", items.join(", ")))
}

/// What became of a key.
#[derive(Debug, PartialEq, Eq)]
pub enum Added {
    /// `[table] key = value`, as it was added.
    Now(String),
    /// The line the table already had for the key: kept.
    Kept(String),
}

/// `config` with `key = value` in `[table]`, unless the table has the key:
/// then that line stays. A table that is not there is added at the end.
pub fn add(config: &str, table: &str, key: &str, value: &str) -> (String, Added) {
    let now = Added::Now(format!("[{table}] {key} = {value}"));
    let header = format!("[{table}]");
    let mut lines: Vec<&str> = config.lines().collect();
    let Some(at) = lines.iter().position(|line| line.trim() == header) else {
        let config = format!("{}\n\n{header}\n{key} = {value}\n", config.trim_end());
        return (config.trim_start().to_owned(), now);
    };
    let end = lines[at + 1..]
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .map_or(lines.len(), |next| at + 1 + next);
    let has_key = |line: &str| {
        line.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    };
    if let Some(line) = lines[at + 1..end].iter().find(|line| has_key(line)) {
        return (config.to_owned(), Added::Kept(line.trim().to_owned()));
    }
    let line = format!("{key} = {value}");
    lines.insert(at + 1, &line);
    (lines.join("\n") + "\n", now)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "# mine\nhost = 'winbox'\n\n[hub]\nurl = 'wss://hub.example.org/collect'\n\n# how sessions start\n[launch]\ncommand = ['claude.exe']\n";

    #[test]
    fn a_key_joins_its_table_or_brings_the_table() {
        let (config, added) = add(CONFIG, "launch", "channel", "true");
        assert_eq!(added, Added::Now("[launch] channel = true".into()));
        assert_eq!(
            config,
            CONFIG.replace("[launch]\n", "[launch]\nchannel = true\n")
        );
        let (config, added) = add(&config, "sync", "enable", "true");
        assert_eq!(added, Added::Now("[sync] enable = true".into()));
        assert!(config.ends_with("command = ['claude.exe']\n\n[sync]\nenable = true\n"));
        assert!(
            config.starts_with("# mine\nhost = 'winbox'\n"),
            "the rest stays"
        );
        let parsed: crate::config::CollectorConfig = toml::from_str(&config).unwrap();
        assert!(parsed.launch.channel && parsed.sync.enable);
    }

    #[test]
    fn a_key_the_table_has_is_kept_as_it_is() {
        let config = CONFIG.replace("[launch]\n", "[launch]\n  channel   = false # not yet\n");
        let (same, added) = add(&config, "launch", "channel", "true");
        assert_eq!(added, Added::Kept("channel   = false # not yet".into()));
        assert_eq!(same, config);
        // NOTE: `command` in [launch] is not `command` in a table that has none yet
        let (config, added) = add(
            &config,
            "attach",
            "command",
            "['claude.exe', 'attach', '{id}']",
        );
        assert!(matches!(added, Added::Now(_)));
        assert!(config.ends_with("[attach]\ncommand = ['claude.exe', 'attach', '{id}']\n"));
        // NOTE: nor is a key that only starts the same
        let (_, added) = add(&config, "launch", "chan", "1");
        assert!(matches!(added, Added::Now(_)));
    }

    #[test]
    fn a_first_key_starts_the_file() {
        let (config, _) = add("", "sync", "enable", "true");
        assert_eq!(config, "[sync]\nenable = true\n");
    }

    #[test]
    fn windows_paths_are_literal_strings() {
        assert_eq!(
            literal(r"C:\Users\me\.local\bin").unwrap(),
            r"'C:\Users\me\.local\bin'"
        );
        assert_eq!(list(&["nvim", "{file}"]).unwrap(), "['nvim', '{file}']");
        assert!(literal("it's").is_err());
        assert!(list(&["a", "b\nc"]).is_err());
    }
}
