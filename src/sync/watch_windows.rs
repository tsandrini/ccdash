//! Watches the Claude config dir with `ReadDirectoryChangesExW`: one watch
//! on the root covers the whole tree, so a new directory needs no watch of
//! its own. Sends what the inotify watcher sends: changed paths as
//! `Cmd::Touched`, lost events as `Cmd::Rescan`, a dead watcher as
//! `Cmd::Unwatched`.

use std::{fs, io, path::PathBuf, sync::mpsc::Sender, thread};

use tracing::{debug, warn};

use super::{HISTORY, collector::Cmd, collector::Filter};
use crate::win::{Change, DirWatch};

/// The running-session registry, watched for sessions ending.
const REGISTRY: &str = "sessions";

/// Starts watching `root`; returns how many watches that takes (one).
///
/// # Errors
///
/// The root cannot be opened for watching (the caller falls back to
/// scanning).
pub(super) fn spawn(root: PathBuf, filter: Filter, tx: Sender<Cmd>) -> io::Result<usize> {
    let watch = DirWatch::open(&root)?;
    thread::Builder::new()
        .name("sync-watch".into())
        .spawn(move || run(watch, &root, &filter, &tx))?;
    Ok(1)
}

fn run(mut watch: DirWatch, root: &std::path::Path, filter: &Filter, tx: &Sender<Cmd>) {
    loop {
        let events = match watch.read() {
            Ok(Some(events)) => events,
            Ok(None) => {
                if tx.send(Cmd::Rescan).is_err() {
                    return;
                }
                continue;
            }
            Err(e) => {
                warn!("sync: reading file events: {e}");
                let _ = tx.send(Cmd::Unwatched);
                return;
            }
        };
        let mut touched = Vec::new();
        let mut rescan = false;
        for event in events {
            let path = event.path;
            if event.is_dir {
                let arrived = matches!(event.change, Change::Added | Change::RenamedTo);
                let left = matches!(event.change, Change::Removed | Change::RenamedFrom);
                if arrived && filter.could_hold(&path) {
                    // NOTE: a directory moved in reports none of its files
                    walk(root, &path, filter, &mut touched);
                } else if left && filter.could_hold(&path) {
                    rescan = true;
                }
                continue;
            }
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            if dir == REGISTRY || path == HISTORY || filter.travels(&path) {
                touched.push(path);
            }
        }
        debug!("sync watch: {} paths changed", touched.len());
        let sent = (!rescan || tx.send(Cmd::Rescan).is_ok())
            && (touched.is_empty() || tx.send(Cmd::Touched(touched)).is_ok());
        if !sent {
            return;
        }
    }
}

/// The travelling files under `rel`.
fn walk(root: &std::path::Path, rel: &str, filter: &Filter, found: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(root.join(rel)) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let path = format!("{rel}/{name}");
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if meta.is_dir() && filter.could_hold(&path) {
            walk(root, &path, filter, found);
        } else if meta.is_file() && filter.travels(&path) {
            found.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::mpsc::{self, Receiver},
        time::{Duration, Instant},
    };

    use super::*;

    /// Every path reported until `want` is among them (or 5 s pass).
    fn collect(rx: &Receiver<Cmd>, want: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let end = Instant::now() + Duration::from_secs(5);
        while !seen.contains(want) && Instant::now() < end {
            if let Ok(Cmd::Touched(paths)) = rx.recv_timeout(Duration::from_millis(100)) {
                seen.extend(paths);
            }
        }
        seen
    }

    #[test]
    fn reports_what_travels_under_the_whole_tree() {
        let root = std::env::temp_dir().join(format!("ccdash-watch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("projects/-w")).unwrap();
        fs::create_dir_all(root.join("debug")).unwrap();
        let filter = Filter {
            include: vec!["projects/*".into(), "file-history/*".into(), HISTORY.into()],
            exclude: vec!["projects/-tmp-*".into()],
        };
        let (tx, rx) = mpsc::channel();
        assert_eq!(spawn(root.clone(), filter, tx).unwrap(), 1);

        // an append to a transcript, the prompt history; not debug logs
        fs::write(root.join("debug/log.txt"), "x").unwrap();
        fs::write(root.join("projects/-w/s.jsonl"), "{}\n").unwrap();
        fs::write(root.join(HISTORY), "{}\n").unwrap();
        let seen = collect(&rx, HISTORY);
        assert!(seen.contains("projects/-w/s.jsonl"), "{seen:?}");
        assert!(!seen.iter().any(|p| p.starts_with("debug")), "{seen:?}");

        // a new tree and its files
        fs::create_dir_all(root.join("file-history/s1/deep")).unwrap();
        fs::write(root.join("file-history/s1/deep/a@v1"), "a").unwrap();
        let seen = collect(&rx, "file-history/s1/deep/a@v1");
        assert!(seen.contains("file-history/s1/deep/a@v1"), "{seen:?}");

        // excluded trees stay quiet; the registry is reported once it exists
        fs::create_dir_all(root.join("projects/-tmp-x")).unwrap();
        fs::write(root.join("projects/-tmp-x/t.jsonl"), "{}\n").unwrap();
        fs::create_dir_all(root.join(REGISTRY)).unwrap();
        fs::write(root.join("sessions/42.json"), "{}").unwrap();
        let seen = collect(&rx, "sessions/42.json");
        assert!(seen.contains("sessions/42.json"), "{seen:?}");
        assert!(!seen.iter().any(|p| p.contains("-tmp-")), "{seen:?}");
        fs::remove_dir_all(&root).unwrap();
    }
}
