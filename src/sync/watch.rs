//! Watches the Claude config dir with inotify so the sync thread settles
//! the files that changed instead of scanning everything: one watch per
//! directory that can hold synced files (a few hundred), plus the root
//! (the prompt history, new top-level directories) and the session
//! registry (`sessions/`, a session ending lets its held files settle).
//!
//! Changes go to the sync thread as `Cmd::Touched` paths; a lost event
//! (queue overflow, a directory moved away) asks for a full scan
//! (`Cmd::Rescan`), and a dead watcher hands over to periodic scans
//! (`Cmd::Unwatched`).

use std::{collections::HashMap, fs, io, path::PathBuf, sync::mpsc::Sender, thread};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use tracing::{debug, warn};

use super::{HISTORY, collector::Cmd, collector::Filter};

/// The running-session registry, watched for sessions ending.
const REGISTRY: &str = "sessions";
const EVENT_BUFFER: usize = 64 << 10;

fn mask() -> WatchMask {
    WatchMask::MODIFY
        | WatchMask::CLOSE_WRITE
        | WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::ONLYDIR
        | WatchMask::DONT_FOLLOW
        | WatchMask::EXCL_UNLINK
}

/// Starts watching `root`; returns how many directories are watched.
///
/// # Errors
///
/// inotify is unavailable or the root cannot be watched (the caller falls
/// back to scanning).
pub(super) fn spawn(root: PathBuf, filter: Filter, tx: Sender<Cmd>) -> io::Result<usize> {
    let mut watcher = Watcher {
        inotify: Inotify::init()?,
        root,
        filter,
        dirs: HashMap::new(),
    };
    watcher.add("")?;
    if watcher.root.join(REGISTRY).is_dir() {
        watcher.add(REGISTRY)?;
    }
    let mut ignored = Vec::new();
    for entry in fs::read_dir(&watcher.root)?.flatten() {
        if let Ok(name) = entry.file_name().into_string()
            && entry.path().symlink_metadata().is_ok_and(|m| m.is_dir())
            && watcher.filter.could_hold(&name)
        {
            watcher.add_tree(&name, &mut ignored);
        }
    }
    let watched = watcher.dirs.len();
    thread::Builder::new()
        .name("sync-watch".into())
        .spawn(move || watcher.run(&tx))?;
    Ok(watched)
}

struct Watcher {
    inotify: Inotify,
    root: PathBuf,
    filter: Filter,
    /// Watched directories, relative to the root (`""` is the root).
    dirs: HashMap<WatchDescriptor, String>,
}

impl Watcher {
    fn add(&mut self, rel: &str) -> io::Result<()> {
        let wd = self.inotify.watches().add(self.root.join(rel), mask())?;
        self.dirs.insert(wd, rel.to_owned());
        Ok(())
    }

    /// Watches `rel` and every directory under it that can hold synced
    /// files; the files already there go to `found` (they may have been
    /// written before the watch existed).
    fn add_tree(&mut self, rel: &str, found: &mut Vec<String>) {
        if let Err(e) = self.add(rel) {
            // NOTE: ENOSPC is the per-user watch limit (fs.inotify.max_user_watches)
            warn!("sync: cannot watch {rel}: {e}; the next full scan covers it");
            return;
        }
        let Ok(entries) = fs::read_dir(self.root.join(rel)) else {
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
            if meta.is_dir() && self.filter.could_hold(&path) {
                self.add_tree(&path, found);
            } else if meta.is_file() && self.filter.travels(&path) {
                found.push(path);
            }
        }
    }

    fn run(mut self, tx: &Sender<Cmd>) {
        let mut buffer = vec![0; EVENT_BUFFER];
        loop {
            let events = match self.inotify.read_events_blocking(&mut buffer) {
                Ok(events) => events,
                Err(e) => {
                    warn!("sync: reading file events: {e}");
                    let _ = tx.send(Cmd::Unwatched);
                    return;
                }
            };
            let (mut touched, mut new_dirs, mut gone) = (Vec::new(), Vec::new(), Vec::new());
            let mut rescan = false;
            for event in events {
                if event.mask.contains(EventMask::Q_OVERFLOW) {
                    rescan = true;
                    continue;
                }
                if event.mask.contains(EventMask::IGNORED) {
                    gone.push(event.wd);
                    continue;
                }
                let (Some(dir), Some(name)) = (
                    self.dirs.get(&event.wd),
                    event.name.and_then(|n| n.to_str()),
                ) else {
                    continue;
                };
                let path = if dir.is_empty() {
                    name.to_owned()
                } else {
                    format!("{dir}/{name}")
                };
                if event.mask.contains(EventMask::ISDIR) {
                    let arrived = event
                        .mask
                        .intersects(EventMask::CREATE | EventMask::MOVED_TO);
                    if arrived && (self.filter.could_hold(&path) || path == REGISTRY) {
                        new_dirs.push(path);
                    } else if !arrived && self.filter.could_hold(&path) {
                        // NOTE: a directory moved or deleted reports none of its files
                        rescan = true;
                    }
                } else if dir == REGISTRY || path == HISTORY || self.filter.travels(&path) {
                    touched.push(path);
                }
            }
            for wd in gone {
                self.dirs.remove(&wd);
            }
            for dir in new_dirs {
                if dir == REGISTRY {
                    let _ = self.add(&dir);
                } else {
                    self.add_tree(&dir, &mut touched);
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
    fn reports_what_travels_and_follows_new_directories() {
        let root = std::env::temp_dir().join(format!("ccdash-watch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("projects/-w")).unwrap();
        fs::create_dir_all(root.join("debug")).unwrap();
        let filter = Filter {
            include: vec!["projects/*".into(), "file-history/*".into(), HISTORY.into()],
            exclude: vec!["projects/-tmp-*".into()],
        };
        let (tx, rx) = mpsc::channel();
        assert_eq!(spawn(root.clone(), filter, tx).unwrap(), 3);

        // an append to a transcript, the prompt history; not debug logs
        fs::write(root.join("debug/log.txt"), "x").unwrap();
        fs::write(root.join("projects/-w/s.jsonl"), "{}\n").unwrap();
        fs::write(root.join(HISTORY), "{}\n").unwrap();
        let seen = collect(&rx, HISTORY);
        assert!(seen.contains("projects/-w/s.jsonl"), "{seen:?}");
        assert!(!seen.iter().any(|p| p.starts_with("debug")), "{seen:?}");

        // a new tree, with files written before its watch could exist
        fs::create_dir_all(root.join("file-history/s1/deep")).unwrap();
        fs::write(root.join("file-history/s1/deep/a@v1"), "a").unwrap();
        let seen = collect(&rx, "file-history/s1/deep/a@v1");
        assert!(seen.contains("file-history/s1/deep/a@v1"), "{seen:?}");
        fs::write(root.join("file-history/s1/b@v1"), "b").unwrap();
        assert!(collect(&rx, "file-history/s1/b@v1").contains("file-history/s1/b@v1"));

        // excluded trees stay unwatched; the registry is watched once it exists
        fs::create_dir_all(root.join("projects/-tmp-x")).unwrap();
        fs::write(root.join("projects/-tmp-x/t.jsonl"), "{}\n").unwrap();
        fs::create_dir_all(root.join(REGISTRY)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        fs::write(root.join("sessions/42.json"), "{}").unwrap();
        let seen = collect(&rx, "sessions/42.json");
        assert!(seen.contains("sessions/42.json"), "{seen:?}");
        assert!(!seen.iter().any(|p| p.contains("-tmp-")), "{seen:?}");
        fs::remove_dir_all(&root).unwrap();
    }
}
