//! `ccdash backup`: a consistent copy of the hub database through SQLite's
//! backup API (page-identical copies dedupe well in restic), checked with
//! `PRAGMA integrity_check` before it replaces the previous copy.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, backup::Backup};

/// Copies `source` to `dest`, failing before `dest` is touched when the copy
/// does not pass the integrity check.
pub fn backup(source: &Path, dest: &Path) -> Result<()> {
    let src = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", source.display()))?;
    let tmp = dest.with_extension("db.partial");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut dst =
            Connection::open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let copy = Backup::new(&src, &mut dst)?;
        copy.run_to_completion(256, std::time::Duration::from_millis(20), None)?;
        drop(copy);
        // NOTE: a WAL copy grows -wal/-shm files that the rename leaves behind
        dst.pragma_update(None, "journal_mode", "DELETE")?;
    }
    let check = Connection::open_with_flags(&tmp, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let verdict: String = check.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    drop(check);
    if verdict != "ok" {
        let _ = std::fs::remove_file(&tmp);
        bail!("integrity check of the copy failed: {verdict}");
    }
    std::fs::rename(&tmp, dest).with_context(|| format!("moving into {}", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    #[test]
    fn copies_and_checks() {
        let dir = std::env::temp_dir().join(format!("ccdash-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("hub.db");
        store::open_writer(&src)
            .unwrap()
            .execute(
                "INSERT INTO sessions (id, host, cwd, kind, started_at) VALUES ('s', 'h', '/', 'interactive', 1)",
                [],
            )
            .unwrap();
        let dest = dir.join("copy.db");
        backup(&src, &dest).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("copy"))
            .collect();
        left.sort();
        assert_eq!(left, ["copy.db"], "no side files of the partial copy");
        let n: i64 = Connection::open(&dest)
            .unwrap()
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(!dir.join("copy.db.partial").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
