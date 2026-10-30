//! Applying another host's snapshot to this unit: fast-forward when
//! nothing changed here, a 3-way merge otherwise. A snapshot's tree holds
//! the working tree under `work/` and the staged state under `index/`, so
//! one `merge-tree` merges both: conflicted text gets markers, binaries
//! keep the local version, a path staged differently on both sides ends
//! unstaged; both blobs are reported. HEAD, branches, tags and stashes
//! follow the snapshot; a branch moved on both sides is set aside under
//! `refs/sync/diverged/`. The working tree is written through `read-tree
//! -u -m`, which refuses when a file changed underneath (the pass retries
//! after the next snapshot). See `meta/plans/ccdash-hub/h9.md`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, bail};
use tracing::debug;

use super::{
    ConflictMode, ConflictReport, DIVERGED_REFS, Diverged, EMPTY_TREE, Rules, SYNC_REFS,
    git::{Conflicted, Git, Opts, Repo},
    snapshot::{Local, Meta, expand, work_index},
    units::blocker,
};
use crate::util::now_ms;

const EMPTY_OID: &str = "0000000000000000000000000000000000000000";

/// What applying came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The remote snapshot is already in this host's chain.
    Ahead,
    /// Adopted as is: `refs/sync/<host>` is the remote snapshot now.
    FastForward { commit: String },
    /// Merged: `refs/sync/<host>` is a two-parent commit to push.
    Merged { commit: String },
    /// Not now (a lock, a file changed underneath); why.
    Retry(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub outcome: Outcome,
    pub conflicts: Vec<ConflictReport>,
    pub diverged: Vec<Diverged>,
    /// Quiescent directories left as they were: path and the process.
    pub waiting: Vec<(String, String)>,
    /// Paths the working tree changed in.
    pub files: u64,
    /// Refs that could not follow (their objects are missing here).
    pub notes: Vec<String>,
}

/// Everything an apply needs.
pub struct Apply<'a> {
    pub git: &'a Git,
    pub repo: &'a Repo,
    pub scratch: &'a Path,
    pub excludes: Option<&'a Path>,
    pub rules: &'a Rules,
    pub host: &'a str,
    /// This host's fresh snapshot; None for a unit being created.
    pub local: Option<&'a Local>,
    pub other: &'a str,
    /// The other host's snapshot commit.
    pub remote: &'a str,
    pub busy: &'a dyn Fn(&Path) -> Option<String>,
}

/// A snapshot commit read back.
#[derive(Debug, Clone)]
pub struct Snap {
    /// The wrapper tree.
    pub tree: String,
    pub work: String,
    pub index: String,
    pub meta: Meta,
}

/// The trees and meta of a snapshot commit; a real commit (one both
/// chains started from) counts as "committed state, nothing staged".
pub fn read_snapshot(git: &Git, repo: &Repo, commit: &str) -> Result<Snap> {
    let tree = git.tree_of(repo, commit)?;
    let raw = git.ok(
        Some(repo),
        &["cat-file", "commit", commit],
        &Opts::default(),
    )?;
    let text = String::from_utf8_lossy(&raw.stdout);
    let message = text.split_once("\n\n").map_or("", |(_, m)| m);
    match Meta::parse(message) {
        Some(mut meta) => {
            let work = git
                .rev(repo, &format!("{tree}:work"))?
                .with_context(|| format!("snapshot {commit} has no work tree"))?;
            let index = git
                .rev(repo, &format!("{tree}:index"))?
                .with_context(|| format!("snapshot {commit} has no index tree"))?;
            meta.index = index.clone();
            Ok(Snap {
                tree,
                work,
                index,
                meta,
            })
        }
        None => Ok(Snap {
            tree: git.wrap(repo, &tree, &tree)?,
            work: tree.clone(),
            index: tree.clone(),
            meta: Meta {
                head: Some(commit.to_owned()),
                branch: None,
                index: tree,
                ..Meta::default()
            },
        }),
    }
}

/// Applies `remote` to the unit.
///
/// # Errors
///
/// git fails.
pub fn apply(a: &Apply) -> Result<Report> {
    let mut report = Report {
        outcome: Outcome::Ahead,
        conflicts: Vec::new(),
        diverged: Vec::new(),
        waiting: Vec::new(),
        files: 0,
        notes: Vec::new(),
    };
    if let Some(reason) = blocker(&a.repo.git_dir) {
        report.outcome = Outcome::Retry(reason.into());
        return Ok(report);
    }
    fs::create_dir_all(a.scratch)?;
    let remote = read_snapshot(a.git, a.repo, a.remote)?;
    let empty = Local {
        commit: String::new(),
        tree: EMPTY_TREE.into(),
        work: EMPTY_TREE.into(),
        index: EMPTY_TREE.into(),
        meta: Meta {
            index: EMPTY_TREE.into(),
            ..Meta::default()
        },
        tracked: 0,
    };
    let creating = a.local.is_none();
    let local = a.local.unwrap_or(&empty);

    // what the merge is based on
    let base = if creating {
        None
    } else {
        if local.commit == a.remote {
            return Ok(report);
        }
        let mb = a.git.merge_base(a.repo, &local.commit, a.remote)?;
        if mb.as_deref() == Some(a.remote) {
            return Ok(report);
        }
        mb
    };
    let fast_forward = creating || base.as_deref() == Some(local.commit.as_str());
    let (target_work, target_index, target_meta) = if fast_forward {
        (
            remote.work.clone(),
            remote.index.clone(),
            remote.meta.clone(),
        )
    } else {
        let (base_snap, base_known) = match &base {
            Some(c) => {
                let snap = read_snapshot(a.git, a.repo, c)?;
                let known = Meta::host_of(&commit_message(a.git, a.repo, c)?).is_some();
                (Some(snap), known)
            }
            None => (None, false),
        };
        let base_tree = base_snap.as_ref().map_or(EMPTY_TREE, |s| s.tree.as_str());
        let base_meta = base_snap
            .as_ref()
            .map_or_else(|| empty.meta.clone(), |s| s.meta.clone());
        let (ours, theirs) = labels(a, local, a.remote)?;
        let merged = a.git.merge_tree(a.repo, base_tree, &ours, &theirs)?;
        let head_tree = pick_head_tree(a, &base_meta, &local.meta, &remote.meta, base_known)?;
        let mut head_entries: Option<BTreeMap<String, (String, String)>> = None;
        let mut set: Vec<(String, String, String)> = Vec::new();
        let mut remove: Vec<String> = Vec::new();
        for c in &merged.conflicts {
            if let Some(path) = c.path.strip_prefix("index/") {
                // NOTE: staged differently on both sides: unstaged (HEAD's version)
                let entries = match &head_entries {
                    Some(h) => h,
                    None => head_entries.insert(a.git.ls_tree(a.repo, &head_tree)?),
                };
                match entries.get(path) {
                    Some((mode, oid)) => set.push((mode.clone(), oid.clone(), c.path.clone())),
                    None => remove.push(c.path.clone()),
                }
                continue;
            }
            let Some(path) = c.path.strip_prefix("work/") else {
                continue;
            };
            let kind = conflict_kind(a, c)?;
            let keep_local = kind == "binary" || a.rules.conflicts == ConflictMode::SetAside;
            if keep_local {
                match &c.ours {
                    Some(oid) => set.push((c.mode.clone(), oid.clone(), c.path.clone())),
                    None => remove.push(c.path.clone()),
                }
            }
            report.conflicts.push(ConflictReport {
                path: path.to_owned(),
                kind,
                ours: c.ours.clone().unwrap_or_default(),
                theirs: c.theirs.clone().unwrap_or_default(),
                other_host: a.other.to_owned(),
                at: now_ms(),
            });
        }
        let tree = if set.is_empty() && remove.is_empty() {
            merged.tree
        } else {
            edit_tree(a, &merged.tree, &set, &remove)?
        };
        // NOTE: a merge that empties a side drops its entry from the wrapper
        let work = a
            .git
            .rev(a.repo, &format!("{tree}:work"))?
            .unwrap_or_else(|| EMPTY_TREE.into());
        let index = a
            .git
            .rev(a.repo, &format!("{tree}:index"))?
            .unwrap_or_else(|| EMPTY_TREE.into());
        let (refs, diverged, notes) =
            merge_refs(a, &base_meta, &local.meta, &remote.meta, base_known)?;
        report.diverged = diverged;
        report.notes = notes;
        let (head, branch) = merge_head(&base_meta, &local.meta, &remote.meta, base_known, &refs);
        let mut stash = local.meta.stash.clone();
        for (_, sha) in &remote.meta.stash {
            if !stash.iter().any(|(_, s)| s == sha) {
                stash.push((0, sha.clone()));
            }
        }
        let stash = stash
            .into_iter()
            .enumerate()
            .map(|(n, (_, sha))| (u32::try_from(n).unwrap_or(u32::MAX), sha))
            .collect();
        let dirs: BTreeSet<String> = local
            .meta
            .dirs
            .iter()
            .chain(&remote.meta.dirs)
            .cloned()
            .collect();
        (
            work,
            index.clone(),
            Meta {
                head,
                branch,
                index,
                refs,
                stash,
                dirs: dirs.into_iter().collect(),
            },
        )
    };

    // quiescent directories a process holds open stay as they are in the
    // working tree; the snapshot still takes the merged content, and the
    // directory catches up once the process lets go (`catch_up`)
    let mut keep: Vec<String> = Vec::new();
    for entry in a.rules.sync_ignored.iter().filter(|e| e.quiescent) {
        for path in expand(&a.repo.work_tree, &entry.pattern) {
            if let Some(process) = (a.busy)(&a.repo.work_tree.join(&path)) {
                keep.push(path.clone());
                report.waiting.push((path, process));
            }
        }
    }
    let written_work = if keep.is_empty() {
        target_work.clone()
    } else {
        keep_subtrees(a, &target_work, &local.work, &keep)?
    };

    // nothing may have moved under the snapshot
    if !creating {
        let real_index = a.repo.git_dir.join("index");
        let check = a.scratch.join("idx-check");
        match fs::copy(&real_index, &check) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = fs::remove_file(&check);
            }
            Err(e) => return Err(e).context("copying the index"),
        }
        if a.git.write_tree(a.repo, &check)? != local.index {
            report.outcome = Outcome::Retry("the index changed since the snapshot".into());
            return Ok(report);
        }
        if a.git.rev(a.repo, "HEAD")? != local.meta.head
            || a.git.head_branch(a.repo)? != local.meta.branch
        {
            report.outcome = Outcome::Retry("HEAD moved since the snapshot".into());
            return Ok(report);
        }
    }

    // NOTE: the branch tips travel in a push of their own; applying without
    // their objects would lose a branch here, so the apply waits for them
    let mut missing = Vec::new();
    for (name, sha) in &target_meta.refs {
        if a.git.rev(a.repo, sha)?.is_none() {
            missing.push(format!("{name} {}", &sha[..8.min(sha.len())]));
        }
    }
    if !missing.is_empty() {
        report.outcome = Outcome::Retry(format!(
            "waiting for objects {} has not pushed yet: {}",
            a.other,
            missing.join(", ")
        ));
        return Ok(report);
    }

    // the working tree
    let real_index = a.repo.git_dir.join("index");
    if a.repo.git_dir.join("index.lock").exists() {
        report.outcome = Outcome::Retry("git is running (index.lock)".into());
        return Ok(report);
    }
    let backup = a.scratch.join("idx-stage");
    if creating {
        a.git
            .ok(Some(a.repo), &["read-tree", "--empty"], &Opts::default())?;
    } else {
        fs::copy(work_index(a.scratch), &real_index).context("installing the work index")?;
    }
    let changed = a.git.diff_names(a.repo, &local.work, &written_work)?;
    report.files = changed.len() as u64;
    let checkout = a.git.run(
        Some(a.repo),
        &["read-tree", "-u", "-m", &local.work, &written_work],
        &Opts {
            excludes: a.excludes,
            ..Opts::default()
        },
    )?;
    if !checkout.ok {
        if !creating {
            // NOTE: nothing was written; the staged index goes back in place
            if fs::copy(&backup, &real_index).is_err() {
                let _ = fs::remove_file(&real_index);
            }
        }
        report.outcome = Outcome::Retry(format!(
            "the working tree changed underneath: {}",
            checkout
                .stderr
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("read-tree refused")
                .trim()
        ));
        return Ok(report);
    }
    set_staged(a, &written_work, &target_index)?;
    for dir in &target_meta.dirs {
        if !dir.contains("..") {
            let _ = fs::create_dir_all(a.repo.work_tree.join(dir));
        }
    }

    // refs, HEAD, stashes
    let current: BTreeMap<String, String> = ["refs/heads/", "refs/tags/"]
        .into_iter()
        .map(|p| a.git.refs(a.repo, p))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    for (name, sha) in &target_meta.refs {
        if current.get(name) != Some(sha) {
            a.git.update_ref(a.repo, name, sha)?;
        }
    }
    for name in current.keys() {
        if !target_meta.refs.contains_key(name) {
            a.git.delete_ref(a.repo, name)?;
        }
    }
    for d in &report.diverged {
        let short = d.branch.strip_prefix("refs/heads/").unwrap_or(&d.branch);
        if a.git.rev(a.repo, &d.remote)?.is_some() {
            a.git.update_ref(
                a.repo,
                &format!("{DIVERGED_REFS}/{}/{short}", d.host),
                &d.remote,
            )?;
        }
    }
    set_head(a, &target_meta)?;
    let have: BTreeSet<String> = if a.git.rev(a.repo, "refs/stash")?.is_some() {
        let out = a.git.run(
            Some(a.repo),
            &["reflog", "show", "--format=%H", "refs/stash"],
            &Opts::default(),
        )?;
        if out.ok {
            out.text().lines().map(|l| l.trim().to_owned()).collect()
        } else {
            BTreeSet::new()
        }
    } else {
        BTreeSet::new()
    };
    for (_, sha) in target_meta.stash.iter().rev() {
        if have.contains(sha) {
            continue;
        }
        if a.git.rev(a.repo, sha)?.is_none() {
            report
                .notes
                .push(format!("stash {}: not here yet", &sha[..8.min(sha.len())]));
            continue;
        }
        let subject = a
            .git
            .ok(
                Some(a.repo),
                &["log", "-1", "--format=%s", sha],
                &Opts::default(),
            )?
            .text();
        let stored = a.git.run(
            Some(a.repo),
            &["stash", "store", "-m", &subject, sha],
            &Opts::default(),
        )?;
        if !stored.ok {
            report
                .notes
                .push(format!("stash {}: {}", &sha[..8], stored.stderr.trim()));
        }
    }

    // this host's snapshot
    let own = format!("{SYNC_REFS}/{}", a.host);
    if fast_forward {
        a.git.update_ref(a.repo, &own, a.remote)?;
        report.outcome = Outcome::FastForward {
            commit: a.remote.to_owned(),
        };
    } else {
        let tree = a.git.wrap(a.repo, &target_work, &target_index)?;
        let same_as_theirs = tree == remote.tree && target_meta == remote.meta;
        let same_as_ours = tree == local.tree && target_meta == local.meta;
        if same_as_theirs && (!same_as_ours || a.remote < local.commit.as_str()) {
            // NOTE: their snapshot holds everything of ours: take it as this
            // host's instead of minting a merge commit the others would merge
            // again (criss-cross between three hosts never ends otherwise);
            // identical content on both sides settles on the smaller id
            a.git.update_ref(a.repo, &own, a.remote)?;
            report.outcome = Outcome::FastForward {
                commit: a.remote.to_owned(),
            };
        } else if same_as_ours && same_as_theirs {
            // identical content, our commit sorts first: theirs is merged as is
            report.outcome = Outcome::Ahead;
        } else {
            let commit = a.git.commit_tree(
                a.repo,
                &tree,
                &[&local.commit, a.remote],
                &target_meta.message(a.host),
            )?;
            a.git.update_ref(a.repo, &own, &commit)?;
            report.outcome = Outcome::Merged { commit };
        }
    }
    debug!(
        "applied {} from {} to {}: {:?}, {} files",
        &a.remote[..8.min(a.remote.len())],
        a.other,
        a.repo.work_tree.display(),
        report.outcome,
        report.files
    );
    Ok(report)
}

/// The commit message of `commit`.
fn commit_message(git: &Git, repo: &Repo, commit: &str) -> Result<String> {
    let raw = git.ok(
        Some(repo),
        &["cat-file", "commit", commit],
        &Opts::default(),
    )?;
    let text = String::from_utf8_lossy(&raw.stdout);
    Ok(text.split_once("\n\n").map_or("", |(_, m)| m).to_owned())
}

/// The arguments naming the two sides: the sync refs when they still
/// point at the commits (so conflict markers carry the host names), else
/// the commits.
fn labels(a: &Apply, local: &Local, remote: &str) -> Result<(String, String)> {
    let own = format!("{SYNC_REFS}/{}", a.host);
    let theirs = format!("{SYNC_REFS}/{}", a.other);
    let ours = if a.git.rev(a.repo, &own)?.as_deref() == Some(local.commit.as_str()) {
        own
    } else {
        local.commit.clone()
    };
    let theirs = if a.git.rev(a.repo, &theirs)?.as_deref() == Some(remote) {
        theirs
    } else {
        remote.to_owned()
    };
    Ok((ours, theirs))
}

/// `text`, `binary` or `deleted` (one side removed the file).
fn conflict_kind(a: &Apply, c: &Conflicted) -> Result<String> {
    if c.ours.is_none() || c.theirs.is_none() {
        return Ok("deleted".into());
    }
    for oid in [&c.ours, &c.theirs].into_iter().flatten() {
        if a.git.is_binary(a.repo, oid)? {
            return Ok("binary".into());
        }
    }
    Ok("text".into())
}

/// `tree` with entries set and paths removed, through a temporary index.
fn edit_tree(
    a: &Apply,
    tree: &str,
    set: &[(String, String, String)],
    remove: &[String],
) -> Result<String> {
    let idx = a.scratch.join("idx-edit");
    let _ = fs::remove_file(&idx);
    a.git.ok(
        Some(a.repo),
        &["read-tree", tree],
        &Opts {
            index: Some(&idx),
            ..Opts::default()
        },
    )?;
    // NOTE: removals first: a path in both lists is replaced, not dropped
    let mut info = Vec::new();
    for path in remove {
        info.extend_from_slice(format!("0 {EMPTY_OID}\t{path}\0").as_bytes());
    }
    for (mode, oid, path) in set {
        info.extend_from_slice(format!("{mode} {oid}\t{path}\0").as_bytes());
    }
    a.git.ok(
        Some(a.repo),
        &["update-index", "-z", "--index-info"],
        &Opts {
            index: Some(&idx),
            stdin: Some(&info),
            ..Opts::default()
        },
    )?;
    a.git.write_tree(a.repo, &idx)
}

/// `target` with the subtrees at `paths` taken from `local` instead.
fn keep_subtrees(a: &Apply, target: &str, local: &str, paths: &[String]) -> Result<String> {
    let mut set = Vec::new();
    let mut remove = Vec::new();
    for path in paths {
        let gone = a.git.run(
            Some(a.repo),
            &["ls-tree", "-r", "-z", target, "--", path],
            &Opts::default(),
        )?;
        for record in gone.records() {
            if let Some((_, p)) = record.split_once('\t') {
                remove.push(p.to_owned());
            }
        }
        let kept = a.git.run(
            Some(a.repo),
            &["ls-tree", "-r", "-z", local, "--", path],
            &Opts::default(),
        )?;
        for record in kept.records() {
            if let Some((head, p)) = record.split_once('\t') {
                let mut parts = head.split(' ');
                if let (Some(mode), Some(_), Some(oid)) = (parts.next(), parts.next(), parts.next())
                {
                    set.push((mode.to_owned(), oid.to_owned(), p.to_owned()));
                }
            }
        }
    }
    if set.is_empty() && remove.is_empty() {
        return Ok(target.to_owned());
    }
    edit_tree(a, target, &set, &remove)
}

/// The tree of the HEAD the merge lands on, for unstaging conflicted
/// index entries.
fn pick_head_tree(
    a: &Apply,
    base: &Meta,
    local: &Meta,
    remote: &Meta,
    base_known: bool,
) -> Result<String> {
    let (head, _) = merge_head(base, local, remote, base_known, &BTreeMap::new());
    match head {
        Some(sha) if a.git.rev(a.repo, &sha)?.is_some() => a.git.tree_of(a.repo, &sha),
        _ => Ok(EMPTY_TREE.to_owned()),
    }
}

/// Branch and tag tips after the merge, the branches that moved on both
/// sides, and notes on what could not be followed.
#[allow(clippy::type_complexity)]
fn merge_refs(
    a: &Apply,
    base: &Meta,
    local: &Meta,
    remote: &Meta,
    base_known: bool,
) -> Result<(BTreeMap<String, String>, Vec<Diverged>, Vec<String>)> {
    let mut out = BTreeMap::new();
    let mut diverged = Vec::new();
    let mut notes = Vec::new();
    let names: BTreeSet<&String> = local
        .refs
        .keys()
        .chain(remote.refs.keys())
        .chain(base.refs.keys().filter(|_| base_known))
        .collect();
    for name in names {
        let (b, l, r) = (
            base.refs.get(name).filter(|_| base_known),
            local.refs.get(name),
            remote.refs.get(name),
        );
        let pick: Option<String> = if l == r || r == b {
            l.cloned()
        } else if l == b {
            r.cloned()
        } else {
            match (l, r) {
                (Some(l), Some(r)) => {
                    if is_ancestor(a, l, r)? {
                        Some(r.clone())
                    } else if is_ancestor(a, r, l)? {
                        Some(l.clone())
                    } else {
                        diverged.push(Diverged {
                            branch: name.clone(),
                            local: l.clone(),
                            remote: r.clone(),
                            host: a.other.to_owned(),
                        });
                        Some(l.clone())
                    }
                }
                // NOTE: added on one side with an unknown base: it stays
                (Some(l), None) => Some(l.clone()),
                (None, Some(r)) if b.is_none() => Some(r.clone()),
                (None, Some(_)) => {
                    notes.push(format!("{name}: deleted here, moved on {}", a.other));
                    None
                }
                (None, None) => None,
            }
        };
        if let Some(sha) = pick {
            out.insert(name.clone(), sha);
        }
    }
    Ok((out, diverged, notes))
}

fn is_ancestor(a: &Apply, ancestor: &str, descendant: &str) -> Result<bool> {
    let out = a.git.run(
        Some(a.repo),
        &["merge-base", "--is-ancestor", ancestor, descendant],
        &Opts::default(),
    )?;
    Ok(out.ok)
}

/// HEAD and the checked-out branch after the merge: the side that moved
/// since the base; both moved → the local one stays.
fn merge_head(
    base: &Meta,
    local: &Meta,
    remote: &Meta,
    base_known: bool,
    refs: &BTreeMap<String, String>,
) -> (Option<String>, Option<String>) {
    let branch = if local.branch == remote.branch {
        local.branch.clone()
    } else if base_known && local.branch == base.branch {
        remote.branch.clone()
    } else {
        local.branch.clone()
    };
    let head = match &branch {
        Some(b) => refs.get(b).cloned().or_else(|| {
            if branch == remote.branch {
                remote.head.clone()
            } else {
                local.head.clone()
            }
        }),
        None => {
            if local.head == remote.head || remote.head == base.head {
                local.head.clone()
            } else if local.head == base.head {
                remote.head.clone()
            } else {
                local.head.clone()
            }
        }
    };
    (head, branch)
}

/// Sets the real index (which lists the working tree after the checkout)
/// to the staged state: only the entries that differ lose their stat data.
fn set_staged(a: &Apply, work_tree: &str, index_tree: &str) -> Result<()> {
    if work_tree == index_tree {
        return Ok(());
    }
    let out = a.git.ok(
        Some(a.repo),
        &["diff-tree", "-r", "-z", work_tree, index_tree],
        &Opts::default(),
    )?;
    let records = out.records();
    let mut info = Vec::new();
    let mut i = 0;
    while i + 1 < records.len() {
        let (head, path) = (&records[i], &records[i + 1]);
        i += 2;
        let fields: Vec<&str> = head.trim_start_matches(':').split(' ').collect();
        let [_mode_a, mode_b, _oid_a, oid_b, status] = fields[..] else {
            continue;
        };
        if status.starts_with('D') {
            info.extend_from_slice(format!("0 {EMPTY_OID}\t{path}\0").as_bytes());
        } else {
            info.extend_from_slice(format!("{mode_b} {oid_b}\t{path}\0").as_bytes());
        }
    }
    if info.is_empty() {
        return Ok(());
    }
    a.git.ok(
        Some(a.repo),
        &["update-index", "-z", "--index-info"],
        &Opts {
            stdin: Some(&info),
            ..Opts::default()
        },
    )?;
    Ok(())
}

/// HEAD as the target meta has it.
fn set_head(a: &Apply, target: &Meta) -> Result<()> {
    match &target.branch {
        Some(branch) => {
            if a.git.head_branch(a.repo)?.as_deref() != Some(branch.as_str()) {
                a.git.ok(
                    Some(a.repo),
                    &["symbolic-ref", "HEAD", branch],
                    &Opts::default(),
                )?;
            }
        }
        None => {
            if let Some(sha) = &target.head
                && a.git.rev(a.repo, sha)?.is_some()
            {
                a.git.ok(
                    Some(a.repo),
                    &["update-ref", "--no-deref", "HEAD", sha],
                    &Opts::default(),
                )?;
            }
        }
    }
    Ok(())
}

/// Fast-forwards an empty repository to `remote` (a unit the hub has and
/// this host lacks).
///
/// # Errors
///
/// git fails.
pub fn create(
    git: &Git,
    repo: &Repo,
    scratch: &Path,
    rules: &Rules,
    host: &str,
    other: &str,
    remote: &str,
) -> Result<Report> {
    apply(&Apply {
        git,
        repo,
        scratch,
        excludes: None,
        rules,
        host,
        local: None,
        other,
        remote,
        busy: &|_| None,
    })
}

/// Writes the snapshot's content of `paths` (quiescent directories that
/// lagged while a process held them) into the working tree, through a
/// temporary index so the real one is untouched; the paths that are
/// still held stay.
///
/// # Errors
///
/// git fails.
pub fn catch_up(
    git: &Git,
    repo: &Repo,
    scratch: &Path,
    snapshot: &Local,
    paths: &[String],
    busy: &dyn Fn(&Path) -> Option<String>,
) -> Result<Vec<String>> {
    let mut done = Vec::new();
    let free: Vec<&String> = paths
        .iter()
        .filter(|p| busy(&repo.work_tree.join(p)).is_none())
        .collect();
    if free.is_empty() {
        return Ok(done);
    }
    let idx = scratch.join("idx-catchup");
    let _ = fs::remove_file(&idx);
    let opts = Opts {
        index: Some(&idx),
        ..Opts::default()
    };
    // the directory as it is now
    git.ok(Some(repo), &["read-tree", &snapshot.work], &opts)?;
    for p in &free {
        let listed = git.run(
            Some(repo),
            &["ls-tree", "-r", "-z", &snapshot.work, "--", p],
            &Opts::default(),
        )?;
        let mut info = Vec::new();
        for record in listed.records() {
            if let Some((_, path)) = record.split_once('\t') {
                info.extend_from_slice(format!("0 {EMPTY_OID}\t{path}\0").as_bytes());
            }
        }
        if !info.is_empty() {
            git.ok(
                Some(repo),
                &["update-index", "-z", "--index-info"],
                &Opts {
                    index: Some(&idx),
                    stdin: Some(&info),
                    ..Opts::default()
                },
            )?;
        }
        git.ok(
            Some(repo),
            &["add", "-f", "--ignore-errors", "--", p],
            &opts,
        )?;
    }
    let current = git.write_tree(repo, &idx)?;
    if current == snapshot.work {
        return Ok(free.into_iter().cloned().collect());
    }
    let out = git.run(
        Some(repo),
        &["read-tree", "-u", "-m", &current, &snapshot.work],
        &opts,
    )?;
    if !out.ok {
        bail!(
            "catching up: {}",
            out.stderr
                .lines()
                .next()
                .unwrap_or("read-tree refused")
                .trim()
        );
    }
    done.extend(free.into_iter().cloned());
    Ok(done)
}

/// Puts the working tree and the staged state back to `previous` from
/// `held_work` (the held snapshot's working tree, which is what the tree
/// holds now): the drop of a held snapshot.
///
/// # Errors
///
/// git fails, or the working tree changed meanwhile.
pub fn roll_back(
    git: &Git,
    repo: &Repo,
    scratch: &Path,
    rules: &Rules,
    held_work: &str,
    previous: &Local,
) -> Result<()> {
    if let Some(reason) = blocker(&repo.git_dir) {
        bail!("{reason}");
    }
    let real_index = repo.git_dir.join("index");
    fs::copy(work_index(scratch), &real_index).context("installing the work index")?;
    let out = git.run(
        Some(repo),
        &["read-tree", "-u", "-m", held_work, &previous.work],
        &Opts::default(),
    )?;
    if !out.ok {
        let _ = fs::copy(scratch.join("idx-stage"), &real_index);
        bail!(
            "the working tree changed underneath: {}",
            out.stderr
                .lines()
                .next()
                .unwrap_or("read-tree refused")
                .trim()
        );
    }
    let a = Apply {
        git,
        repo,
        scratch,
        excludes: None,
        rules,
        host: &git.host,
        local: Some(previous),
        other: "",
        remote: "",
        busy: &|_| None,
    };
    set_staged(&a, &previous.work, &previous.index)
}

/// Writes one side of a conflict over the file: `mine` the local blob,
/// `theirs` the other host's; the next snapshot carries it. An empty blob
/// id deletes the file (the side that had none).
///
/// # Errors
///
/// The blob is missing or the file cannot be written.
pub fn resolve(git: &Git, repo: &Repo, path: &str, blob: &str) -> Result<()> {
    if !crate::sync::valid_path(path) {
        bail!("bad path {path:?}");
    }
    let target = repo.work_tree.join(path);
    if blob.is_empty() {
        return match fs::remove_file(&target) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        };
    }
    let data = git.blob(repo, blob)?;
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = target.with_extension("ccdash-resolve");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, &target)?;
    Ok(())
}

/// Whether a file still carries conflict markers.
pub fn has_markers(path: &Path) -> bool {
    let Ok(text) = fs::read(path) else {
        return false;
    };
    let text = String::from_utf8_lossy(&text);
    text.lines().any(|l| l.starts_with("<<<<<<< "))
        && text.lines().any(|l| l.starts_with(">>>>>>> "))
}

/// `refs/sync/<host>` read back as this host's snapshot (after a restart).
pub fn own_snapshot(git: &Git, repo: &Repo, host: &str) -> Result<Option<Local>> {
    let Some(commit) = git.rev(repo, &format!("{SYNC_REFS}/{host}"))? else {
        return Ok(None);
    };
    let snap = read_snapshot(git, repo, &commit)?;
    let tracked = git.ls_tree(repo, &snap.work)?.len() as u64;
    Ok(Some(Local {
        commit,
        tree: snap.tree,
        work: snap.work,
        index: snap.index,
        meta: snap.meta,
        tracked,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{
        Ignored,
        git::tests::fixture,
        snapshot::{Outcome as Snap, tests::take_in},
    };

    fn rules() -> Rules {
        Rules {
            always_exclude: vec!["node_modules".into()],
            sync_ignored: vec![Ignored {
                pattern: ".devenv/state/pg".into(),
                quiescent: true,
            }],
            max_file_mb: 64,
            conflicts: ConflictMode::Markers,
            paused: false,
        }
    }

    /// A second clone of the fixture repository, as another host has it.
    fn clone_as(git: &Git, from: &Repo, dir: &Path, host: &str) -> (Git, Repo) {
        let to = Repo {
            git_dir: dir.join(format!("{host}/.git")),
            work_tree: dir.join(host),
        };
        git.ok(
            None,
            &[
                "clone",
                "-q",
                &from.work_tree.to_string_lossy(),
                &to.work_tree.to_string_lossy(),
            ],
            &Opts::default(),
        )
        .unwrap();
        let mut g = git.clone();
        g.host = host.to_owned();
        (g, to)
    }

    /// Carries `refs/sync/<host>` and the tips from one repository to
    /// another, as a fetch from the hub would.
    fn carry(from: &Repo, to: &Repo, git: &Git, host: &str) {
        git.ok(
            Some(to),
            &[
                "fetch",
                "-q",
                &from.git_dir.to_string_lossy(),
                &format!("+refs/sync/{host}:refs/sync/{host}"),
                &format!("+refs/heads/*:refs/sync-tips/{host}/heads/*"),
            ],
            &Opts::default(),
        )
        .unwrap();
        // NOTE: the stash ref may not exist on the source
        let _ = git.run(
            Some(to),
            &[
                "fetch",
                "-q",
                &from.git_dir.to_string_lossy(),
                &format!("+refs/stash:refs/sync-tips/{host}/stash/0"),
            ],
            &Opts::default(),
        );
    }

    fn snapshot(git: &Git, repo: &Repo, scratch: &Path, prev: Option<&Local>) -> Local {
        match take_in(git, repo, scratch, &rules(), prev, &|_| None).outcome {
            Snap::Taken(local) => local,
            Snap::Unchanged => prev.unwrap().clone(),
            other => panic!("{other:?}"),
        }
    }

    /// This host's snapshot as it stands, with the scratch work index
    /// refreshed (what the scheduler does before every apply).
    fn fresh(git: &Git, repo: &Repo, scratch: &Path) -> Local {
        let own = own_snapshot(git, repo, &git.host).unwrap().unwrap();
        snapshot(git, repo, scratch, Some(&own))
    }

    fn apply_from(
        git: &Git,
        repo: &Repo,
        scratch: &Path,
        local: &Local,
        other: &str,
        remote: &str,
    ) -> Report {
        apply(&Apply {
            git,
            repo,
            scratch,
            excludes: None,
            rules: &rules(),
            host: &git.host,
            local: Some(local),
            other,
            remote,
            busy: &|_| None,
        })
        .unwrap()
    }

    /// Three hosts merging each other's snapshots at once settle on one
    /// commit instead of merging each other's merge commits forever.
    #[test]
    fn three_hosts_settle_without_criss_cross() {
        let (dir, navi_git, navi) = fixture("crisscross");
        let (flat_git, flat) = clone_as(&navi_git, &navi, &dir, "flat");
        let (krup_git, krup) = clone_as(&navi_git, &navi, &dir, "krup");
        let hosts = [
            ("navi", &navi_git, &navi, dir.join("s-navi")),
            ("flat", &flat_git, &flat, dir.join("s-flat")),
            ("krup", &krup_git, &krup, dir.join("s-krup")),
        ];
        fs::write(navi.work_tree.join("a.txt"), "navi\n").unwrap();
        fs::write(flat.work_tree.join("b.txt"), "flat\n").unwrap();
        for (_, git, repo, scratch) in &hosts {
            snapshot(git, repo, scratch, None);
        }
        let mut merges = Vec::new();
        for _round in 0..8 {
            let mut made = 0;
            for (name, git, repo, scratch) in &hosts {
                for (other, _, other_repo, _) in &hosts {
                    if other == name {
                        continue;
                    }
                    carry(other_repo, repo, git, other);
                    let local = fresh(git, repo, scratch);
                    let remote = git
                        .rev(repo, &format!("refs/sync/{other}"))
                        .unwrap()
                        .unwrap();
                    let r = apply_from(git, repo, scratch, &local, other, &remote);
                    if matches!(r.outcome, Outcome::Merged { .. }) {
                        made += 1;
                    }
                    assert!(r.conflicts.is_empty(), "{r:?}");
                }
            }
            merges.push(made);
            if made == 0 {
                break;
            }
        }
        assert_eq!(
            *merges.last().unwrap(),
            0,
            "merge commits kept coming: {merges:?}"
        );
        assert!(merges.len() <= 4, "{merges:?}");
        let tips: Vec<String> = hosts
            .iter()
            .map(|(name, git, repo, _)| {
                git.rev(repo, &format!("refs/sync/{name}"))
                    .unwrap()
                    .unwrap()
            })
            .collect();
        assert!(tips.iter().all(|t| t == &tips[0]), "{tips:?}");
        for (_, _, repo, _) in &hosts {
            assert_eq!(
                fs::read_to_string(repo.work_tree.join("a.txt")).unwrap(),
                "navi\n"
            );
            assert_eq!(
                fs::read_to_string(repo.work_tree.join("b.txt")).unwrap(),
                "flat\n"
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    fn status(git: &Git, repo: &Repo) -> String {
        let out = git
            .ok(Some(repo), &["status", "--porcelain"], &Opts::default())
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
    }

    #[test]
    fn fast_forwards_merges_and_marks_conflicts() {
        let (dir, navi_git, navi) = fixture("apply");
        let (flat_git, flat) = clone_as(&navi_git, &navi, &dir, "flat");
        let (ns, fsx) = (dir.join("scratch-navi"), dir.join("scratch-flat"));

        // both take a first snapshot off the same commit: identical content,
        // so they settle on one of the two commits without a merge commit
        let f0 = snapshot(&flat_git, &flat, &fsx, None);
        let n0 = snapshot(&navi_git, &navi, &ns, None);
        carry(&navi, &flat, &flat_git, "navi");
        let r = apply_from(&flat_git, &flat, &fsx, &f0, "navi", &n0.commit);
        assert!(!matches!(r.outcome, Outcome::Merged { .. }), "{r:?}");
        let f1 = fresh(&flat_git, &flat, &fsx);
        carry(&flat, &navi, &navi_git, "flat");
        let r = apply_from(&navi_git, &navi, &ns, &n0, "flat", &f1.commit);
        assert!(!matches!(r.outcome, Outcome::Merged { .. }), "{r:?}");
        let n1 = fresh(&navi_git, &navi, &ns);
        assert_eq!(n1.commit, f1.commit);
        assert!(n1.commit == n0.commit || n1.commit == f0.commit);

        // an edit, a staged file and an empty directory on navi fast-forward flat
        fs::write(navi.work_tree.join("a.txt"), "one\ntwo\n").unwrap();
        fs::write(navi.work_tree.join("new.txt"), "n").unwrap();
        fs::create_dir_all(navi.work_tree.join("empty")).unwrap();
        navi_git
            .ok(Some(&navi), &["add", "new.txt"], &Opts::default())
            .unwrap();
        let n2 = snapshot(&navi_git, &navi, &ns, Some(&n1));
        assert_ne!(n2.index, n1.index, "new.txt is staged");
        carry(&navi, &flat, &flat_git, "navi");
        let r = apply_from(&flat_git, &flat, &fsx, &f1, "navi", &n2.commit);
        assert!(matches!(r.outcome, Outcome::FastForward { .. }), "{r:?}");
        assert!(r.conflicts.is_empty());
        assert_eq!(
            fs::read_to_string(flat.work_tree.join("a.txt")).unwrap(),
            "one\ntwo\n"
        );
        assert!(flat.work_tree.join("empty").is_dir());
        let staged = flat_git
            .write_tree(&flat, &flat.git_dir.join("index"))
            .unwrap();
        assert_eq!(staged, n2.index, "new.txt is staged on flat too");
        assert_eq!(status(&flat_git, &flat), " M a.txt\nA  new.txt");
        assert_eq!(
            flat_git.rev(&flat, "refs/sync/flat").unwrap().as_deref(),
            Some(n2.commit.as_str())
        );
        let f2 = fresh(&flat_git, &flat, &fsx);
        assert_eq!(f2.commit, n2.commit, "the trees agree, nothing new");

        // disjoint concurrent edits merge on both sides
        fs::write(navi.work_tree.join("a.txt"), "zero\none\ntwo\n").unwrap();
        let n3 = snapshot(&navi_git, &navi, &ns, Some(&n2));
        fs::write(flat.work_tree.join("b.txt"), "b\n").unwrap();
        let f3 = snapshot(&flat_git, &flat, &fsx, Some(&f2));
        carry(&navi, &flat, &flat_git, "navi");
        let r = apply_from(&flat_git, &flat, &fsx, &f3, "navi", &n3.commit);
        let Outcome::Merged { commit: m } = &r.outcome else {
            panic!("{r:?}");
        };
        assert!(r.conflicts.is_empty());
        assert_eq!(
            fs::read_to_string(flat.work_tree.join("a.txt")).unwrap(),
            "zero\none\ntwo\n"
        );
        assert!(flat.work_tree.join("b.txt").is_file());
        assert_eq!(status(&flat_git, &flat), " M a.txt\nA  new.txt\n?? b.txt");
        carry(&flat, &navi, &navi_git, "flat");
        let r = apply_from(&navi_git, &navi, &ns, &n3, "flat", m);
        assert!(matches!(r.outcome, Outcome::FastForward { .. }), "{r:?}");
        assert!(navi.work_tree.join("b.txt").is_file());
        assert_eq!(status(&navi_git, &navi), status(&flat_git, &flat));
        let n4 = fresh(&navi_git, &navi, &ns);
        let f4 = fresh(&flat_git, &flat, &fsx);
        assert_eq!(
            (n4.commit.as_str(), f4.commit.as_str()),
            (m.as_str(), m.as_str())
        );

        // the same hunk on both sides: markers on the applying side, both blobs reported
        fs::write(navi.work_tree.join("a.txt"), "NAVI\none\ntwo\n").unwrap();
        let n5 = snapshot(&navi_git, &navi, &ns, Some(&n4));
        fs::write(flat.work_tree.join("a.txt"), "FLAT\none\ntwo\n").unwrap();
        let f5 = snapshot(&flat_git, &flat, &fsx, Some(&f4));
        carry(&navi, &flat, &flat_git, "navi");
        let r = apply_from(&flat_git, &flat, &fsx, &f5, "navi", &n5.commit);
        assert!(matches!(r.outcome, Outcome::Merged { .. }), "{r:?}");
        assert_eq!(r.conflicts.len(), 1, "{r:?}");
        let c = &r.conflicts[0];
        assert_eq!(
            (c.path.as_str(), c.kind.as_str(), c.other_host.as_str()),
            ("a.txt", "text", "navi")
        );
        let text = fs::read_to_string(flat.work_tree.join("a.txt")).unwrap();
        assert!(
            text.contains("<<<<<<< refs/sync/flat") && text.contains(">>>>>>> refs/sync/navi"),
            "{text}"
        );
        assert!(has_markers(&flat.work_tree.join("a.txt")));
        assert_eq!(flat_git.blob(&flat, &c.ours).unwrap(), b"FLAT\none\ntwo\n");
        assert_eq!(
            flat_git.blob(&flat, &c.theirs).unwrap(),
            b"NAVI\none\ntwo\n"
        );
        // keep mine, as the panel would
        resolve(&flat_git, &flat, "a.txt", &c.ours).unwrap();
        assert!(!has_markers(&flat.work_tree.join("a.txt")));
        let f6 = fresh(&flat_git, &flat, &fsx);
        carry(&flat, &navi, &navi_git, "flat");
        let r = apply_from(&navi_git, &navi, &ns, &n5, "flat", &f6.commit);
        assert!(matches!(r.outcome, Outcome::FastForward { .. }), "{r:?}");
        assert_eq!(
            fs::read_to_string(navi.work_tree.join("a.txt")).unwrap(),
            "FLAT\none\ntwo\n"
        );

        // a binary conflict keeps the local bytes
        let n7 = fresh(&navi_git, &navi, &ns);
        fs::write(navi.work_tree.join("img.bin"), [0u8, 1, 2, 3, 0, 9]).unwrap();
        let n8 = snapshot(&navi_git, &navi, &ns, Some(&n7));
        let f8 = fresh(&flat_git, &flat, &fsx);
        fs::write(flat.work_tree.join("img.bin"), [0u8, 7, 7, 7, 0, 9]).unwrap();
        let f9 = snapshot(&flat_git, &flat, &fsx, Some(&f8));
        carry(&navi, &flat, &flat_git, "navi");
        let r = apply_from(&flat_git, &flat, &fsx, &f9, "navi", &n8.commit);
        assert!(matches!(r.outcome, Outcome::Merged { .. }), "{r:?}");
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].kind, "binary");
        assert_eq!(
            fs::read(flat.work_tree.join("img.bin")).unwrap(),
            [0u8, 7, 7, 7, 0, 9]
        );

        // staging the same file differently on both sides unstages it
        let n9 = fresh(&navi_git, &navi, &ns);
        let f10 = fresh(&flat_git, &flat, &fsx);
        for (git, repo, text) in [(&navi_git, &navi, "N\n"), (&flat_git, &flat, "F\n")] {
            fs::write(repo.work_tree.join("s.txt"), text).unwrap();
            git.ok(Some(repo), &["add", "s.txt"], &Opts::default())
                .unwrap();
        }
        let n10 = snapshot(&navi_git, &navi, &ns, Some(&n9));
        let f11 = snapshot(&flat_git, &flat, &fsx, Some(&f10));
        carry(&navi, &flat, &flat_git, "navi");
        let r = apply_from(&flat_git, &flat, &fsx, &f11, "navi", &n10.commit);
        assert!(matches!(r.outcome, Outcome::Merged { .. }), "{r:?}");
        assert!(r.conflicts.iter().any(|c| c.path == "s.txt"), "{r:?}");
        let st = status(&flat_git, &flat);
        assert!(st.contains("?? s.txt"), "unstaged again: {st}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn branches_head_and_stashes_follow_and_diverge() {
        let (dir, navi_git, navi) = fixture("refs");
        let (flat_git, flat) = clone_as(&navi_git, &navi, &dir, "flat");
        let (ns, fsx) = (dir.join("scratch-navi"), dir.join("scratch-flat"));
        let f0 = snapshot(&flat_git, &flat, &fsx, None);
        let n0 = snapshot(&navi_git, &navi, &ns, None);
        carry(&navi, &flat, &flat_git, "navi");
        let r = apply_from(&flat_git, &flat, &fsx, &f0, "navi", &n0.commit);
        assert!(!matches!(r.outcome, Outcome::Merged { .. }), "{r:?}");
        let f1 = fresh(&flat_git, &flat, &fsx);
        carry(&flat, &navi, &navi_git, "flat");
        let r = apply_from(&navi_git, &navi, &ns, &n0, "flat", &f1.commit);
        assert!(!matches!(r.outcome, Outcome::Merged { .. }), "{r:?}");
        let n1 = fresh(&navi_git, &navi, &ns);
        assert_eq!(n1.commit, f1.commit);

        // navi commits on main and switches to a new branch; flat follows
        fs::write(navi.work_tree.join("c.txt"), "c").unwrap();
        navi_git
            .ok(Some(&navi), &["add", "c.txt"], &Opts::default())
            .unwrap();
        let tree = navi_git
            .write_tree(&navi, &navi.git_dir.join("index"))
            .unwrap();
        let head = navi_git.rev(&navi, "HEAD").unwrap().unwrap();
        let c = navi_git.commit_tree(&navi, &tree, &[&head], "c").unwrap();
        navi_git.update_ref(&navi, "refs/heads/main", &c).unwrap();
        navi_git
            .update_ref(&navi, "refs/heads/feature", &c)
            .unwrap();
        navi_git
            .ok(
                Some(&navi),
                &["symbolic-ref", "HEAD", "refs/heads/feature"],
                &Opts::default(),
            )
            .unwrap();
        // a stash made of plumbing: the index commit as the second parent
        let idx = navi_git
            .commit_tree(&navi, &tree, &[&c], "index on feature")
            .unwrap();
        let st = navi_git
            .commit_tree(&navi, &tree, &[&c, &idx], "WIP on feature: stash")
            .unwrap();
        navi_git
            .ok(
                Some(&navi),
                &[
                    "update-ref",
                    "--create-reflog",
                    "-m",
                    "WIP on feature",
                    "refs/stash",
                    &st,
                ],
                &Opts::default(),
            )
            .unwrap();
        let n2 = snapshot(&navi_git, &navi, &ns, Some(&n1));
        assert_eq!(n2.meta.branch.as_deref(), Some("refs/heads/feature"));
        assert_eq!(n2.meta.stash.len(), 1);
        carry(&navi, &flat, &flat_git, "navi");
        let r = apply_from(&flat_git, &flat, &fsx, &f1, "navi", &n2.commit);
        assert!(matches!(r.outcome, Outcome::FastForward { .. }), "{r:?}");
        assert_eq!(
            flat_git.head_branch(&flat).unwrap().as_deref(),
            Some("refs/heads/feature")
        );
        assert_eq!(
            flat_git.rev(&flat, "refs/heads/main").unwrap().as_deref(),
            Some(c.as_str())
        );
        assert_eq!(
            flat_git
                .rev(&flat, "refs/heads/feature")
                .unwrap()
                .as_deref(),
            Some(c.as_str())
        );
        assert!(flat.work_tree.join("c.txt").is_file());
        let stashes = flat_git
            .ok(Some(&flat), &["stash", "list"], &Opts::default())
            .unwrap()
            .text();
        assert!(
            stashes.contains("WIP on feature"),
            "{stashes} {:?}",
            r.notes
        );
        assert_eq!(
            status(&flat_git, &flat),
            "",
            "clean after following a commit"
        );

        // both move main: the local one stays, the remote is set aside
        let f2 = fresh(&flat_git, &flat, &fsx);
        for (git, repo, name) in [(&navi_git, &navi, "n"), (&flat_git, &flat, "f")] {
            fs::write(repo.work_tree.join(format!("{name}.txt")), name).unwrap();
            git.ok(
                Some(repo),
                &["add", &format!("{name}.txt")],
                &Opts::default(),
            )
            .unwrap();
            let t = git.write_tree(repo, &repo.git_dir.join("index")).unwrap();
            let cm = git.commit_tree(repo, &t, &[&c], name).unwrap();
            git.update_ref(repo, "refs/heads/main", &cm).unwrap();
        }
        let n3 = snapshot(&navi_git, &navi, &ns, Some(&n2));
        let f3 = snapshot(&flat_git, &flat, &fsx, Some(&f2));
        carry(&navi, &flat, &flat_git, "navi");
        let r = apply_from(&flat_git, &flat, &fsx, &f3, "navi", &n3.commit);
        assert!(matches!(r.outcome, Outcome::Merged { .. }), "{r:?}");
        assert_eq!(r.diverged.len(), 1, "{r:?}");
        assert_eq!(r.diverged[0].branch, "refs/heads/main");
        assert_eq!(
            flat_git.rev(&flat, "refs/heads/main").unwrap().as_deref(),
            Some(f3.meta.refs["refs/heads/main"].as_str())
        );
        assert_eq!(
            flat_git
                .rev(&flat, "refs/sync/diverged/navi/main")
                .unwrap()
                .as_deref(),
            Some(n3.meta.refs["refs/heads/main"].as_str())
        );
        assert!(r.conflicts.is_empty(), "{r:?}");
        assert!(flat.work_tree.join("n.txt").is_file() && flat.work_tree.join("f.txt").is_file());

        // a file changed under the apply makes it retry, the index intact
        let f4 = fresh(&flat_git, &flat, &fsx);
        fs::write(navi.work_tree.join("z.txt"), "z").unwrap();
        let n4 = snapshot(&navi_git, &navi, &ns, Some(&n3));
        carry(&navi, &flat, &flat_git, "navi");
        fs::write(flat.work_tree.join("z.txt"), "mine").unwrap();
        let r = apply_from(&flat_git, &flat, &fsx, &f4, "navi", &n4.commit);
        assert!(matches!(r.outcome, Outcome::Retry(_)), "{r:?}");
        assert_eq!(
            fs::read_to_string(flat.work_tree.join("z.txt")).unwrap(),
            "mine"
        );
        let staged = flat_git
            .write_tree(&flat, &flat.git_dir.join("index"))
            .unwrap();
        assert_eq!(staged, f4.index, "the staged state survived the refusal");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creates_a_unit_from_the_hub_and_keeps_busy_state_dirs() {
        let (dir, navi_git, navi) = fixture("create");
        let ns = dir.join("scratch-navi");
        fs::create_dir_all(navi.work_tree.join(".devenv/state/pg")).unwrap();
        fs::write(navi.work_tree.join(".devenv/state/pg/data"), "v1").unwrap();
        fs::write(navi.work_tree.join(".gitignore"), ".devenv\n").unwrap();
        let n0 = snapshot(&navi_git, &navi, &ns, None);
        assert!(
            navi_git
                .ls_tree(&navi, &n0.work)
                .unwrap()
                .contains_key(".devenv/state/pg/data")
        );

        // an empty repository on another host fast-forwards from nothing
        let krup = Repo {
            git_dir: dir.join("krup/.git"),
            work_tree: dir.join("krup"),
        };
        fs::create_dir_all(&krup.work_tree).unwrap();
        let mut krup_git = navi_git.clone();
        krup_git.host = "krup".into();
        krup_git
            .ok(Some(&krup), &["init", "-q"], &Opts::default())
            .unwrap();
        carry(&navi, &krup, &krup_git, "navi");
        let ks = dir.join("scratch-krup");
        let r = create(&krup_git, &krup, &ks, &rules(), "krup", "navi", &n0.commit).unwrap();
        assert!(matches!(r.outcome, Outcome::FastForward { .. }), "{r:?}");
        assert_eq!(
            fs::read_to_string(krup.work_tree.join("a.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            fs::read_to_string(krup.work_tree.join(".devenv/state/pg/data")).unwrap(),
            "v1"
        );
        assert_eq!(
            krup_git.head_branch(&krup).unwrap().as_deref(),
            Some("refs/heads/main")
        );
        assert_eq!(krup_git.rev(&krup, "HEAD").unwrap(), n0.meta.head);
        assert_eq!(status(&krup_git, &krup), "?? .gitignore", "as on navi");

        // the state dir is left alone while a process holds it
        let k0 = fresh(&krup_git, &krup, &ks);
        fs::write(navi.work_tree.join(".devenv/state/pg/data"), "v2").unwrap();
        fs::write(navi.work_tree.join("a.txt"), "edited\n").unwrap();
        let n1 = snapshot(&navi_git, &navi, &ns, Some(&n0));
        carry(&navi, &krup, &krup_git, "navi");
        let busy = |p: &Path| {
            p.ends_with(".devenv/state/pg")
                .then(|| "postgres".to_owned())
        };
        let r = apply(&Apply {
            git: &krup_git,
            repo: &krup,
            scratch: &ks,
            excludes: None,
            rules: &rules(),
            host: "krup",
            local: Some(&k0),
            other: "navi",
            remote: &n1.commit,
            busy: &busy,
        })
        .unwrap();
        assert!(matches!(r.outcome, Outcome::FastForward { .. }), "{r:?}");
        assert_eq!(
            r.waiting,
            [(".devenv/state/pg".to_owned(), "postgres".to_owned())]
        );
        assert_eq!(
            fs::read_to_string(krup.work_tree.join("a.txt")).unwrap(),
            "edited\n"
        );
        assert_eq!(
            fs::read_to_string(krup.work_tree.join(".devenv/state/pg/data")).unwrap(),
            "v1"
        );
        // once the process is gone the next snapshot carries v1 again
        let k1 = fresh(&krup_git, &krup, &ks);
        assert_ne!(
            k1.commit, n1.commit,
            "the kept directory made a new snapshot"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
