//! Units of the workspace root: where they are, what kind they are, the
//! hidden repository of a plain directory, and what makes a unit unsafe
//! to touch (a lock, a rebase in progress).

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use super::{
    UnitKind,
    git::{Git, Opts, Repo},
};

/// A directory found at unit depth, or a repository nested deeper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Relative to the root, `/`-separated.
    pub id: String,
    pub kind: UnitKind,
    /// Why it can never be a unit (a linked worktree: its refs are the
    /// main repository's), naming that repository.
    pub unsyncable: Option<String>,
}

/// The main repository of a linked worktree whose `.git` file is at
/// `dot`, when it is one.
fn linked_worktree(dir: &Path) -> Option<String> {
    let dot = dir.join(".git");
    if !fs::symlink_metadata(&dot).ok()?.is_file() {
        return None;
    }
    let text = fs::read_to_string(&dot).ok()?;
    let target = text
        .lines()
        .next()?
        .strip_prefix("gitdir:")
        .map(str::trim)?;
    let target = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        dir.join(target)
    };
    if !target.components().any(|c| c.as_os_str() == "worktrees") {
        return None;
    }
    let main = target
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == ".git"))
        .and_then(Path::parent)
        .map_or_else(|| target.display().to_string(), |p| p.display().to_string());
    Some(format!("a linked worktree of {main}"))
}

/// Where everything lives: the workspace root and the collector's state
/// directory for the workspace (`$XDG_STATE_HOME/ccdash/workspace`).
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
    pub state: PathBuf,
}

impl Layout {
    pub fn unit_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// The hidden repository of a plain unit.
    pub fn hidden_git(&self, id: &str) -> PathBuf {
        self.state.join(format!("{id}.git"))
    }

    /// Temporary index files and the unit's excludes file.
    pub fn scratch(&self, id: &str) -> PathBuf {
        self.state.join(format!("{id}.ccdash"))
    }

    /// The git dir of a unit: `.git` under it (a directory, or a file
    /// pointing at one), or the hidden one.
    ///
    /// # Errors
    ///
    /// A `.git` file that cannot be followed, or a linked worktree (its
    /// refs are shared with another unit).
    pub fn git_dir(&self, id: &str, kind: UnitKind) -> Result<PathBuf> {
        match kind {
            UnitKind::Plain => Ok(self.hidden_git(id)),
            UnitKind::Git => {
                let dot = self.unit_dir(id).join(".git");
                let meta = fs::symlink_metadata(&dot)
                    .with_context(|| format!("{} has no .git", self.unit_dir(id).display()))?;
                if meta.is_dir() {
                    return Ok(dot);
                }
                let text = fs::read_to_string(&dot)?;
                let target = text
                    .lines()
                    .next()
                    .and_then(|l| l.strip_prefix("gitdir:"))
                    .map(str::trim)
                    .with_context(|| format!("{} is not a gitdir link", dot.display()))?;
                let target = if Path::new(target).is_absolute() {
                    PathBuf::from(target)
                } else {
                    self.unit_dir(id).join(target)
                };
                if target.components().any(|c| c.as_os_str() == "worktrees") {
                    // NOTE: its refs live in the main repository; it cannot be a unit
                    bail!(
                        "a linked worktree of {}: exclude it",
                        target
                            .ancestors()
                            .find(|p| p.file_name().is_some_and(|n| n == ".git"))
                            .and_then(Path::parent)
                            .map_or_else(
                                || target.display().to_string(),
                                |p| p.display().to_string()
                            )
                    );
                }
                Ok(target)
            }
        }
    }

    /// The repository to run git in for a unit.
    ///
    /// # Errors
    ///
    /// As `git_dir`.
    pub fn repo(&self, id: &str, kind: UnitKind) -> Result<Repo> {
        Ok(Repo {
            git_dir: self.git_dir(id, kind)?,
            work_tree: self.unit_dir(id),
        })
    }

    /// Creates the hidden repository of a plain unit when it is missing.
    ///
    /// # Errors
    ///
    /// git fails.
    pub fn ensure_hidden(&self, git: &Git, id: &str) -> Result<Repo> {
        let repo = Repo {
            git_dir: self.hidden_git(id),
            work_tree: self.unit_dir(id),
        };
        fs::create_dir_all(&repo.work_tree)?;
        if repo.git_dir.join("HEAD").is_file() {
            return Ok(repo);
        }
        if let Some(parent) = repo.git_dir.parent() {
            fs::create_dir_all(parent)?;
        }
        git.ok(Some(&repo), &["init", "-q"], &Opts::default())?;
        git.ok(
            Some(&repo),
            &["config", "core.worktree", &repo.work_tree.to_string_lossy()],
            &Opts::default(),
        )?;
        Ok(repo)
    }

    /// Creates the repository of a git unit the hub has and this host
    /// lacks: `git init` in the (new, empty, or repository-less)
    /// directory. Returns the repository and whether the directory held
    /// files already (a hand-copied unit: to adopt, not to fast-forward).
    ///
    /// # Errors
    ///
    /// The directory holds a repository with commits, or git fails.
    pub fn create_git(&self, git: &Git, id: &str) -> Result<(Repo, bool)> {
        let dir = self.unit_dir(id);
        let repo = Repo {
            git_dir: dir.join(".git"),
            work_tree: dir.clone(),
        };
        let had_files = dir.exists()
            && fs::read_dir(&dir)?
                .flatten()
                .any(|e| e.file_name() != ".git");
        if repo.git_dir.join("HEAD").is_file() {
            if git.rev(&repo, "HEAD")?.is_some() || !git.refs(&repo, "refs/")?.is_empty() {
                bail!("{} already holds a repository with history", dir.display());
            }
            return Ok((repo, had_files));
        }
        fs::create_dir_all(&dir)?;
        git.ok(Some(&repo), &["init", "-q"], &Opts::default())?;
        Ok((repo, had_files))
    }

    /// Whether a unit's directory holds files (besides `.git`).
    pub fn has_files(&self, id: &str) -> bool {
        fs::read_dir(self.unit_dir(id))
            .map(|d| d.flatten().any(|e| e.file_name() != ".git"))
            .unwrap_or(false)
    }
}

/// Directories at unit depth under `root`, and the repositories at
/// shallower levels (bundle roots). Dot-directories count; symlinks are
/// not followed. Under a bundle root that is a repository only
/// repositories are units: a plain directory there is part of the root's
/// working tree.
pub fn discover(root: &Path, depth: u32) -> Vec<Found> {
    let mut out = Vec::new();
    walk(root, "", 1, depth.max(1), false, &mut out);
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn walk(root: &Path, rel: &str, level: u32, depth: u32, in_repo: bool, out: &mut Vec<Found>) {
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name == ".git" {
            continue;
        }
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let id = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let has_git = entry.path().join(".git").exists();
        let unsyncable = has_git.then(|| linked_worktree(&entry.path())).flatten();
        if level >= depth {
            if has_git {
                out.push(Found {
                    id,
                    kind: UnitKind::Git,
                    unsyncable,
                });
            } else if !in_repo {
                out.push(Found {
                    id,
                    kind: UnitKind::Plain,
                    unsyncable: None,
                });
            }
        } else {
            if has_git {
                out.push(Found {
                    id: id.clone(),
                    kind: UnitKind::Git,
                    unsyncable,
                });
            }
            walk(root, &id, level + 1, depth, in_repo || has_git, out);
        }
    }
}

/// Whether a directory name is one `always_exclude` names (a pattern
/// without `/`).
pub fn excluded_name(name: &str, always_exclude: &[String]) -> bool {
    always_exclude
        .iter()
        .filter(|p| !p.contains('/'))
        .any(|p| crate::sync::glob(p, name))
}

/// Files and bytes under `dir` (`.git` included), the always excluded
/// directories left out, symlinks counted but not followed.
pub fn size_of(dir: &Path, always_exclude: &[String]) -> (u64, u64) {
    let m = measure(dir, always_exclude, false);
    (m.files, m.bytes)
}

/// What a directory weighs: what would travel, and what the always
/// excluded directories inside keep home, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Measured {
    pub files: u64,
    pub bytes: u64,
    /// Excluded directory name → bytes under every directory of that name
    /// (only when asked for).
    pub excluded: BTreeMap<String, u64>,
}

/// Measures `dir`; with `excluded_too` the always excluded directories are
/// walked as well and summed by name.
pub fn measure(dir: &Path, always_exclude: &[String], excluded_too: bool) -> Measured {
    let mut m = Measured::default();
    // (directory, the excluded name it is under, if any)
    let mut stack = vec![(dir.to_path_buf(), None::<String>)];
    while let Some((d, under)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if meta.is_dir() {
                if under.is_some() {
                    stack.push((entry.path(), under.clone()));
                } else if !excluded_name(&name, always_exclude) {
                    stack.push((entry.path(), None));
                } else if excluded_too {
                    stack.push((entry.path(), Some(name)));
                }
            } else {
                match &under {
                    Some(name) => *m.excluded.entry(name.clone()).or_insert(0) += meta.len(),
                    None => {
                        m.files += 1;
                        m.bytes += meta.len();
                    }
                }
            }
        }
    }
    m
}

/// A git operation in progress makes the unit unsafe to snapshot or
/// write; the reason, or None.
pub fn blocker(git_dir: &Path) -> Option<&'static str> {
    [
        ("index.lock", "git is running (index.lock)"),
        ("MERGE_HEAD", "merge in progress"),
        ("rebase-merge", "rebase in progress"),
        ("rebase-apply", "rebase in progress"),
        ("CHERRY_PICK_HEAD", "cherry-pick in progress"),
        ("REVERT_HEAD", "revert in progress"),
        ("BISECT_LOG", "bisect in progress"),
    ]
    .into_iter()
    .find(|(marker, _)| git_dir.join(marker).exists())
    .map(|(_, reason)| reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_units_at_depth_and_bundle_roots() {
        let root = std::env::temp_dir().join(format!("ccdash-units-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in [
            "tsandrini/.git",
            "tsandrini/ccdash/.git",
            "tsandrini/notes",
            "tsandrini/.claude",
            "meteopress/radar/.git/objects",
            "meteopress/tmp/x",
            ".hidden/repo/.git",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("tsandrini/README"), "x").unwrap();
        fs::write(root.join("meteopress/tmp/x/f"), "0123").unwrap();
        let found = discover(&root, 2);
        assert!(found.iter().all(|f| f.unsyncable.is_none()));
        let ids: Vec<(&str, UnitKind)> = found.iter().map(|f| (f.id.as_str(), f.kind)).collect();
        // NOTE: `.claude` and `notes` are part of the tsandrini root repository
        assert_eq!(
            ids,
            vec![
                (".hidden/repo", UnitKind::Git),
                ("meteopress/radar", UnitKind::Git),
                ("meteopress/tmp", UnitKind::Plain),
                ("tsandrini", UnitKind::Git),
                ("tsandrini/ccdash", UnitKind::Git),
            ]
        );
        let layout = Layout {
            root: root.clone(),
            state: root.join("state"),
        };
        assert_eq!(
            layout.git_dir("tsandrini/ccdash", UnitKind::Git).unwrap(),
            root.join("tsandrini/ccdash/.git")
        );
        assert_eq!(
            layout.git_dir("tsandrini/notes", UnitKind::Plain).unwrap(),
            root.join("state/tsandrini/notes.git")
        );
        fs::create_dir_all(root.join("main/.git/worktrees/wt")).unwrap();
        fs::create_dir_all(root.join("wt")).unwrap();
        fs::write(root.join("wt/.git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
        assert!(layout.git_dir("wt", UnitKind::Git).is_err());
        let wt = discover(&root, 1)
            .into_iter()
            .find(|f| f.id == "wt")
            .unwrap();
        assert_eq!(
            wt.unsyncable.as_deref(),
            Some(
                format!(
                    "a linked worktree of {}",
                    root.join("wt").join("../main").display()
                )
                .as_str()
            )
        );
        fs::create_dir_all(root.join("main/.git/modules/sub")).unwrap();
        fs::create_dir_all(root.join("main/sub")).unwrap();
        fs::write(root.join("main/sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        assert_eq!(
            layout.git_dir("main/sub", UnitKind::Git).unwrap(),
            root.join("main/sub/../.git/modules/sub")
        );
        let (files, bytes) = size_of(&root.join("meteopress/tmp"), &["x".into()]);
        assert_eq!((files, bytes), (0, 0));
        let (files, bytes) = size_of(&root.join("meteopress/tmp"), &["node_modules".into()]);
        assert_eq!((files, bytes), (1, 4));
        fs::create_dir_all(root.join("meteopress/tmp/node_modules/pkg/node_modules")).unwrap();
        fs::write(root.join("meteopress/tmp/node_modules/pkg/i.js"), "12345").unwrap();
        fs::write(
            root.join("meteopress/tmp/node_modules/pkg/node_modules/j.js"),
            "12",
        )
        .unwrap();
        let m = measure(&root.join("meteopress/tmp"), &["node_modules".into()], true);
        assert_eq!((m.files, m.bytes), (1, 4));
        // NOTE: a nested excluded directory counts under the outer name
        assert_eq!(m.excluded, BTreeMap::from([("node_modules".to_owned(), 7)]));
        assert!(excluded_name("result-3", &["result-*".into()]));
        assert!(!excluded_name(
            "src",
            &["node_modules".into(), "a/b".into()]
        ));
        fs::write(root.join("tsandrini/.git/index.lock"), "").unwrap();
        assert_eq!(
            blocker(&root.join("tsandrini/.git")),
            Some("git is running (index.lock)")
        );
        assert_eq!(blocker(&root.join("tsandrini/ccdash/.git")), None);
        fs::remove_dir_all(&root).unwrap();
    }
}
