//! The workspace watcher on Windows: one `ReadDirectoryChangesExW` watch
//! over the whole root instead of a watch per directory. The directories
//! the scheduler registers are kept in a table; changes anywhere else
//! (ignored trees, git objects) are dropped. Sends the same `Event`s as
//! the inotify watcher.

use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
    sync::{Arc, Mutex, mpsc::Sender},
    thread,
};

use tracing::warn;

use super::watch::Event;
use crate::win::{Change, DirEvent, DirWatch};

/// Registered directories by unit, relative to the unit (`""` its root).
#[derive(Default)]
struct Map {
    by_unit: HashMap<String, HashSet<String>>,
}

/// Registers and forgets directories from any thread; the reader thread
/// filters changes through the shared table.
#[derive(Clone)]
pub struct Registry {
    map: Arc<Mutex<Map>>,
}

impl Registry {
    /// Starts the reader thread on `root`; directories above unit depth
    /// (`depth` levels) report layout changes.
    ///
    /// # Errors
    ///
    /// The root cannot be opened for watching.
    pub fn spawn(root: &Path, depth: u32, tx: Sender<Event>) -> io::Result<Self> {
        let watch = DirWatch::open(root)?;
        let registry = Self {
            map: Arc::default(),
        };
        let map = registry.map.clone();
        let depth = depth.max(1) as usize;
        thread::Builder::new()
            .name("workspace-watch".into())
            .spawn(move || run(watch, &map, depth, &tx))?;
        Ok(registry)
    }

    /// Registers one directory of a unit.
    ///
    /// # Errors
    ///
    /// The directory is gone.
    pub fn watch(&self, unit: &str, rel: &str, dir: &Path) -> io::Result<()> {
        if !dir.is_dir() {
            return Err(io::ErrorKind::NotFound.into());
        }
        if let Ok(mut map) = self.map.lock() {
            map.by_unit
                .entry(unit.to_owned())
                .or_default()
                .insert(rel.replace('\\', "/"));
        }
        Ok(())
    }

    /// Drops every directory of a unit.
    pub fn forget(&self, unit: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.by_unit.remove(unit);
        }
    }

    /// Drops a unit's directories at `rel` and below.
    pub fn forget_under(&self, unit: &str, rel: &str) {
        let prefix = format!("{rel}/");
        if let Ok(mut map) = self.map.lock()
            && let Some(dirs) = map.by_unit.get_mut(unit)
        {
            dirs.retain(|r| r != rel && !r.starts_with(&prefix));
        }
    }

    /// Every directory registered.
    pub fn total(&self) -> usize {
        self.map
            .lock()
            .ok()
            .map_or(0, |map| map.by_unit.values().map(HashSet::len).sum())
    }

    /// Registered directories of a unit.
    pub fn count(&self, unit: &str) -> usize {
        self.map
            .lock()
            .ok()
            .map_or(0, |map| map.by_unit.get(unit).map_or(0, HashSet::len))
    }
}

/// Drops the registered directory at `path` (relative to the root) and
/// those below it; a unit's own root stays, for the layout pass to forget.
fn forget_vanished(table: &mut Map, path: &str) {
    let Some((unit, rel)) = owner(table, path).map(|(u, r)| (u.to_owned(), r.to_owned())) else {
        return;
    };
    if rel.is_empty() {
        return;
    }
    let below = format!("{rel}/");
    if let Some(dirs) = table.by_unit.get_mut(&unit) {
        dirs.retain(|d| *d != rel && !d.starts_with(&below));
    }
}

/// The deepest unit that registered `dir` (relative to the root), with
/// `dir` relative to it: units sit at any depth (bundle roots above, nested
/// repositories below).
fn owner<'a>(table: &'a Map, dir: &'a str) -> Option<(&'a str, &'a str)> {
    let mut end = dir.len();
    loop {
        let unit = &dir[..end];
        let rel = dir.get(end + 1..).unwrap_or("");
        if let Some((unit, dirs)) = table.by_unit.get_key_value(unit)
            && dirs.contains(rel)
        {
            return Some((unit.as_str(), rel));
        }
        end = unit.rfind('/')?;
    }
}

/// What `changes` mean for the units registered in `table`.
/// Registered directories that vanished are forgotten, as inotify drops
/// their watches.
fn events_of(table: &mut Map, depth: usize, changes: &[DirEvent]) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    let mut touched: Vec<String> = Vec::new();
    for change in changes {
        let path = change.path.as_str();
        if matches!(change.change, Change::Removed | Change::RenamedFrom) {
            forget_vanished(table, path);
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        // NOTE: a directory above unit depth, or a unit's own, appeared or vanished
        if change.is_dir
            && change.change != Change::Modified
            && (path.split('/').count() <= depth || table.by_unit.contains_key(path))
        {
            out.push(Event::Layout);
        }
        // NOTE: a directory's own write time moves when an entry in it changes
        // (`.git/objects` as a snapshot writes, `node_modules`): inotify reports none of that
        if change.is_dir && change.change == Change::Modified {
            continue;
        }
        let Some((unit, rel)) = owner(table, parent) else {
            continue;
        };
        if change.is_dir && matches!(change.change, Change::Added | Change::RenamedTo) {
            out.push(Event::NewDir {
                unit: unit.to_owned(),
                rel: if rel.is_empty() {
                    name.to_owned()
                } else {
                    format!("{rel}/{name}")
                },
            });
        }
        if !touched.iter().any(|t| t == unit) {
            touched.push(unit.to_owned());
        }
    }
    out.sort_by_key(|e| matches!(e, Event::Layout | Event::Overflow));
    out.dedup();
    out.extend(touched.into_iter().map(Event::Touched));
    out
}

fn run(mut watch: DirWatch, map: &Mutex<Map>, depth: usize, tx: &Sender<Event>) {
    loop {
        let changes = match watch.read() {
            Ok(Some(changes)) => changes,
            Ok(None) => {
                if tx.send(Event::Overflow).is_err() {
                    return;
                }
                continue;
            }
            Err(e) => {
                warn!("workspace: reading file events: {e}");
                let _ = tx.send(Event::Stopped);
                return;
            }
        };
        let out = {
            let Ok(mut table) = map.lock() else {
                return;
            };
            events_of(&mut table, depth, &changes)
        };
        for event in out {
            if tx.send(event).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_s_own_write_time_touches_nothing() {
        let mut table = Map::default();
        table.by_unit.insert(
            "b/u".into(),
            ["", ".git", "dist", "dist/sub"].map(String::from).into(),
        );
        let change = |change, path: &str, is_dir| DirEvent {
            change,
            path: path.into(),
            is_dir,
        };
        let mut events = |changes: &[DirEvent]| events_of(&mut table, 2, changes);
        assert!(events(&[change(Change::Modified, "b/u/node_modules", true)]).is_empty());
        assert!(events(&[change(Change::Modified, "b/u/.git/objects", true)]).is_empty());
        assert_eq!(
            events(&[change(Change::Modified, "b/u/a.rs", false)]),
            [Event::Touched("b/u".into())]
        );
        // NOTE: a registered dist/ removed, then made again (ignored now) by a build
        assert_eq!(
            events(&[change(Change::Removed, "b/u/dist", true)]),
            [Event::Touched("b/u".into())]
        );
        assert!(events(&[change(Change::Modified, "b/u/dist/app.js", false)]).is_empty());
        assert!(events(&[change(Change::Modified, "b/u/dist/sub/x.js", false)]).is_empty());
        assert_eq!(
            events(&[change(Change::Added, "b/u/src", true)]),
            [
                Event::NewDir {
                    unit: "b/u".into(),
                    rel: "src".into()
                },
                Event::Touched("b/u".into())
            ]
        );
    }
}
