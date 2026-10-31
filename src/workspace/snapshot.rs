//! One snapshot of a unit: the working tree as a tree object (a temporary
//! index copied from the real one, `add -A`, `add -f` for the opted-in
//! ignored paths), the staged state as a second tree, both under one
//! wrapper tree (`work/`, `index/`) so they travel together, and a commit
//! on the host's own chain whose message carries HEAD, the branch, the
//! index tree, every branch and tag, the stashes and the empty
//! directories. The real index is never written. See
//! `meta/plans/ccdash-hub/h9.md`.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use super::{
    EMPTY_TREE, HELD_FILES, HELD_SHARE, HeldReport, Rules, SYNC_REFS,
    git::{Git, Opts, Repo},
    units::blocker,
};
use crate::util::now_ms;

/// Below this many tracked files the share rule of the held guard is off.
const HELD_MIN_TRACKED: u64 = 100;
/// Oversize files excluded from one `add` at most (argv has limits).
const MAX_OVERSIZE_EXCLUDES: usize = 500;
const MESSAGE_HEAD: &str = "ccdash snapshot";

/// What a snapshot's message carries besides the tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    /// None for an unborn branch.
    pub head: Option<String>,
    /// `refs/heads/<name>`; None when detached.
    pub branch: Option<String>,
    /// The staged state.
    pub index: String,
    /// Every `refs/heads/*` and `refs/tags/*`.
    pub refs: BTreeMap<String, String>,
    /// Stash entries, newest first (`stash@{n}`).
    pub stash: Vec<(u32, String)>,
    /// Empty directories, which git does not track.
    pub dirs: Vec<String>,
}

impl Meta {
    /// The commit message.
    pub fn message(&self, host: &str) -> String {
        let mut out = format!("{MESSAGE_HEAD} {host}\n\n");
        out.push_str(&format!(
            "head {}\n",
            self.head.as_deref().unwrap_or("unborn")
        ));
        out.push_str(&format!(
            "branch {}\n",
            self.branch.as_deref().unwrap_or("detached")
        ));
        out.push_str(&format!("index {}\n", self.index));
        for (name, sha) in &self.refs {
            out.push_str(&format!("ref {name} {sha}\n"));
        }
        for (n, sha) in &self.stash {
            out.push_str(&format!("stash {n} {sha}\n"));
        }
        for dir in &self.dirs {
            if !dir.contains('\n') {
                out.push_str(&format!("dir {dir}\n"));
            }
        }
        out
    }

    /// The meta of a commit message; None for a commit that is not a
    /// snapshot (a real commit both chains started from).
    pub fn parse(message: &str) -> Option<Self> {
        let mut lines = message.lines();
        if !lines.next()?.starts_with(MESSAGE_HEAD) {
            return None;
        }
        let mut meta = Self::default();
        for line in lines {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            match key {
                "head" => meta.head = (value != "unborn").then(|| value.to_owned()),
                "branch" => meta.branch = (value != "detached").then(|| value.to_owned()),
                "index" => meta.index = value.to_owned(),
                "ref" => {
                    if let Some((name, sha)) = value.rsplit_once(' ') {
                        meta.refs.insert(name.to_owned(), sha.to_owned());
                    }
                }
                "stash" => {
                    if let Some((n, sha)) = value.split_once(' ')
                        && let Ok(n) = n.parse()
                    {
                        meta.stash.push((n, sha.to_owned()));
                    }
                }
                "dir" => meta.dirs.push(value.to_owned()),
                _ => {}
            }
        }
        if meta.index.is_empty() {
            meta.index = EMPTY_TREE.into();
        }
        Some(meta)
    }

    /// The host named in a snapshot message.
    pub fn host_of(message: &str) -> Option<&str> {
        message
            .lines()
            .next()?
            .strip_prefix(MESSAGE_HEAD)
            .map(str::trim)
            .filter(|h| !h.is_empty())
    }
}

/// This host's snapshot of a unit as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub commit: String,
    /// The commit's tree: `work/` and `index/`.
    pub tree: String,
    /// The working tree.
    pub work: String,
    /// The staged state.
    pub index: String,
    pub meta: Meta,
    /// Entries in the working tree.
    pub tracked: u64,
}

/// What taking a snapshot came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Left alone; why.
    Skipped(String),
    /// Nothing changed since `previous`.
    Unchanged,
    /// Too many files added or deleted: kept under `refs/sync/held/<host>`.
    Held(HeldReport),
    Taken(Local),
}

/// A snapshot and what it noticed on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub outcome: Outcome,
    /// Quiescent directories a process holds open: path and process.
    pub waiting: Vec<(String, String)>,
    /// Untracked files above `max_file_mb`, left out.
    pub oversize: Vec<String>,
    /// Nested repositories found (relative to the unit).
    pub nested: Vec<String>,
}

/// Everything a snapshot needs.
pub struct Take<'a> {
    pub git: &'a Git,
    pub repo: &'a Repo,
    /// The unit's scratch directory (temporary indexes).
    pub scratch: &'a Path,
    /// The unit's excludes file, when its rules add to the defaults.
    pub excludes: Option<&'a Path>,
    pub rules: &'a Rules,
    pub host: &'a str,
    /// Directories of nested units, relative to this one: left out of
    /// the tree.
    pub children: &'a [String],
    pub previous: Option<&'a Local>,
    /// The process holding a file open under a quiescent directory.
    pub busy: &'a dyn Fn(&Path) -> Option<String>,
}

/// Where the work index of a unit is kept between the snapshot and the
/// apply that uses it.
pub fn work_index(scratch: &Path) -> PathBuf {
    scratch.join("idx-work")
}

/// Takes the snapshot: `Taken` moves `refs/sync/<host>`.
///
/// # Errors
///
/// git fails, or the scratch directory cannot be written.
pub fn take(t: &Take) -> Result<Report> {
    let mut report = Report {
        outcome: Outcome::Unchanged,
        waiting: Vec::new(),
        oversize: Vec::new(),
        nested: Vec::new(),
    };
    if let Some(reason) = blocker(&t.repo.git_dir) {
        report.outcome = Outcome::Skipped(reason.into());
        return Ok(report);
    }
    fs::create_dir_all(t.scratch)?;
    let real_index = t.repo.git_dir.join("index");
    let (idx_stage, idx_work) = (t.scratch.join("idx-stage"), work_index(t.scratch));

    // the staged state
    copy_index(&real_index, &idx_stage)?;
    let index_tree = match t.git.run(
        Some(t.repo),
        &["write-tree"],
        &with_index(t.excludes, &idx_stage),
    )? {
        out if out.ok => out.text(),
        out if out.stderr.contains("unmerged") || out.stderr.contains("not up to date") => {
            report.outcome = Outcome::Skipped("the index has unmerged entries".into());
            return Ok(report);
        }
        out => anyhow::bail!("git write-tree failed: {}", out.stderr.trim()),
    };

    // the working tree: untracked files and directories first
    copy_index(&real_index, &idx_work)?;
    let listed = t.git.run(
        Some(t.repo),
        &[
            "ls-files",
            "-z",
            "--others",
            "--directory",
            "--exclude-standard",
        ],
        &with_index(t.excludes, &idx_work),
    )?;
    let mut dirs = Vec::new();
    let mut excluded: Vec<String> = t.children.to_vec();
    for entry in listed.records() {
        let Some(path) = entry.strip_suffix('/') else {
            continue;
        };
        let full = t.repo.work_tree.join(path);
        if full.join(".git").exists() {
            report.nested.push(path.to_owned());
            if !excluded.iter().any(|e| e == path) {
                excluded.push(path.to_owned());
            }
        } else if fs::read_dir(&full).is_ok_and(|mut d| d.next().is_none()) {
            dirs.push(path.to_owned());
        }
    }
    let max_bytes = t.rules.max_file_mb << 20;
    let untracked = t.git.run(
        Some(t.repo),
        &["ls-files", "-z", "--others", "--exclude-standard"],
        &with_index(t.excludes, &idx_work),
    )?;
    for path in untracked.records() {
        if report.oversize.len() >= MAX_OVERSIZE_EXCLUDES {
            break;
        }
        if let Ok(meta) = fs::symlink_metadata(t.repo.work_tree.join(&path))
            && meta.is_file()
            && meta.len() > max_bytes
        {
            report.oversize.push(path);
        }
    }
    // NOTE: `git add` refuses an `:(exclude)` pathspec that names an ignored
    // path ("The following paths are ignored"); a bundle root ignoring its
    // repositories (`/*/`) hit this. Ignored children need no exclusion.
    let ignored = t.git.ignored(t.repo, t.excludes, &excluded)?;
    excluded.retain(|p| !ignored.contains(p));
    let mut add: Vec<String> = vec![
        "add".into(),
        "-A".into(),
        "--ignore-errors".into(),
        "--".into(),
        ".".into(),
    ];
    for path in excluded.iter().chain(&report.oversize) {
        add.push(format!(":(exclude){path}"));
    }
    let add_args: Vec<&str> = add.iter().map(String::as_str).collect();
    let added = t
        .git
        .run(Some(t.repo), &add_args, &with_index(t.excludes, &idx_work))?;
    if !added.ok {
        anyhow::bail!("git add failed: {}", added.stderr.trim());
    }

    // opted-in ignored paths
    let previous_tree = t.previous.map(|p| p.work.as_str());
    for entry in &t.rules.sync_ignored {
        for path in expand(&t.repo.work_tree, &entry.pattern) {
            let full = t.repo.work_tree.join(&path);
            if entry.quiescent
                && let Some(process) = (t.busy)(&full)
            {
                // NOTE: a dev database must not travel while its server has it open
                if let Some(prev) = previous_tree {
                    carry_over(t, &idx_work, prev, &path)?;
                }
                report.waiting.push((path, process));
                continue;
            }
            let forced = t.git.run(
                Some(t.repo),
                &["add", "-f", "--ignore-errors", "--", &path],
                &with_index(t.excludes, &idx_work),
            )?;
            if !forced.ok {
                anyhow::bail!("git add -f {path} failed: {}", forced.stderr.trim());
            }
        }
    }
    let work_tree = t.git.write_tree(t.repo, &idx_work)?;
    let tracked = t
        .git
        .run(
            Some(t.repo),
            &["ls-files", "-z"],
            &with_index(t.excludes, &idx_work),
        )?
        .records()
        .len() as u64;

    // HEAD, branches, tags, stashes
    let head = t.git.rev(t.repo, "HEAD")?;
    let branch = t.git.head_branch(t.repo)?;
    let mut refs = BTreeMap::new();
    for prefix in ["refs/heads/", "refs/tags/"] {
        refs.extend(t.git.refs(t.repo, prefix)?);
    }
    let stash_list = match t.git.rev(t.repo, "refs/stash")? {
        Some(_) => t.git.run(
            Some(t.repo),
            &["reflog", "show", "--format=%H", "refs/stash"],
            &Opts::default(),
        )?,
        None => super::git::Output {
            ok: false,
            code: None,
            stdout: Vec::new(),
            stderr: String::new(),
        },
    };
    let stash: Vec<(u32, String)> = if stash_list.ok {
        stash_list
            .text()
            .lines()
            .enumerate()
            .filter_map(|(n, sha)| Some((u32::try_from(n).ok()?, sha.trim().to_owned())))
            .filter(|(_, sha)| !sha.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    dirs.sort();
    let meta = Meta {
        head: head.clone(),
        branch,
        index: index_tree,
        refs,
        stash,
        dirs,
    };

    if let Some(prev) = t.previous
        && prev.work == work_tree
        && prev.meta == meta
    {
        return Ok(report);
    }
    let tree = t.git.wrap(t.repo, &work_tree, &meta.index)?;

    // the held guard, after the first snapshot
    if let Some(prev) = t.previous {
        let (mut added, mut deleted) = (0u64, 0u64);
        for (status, _) in t.git.diff_names(t.repo, &prev.work, &work_tree)? {
            match status {
                'A' => added += 1,
                'D' => deleted += 1,
                _ => {}
            }
        }
        let churn = added + deleted;
        let share =
            prev.tracked >= HELD_MIN_TRACKED && churn as f64 > HELD_SHARE * prev.tracked as f64;
        if churn > HELD_FILES || share {
            let commit =
                t.git
                    .commit_tree(t.repo, &tree, &[&prev.commit], &meta.message(t.host))?;
            t.git
                .update_ref(t.repo, &format!("{}/{}", super::HELD_REFS, t.host), &commit)?;
            report.outcome = Outcome::Held(HeldReport {
                commit,
                added,
                deleted,
                tracked: prev.tracked,
                at: now_ms(),
            });
            return Ok(report);
        }
    }

    let parent = t
        .previous
        .map(|p| p.commit.clone())
        .or_else(|| head.clone());
    let parents: Vec<&str> = parent.iter().map(String::as_str).collect();
    let commit = t
        .git
        .commit_tree(t.repo, &tree, &parents, &meta.message(t.host))?;
    t.git
        .update_ref(t.repo, &format!("{SYNC_REFS}/{}", t.host), &commit)?;
    report.outcome = Outcome::Taken(Local {
        commit,
        tree,
        work: work_tree,
        index: meta.index.clone(),
        meta,
        tracked,
    });
    Ok(report)
}

/// Call options on a temporary index.
fn with_index<'b>(excludes: Option<&'b Path>, index: &'b Path) -> Opts<'b> {
    Opts {
        index: Some(index),
        excludes,
        ..Opts::default()
    }
}

/// A copy of the real index at `to`; none when there is no index (an
/// unborn branch), so git reads an empty one.
fn copy_index(real: &Path, to: &Path) -> Result<()> {
    match fs::copy(real, to) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = fs::remove_file(to);
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("copying {}", real.display())),
    }
}

/// Puts the previous snapshot's entries under `path` into the work index.
fn carry_over(t: &Take, index: &Path, previous_tree: &str, path: &str) -> Result<()> {
    let listed = t.git.run(
        Some(t.repo),
        &["ls-tree", "-r", "-z", previous_tree, "--", path],
        &Opts::default(),
    )?;
    if !listed.ok {
        return Ok(());
    }
    let mut info = Vec::new();
    for record in listed.records() {
        let Some((head, p)) = record.split_once('\t') else {
            continue;
        };
        let mut parts = head.split(' ');
        let (Some(mode), Some(_), Some(oid)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        info.extend_from_slice(format!("{mode} {oid}\t{p}\0").as_bytes());
    }
    if info.is_empty() {
        return Ok(());
    }
    t.git.ok(
        Some(t.repo),
        &["update-index", "-z", "--index-info"],
        &Opts {
            index: Some(index),
            stdin: Some(&info),
            ..Opts::default()
        },
    )?;
    Ok(())
}

/// Paths under `work_tree` matching `pattern`: a literal path, or a glob
/// in its last segment (`.env.*`).
pub fn expand(work_tree: &Path, pattern: &str) -> Vec<String> {
    let pattern = pattern.trim_matches('/');
    if pattern.is_empty() || pattern.contains("..") {
        return Vec::new();
    }
    let (dir, last) = match pattern.rsplit_once('/') {
        Some((d, l)) => (d, l),
        None => ("", pattern),
    };
    if dir.contains(['*', '?']) {
        return Vec::new();
    }
    if !last.contains(['*', '?']) {
        return work_tree
            .join(pattern)
            .symlink_metadata()
            .is_ok()
            .then(|| pattern.to_owned())
            .into_iter()
            .collect();
    }
    let base = if dir.is_empty() {
        work_tree.to_path_buf()
    } else {
        work_tree.join(dir)
    };
    let mut out: Vec<String> = fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| crate::sync::glob(last, name))
        .map(|name| {
            if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            }
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::workspace::{Ignored, git::tests::fixture};

    fn rules() -> Rules {
        Rules {
            always_exclude: vec!["node_modules".into(), "target".into()],
            sync_ignored: vec![
                Ignored {
                    pattern: ".env".into(),
                    quiescent: false,
                },
                Ignored {
                    pattern: ".devenv/state/pg".into(),
                    quiescent: true,
                },
            ],
            max_file_mb: 1,
            conflicts: Default::default(),
            paused: false,
        }
    }

    pub(crate) fn take_in(
        git: &Git,
        repo: &Repo,
        scratch: &Path,
        rules: &Rules,
        previous: Option<&Local>,
        busy: &dyn Fn(&Path) -> Option<String>,
    ) -> Report {
        take(&Take {
            git,
            repo,
            scratch,
            excludes: None,
            rules,
            host: &git.host,
            children: &[],
            previous,
            busy,
        })
        .unwrap()
    }

    /// A bundle root that ignores its repositories (`/*/`) snapshots its
    /// own files; the ignored children are neither added nor named.
    #[test]
    fn root_ignoring_its_children_snapshots() {
        let (dir, git, repo) = fixture("rootignore");
        fs::write(repo.work_tree.join(".gitignore"), "/*/\n!/meta/\n").unwrap();
        fs::create_dir_all(repo.work_tree.join("child")).unwrap();
        git.ok(
            Some(&Repo {
                git_dir: repo.work_tree.join("child/.git"),
                work_tree: repo.work_tree.join("child"),
            }),
            &["init", "-q"],
            &Opts::default(),
        )
        .unwrap();
        fs::write(repo.work_tree.join("child/c.txt"), "c").unwrap();
        fs::create_dir_all(repo.work_tree.join("meta")).unwrap();
        fs::write(repo.work_tree.join("meta/plan.md"), "plan").unwrap();
        let children = vec!["child".to_owned()];
        let report = take(&Take {
            git: &git,
            repo: &repo,
            scratch: &dir.join("scratch"),
            excludes: None,
            rules: &rules(),
            host: &git.host,
            children: &children,
            previous: None,
            busy: &|_| None,
        })
        .unwrap();
        let Outcome::Taken(local) = report.outcome else {
            panic!("{report:?}");
        };
        let files = git
            .ok(
                Some(&repo),
                &["ls-tree", "-r", "--name-only", &local.work],
                &Opts::default(),
            )
            .unwrap()
            .text();
        assert!(files.contains("meta/plan.md"), "{files}");
        assert!(!files.contains("child/"), "{files}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn messages_round_trip() {
        let meta = Meta {
            head: Some("a".repeat(40)),
            branch: Some("refs/heads/main".into()),
            index: "b".repeat(40),
            refs: BTreeMap::from([
                ("refs/heads/main".to_owned(), "a".repeat(40)),
                ("refs/tags/v1 .0".to_owned(), "c".repeat(40)),
            ]),
            stash: vec![(0, "d".repeat(40))],
            dirs: vec!["empty dir".into(), "x/y".into()],
        };
        let text = meta.message("navi");
        assert_eq!(Meta::host_of(&text), Some("navi"));
        assert_eq!(Meta::parse(&text), Some(meta));
        let unborn = Meta::default();
        let back = Meta::parse(&unborn.message("h")).unwrap();
        assert!(back.head.is_none() && back.branch.is_none());
        assert_eq!(back.index, EMPTY_TREE);
        assert!(Meta::parse("a real commit\n\nbody").is_none());
    }

    #[test]
    fn snapshots_carry_tree_index_refs_and_hold_storms() {
        let (dir, git, repo) = fixture("snapshot");
        let scratch = dir.join("scratch");
        let rules = rules();
        let never = |_: &Path| None;
        fs::create_dir_all(repo.work_tree.join("node_modules/pkg")).unwrap();
        fs::write(repo.work_tree.join("node_modules/pkg/i.js"), "x").unwrap();
        fs::create_dir_all(repo.work_tree.join("empty")).unwrap();
        fs::write(repo.work_tree.join(".env"), "SECRET=1").unwrap();
        fs::write(repo.work_tree.join(".gitignore"), ".env\n.devenv\n").unwrap();
        fs::write(repo.work_tree.join("big.bin"), vec![0u8; (1 << 20) + 1]).unwrap();
        fs::write(repo.work_tree.join("staged.txt"), "s").unwrap();
        git.ok(Some(&repo), &["add", "staged.txt"], &Opts::default())
            .unwrap();
        fs::write(repo.work_tree.join("staged.txt"), "s2").unwrap();

        let first = take_in(&git, &repo, &scratch, &rules, None, &never);
        let Outcome::Taken(local) = &first.outcome else {
            panic!("{first:?}");
        };
        assert_eq!(first.oversize, ["big.bin"]);
        let entries = git.ls_tree(&repo, &local.work).unwrap();
        assert!(
            entries.contains_key(".env"),
            "opted-in ignored file travels"
        );
        let wrapper = git
            .ls_tree(&repo, &format!("{}^{{tree}}", local.commit))
            .unwrap();
        assert!(
            wrapper
                .keys()
                .all(|k| k.starts_with("work/") || k.starts_with("index/"))
        );
        assert_eq!(
            git.rev(&repo, &format!("{}:work", local.tree))
                .unwrap()
                .as_deref(),
            Some(local.work.as_str())
        );
        assert_eq!(
            git.rev(&repo, &format!("{}:index", local.tree))
                .unwrap()
                .as_deref(),
            Some(local.index.as_str())
        );
        assert!(!entries.keys().any(|p| p.starts_with("node_modules")));
        assert!(!entries.contains_key("big.bin"));
        assert_eq!(local.meta.dirs, ["empty"]);
        assert_eq!(local.meta.branch.as_deref(), Some("refs/heads/main"));
        assert_eq!(local.meta.refs.len(), 1);
        // the staged version is in the index tree, the working one in the work tree
        let staged = git.ls_tree(&repo, &local.meta.index).unwrap();
        assert_eq!(git.blob(&repo, &staged["staged.txt"].1).unwrap(), b"s");
        assert_eq!(git.blob(&repo, &entries["staged.txt"].1).unwrap(), b"s2");
        assert_eq!(
            git.rev(&repo, "refs/sync/navi").unwrap().as_deref(),
            Some(local.commit.as_str())
        );
        // the parent of the first snapshot is HEAD
        let parent = git
            .ok(
                Some(&repo),
                &["rev-parse", &format!("{}^", local.commit)],
                &Opts::default(),
            )
            .unwrap()
            .text();
        assert_eq!(Some(parent), git.rev(&repo, "HEAD").unwrap());
        // the real index was not touched
        let real = git.write_tree(&repo, &repo.git_dir.join("index")).unwrap();
        assert_eq!(real, local.meta.index);

        let again = take_in(&git, &repo, &scratch, &rules, Some(local), &never);
        assert_eq!(again.outcome, Outcome::Unchanged);

        // a nested repository is left to its own unit
        fs::create_dir_all(repo.work_tree.join("nested/.git")).unwrap();
        fs::write(
            repo.work_tree.join("nested/.git/HEAD"),
            "ref: refs/heads/main\n",
        )
        .unwrap();
        fs::write(repo.work_tree.join("nested/f"), "n").unwrap();
        let third = take_in(&git, &repo, &scratch, &rules, Some(local), &never);
        assert_eq!(third.nested, ["nested"]);
        assert_eq!(third.outcome, Outcome::Unchanged, "nested stays out");
        let local3 = local;

        // a quiescent directory held open keeps the previous content
        fs::create_dir_all(repo.work_tree.join(".devenv/state/pg")).unwrap();
        fs::write(repo.work_tree.join(".devenv/state/pg/data"), "v1").unwrap();
        let with_pg = take_in(&git, &repo, &scratch, &rules, Some(local3), &never);
        let Outcome::Taken(local4) = &with_pg.outcome else {
            panic!("{with_pg:?}");
        };
        assert!(
            git.ls_tree(&repo, &local4.work)
                .unwrap()
                .contains_key(".devenv/state/pg/data")
        );
        fs::write(repo.work_tree.join(".devenv/state/pg/data"), "v2").unwrap();
        let busy = |p: &Path| {
            p.ends_with(".devenv/state/pg")
                .then(|| "postgres".to_owned())
        };
        let held_open = take_in(&git, &repo, &scratch, &rules, Some(local4), &busy);
        assert_eq!(
            held_open.waiting,
            [(".devenv/state/pg".to_owned(), "postgres".to_owned())]
        );
        assert_eq!(
            held_open.outcome,
            Outcome::Unchanged,
            "the old content was carried"
        );

        // a storm of files is held, not snapshotted
        for i in 0..2100 {
            fs::write(repo.work_tree.join(format!("gen{i}.txt")), "g").unwrap();
        }
        let storm = take_in(&git, &repo, &scratch, &rules, Some(local4), &never);
        let Outcome::Held(held) = &storm.outcome else {
            panic!("{:?}", storm.outcome);
        };
        assert_eq!((held.added, held.deleted), (2100, 0));
        assert!(git.rev(&repo, "refs/sync/held/navi").unwrap().is_some());
        assert_eq!(
            git.rev(&repo, "refs/sync/navi").unwrap().as_deref(),
            Some(local4.commit.as_str()),
            "the own ref did not move"
        );

        // a lock skips the unit
        fs::write(repo.git_dir.join("index.lock"), "").unwrap();
        let locked = take_in(&git, &repo, &scratch, &rules, Some(local4), &never);
        assert!(matches!(locked.outcome, Outcome::Skipped(_)));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unborn_repositories_get_a_root_snapshot() {
        let dir = std::env::temp_dir().join(format!("ccdash-unborn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("u")).unwrap();
        let git = Git::prepare(&dir.join("state"), "navi", &[], None, None).unwrap();
        let repo = Repo {
            git_dir: dir.join("u/.git"),
            work_tree: dir.join("u"),
        };
        git.ok(Some(&repo), &["init", "-q"], &Opts::default())
            .unwrap();
        fs::write(dir.join("u/f"), "f").unwrap();
        let report = take_in(&git, &repo, &dir.join("scratch"), &rules(), None, &|_| None);
        let Outcome::Taken(local) = report.outcome else {
            panic!("{report:?}");
        };
        assert!(local.meta.head.is_none());
        assert_eq!(local.meta.index, EMPTY_TREE);
        let parents = git
            .ok(
                Some(&repo),
                &["rev-list", "--parents", "-n", "1", &local.commit],
                &Opts::default(),
            )
            .unwrap()
            .text();
        assert_eq!(parents.split(' ').count(), 1, "no parent");
        assert_eq!(expand(&dir.join("u"), ".env.*"), Vec::<String>::new());
        fs::write(dir.join("u/.env.local"), "").unwrap();
        fs::write(dir.join("u/.env.prod"), "").unwrap();
        assert_eq!(
            expand(&dir.join("u"), ".env.*"),
            [".env.local", ".env.prod"]
        );
        assert_eq!(expand(&dir.join("u"), "f"), ["f"]);
        assert!(expand(&dir.join("u"), "../x").is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
