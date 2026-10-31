//! Units of the workspace root: where they are, what kind they are, the
//! hidden repository of a plain directory, and what makes a unit unsafe
//! to touch (a lock, a rebase in progress).

use std::{
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
        if level >= depth {
            if has_git {
                out.push(Found {
                    id,
                    kind: UnitKind::Git,
                });
            } else if !in_repo {
                out.push(Found {
                    id,
                    kind: UnitKind::Plain,
                });
            }
        } else {
            if has_git {
                out.push(Found {
                    id: id.clone(),
                    kind: UnitKind::Git,
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
    let (mut files, mut bytes) = (0, 0);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if meta.is_dir() {
                if !excluded_name(&name, always_exclude) {
                    stack.push(entry.path());
                }
            } else {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    (files, bytes)
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
