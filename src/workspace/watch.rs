//! One inotify instance over the whole workspace: a watch on every
//! directory of every unit that is not ignored (79,592 on navibundle,
//! registered in 0.11 s), on each unit's git dir and its refs, and on the
//! root and bundle directories for units appearing or vanishing. New
//! directories are reported for the scheduler to check with
//! `git check-ignore` before they are watched, so an `npm install` costs
//! two events. Events reach the scheduler as `Event`s; a queue overflow
//! asks for a sweep.

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc::Sender},
    thread,
};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};
use tracing::{debug, warn};

const EVENT_BUFFER: usize = 256 << 10;

/// What the watcher tells the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Something under the unit changed (a file, `.git/HEAD`, a ref).
    Touched(String),
    /// A directory appeared in the unit: watch it unless it is ignored.
    NewDir { unit: String, rel: String },
    /// A directory at bundle or unit depth appeared or vanished.
    Layout,
    /// Events were lost: sweep everything.
    Overflow,
    /// The watcher thread ended.
    Stopped,
}

fn mask() -> WatchMask {
    WatchMask::MODIFY
        | WatchMask::CLOSE_WRITE
        | WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::ATTRIB
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
        | WatchMask::ONLYDIR
        | WatchMask::DONT_FOLLOW
        | WatchMask::EXCL_UNLINK
}

#[derive(Debug, Clone)]
enum Entry {
    /// A directory of a unit, `rel` relative to the unit (`""` its root).
    Unit { unit: String, rel: String },
    /// The root (level 0) or a directory above unit depth, with its path.
    Layout { level: u32, path: PathBuf },
}

#[derive(Default)]
struct Map {
    by_wd: HashMap<WatchDescriptor, Entry>,
    by_unit: HashMap<String, Vec<WatchDescriptor>>,
}

/// Adds and removes watches from any thread; the reader thread maps
/// events through the shared table.
#[derive(Clone)]
pub struct Registry {
    watches: Watches,
    map: Arc<Mutex<Map>>,
}

impl Registry {
    /// Starts the reader thread with the root (and the bundle directories
    /// at every level above unit depth) watched for layout changes.
    ///
    /// # Errors
    ///
    /// inotify is unavailable or the root cannot be watched.
    pub fn spawn(root: &Path, depth: u32, tx: Sender<Event>) -> io::Result<Self> {
        let inotify = Inotify::init()?;
        let depth = depth.max(1);
        let registry = Self {
            watches: inotify.watches(),
            map: Arc::new(Mutex::new(Map::default())),
        };
        registry.watch_layout(root, 0)?;
        let mut level = vec![root.to_path_buf()];
        for n in 1..depth {
            let mut next = Vec::new();
            for dir in &level {
                for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                    let path = entry.path();
                    if path.symlink_metadata().is_ok_and(|m| m.is_dir())
                        && path.file_name().is_some_and(|n| n != ".git")
                    {
                        if let Err(e) = registry.watch_layout(&path, n) {
                            debug!("workspace: cannot watch {}: {e}", path.display());
                        }
                        next.push(path);
                    }
                }
            }
            level = next;
        }
        let (map, watches) = (registry.map.clone(), registry.watches.clone());
        thread::Builder::new()
            .name("workspace-watch".into())
            .spawn(move || run(inotify, map, watches, depth, &tx))?;
        Ok(registry)
    }

    fn watch_layout(&self, dir: &Path, level: u32) -> io::Result<()> {
        // NOTE: `Watches` is a handle on the shared fd; a clone adds to the same instance
        let wd = self.watches.clone().add(dir, mask())?;
        if let Ok(mut map) = self.map.lock() {
            map.by_wd.insert(
                wd,
                Entry::Layout {
                    level,
                    path: dir.to_path_buf(),
                },
            );
        }
        Ok(())
    }

    /// Watches one directory of a unit.
    ///
    /// # Errors
    ///
    /// The directory cannot be watched (gone, or the per-user limit
    /// `fs.inotify.max_user_watches` is reached).
    pub fn watch(&self, unit: &str, rel: &str, dir: &Path) -> io::Result<()> {
        let wd = self.watches.clone().add(dir, mask())?;
        if let Ok(mut map) = self.map.lock() {
            map.by_wd.insert(
                wd.clone(),
                Entry::Unit {
                    unit: unit.to_owned(),
                    rel: rel.to_owned(),
                },
            );
            map.by_unit.entry(unit.to_owned()).or_default().push(wd);
        }
        Ok(())
    }

    /// Drops every watch of a unit (it left the configuration or vanished).
    pub fn forget(&self, unit: &str) {
        let wds = self
            .map
            .lock()
            .ok()
            .and_then(|mut map| map.by_unit.remove(unit))
            .unwrap_or_default();
        for wd in wds {
            let _ = self.watches.clone().remove(wd.clone());
            if let Ok(mut map) = self.map.lock() {
                map.by_wd.remove(&wd);
            }
        }
    }

    /// Watched directories of a unit.
    pub fn count(&self, unit: &str) -> usize {
        self.map
            .lock()
            .ok()
            .map_or(0, |map| map.by_unit.get(unit).map_or(0, Vec::len))
    }

    /// Watches `dir` and every directory under it (a new, non-ignored
    /// directory whose files are not in `ls-files` yet), pruning the
    /// always excluded names and nested repositories; the nested
    /// repositories found, relative to the unit.
    pub fn watch_tree(
        &self,
        unit: &str,
        unit_dir: &Path,
        rel: &str,
        always_exclude: &[String],
    ) -> Vec<String> {
        let mut nested = Vec::new();
        let mut stack = vec![rel.to_owned()];
        while let Some(rel) = stack.pop() {
            let dir = if rel.is_empty() {
                unit_dir.to_path_buf()
            } else {
                unit_dir.join(&rel)
            };
            if let Err(e) = self.watch(unit, &rel, &dir) {
                debug!("workspace: cannot watch {}: {e}", dir.display());
                continue;
            }
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if name == ".git"
                    || !entry.path().symlink_metadata().is_ok_and(|m| m.is_dir())
                    || super::units::excluded_name(&name, always_exclude)
                {
                    continue;
                }
                let sub = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                if entry.path().join(".git").exists() {
                    nested.push(sub);
                    continue;
                }
                stack.push(sub);
            }
        }
        nested
    }
}

/// The directories that hold the paths of `ls-files` output, the unit
/// root included, relative to the unit.
pub fn parent_dirs(paths: &[String]) -> Vec<String> {
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    dirs.insert(String::new());
    for path in paths {
        let mut at = path.as_str();
        while let Some((dir, _)) = at.rsplit_once('/') {
            if !dirs.insert(dir.to_owned()) {
                break;
            }
            at = dir;
        }
    }
    dirs.into_iter().collect()
}

/// The git dir's directories worth watching: itself and `refs/`
/// recursively (HEAD, the index, packed-refs, branches, tags, the stash).
pub fn git_dirs(git_dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![git_dir.to_path_buf()];
    let mut stack = vec![git_dir.join("refs")];
    while let Some(dir) = stack.pop() {
        if !dir.is_dir() {
            continue;
        }
        out.push(dir.clone());
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                stack.push(entry.path());
            }
        }
    }
    out
}

fn run(
    mut inotify: Inotify,
    map: Arc<Mutex<Map>>,
    watches: Watches,
    depth: u32,
    tx: &Sender<Event>,
) {
    let mut buffer = vec![0; EVENT_BUFFER];
    loop {
        let events = match inotify.read_events_blocking(&mut buffer) {
            Ok(events) => events,
            Err(e) => {
                warn!("workspace: reading file events: {e}");
                let _ = tx.send(Event::Stopped);
                return;
            }
        };
        let mut touched: Vec<String> = Vec::new();
        let mut out: Vec<Event> = Vec::new();
        let mut gone: Vec<WatchDescriptor> = Vec::new();
        // NOTE: a directory appearing above unit depth (a new bundle) is watched too
        let mut new_layout: Vec<(PathBuf, u32)> = Vec::new();
        {
            let Ok(table) = map.lock() else {
                return;
            };
            for event in events {
                if event.mask.contains(EventMask::Q_OVERFLOW) {
                    out.push(Event::Overflow);
                    continue;
                }
                if event.mask.contains(EventMask::IGNORED) {
                    gone.push(event.wd);
                    continue;
                }
                let Some(entry) = table.by_wd.get(&event.wd) else {
                    continue;
                };
                let name = event.name.and_then(|n| n.to_str()).unwrap_or_default();
                let is_dir = event.mask.contains(EventMask::ISDIR);
                let arrived = event
                    .mask
                    .intersects(EventMask::CREATE | EventMask::MOVED_TO);
                let left = event
                    .mask
                    .intersects(EventMask::DELETE | EventMask::MOVED_FROM);
                let self_gone = event
                    .mask
                    .intersects(EventMask::DELETE_SELF | EventMask::MOVE_SELF);
                match entry {
                    Entry::Layout { level, path } => {
                        if (is_dir && (arrived || left)) || self_gone {
                            out.push(Event::Layout);
                        }
                        if is_dir && arrived && level + 1 < depth && name != ".git" {
                            new_layout.push((path.join(name), *level + 1));
                        }
                    }
                    Entry::Unit { unit, rel } => {
                        if self_gone && rel.is_empty() {
                            out.push(Event::Layout);
                        } else if is_dir && arrived {
                            let rel = if rel.is_empty() {
                                name.to_owned()
                            } else {
                                format!("{rel}/{name}")
                            };
                            out.push(Event::NewDir {
                                unit: unit.clone(),
                                rel,
                            });
                        }
                        if !touched.contains(unit) {
                            touched.push(unit.clone());
                        }
                    }
                }
            }
        }
        if !gone.is_empty()
            && let Ok(mut table) = map.lock()
        {
            for wd in gone {
                if let Some(Entry::Unit { unit, .. }) = table.by_wd.remove(&wd)
                    && let Some(list) = table.by_unit.get_mut(&unit)
                {
                    list.retain(|w| *w != wd);
                }
            }
        }
        for (path, level) in new_layout {
            match watches.clone().add(&path, mask()) {
                Ok(wd) => {
                    if let Ok(mut table) = map.lock() {
                        table.by_wd.insert(wd, Entry::Layout { level, path });
                    }
                }
                Err(e) => debug!("workspace: cannot watch {}: {e}", path.display()),
            }
        }
        out.sort_by_key(|e| matches!(e, Event::Layout | Event::Overflow));
        out.dedup();
        for unit in touched {
            out.push(Event::Touched(unit));
        }
        for event in out {
            if tx.send(event).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc::{self, Receiver},
        time::{Duration, Instant},
    };

    use super::*;

    fn until(rx: &Receiver<Event>, want: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut seen = Vec::new();
        let end = Instant::now() + Duration::from_secs(5);
        while !seen.iter().any(&want) && Instant::now() < end {
            if let Ok(e) = rx.recv_timeout(Duration::from_millis(100)) {
                seen.push(e);
            }
        }
        seen
    }

    #[test]
    fn units_report_touches_new_dirs_and_layout_changes() {
        let root = std::env::temp_dir().join(format!("ccdash-wswatch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("b/u/src")).unwrap();
        fs::create_dir_all(root.join("b/u/.git/refs/heads")).unwrap();
        fs::create_dir_all(root.join("b/u/node_modules/x")).unwrap();
        let (tx, rx) = mpsc::channel();
        let reg = Registry::spawn(&root, 2, tx).unwrap();
        let unit_dir = root.join("b/u");
        let nested = reg.watch_tree("b/u", &unit_dir, "", &["node_modules".into()]);
        assert!(nested.is_empty());
        for dir in git_dirs(&unit_dir.join(".git")) {
            let rel = dir
                .strip_prefix(&unit_dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            reg.watch("b/u", &rel, &dir).unwrap();
        }
        assert_eq!(reg.count("b/u"), 2 + 3, "root, src, .git, refs, heads");

        fs::write(unit_dir.join("src/a.rs"), "x").unwrap();
        let seen = until(&rx, |e| *e == Event::Touched("b/u".into()));
        assert!(seen.contains(&Event::Touched("b/u".into())), "{seen:?}");
        // the ignored tree is not watched
        fs::write(unit_dir.join("node_modules/x/i.js"), "x").unwrap();
        fs::write(unit_dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let seen = until(&rx, |e| *e == Event::Touched("b/u".into()));
        assert!(seen.contains(&Event::Touched("b/u".into())), "{seen:?}");

        fs::create_dir_all(unit_dir.join("src/new")).unwrap();
        let seen = until(&rx, |e| matches!(e, Event::NewDir { .. }));
        assert!(
            seen.contains(&Event::NewDir {
                unit: "b/u".into(),
                rel: "src/new".into()
            }),
            "{seen:?}"
        );
        fs::create_dir_all(root.join("b/fresh")).unwrap();
        let seen = until(&rx, |e| *e == Event::Layout);
        assert!(seen.contains(&Event::Layout), "{seen:?}");
        fs::create_dir_all(root.join("c")).unwrap();
        let seen = until(&rx, |e| *e == Event::Layout);
        assert!(seen.contains(&Event::Layout), "{seen:?}");

        reg.forget("b/u");
        assert_eq!(reg.count("b/u"), 0);
        assert_eq!(
            parent_dirs(&["a/b/c.txt".into(), "a/d.txt".into(), "e.txt".into()]),
            ["", "a", "a/b"]
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
