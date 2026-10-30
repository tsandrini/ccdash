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
pub mod snapshot;
pub mod units;
mod watch;

use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Configured workspace directories, named by their last path component.
#[derive(Debug, Clone, Default)]
pub struct Workspaces {
    dirs: Vec<(String, String)>,
}

impl Workspaces {
    /// `~/` expands to `home`; trailing slashes are dropped.
    pub fn new(dirs: &[String], home: &str) -> Self {
        let dirs = dirs
            .iter()
            .map(|dir| {
                let dir = match dir.strip_prefix("~/") {
                    Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
                    None => dir.clone(),
                };
                let dir = dir.trim_end_matches('/').to_owned();
                let name = Path::new(&dir)
                    .file_name()
                    .map_or_else(|| dir.clone(), |n| n.to_string_lossy().into_owned());
                (name, dir)
            })
            .collect();
        Self { dirs }
    }

    pub fn names(&self) -> Vec<String> {
        self.dirs.iter().map(|(name, _)| name.clone()).collect()
    }

    /// Name and directory of every workspace, for collectors.
    pub fn entries(&self) -> Vec<crate::protocol::WorkspaceDir> {
        self.dirs
            .iter()
            .map(|(name, dir)| crate::protocol::WorkspaceDir {
                name: name.clone(),
                dir: dir.clone(),
            })
            .collect()
    }

    /// Rebuilt from what a hub sent.
    pub fn from_entries(entries: &[crate::protocol::WorkspaceDir]) -> Self {
        Self {
            dirs: entries
                .iter()
                .map(|e| (e.name.clone(), e.dir.clone()))
                .collect(),
        }
    }

    pub fn dir(&self, name: &str) -> Option<&str> {
        self.dirs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, dir)| dir.as_str())
    }

    /// The workspace holding `cwd` (the longest matching directory).
    pub fn of(&self, cwd: &str) -> Option<String> {
        self.locate(cwd).map(|(name, _)| name)
    }

    /// The workspace holding `cwd` and the project inside it: the first
    /// path component below the workspace directory (a repo clone), None
    /// at the workspace root itself.
    pub fn locate(&self, cwd: &str) -> Option<(String, Option<String>)> {
        let cwd = cwd.trim_end_matches('/');
        let (name, dir) = self
            .dirs
            .iter()
            .filter(|(_, dir)| within(cwd, dir))
            .max_by_key(|(_, dir)| dir.len())?;
        let project = cwd
            .strip_prefix(dir.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|rest| rest.split('/').next())
            .filter(|p| !p.is_empty())
            .map(str::to_owned);
        Some((name.clone(), project))
    }
}

fn within(cwd: &str, dir: &str) -> bool {
    cwd == dir
        || cwd
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
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
    /// Whether a session on `host` running in `cwd` is in scope.
    pub fn covers(&self, host: &str, cwd: &str) -> bool {
        self.host.as_deref().is_none_or(|h| h == host)
            && self
                .dirs
                .as_ref()
                .is_none_or(|dirs| dirs.iter().any(|dir| within(cwd, dir)))
    }

    /// The directories as a JSON array for [`IN_SCOPE`]'s `:dirs`.
    pub fn dirs_param(&self) -> Option<String> {
        self.dirs
            .as_ref()
            .map(|dirs| serde_json::to_string(dirs).unwrap_or_default())
    }
}

/// SQL condition on a `sessions` row aliased `x`, bound to `:dirs`
/// ([`Scope::dirs_param`]) and `:host` ([`Scope::host`]).
pub const IN_SCOPE: &str = "((:host IS NULL OR x.host = :host)
    AND (:dirs IS NULL OR EXISTS (SELECT 1 FROM json_each(:dirs) d
    WHERE x.cwd = d.value OR substr(x.cwd, 1, length(d.value) + 1) = d.value || '/')))";

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// Has its own `.git` (a repository at unit depth, nested deeper, or a
    /// bundle root).
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
    /// `waiting` (over the threshold), `excluded` (the globs) or
    /// `ignored` (the owner said so).
    pub state: String,
    pub at: i64,
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
                    &format!("SELECT {IN_SCOPE} FROM (SELECT :cwd AS cwd, 'h' AS host) x"),
                    named_params! {":cwd": cwd, ":dirs": scope.dirs_param(), ":host": scope.host},
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(sql, scope.covers("h", cwd), "{cwd} in {dirs:?}");
            sql
        };
        assert!(hit("/a/b_c/d", Some(&["/a/b_c"])));
        assert!(!hit("/a/bxc/d", Some(&["/a/b_c"])));
        assert!(!hit("/a/b_cd", Some(&["/a/b_c"])));
        assert!(hit("/z/y", Some(&["/a/b_c", "/z"])));
        assert!(!hit("/q", Some(&["/a/b_c", "/z"])));
        assert!(!hit("/q", Some(&[])));
        assert!(hit("/anything", None));

        let one = Scope {
            since: 0,
            until: 0,
            now: 0,
            dirs: None,
            host: Some("h".into()),
        };
        let on = |host: &str| -> bool {
            conn.query_row(
                &format!("SELECT {IN_SCOPE} FROM (SELECT '/w' AS cwd, :h AS host) x"),
                named_params! {":h": host, ":dirs": one.dirs_param(), ":host": one.host},
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(on("h") && one.covers("h", "/w"));
        assert!(!on("other") && !one.covers("other", "/w"));
    }
}
