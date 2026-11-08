//! Workspaces: the configured top-level directories, the scope of history
//! and analytics queries, and (H9, `meta/plans/ccdash-hub/h9.md`) the
//! workspace sync that keeps every unit of the workspace root in step
//! across machines through bare repositories on the hub. This file holds
//! what hub, collector and panel share: the configuration, the unit
//! identities and the wire types.

pub mod apply;
pub mod collector;
pub mod git;
pub mod hub;
pub mod remotes;
pub mod snapshot;
pub mod units;
mod watch;
#[cfg(windows)]
mod watch_windows;

use std::{borrow::Cow, collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The length of a Windows path's root: `C:\` or `C:/`, or a share's
/// `\\server\share\` (either separator); none for any other path.
pub fn windows_root(path: &str) -> Option<usize> {
    let b = path.as_bytes();
    let sep = |c: &u8| matches!(c, b'\\' | b'/');
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && sep(&b[2]) {
        return Some(3);
    }
    if !(b.len() >= 2 && sep(&b[0]) && sep(&b[1])) {
        return None;
    }
    // NOTE: past the server and the share
    let mut end = 2;
    for _ in 0..2 {
        match b[end..].iter().position(sep) {
            Some(i) => end += i + 1,
            None => return Some(b.len()),
        }
    }
    Some(end)
}

/// Whether `path` is a Windows path: `C:\…`, `C:/…`, `\\server\…` or
/// `//server/…`.
pub fn windows_path(path: &str) -> bool {
    windows_root(path).is_some()
}

/// `path` with `/` separators when it is a Windows path, so a hub on Linux
/// and a collector on Windows match the same way; any other path unchanged.
pub fn slashed(path: &str) -> Cow<'_, str> {
    if path.contains('\\') && windows_path(path) {
        Cow::Owned(path.replace('\\', "/"))
    } else {
        Cow::Borrowed(path)
    }
}

/// `path` as directories and cwds compare: a Windows path with `/`
/// separators and in lower case (any letter: Windows ignores case for
/// `Účty` too); any other path unchanged. The store keeps each session's
/// as `cwd_key`, which [`IN_SCOPE`] compares.
pub fn path_key(path: &str) -> Cow<'_, str> {
    if windows_path(path) {
        Cow::Owned(path.chars().flat_map(key_char).collect())
    } else {
        Cow::Borrowed(path)
    }
}

/// A character of a Windows path's key: one by one, as `str::to_lowercase`
/// would read a final sigma by what follows it.
fn key_char(c: char) -> std::char::ToLowercase {
    if c == '\\' { '/' } else { c }.to_lowercase()
}

/// What follows the start of `path` whose [`path_key`] is `key_len` bytes
/// long; a letter's lower case may be longer or shorter than it.
pub fn after_key(path: &str, key_len: usize) -> Option<&str> {
    if !windows_path(path) {
        return path.get(key_len..);
    }
    let mut len = 0;
    for (at, c) in path.char_indices() {
        if len >= key_len {
            return (len == key_len).then(|| &path[at..]);
        }
        len += key_char(c).map(char::len_utf8).sum::<usize>();
    }
    (len == key_len).then_some("")
}

/// Configured workspace directories, named by their last path component.
#[derive(Debug, Clone, Default)]
pub struct Workspaces {
    dirs: Vec<Dir>,
}

/// One configured directory, with its [`path_key`] worked out once.
#[derive(Debug, Clone)]
struct Dir {
    name: String,
    dir: String,
    key: String,
}

impl Dir {
    fn new(name: String, dir: String) -> Self {
        let key = path_key(&dir).into_owned();
        Self { name, dir, key }
    }
}

impl Workspaces {
    /// `~/` expands to `home`; trailing slashes are dropped (a root keeps
    /// its own); Windows paths take `/` separators. A share is written as
    /// Windows writes it (`\\nas\x`): a POSIX `//x` is `/x`.
    pub fn new(dirs: &[String], home: &str) -> Self {
        let dirs = dirs
            .iter()
            .map(|dir| {
                let dir = match dir.strip_prefix("~/") {
                    Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
                    None if dir.starts_with("//") => format!("/{}", dir.trim_start_matches('/')),
                    None => dir.clone(),
                };
                let dir = trimmed(&slashed(&dir)).to_owned();
                let name = Path::new(&dir)
                    .file_name()
                    .map_or_else(|| dir.clone(), |n| n.to_string_lossy().into_owned());
                Dir::new(name, dir)
            })
            .collect();
        Self { dirs }
    }

    /// Why `path` belongs to no workspace, for an error: with none
    /// configured, also how to set one.
    pub fn outside(&self, path: &str) -> String {
        if self.dirs.is_empty() {
            format!(
                "{path} is outside every ccdash workspace: the hub has none yet \
                (`[workspaces] dirs`; NixOS: `services.ccdash-hub.workspaces`)"
            )
        } else {
            format!("{path} is outside every ccdash workspace")
        }
    }

    /// Each name once: machines may list the same workspace under paths of
    /// their own (`/home/me/w`, `C:\Users\me\w`).
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for d in &self.dirs {
            if !names.contains(&d.name) {
                names.push(d.name.clone());
            }
        }
        names
    }

    /// Name and directory of every workspace, for collectors.
    pub fn entries(&self) -> Vec<crate::protocol::WorkspaceDir> {
        self.dirs
            .iter()
            .map(|d| crate::protocol::WorkspaceDir {
                name: d.name.clone(),
                dir: d.dir.clone(),
            })
            .collect()
    }

    /// Rebuilt from what a hub sent.
    pub fn from_entries(entries: &[crate::protocol::WorkspaceDir]) -> Self {
        Self {
            dirs: entries
                .iter()
                .map(|e| Dir::new(e.name.clone(), e.dir.clone()))
                .collect(),
        }
    }

    /// The first directory of `name`.
    pub fn dir(&self, name: &str) -> Option<&str> {
        self.dirs
            .iter()
            .find(|d| d.name == name)
            .map(|d| d.dir.as_str())
    }

    /// Every directory of `name`.
    pub fn dirs(&self, name: &str) -> Vec<String> {
        self.dirs
            .iter()
            .filter(|d| d.name == name)
            .map(|d| d.dir.clone())
            .collect()
    }

    /// The workspace holding `cwd` (the longest matching directory).
    pub fn of(&self, cwd: &str) -> Option<String> {
        self.locate(cwd).map(|(name, _)| name)
    }

    /// `cwd` moved to each other directory of its workspace (a machine
    /// lists its own): the same place below it.
    pub fn elsewhere(&self, cwd: &str) -> Vec<String> {
        let Some(found) = self.matching(cwd) else {
            return Vec::new();
        };
        let cwd = slashed(cwd);
        let rest = after_key(trimmed(&cwd), found.key.len()).unwrap_or("");
        let rest = rest.trim_start_matches('/');
        self.dirs
            .iter()
            .filter(|d| d.name == found.name && d.dir != found.dir)
            .map(|d| match rest {
                "" => d.dir.clone(),
                rest => join(&d.dir, rest),
            })
            .collect()
    }

    /// The directory holding `cwd`, of all the workspaces'.
    pub fn dir_of(&self, cwd: &str) -> Option<&str> {
        self.matching(cwd).map(|d| d.dir.as_str())
    }

    fn matching(&self, cwd: &str) -> Option<&Dir> {
        let key = path_key(trimmed(&slashed(cwd))).into_owned();
        self.dirs
            .iter()
            .filter(|d| within(&key, &d.key))
            .max_by_key(|d| d.dir.len())
    }

    /// The workspace holding `cwd` and the project inside it: the first
    /// path component below the workspace directory (a repo clone), None
    /// at the workspace root itself.
    pub fn locate(&self, cwd: &str) -> Option<(String, Option<String>)> {
        let found = self.matching(cwd)?;
        let cwd = slashed(cwd);
        let cwd = trimmed(&cwd);
        // NOTE: cut from the cwd itself, so the project keeps its case
        let project = after_key(cwd, found.key.len())
            .map(|rest| rest.trim_start_matches('/'))
            .and_then(|rest| rest.split('/').next())
            .filter(|p| !p.is_empty())
            .map(str::to_owned);
        Some((found.name.clone(), project))
    }
}

/// `name` under the workspace directory `dir`, whose root keeps its own
/// slash (`C:/`, `/`).
pub fn join(dir: &str, name: &str) -> String {
    format!("{}/{name}", dir.trim_end_matches('/'))
}

/// Whether `cwd` is `dir` or below it; a root (`/`, `c:/`) ends in `/`.
fn within(cwd: &str, dir: &str) -> bool {
    cwd == dir
        || cwd
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/') || dir.ends_with('/'))
}

/// `path` without trailing slashes; a root keeps its own (`/`, `C:/`).
fn trimmed(path: &str) -> &str {
    let rest = path.trim_end_matches('/');
    if rest.is_empty() && !path.is_empty() {
        "/"
    } else if windows_root(path) == Some(3) && rest.len() == 2 {
        &path[..3]
    } else {
        rest
    }
}

/// What a history or analytics query covers.
#[derive(Debug, Clone)]
pub struct Scope {
    pub since: i64,
    pub until: i64,
    pub now: i64,
    /// Workspace directories; sessions outside all of them are left out.
    /// None covers everything, sessions outside every workspace included.
    pub dirs: Option<Vec<String>>,
    /// One machine's sessions only; None covers every host.
    pub host: Option<String>,
}

impl Scope {
    /// What the scope covers, for one session after another: the
    /// directories' keys worked out once.
    pub fn matcher(&self) -> Matcher<'_> {
        Matcher {
            host: self.host.as_deref(),
            keys: self
                .dirs
                .as_ref()
                .map(|dirs| dirs.iter().map(|dir| path_key(dir).into_owned()).collect()),
        }
    }

    /// The directories for [`IN_SCOPE`]'s `:dirs`.
    pub fn dirs_param(&self) -> Option<String> {
        self.dirs.as_deref().map(dirs_param)
    }
}

/// What a [`Scope`] covers, its directories' keys at hand.
pub struct Matcher<'a> {
    host: Option<&'a str>,
    keys: Option<Vec<String>>,
}

impl Matcher<'_> {
    /// Whether a session on `host` running in `cwd` is in scope.
    pub fn covers(&self, host: &str, cwd: &str) -> bool {
        let cwd = path_key(cwd);
        self.host.is_none_or(|h| h == host)
            && self
                .keys
                .as_ref()
                .is_none_or(|keys| keys.iter().any(|key| within(&cwd, key)))
    }
}

/// `dirs`' [`path_key`]s as a JSON array, for [`IN_SCOPE`]'s `:dirs`.
pub fn dirs_param(dirs: &[String]) -> String {
    let keys: Vec<Cow<'_, str>> = dirs.iter().map(|dir| path_key(dir)).collect();
    serde_json::to_string(&keys).unwrap_or_default()
}

/// SQL condition on a `sessions` row aliased `x`, bound to `:dirs`
/// ([`Scope::dirs_param`]) and `:host` ([`Scope::host`]): its `cwd_key`
/// ([`path_key`], stored once per session) against each directory's.
pub const IN_SCOPE: &str = "((:host IS NULL OR x.host = :host)
    AND (:dirs IS NULL OR EXISTS (SELECT 1 FROM (SELECT COALESCE(x.cwd_key, x.cwd) AS k) c,
        json_each(:dirs) d
    WHERE c.k = d.value OR substr(c.k, 1, length(d.value) + 1) = d.value || '/'
        OR (substr(d.value, -1) = '/' AND substr(c.k, 1, length(d.value)) = d.value))))";

/// Local ref of this host's snapshots in every unit repository, pushed to
/// the hub under the same name; other hosts' snapshots are fetched under
/// theirs. Only the host writes its own ref.
pub const SYNC_REFS: &str = "refs/sync";
/// Branch tips, tags and stashes of a host, mirrored so their objects
/// travel with the snapshot (`<prefix>/<host>/heads/*`, `tags/*`,
/// `stash/<n>`).
pub const TIPS_REFS: &str = "refs/sync-tips";
/// A branch moved on both sides since the base: the remote tip is kept
/// here (`<prefix>/<host>/<branch>`) for the owner to merge by hand.
pub const DIVERGED_REFS: &str = "refs/sync/diverged";
/// A snapshot the held guard stopped, kept until the panel applies or
/// drops it (`<prefix>/<host>`).
pub const HELD_REFS: &str = "refs/sync/held";
/// The empty tree, the merge base of units without a common ancestor.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// Directories that never travel, wherever they appear, unless
/// `sync_ignored` names them for a unit.
pub const DEFAULT_ALWAYS_EXCLUDE: &[&str] = &[
    "node_modules",
    ".venv",
    "venv",
    ".direnv",
    ".devenv",
    "target",
    "result",
    "result-*",
    "__pycache__",
    ".cache",
];
/// Gitignored files that travel anyway (handoff's `keep_ignored`).
pub const DEFAULT_SYNC_IGNORED: &[&str] = &[".env", ".env.*", ".envrc.local"];
/// Untracked files above this are skipped and listed.
pub const DEFAULT_MAX_FILE_MB: u64 = 64;
/// A new directory under this (working tree plus `.git`, the always
/// excluded directories left out) is included by itself.
pub const DEFAULT_AUTO_INCLUDE_MB: u64 = 500;
/// A snapshot adding or deleting more files than this in one settle is held.
pub const HELD_FILES: u64 = 2000;
/// …or more than this share of the tracked set.
pub const HELD_SHARE: f64 = 0.2;
/// Directories whose `sync_ignored` entries are quiescent by default.
pub const QUIESCENT_PREFIX: &str = ".devenv/state";

/// A git repository or a plain directory under the workspace root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[ts(export_to = "types.ts")]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// Has its own `.git` (a repository at unit depth, nested deeper, or a
    /// bundle root).
    #[default]
    Git,
    /// A directory without `.git`: a hidden repository under the state dir
    /// snapshots it.
    Plain,
}

/// How a text conflict lands in the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[ts(export_to = "types.ts")]
#[serde(rename_all = "snake_case")]
pub enum ConflictMode {
    /// Standard markers in the file, both versions on the hub.
    #[default]
    Markers,
    /// The local version stays; the other is held on the hub.
    SetAside,
}

/// A gitignored file or directory that travels anyway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Ignored {
    /// Glob relative to the unit (`*` spans directories).
    pub pattern: String,
    /// Snapshotted and applied only while no local process holds a file
    /// under it open (a dev database, a running server).
    #[serde(default)]
    pub quiescent: bool,
}

/// What one bundle (a directory directly under the root) overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(default)]
pub struct BundleOverride {
    pub auto_include_mb: Option<u64>,
    pub sync_ignored: Vec<Ignored>,
    pub always_exclude: Vec<String>,
}

/// What one unit overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(default)]
pub struct UnitOverride {
    /// Forces the unit in or out, over the globs.
    pub include: Option<bool>,
    /// Nothing moves in either direction.
    pub paused: bool,
    pub always_exclude: Vec<String>,
    pub sync_ignored: Vec<Ignored>,
    pub max_file_mb: Option<u64>,
    pub conflicts: Option<ConflictMode>,
}

/// The unit scope and the rules, owned by the hub and edited in the
/// panel; collectors cache the last one they got for an offline start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(default)]
pub struct WorkspaceConfig {
    /// Units are expected this many levels below the root (`2` means
    /// `<root>/<bundle>/<repo>`); a repository at a shallower level (a
    /// bundle root) is a unit too, a nested one deeper is its own unit.
    pub depth: u32,
    /// Globs on unit ids (`<bundle>/<repo>`); empty includes every unit.
    pub include: Vec<String>,
    /// Globs on unit ids that never travel; wins over `include`.
    pub exclude: Vec<String>,
    /// Directory names (or `/`-containing relative paths) that never
    /// travel, wherever they appear.
    pub always_exclude: Vec<String>,
    /// Gitignored files and directories that travel anyway; wins over
    /// `always_exclude` for the unit.
    pub sync_ignored: Vec<Ignored>,
    /// Untracked files above this are skipped and listed.
    pub max_file_mb: u64,
    /// New directories under this are included by themselves.
    pub auto_include_mb: u64,
    pub conflicts: ConflictMode,
    pub bundles: BTreeMap<String, BundleOverride>,
    pub units: BTreeMap<String, UnitOverride>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            depth: 2,
            include: Vec::new(),
            exclude: Vec::new(),
            always_exclude: DEFAULT_ALWAYS_EXCLUDE
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            sync_ignored: DEFAULT_SYNC_IGNORED
                .iter()
                .map(|s| Ignored {
                    pattern: (*s).to_owned(),
                    quiescent: false,
                })
                .collect(),
            max_file_mb: DEFAULT_MAX_FILE_MB,
            auto_include_mb: DEFAULT_AUTO_INCLUDE_MB,
            conflicts: ConflictMode::Markers,
            bundles: BTreeMap::new(),
            units: BTreeMap::new(),
        }
    }
}

/// The rules in force for one unit: the defaults, the bundle's and the
/// unit's overrides folded in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    pub always_exclude: Vec<String>,
    pub sync_ignored: Vec<Ignored>,
    pub max_file_mb: u64,
    pub conflicts: ConflictMode,
    pub paused: bool,
}

impl WorkspaceConfig {
    /// The bundle a unit id belongs to: its first path component.
    pub fn bundle_of(unit: &str) -> &str {
        unit.split('/').next().unwrap_or(unit)
    }

    /// Whether the globs (and the unit's override) select `unit`.
    pub fn selects(&self, unit: &str) -> bool {
        if let Some(force) = self.units.get(unit).and_then(|u| u.include) {
            return force;
        }
        if self.exclude.iter().any(|p| crate::sync::glob(p, unit)) {
            return false;
        }
        // NOTE: a bundle root (`tsandrini`) counts as in when its units are (`tsandrini/*`)
        let as_bundle = (!unit.contains('/')).then(|| format!("{unit}/"));
        self.include.is_empty()
            || self.include.iter().any(|p| {
                crate::sync::glob(p, unit)
                    || as_bundle
                        .as_deref()
                        .is_some_and(|b| crate::sync::glob(p, b))
            })
    }

    /// Why the globs select `unit` or not: the rule that decided.
    pub fn explain(&self, unit: &str) -> Decision {
        if let Some(force) = self.units.get(unit).and_then(|u| u.include) {
            return if force {
                Decision::ForcedIn
            } else {
                Decision::ForcedOut
            };
        }
        if let Some(rule) = self.exclude.iter().find(|p| crate::sync::glob(p, unit)) {
            return Decision::Excluded { rule: rule.clone() };
        }
        if self.include.is_empty() {
            return Decision::Selected { rule: None };
        }
        let as_bundle = (!unit.contains('/')).then(|| format!("{unit}/"));
        match self.include.iter().find(|p| {
            crate::sync::glob(p, unit)
                || as_bundle
                    .as_deref()
                    .is_some_and(|b| crate::sync::glob(p, b))
        }) {
            Some(rule) => Decision::Selected {
                rule: Some(rule.clone()),
            },
            None => Decision::Unmatched,
        }
    }

    /// The size under which a new directory of `bundle` is included by
    /// itself, in bytes.
    pub fn auto_include_bytes(&self, bundle: &str) -> u64 {
        self.bundles
            .get(bundle)
            .and_then(|b| b.auto_include_mb)
            .unwrap_or(self.auto_include_mb)
            << 20
    }

    /// The rules in force for `unit`.
    pub fn rules(&self, unit: &str) -> Rules {
        let bundle = self.bundles.get(Self::bundle_of(unit));
        let over = self.units.get(unit);
        let mut always_exclude = self.always_exclude.clone();
        let mut sync_ignored = self.sync_ignored.clone();
        if let Some(b) = bundle {
            always_exclude.extend(b.always_exclude.iter().cloned());
            sync_ignored.extend(b.sync_ignored.iter().cloned());
        }
        if let Some(u) = over {
            always_exclude.extend(u.always_exclude.iter().cloned());
            sync_ignored.extend(u.sync_ignored.iter().cloned());
        }
        for entry in &mut sync_ignored {
            if entry.pattern.starts_with(QUIESCENT_PREFIX) {
                entry.quiescent = true;
            }
        }
        Rules {
            always_exclude,
            sync_ignored,
            max_file_mb: over.and_then(|u| u.max_file_mb).unwrap_or(self.max_file_mb),
            conflicts: over.and_then(|u| u.conflicts).unwrap_or(self.conflicts),
            paused: over.is_some_and(|u| u.paused),
        }
    }
}

/// Whether `unit` is a well-formed unit id: plain path segments, no `.git`
/// segment, short enough to be a directory on the hub.
pub fn valid_unit(unit: &str) -> bool {
    !unit.is_empty()
        && unit.len() <= 512
        && crate::sync::valid_path(unit)
        && !unit.split('/').any(|seg| seg.eq_ignore_ascii_case(".git"))
}

/// Whether `host` can name a ref and a directory.
pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 64
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !host.starts_with('.')
        && host != "diverged"
        && host != "held"
}

/// What the hub sends collectors: the configuration and the units it
/// has decided are in (`units`), versioned so a collector knows what it
/// has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkspaceSetup {
    pub version: i64,
    pub config: WorkspaceConfig,
    /// Unit id → kind, for every unit that travels.
    pub units: BTreeMap<String, UnitKind>,
}

/// A directory at unit depth (or a nested repository) a collector found,
/// for the hub's list of what exists where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct SeenUnit {
    pub unit: String,
    pub kind: UnitKind,
    /// Working tree plus `.git`, the always excluded directories left out.
    pub size: u64,
    /// Whether the globs select it.
    pub selected: bool,
    /// Bytes under each never-travelling directory name found inside
    /// (`node_modules` → 347 MB): what stays home, by rule.
    #[serde(default)]
    pub excluded: BTreeMap<String, u64>,
    /// Why the directory can never be a unit (a linked worktree): the hub
    /// leaves it out and says so.
    #[serde(default)]
    pub unsyncable: Option<String>,
}

/// Where a unit stands on one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(rename_all = "snake_case")]
pub enum UnitState {
    /// The hub's snapshots and this copy agree.
    InStep,
    /// Changes here not yet on the hub.
    Ahead,
    /// Snapshots from other hosts not yet applied.
    Behind,
    /// A merge or fast-forward is under way.
    Merging,
    /// Open conflicts in the working tree.
    Conflict,
    /// A snapshot waits for the owner (the held guard).
    Held,
    Paused,
    /// Left alone for now (`index.lock`, a rebase in progress, a live
    /// tool call); `detail` says why.
    Skipped,
    /// A quiescent directory is open on this host.
    Waiting,
    /// The last pass failed; `detail` says how.
    Failed,
    /// The first snapshot of a unit the hub does not have yet.
    Seeding,
    /// Exists here and on the hub without a sync record: `ccdash
    /// workspace adopt` merges them.
    Unadopted,
    /// The hub has it, this host does not yet: being created.
    Creating,
    /// The hub has it; the directory disappeared here.
    Missing,
}

/// A conflicted path, as the collector that holds it reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct ConflictReport {
    pub path: String,
    /// `text` (markers in the file) or `binary` (the local version kept).
    pub kind: String,
    /// This host's blob, as the unit's repository has it.
    pub ours: String,
    /// The other host's blob (in the hub's repository).
    pub theirs: String,
    pub other_host: String,
    pub at: i64,
}

/// A held snapshot, with the numbers that held it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct HeldReport {
    pub commit: String,
    pub added: u64,
    pub deleted: u64,
    pub tracked: u64,
    pub at: i64,
}

/// A branch that moved on both sides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Diverged {
    pub branch: String,
    pub local: String,
    pub remote: String,
    pub host: String,
}

/// An ignored directory present in a unit, with what carrying it would cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct IgnoredDir {
    pub path: String,
    pub files: u64,
    pub bytes: u64,
    /// Whether `sync_ignored` carries it.
    pub carried: bool,
    pub quiescent: bool,
}

/// One unit on one host, as the collector reports it after each pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct UnitStatus {
    pub unit: String,
    pub kind: UnitKind,
    pub state: UnitState,
    #[serde(default)]
    pub detail: Option<String>,
    /// This host's snapshot (`refs/sync/<host>`).
    #[serde(default)]
    pub snapshot: Option<String>,
    /// Files tracked by the last snapshot.
    pub files: u64,
    #[serde(default)]
    pub last_pass: Option<i64>,
    /// Since when passes have failed, when they do.
    #[serde(default)]
    pub failing_since: Option<i64>,
    #[serde(default)]
    pub conflicts: Vec<ConflictReport>,
    #[serde(default)]
    pub held: Option<HeldReport>,
    #[serde(default)]
    pub diverged: Vec<Diverged>,
    #[serde(default)]
    pub ignored: Vec<IgnoredDir>,
}

/// What the sync has cost this host since its collector started:
/// cumulative counters (the hub keeps the deltas per hour) and two gauges.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(default)]
pub struct Cost {
    /// When the counters started (the collector's start), so the hub
    /// tells a restart from a decrease.
    pub since: i64,
    /// CPU time of the sync's own threads (scheduler, workers, watcher).
    pub cpu_ms: u64,
    /// CPU time of the git processes it ran (user + system).
    pub git_cpu_ms: u64,
    pub git_calls: u64,
    pub git_wall_ms: u64,
    /// Bytes git read from and wrote to the disk (block I/O, not cache hits).
    pub io_read: u64,
    pub io_write: u64,
    /// The largest resident set a git process reached, in kB.
    pub max_rss_kb: u64,
    pub passes: u64,
    /// Snapshot commits taken.
    pub snapshots: u64,
    pub pushes: u64,
    pub fetches: u64,
    pub push_ms: u64,
    pub fetch_ms: u64,
    /// Passes that ended in an error.
    pub failures: u64,
    /// The collector process's resident set now, in kB (the whole
    /// process, not only the sync).
    pub rss_kb: u64,
    /// inotify watches held for the workspace.
    pub watches: u64,
}

/// How a host's collector stands as a whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct WorkspaceStatus {
    pub root: String,
    /// The setup version the collector runs on.
    pub setup_version: i64,
    pub watching: bool,
    pub units: u64,
    pub in_step: u64,
    pub pending: u64,
    #[serde(default)]
    pub error: Option<String>,
    /// While every unit is seeded, created or adopted: done and total.
    #[serde(default)]
    pub progress: Option<(u64, u64)>,
    pub last_pass: Option<i64>,
    #[serde(default)]
    pub cost: Option<Cost>,
}

/// Which side a conflict is resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// This host's version (`ours`).
    Mine,
    /// The other host's version (`theirs`).
    Theirs,
}

/// What to do with a held snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(rename_all = "snake_case")]
pub enum HeldAction {
    /// Push it as it is.
    Apply,
    /// Restore the previous snapshot's working tree.
    Drop,
}

/// One snapshot in a unit's history (`GET /api/v1/workspace/units/{unit}/history`).
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct HistoryEntry {
    pub commit: String,
    pub host: String,
    pub at: i64,
    /// A merge of another host's snapshot.
    pub merge: bool,
    /// Files changed against the previous snapshot of the host.
    pub files: u64,
    pub branch: Option<String>,
}

/// One host's column in the panel's grid.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct WorkspaceHost {
    pub host: String,
    pub connected: bool,
    /// Round trip of the hub's last WebSocket ping to the collector.
    pub rtt_ms: Option<u64>,
    pub status: Option<WorkspaceStatus>,
}

/// One unit as the hub knows it, with every host's cell.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct UnitView {
    pub unit: String,
    pub workspace: String,
    pub kind: UnitKind,
    /// `included`, `waiting`, `excluded` or `ignored`.
    pub state: String,
    pub size: u64,
    /// Bumped on every accepted push.
    pub version: i64,
    pub at: i64,
    /// Host → its last report of the unit.
    pub hosts: BTreeMap<String, UnitStatus>,
    /// Host → its snapshot on the hub.
    pub refs: BTreeMap<String, String>,
    pub paused: bool,
}

/// A conflict the hub recorded.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct WorkspaceConflict {
    pub id: i64,
    pub unit: String,
    pub host: String,
    pub path: String,
    pub kind: String,
    pub ours: String,
    pub theirs: String,
    pub other_host: String,
    pub at: i64,
    pub pending: Option<Side>,
    pub error: Option<String>,
}

/// A held snapshot the hub knows about.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct HeldView {
    pub unit: String,
    pub host: String,
    pub held: HeldReport,
}

/// A directory at unit depth the hub has not included.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct NewDir {
    pub unit: String,
    pub kind: UnitKind,
    pub size: u64,
    pub hosts: Vec<String>,
    /// `waiting` (over the threshold), `excluded` (the globs), `ignored`
    /// (the owner said so) or `unsyncable` (a linked worktree).
    pub state: String,
    pub at: i64,
    /// Why it can never travel, when `unsyncable`.
    pub reason: Option<String>,
}

/// `GET /api/v1/workspace`.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct WorkspaceView {
    pub enabled: bool,
    pub hosts: Vec<WorkspaceHost>,
    pub units: Vec<UnitView>,
    pub new_dirs: Vec<NewDir>,
    pub conflicts: Vec<WorkspaceConflict>,
    pub held: Vec<HeldView>,
    pub config: WorkspaceConfig,
    pub config_version: i64,
    /// Bytes the repositories take on the hub.
    pub bytes: u64,
    /// Bytes pushed per day (local dates), the last 30 days.
    pub growth: Vec<(String, i64)>,
}

/// What the globs decided for one unit, and by which rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    /// An include glob matched (`rule`), or no include glob is set.
    Selected { rule: Option<String> },
    /// The unit's override says in.
    ForcedIn,
    /// The unit's override says out.
    ForcedOut,
    /// An exclude glob matched.
    Excluded { rule: String },
    /// Include globs are set and none matches.
    Unmatched,
}

/// One directory at unit depth as the coverage view sees it: whether it
/// travels, what decided, what it weighs and what inside it stays home.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct CoverageUnit {
    pub unit: String,
    pub kind: UnitKind,
    pub hosts: Vec<String>,
    /// What would travel (working tree plus `.git`), the largest copy.
    pub size: u64,
    /// Never-travelling directory name → bytes inside, the largest copy.
    pub excluded: BTreeMap<String, u64>,
    /// `travels`, `waiting` (over the threshold), `ignored` (the owner),
    /// `unsyncable` (a linked worktree) or `left_out` (the globs).
    pub status: String,
    pub decision: Decision,
    /// Why it can never travel, when `unsyncable`.
    pub reason: Option<String>,
}

/// One machine's share of the coverage.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct CoverageHost {
    pub host: String,
    /// Directories at unit depth seen there.
    pub units: u64,
    /// Everything they weigh, the never-travelling directories included.
    pub bytes: u64,
    /// What travels from there.
    pub travels: u64,
}

/// A never-travelling directory name and what it keeps home.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct LeftOutName {
    pub name: String,
    pub bytes: u64,
    pub units: u64,
}

/// A gitignored directory inside a travelling unit (what the collectors
/// report live), carried or not.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct HomeDir {
    pub unit: String,
    pub path: String,
    pub bytes: u64,
    pub files: u64,
    pub hosts: Vec<String>,
    pub carried: bool,
}

/// `GET /api/v1/workspace/coverage`: what travels and what stays home.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Coverage {
    pub hosts: Vec<CoverageHost>,
    pub units: Vec<CoverageUnit>,
    /// The never-travelling names, largest first.
    pub names: Vec<LeftOutName>,
    pub home: Vec<HomeDir>,
    /// Bytes the repositories take on the hub.
    pub hub_bytes: u64,
}

/// One glob of a proposed configuration and how many units it matches.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct GlobMatch {
    pub pattern: String,
    pub exclude: bool,
    pub matches: u64,
}

/// `POST /api/v1/workspace/config/preview`: what a configuration would
/// decide for every directory the hub knows.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Preview {
    pub units: Vec<CoverageUnit>,
    pub globs: Vec<GlobMatch>,
}

/// Traffic of one host in one bucket, one direction.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct TrafficRow {
    pub host: String,
    pub at: i64,
    /// `push` (receive-pack) or `fetch` (upload-pack, info/refs).
    pub kind: String,
    pub requests: u64,
    /// Bytes the host sent to the hub.
    pub bytes_in: u64,
    /// Bytes the hub sent to the host.
    pub bytes_out: u64,
    pub ms: u64,
    pub failures: u64,
}

/// The sync's cost on one host in one bucket (deltas of [`Cost`]).
#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct CostRow {
    pub host: String,
    pub at: i64,
    pub cpu_ms: u64,
    pub git_cpu_ms: u64,
    pub git_calls: u64,
    pub git_wall_ms: u64,
    pub io_read: u64,
    pub io_write: u64,
    pub passes: u64,
    pub snapshots: u64,
    pub pushes: u64,
    pub fetches: u64,
    pub push_ms: u64,
    pub fetch_ms: u64,
    pub failures: u64,
    /// The largest resident set reported in the bucket, in kB.
    pub rss_kb: u64,
    /// The last watch count reported in the bucket.
    pub watches: u64,
}

/// A large transfer, for the throughput view.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Transfer {
    pub host: String,
    pub unit: String,
    pub kind: String,
    pub bytes: u64,
    pub ms: u64,
    pub at: i64,
}

/// Pushes of one host in one bucket (from the event log).
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct PushRow {
    pub host: String,
    pub at: i64,
    pub pushes: u64,
    pub bytes: u64,
}

/// A unit's repository on the hub and its size.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct RepoSize {
    pub unit: String,
    pub bytes: u64,
}

/// `GET /api/v1/workspace/stats?since=&until=`: traffic, cost and growth
/// over the span, in hourly buckets up to three days, else daily (local
/// midnights of the hub).
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct WorkspaceStats {
    pub since: i64,
    pub until: i64,
    pub bucket_ms: i64,
    pub traffic: Vec<TrafficRow>,
    pub cost: Vec<CostRow>,
    pub transfers: Vec<Transfer>,
    pub pushes: Vec<PushRow>,
    /// Conflicts recorded per bucket.
    pub conflicts: Vec<(i64, u64)>,
    pub repos: Vec<RepoSize>,
    pub hub_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, named_params};

    #[test]
    fn config_selects_and_folds_rules() {
        let mut cfg = WorkspaceConfig {
            include: vec!["tsandrini/*".into(), "meteopress/*".into()],
            exclude: vec!["tsandrini/nixpkgs".into(), "*_bkp".into()],
            ..WorkspaceConfig::default()
        };
        assert!(cfg.selects("tsandrini/ccdash"));
        assert!(cfg.selects("tsandrini"));
        assert!(!cfg.selects("tsandrini/nixpkgs"));
        assert!(!cfg.selects("meteopress/x_bkp"));
        assert!(!cfg.selects("other/repo"));
        cfg.units.insert(
            "other/repo".into(),
            UnitOverride {
                include: Some(true),
                sync_ignored: vec![Ignored {
                    pattern: "node_modules".into(),
                    quiescent: false,
                }],
                ..UnitOverride::default()
            },
        );
        cfg.units.insert(
            "tsandrini/ccdash".into(),
            UnitOverride {
                include: Some(false),
                ..UnitOverride::default()
            },
        );
        cfg.bundles.insert(
            "other".into(),
            BundleOverride {
                auto_include_mb: Some(10),
                sync_ignored: vec![Ignored {
                    pattern: ".devenv/state/postgres".into(),
                    quiescent: false,
                }],
                always_exclude: vec!["data".into()],
            },
        );
        assert!(cfg.selects("other/repo"));
        assert!(!cfg.selects("tsandrini/ccdash"));
        assert_eq!(cfg.explain("other/repo"), Decision::ForcedIn);
        assert_eq!(cfg.explain("tsandrini/ccdash"), Decision::ForcedOut);
        assert_eq!(
            cfg.explain("tsandrini/nixpkgs"),
            Decision::Excluded {
                rule: "tsandrini/nixpkgs".into()
            }
        );
        assert_eq!(
            cfg.explain("meteopress/radar"),
            Decision::Selected {
                rule: Some("meteopress/*".into())
            }
        );
        assert_eq!(
            cfg.explain("tsandrini"),
            Decision::Selected {
                rule: Some("tsandrini/*".into())
            }
        );
        assert_eq!(cfg.explain("stranger/repo"), Decision::Unmatched);
        assert_eq!(
            WorkspaceConfig::default().explain("any/thing"),
            Decision::Selected { rule: None }
        );
        let rules = cfg.rules("other/repo");
        assert!(rules.always_exclude.contains(&"node_modules".to_owned()));
        assert!(rules.always_exclude.contains(&"data".to_owned()));
        let carried: Vec<(&str, bool)> = rules
            .sync_ignored
            .iter()
            .map(|i| (i.pattern.as_str(), i.quiescent))
            .collect();
        assert!(carried.contains(&(".env", false)));
        assert!(carried.contains(&("node_modules", false)));
        // NOTE: state directories are quiescent whatever the entry says
        assert!(carried.contains(&(".devenv/state/postgres", true)));
        assert_eq!(cfg.auto_include_bytes("other"), 10 << 20);
        assert_eq!(cfg.auto_include_bytes("tsandrini"), 500 << 20);
        assert_eq!(WorkspaceConfig::bundle_of("a/b/c"), "a");
        assert!(valid_unit("tsandrini/ccdash") && valid_unit("a") && valid_unit("a/b/c.d"));
        for bad in ["", "a/.git/b", "a/../b", "/a", "a/", ".git"] {
            assert!(!valid_unit(bad), "{bad}");
        }
        assert!(valid_host("navibundle") && valid_host("navi-dev.2"));
        for bad in ["", "diverged", "held", ".x", "a/b", "a b"] {
            assert!(!valid_host(bad), "{bad}");
        }
    }

    #[test]
    fn names_and_lookup() {
        let ws = Workspaces::new(
            &["~/PB/meteopress".into(), "/srv/PB/tsandrini/".into()],
            "/home/u",
        );
        assert_eq!(ws.names(), ["meteopress", "tsandrini"]);
        assert!(!ws.outside("/x").contains("has none"));
        assert!(
            Workspaces::new(&[], "/home/u")
                .outside("/x")
                .contains("has none yet")
        );
        assert_eq!(ws.dir("meteopress"), Some("/home/u/PB/meteopress"));
        assert_eq!(
            ws.of("/home/u/PB/meteopress/radar"),
            Some("meteopress".into())
        );
        assert_eq!(ws.of("/home/u/PB/meteopress-old"), None);
        assert_eq!(ws.of("/srv/PB/tsandrini"), Some("tsandrini".into()));
        assert_eq!(
            ws.locate("/home/u/PB/meteopress/radar/src"),
            Some(("meteopress".into(), Some("radar".into())))
        );
        assert_eq!(
            ws.locate("/srv/PB/tsandrini/"),
            Some(("tsandrini".into(), None))
        );
    }

    #[test]
    fn windows_directories_match_windows_cwds() {
        let ws = Workspaces::new(
            &[r"C:\Users\u\PB\meteopress\".into(), "~/PB/tsandrini".into()],
            r"C:\Users\u",
        );
        assert_eq!(ws.names(), ["meteopress", "tsandrini"]);
        assert_eq!(ws.dir("meteopress"), Some("C:/Users/u/PB/meteopress"));
        assert_eq!(ws.dir("tsandrini"), Some("C:/Users/u/PB/tsandrini"));
        assert_eq!(
            ws.locate(r"C:\Users\u\PB\meteopress\radar\src"),
            Some(("meteopress".into(), Some("radar".into())))
        );
        assert_eq!(ws.of(r"C:\Users\u\PB\meteopress-old"), None);
        // NOTE: one workspace, a directory per machine
        let both = Workspaces::new(&["/home/u/w".into(), r"C:\Users\u\w".into()], "/home/u");
        assert_eq!(both.names(), ["w"]);
        assert_eq!(both.dirs("w"), ["/home/u/w", "C:/Users/u/w"]);
        assert_eq!(both.dir_of(r"C:\Users\u\w\p"), Some("C:/Users/u/w"));
        assert_eq!(both.dir_of("/home/u/w/p"), Some("/home/u/w"));
        assert_eq!(both.dir_of("/elsewhere"), None);
        assert_eq!(both.elsewhere("/home/u/w/p"), ["C:/Users/u/w/p"]);
        assert_eq!(both.elsewhere(r"C:\Users\u\w"), ["/home/u/w"]);
        assert!(both.elsewhere("/elsewhere").is_empty());
        // NOTE: Windows ignores case; the project keeps the cwd's
        assert_eq!(
            ws.locate(r"c:\users\U\pb\Meteopress\Radar"),
            Some(("meteopress".into(), Some("Radar".into())))
        );
        assert_eq!(slashed(r"\\nas\PB"), "//nas/PB");
        // NOTE: any letter's case, also one whose lower case is shorter (the Kelvin sign)
        let czech = Workspaces::new(
            &[r"C:\Users\u\Účty".into(), "C:\\Users\\u\\\u{212a}".into()],
            "/",
        );
        assert_eq!(
            czech.locate(r"c:\users\U\účty\Projekt\src"),
            Some(("Účty".into(), Some("Projekt".into())))
        );
        assert_eq!(czech.locate(r"C:\Users\u\k\p").unwrap().1, Some("p".into()));
        assert_eq!(after_key(r"C:\Ú\x", "c:/ú".len()), Some(r"\x"));
        assert_eq!(after_key("/a/b", 2), Some("/b"));
        // NOTE: a drive's root keeps its slash; a POSIX `//` is `/`
        let roots = Workspaces::new(&[r"C:\".into(), "//srv/Data/".into()], "/home/u");
        assert_eq!(roots.dir_of(r"c:\Users\x"), Some("C:/"));
        assert_eq!(roots.locate(r"C:\Users\x").unwrap().1, Some("Users".into()));
        assert_eq!(roots.dir_of("/srv/Data/x"), Some("/srv/Data"));
        assert_eq!(roots.dir_of("/srv/data/x"), None, "case counts on Linux");
        // NOTE: a project under a root: no doubled slash, which would read as a share
        assert_eq!(join("C:/", "proj"), "C:/proj");
        assert_eq!(join("/", "proj"), "/proj");
        assert_eq!(join("/home/u/w", "proj"), "/home/u/w/proj");
        let root = join(roots.dir_of(r"C:\proj\src").unwrap(), "proj");
        assert!(crate::claims::within("C:/proj/src", &root), "{root}");
        assert_eq!(slashed(r"/home/u/a\b"), r"/home/u/a\b");
    }

    #[test]
    fn sql_scope_matches_rust() {
        let conn = Connection::open_in_memory().unwrap();
        let hit = |cwd: &str, dirs: Option<&[&str]>| -> bool {
            let scope = Scope {
                since: 0,
                until: 0,
                now: 0,
                dirs: dirs.map(|d| d.iter().map(|d| (*d).to_owned()).collect()),
                host: None,
            };
            let sql: bool = conn
                .query_row(
                    &format!(
                        "SELECT {IN_SCOPE} FROM (SELECT :cwd AS cwd, :key AS cwd_key, 'h' AS host) x"
                    ),
                    named_params! {
                        ":cwd": cwd,
                        ":key": path_key(cwd),
                        ":dirs": scope.dirs_param(),
                        ":host": scope.host
                    },
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(sql, scope.matcher().covers("h", cwd), "{cwd} in {dirs:?}");
            sql
        };
        assert!(hit("/a/b_c/d", Some(&["/a/b_c"])));
        assert!(!hit("/a/bxc/d", Some(&["/a/b_c"])));
        assert!(!hit("/a/b_cd", Some(&["/a/b_c"])));
        assert!(hit("/z/y", Some(&["/a/b_c", "/z"])));
        assert!(!hit("/q", Some(&["/a/b_c", "/z"])));
        assert!(!hit("/q", Some(&[])));
        assert!(hit("/anything", None));
        // NOTE: Windows cwds are stored as Windows has them; directories as configured
        assert!(hit(r"C:\Users\u\PB\x", Some(&["C:/Users/u/PB"])));
        assert!(hit(r"c:\users\U\pb", Some(&["C:/Users/u/PB"])));
        assert!(!hit(r"C:\Users\u\PBx", Some(&["C:/Users/u/PB"])));
        assert!(hit(r"\\NAS\PB\y", Some(&["//nas/PB"])));
        assert!(hit(r"C:\Users\u\účty\x", Some(&[r"C:\Users\u\Účty"])));
        assert!(!hit("/A/b_c/d", Some(&["/a/b_c"])));
        assert!(!hit(r"/a/b_c\d", Some(&["/a/b_c"])));
        assert!(hit(r"C:\Users\x", Some(&["C:/"])));
        assert!(hit(r"c:\", Some(&["C:/"])));
        assert!(!hit(r"D:\x", Some(&["C:/"])));
        assert!(hit("/a", Some(&["/"])));

        let one = Scope {
            since: 0,
            until: 0,
            now: 0,
            dirs: None,
            host: Some("h".into()),
        };
        let on = |host: &str| -> bool {
            conn.query_row(
                &format!(
                    "SELECT {IN_SCOPE} FROM (SELECT '/w' AS cwd, NULL AS cwd_key, :h AS host) x"
                ),
                named_params! {":h": host, ":dirs": one.dirs_param(), ":host": one.host},
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(on("h") && one.matcher().covers("h", "/w"));
        assert!(!on("other") && !one.matcher().covers("other", "/w"));
    }
}
