//! `ccdash import db`: copies the history of a single-host ccdash database
//! (what the daemon kept before the hub) into the hub's store. Rows keep
//! their `host`; running it twice adds nothing.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use crate::store;

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
        assert_eq!(
            first,
            [
                ("sessions".to_owned(), 1),
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
        std::fs::remove_dir_all(dir).unwrap();
    }
}
