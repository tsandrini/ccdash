//! Configuration of the two daemons. `ccdash hub` reads a [`HubConfig`],
//! `ccdash collector` a [`CollectorConfig`]; both default to
//! `$XDG_CONFIG_HOME/ccdash/config.toml` and reject unknown keys.

use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, de::DeserializeOwned};

use crate::paths;

/// `ccdash hub`: the store, the web UI and API, the collector links and
/// notifications.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HubConfig {
    /// Name reported to clients; defaults to the kernel host name.
    pub host: Option<String>,
    pub http: Http,
    pub collect: Collect,
    pub store: Store,
    pub workspaces: Workspaces,
    pub notify: Notify,
    pub push: Push,
    pub knowledge: Knowledge,
    pub sync: HubSync,
    pub workspace: HubWorkspace,
    pub reports: HubReports,
    pub wake: HubWake,
}

/// `ccdash collector`: the hub link plus everything read on this machine.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CollectorConfig {
    /// Name this machine reports to the hub; defaults to the kernel host
    /// name. The hub must hold a token under the same name.
    pub host: Option<String>,
    pub hub: HubLink,
    pub agents: Agents,
    pub niri: Niri,
    pub procs: Procs,
    pub attach: Attach,
    pub edit: Edit,
    pub launch: Launch,
    pub telemetry: Telemetry,
    pub usage: Usage,
    pub skills: Skills,
    pub queue: Queue,
    pub inject: Inject,
    pub sync: Sync,
    pub workspace: Workspace,
    pub power: Power,
}

/// Waking machines over the LAN. The hub sends the magic packet to the
/// broadcast address of a network it shares with the machine's wake-on-LAN
/// card, learned from the machine's reports.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HubWake {
    /// Host → where its magic packet goes when the hub shares no network
    /// with it: an address the hub routes to, `ip` or `ip:port` (port 9 by
    /// default). Either the machine's own address, when its router keeps a
    /// permanent ARP entry for it (a sleeping machine answers no ARP), or
    /// its network's broadcast address, when the router forwards directed
    /// broadcasts.
    pub via: BTreeMap<String, String>,
}

/// What the panel may do to this machine's power.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Power {
    /// Suspend the machine when the panel asks.
    pub suspend: bool,
    /// Suspends the machine; it must return once the sleep is on its way
    /// (`systemctl suspend` does). logind allows it to the user of an
    /// active seat session. Windows: see `WINDOWS_SUSPEND`.
    pub suspend_command: Vec<String>,
    /// Lock the user's graphical session when the panel asks.
    pub lock: bool,
    /// Locks it; `{session}` is replaced with the session's id (logind's
    /// `Display`; on Windows the Remote Desktop session's). `loginctl
    /// lock-session` reaches any locker that listens to logind,
    /// DankMaterialShell's included; Windows: `LockWorkStation`.
    pub lock_command: Vec<String>,
}

/// Where the hub keeps Claude Code's state for the collectors.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HubSync {
    /// Defaults to `claude/` next to the database. `$VAR` expands.
    pub dir: Option<PathBuf>,
}

/// The workspace sync on the hub: bare repositories under
/// `sync.dir/workspace/`, the git smart HTTP routes, the configuration.
/// The HTML reports sessions publish (`report_publish`), kept as plain
/// files outside the database.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HubReports {
    /// Defaults to `reports/` next to the database. `$VAR` expands.
    pub dir: Option<PathBuf>,
    /// The largest report taken, in MiB.
    pub max_mb: u32,
}

impl Default for HubReports {
    fn default() -> Self {
        Self {
            dir: None,
            max_mb: 32,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HubWorkspace {
    pub enable: bool,
}

/// Keeping the workspace root (every repository and plain directory
/// under it) in step with the other machines through the hub.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Workspace {
    pub enable: bool,
    /// The workspace root, e.g. `~/ProjectBundle`. `$VAR` and `~/` expand.
    pub root: Option<PathBuf>,
    /// The starting configuration (unit globs, excludes, the opted-in
    /// ignored paths), until the hub's takes over.
    pub depth: u32,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub always_exclude: Vec<String>,
    pub sync_ignored: Vec<String>,
    pub max_file_mb: u64,
    pub auto_include_mb: u64,
}

impl Default for Workspace {
    fn default() -> Self {
        let defaults = crate::workspace::WorkspaceConfig::default();
        Self {
            enable: false,
            root: None,
            depth: defaults.depth,
            include: Vec::new(),
            exclude: Vec::new(),
            always_exclude: defaults.always_exclude,
            sync_ignored: defaults
                .sync_ignored
                .into_iter()
                .map(|i| i.pattern)
                .collect(),
            max_file_mb: defaults.max_file_mb,
            auto_include_mb: defaults.auto_include_mb,
        }
    }
}

impl Workspace {
    /// The starting configuration for the hub.
    pub fn starting(&self) -> crate::workspace::WorkspaceConfig {
        crate::workspace::WorkspaceConfig {
            depth: self.depth.max(1),
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            always_exclude: self.always_exclude.clone(),
            sync_ignored: self
                .sync_ignored
                .iter()
                .map(|p| crate::workspace::Ignored {
                    pattern: p.clone(),
                    quiescent: p.starts_with(crate::workspace::QUIESCENT_PREFIX),
                })
                .collect(),
            max_file_mb: self.max_file_mb,
            auto_include_mb: self.auto_include_mb,
            ..crate::workspace::WorkspaceConfig::default()
        }
    }
}

/// Keeping this machine's Claude Code state (sessions, rewind
/// checkpoints, plans, tasks, pasted content, prompt history) in step
/// with the hub.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sync {
    pub enable: bool,
    /// The Claude config dir; defaults to `$CLAUDE_CONFIG_DIR`, else
    /// `~/.claude`. `$VAR` expands.
    pub root: Option<PathBuf>,
    /// Globs relative to the root (`*` spans directories) of what travels.
    pub include: Vec<String>,
    /// Globs of what stays home even when included.
    pub exclude: Vec<String>,
    /// Seconds between full scans when inotify is unavailable; with it,
    /// changes are seen as they happen.
    pub interval_secs: u64,
}

impl Default for Sync {
    fn default() -> Self {
        Self {
            enable: false,
            root: None,
            include: crate::sync::collector::DEFAULT_INCLUDE
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            exclude: crate::sync::collector::DEFAULT_EXCLUDE
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            interval_secs: 3,
        }
    }
}

/// The knowledge layer's lifecycle: a daily pass that flags memories.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Knowledge {
    /// Whether the daily pass runs at all.
    pub reflect: bool,
    /// Memories not verified for this many days turn stale; 0 never.
    pub stale_unverified_days: u32,
    /// Memories not recalled for this many days turn stale; 0 never.
    pub stale_unrecalled_days: u32,
}

/// What the hooks inject into sessions from the knowledge layer.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Inject {
    /// Open issues, memories, documents and recent progress at session start.
    pub session_start: bool,
    /// Other sessions active in the project, at every prompt.
    pub prompt: bool,
    /// Who else edited the file, before an edit.
    pub edits: bool,
}

impl Default for Knowledge {
    fn default() -> Self {
        Self {
            reflect: true,
            stale_unverified_days: 90,
            stale_unrecalled_days: 60,
        }
    }
}

impl Default for Inject {
    fn default() -> Self {
        Self {
            session_start: true,
            prompt: true,
            edits: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Http {
    pub listen: Vec<SocketAddr>,
    /// File holding the bearer token of the web UI and API; generated in
    /// the state dir when unset. `$VAR` and `${VAR}` expand.
    pub token_file: Option<PathBuf>,
    /// How phones reach the web UI (links in notifications, pairing),
    /// e.g. `https://ccdash.example.org`. Defaults to this machine's LAN
    /// address and the first HTTP port.
    pub public_url: Option<String>,
    /// Directory holding the built panel (`index.html`, `_app/`). Defaults
    /// to the one the Nix build bakes in, else `web/build` under the
    /// working directory; the `CCDASH_WEB_DIR` environment variable
    /// overrides both.
    pub web_dir: Option<PathBuf>,
}

/// Collector links: one token per machine, keyed by the host name the
/// collector announces.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Collect {
    /// Host name → file holding that machine's token (`$VAR` expands).
    pub tokens: BTreeMap<String, PathBuf>,
}

/// How a collector reaches the hub.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HubLink {
    /// The `/collect` endpoint, e.g. `wss://ccdash.example.org/collect`.
    pub url: String,
    /// File holding this machine's collector token (`$VAR` expands).
    pub token_file: Option<PathBuf>,
}

/// The on-disk queue of hook events kept while the hub is unreachable.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Queue {
    /// Newer events are dropped once the queue file reaches this size.
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Agents {
    pub command: Vec<String>,
    pub interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Niri {
    pub enable: bool,
    /// Window `app_id`s that count as terminals hosting Claude Code.
    pub terminals: Vec<String>,
}

/// Top-level directories the UI can filter by, named by their last
/// component; `~/` is the hub user's home, so prefer absolute paths.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Workspaces {
    pub dirs: Vec<String>,
}

/// CPU and memory of each session's process tree, from `/proc`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Procs {
    pub enable: bool,
    pub interval_secs: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Attach {
    /// Opens a background session in a terminal; `{id}` is replaced with its
    /// short id (`["ghostty", "-e", "claude", "attach", "{id}"]`). Empty (the
    /// default) turns the panel's Open off on this machine.
    pub command: Vec<String>,
}

/// The panel's drafts opened in an editor on this machine (the panel's
/// Ctrl+G, as Claude Code's own opens the prompt in `$EDITOR`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Edit {
    /// Opens the draft's file and returns when the editor closes; `{file}`
    /// is replaced with its path (`["ghostty", "-e", "nvim", "{file}"]`).
    /// Exit status 0 takes the file's content, anything else (`:cq`)
    /// cancels. Empty (the default) turns editing from the panel off on
    /// this machine.
    pub command: Vec<String>,
}

/// Sessions the panel starts, continues and stops on this machine.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Launch {
    /// The Claude Code executable and any leading arguments (never
    /// `--settings`: ccdash passes its own); ccdash adds `--bg`, the model,
    /// the permission mode, `--resume`, the channel (with `channel`) and
    /// the prompt. Empty turns starting sessions from the panel off.
    pub command: Vec<String>,
    /// Sessions here load the ccdash channel (`--channels
    /// plugin:ccdash@inline`, the plugin through `--plugin-dir` in
    /// `command`), so the panel can reply to them. The Home Manager module
    /// sets it with `claudeCode.channel.enable`.
    pub channel: bool,
    /// Stops a background session; `{id}` is replaced with its short id.
    pub stop: Vec<String>,
    /// Removes a background session no process runs any more, when `stop`
    /// fails on it (Claude Code keeps listing blocked jobs); `{id}` as
    /// above. The conversation is kept. Empty never removes.
    pub remove: Vec<String>,
    /// Prints, as JSON, the environment direnv exports in the launch
    /// directory (`["direnv", "export", "json"]`); the variables matching
    /// `env` reach the session through its settings file (a `--bg` session
    /// never sees the collector's environment). Empty leaves the
    /// environment alone; a failure (not allowed, an error) fails the
    /// launch.
    pub direnv: Vec<String>,
    /// Globs (`*`, `?`) of the direnv variables to pass on, such as
    /// `CLAUDE_META_*`. Nothing passes without a match.
    pub env: Vec<String>,
}

/// Polls Claude Code's `/usage` report for the per-model weekly limits the
/// statusline doesn't carry, such as Fable's.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Usage {
    /// The Claude Code executable and any leading arguments; ccdash adds the
    /// `-p /usage` ones. Empty turns the poll off.
    pub command: Vec<String>,
    pub interval_secs: u64,
}

/// What the panel's composer offers after a `/`: the collector reports the
/// skills Claude Code loads on this machine.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Skills {
    /// Plugin directories sessions are started with (`--plugin-dir`), whose
    /// `skills/*/SKILL.md` count as `<plugin>:<skill>`.
    pub plugin_dirs: Vec<PathBuf>,
}

/// Which prompts and turns notify the phone (through Web Push, `[push]`),
/// and when; prompt notifications carry buttons that answer them.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notify {
    /// Where tap links point; defaults to `http.public_url`.
    pub web_url: Option<String>,
    /// Seconds a prompt or a finished turn waits before it notifies, so
    /// one answered at the desk never does.
    pub delay_secs: u64,
    /// The wait while the session's window has focus.
    pub focused_delay_secs: u64,
    pub permissions: bool,
    pub questions: bool,
    pub plans: bool,
    /// Finished turns of interactive sessions and of sessions started
    /// from the panel ("Finished").
    pub turns: bool,
    /// Workspace names to notify for; empty notifies for every session.
    pub workspaces: Vec<String>,
}

/// Web Push (RFC 8030) to the panel installed on phones.
/// Subscriptions come from the panel's pairing dialog; the hub signs
/// (VAPID) and encrypts every message itself.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Push {
    pub enabled: bool,
    /// VAPID contact (`mailto:` or `https:` URL) push services may use to
    /// reach the operator; defaults to `http.public_url`.
    pub subject: Option<String>,
    /// How long a push service keeps an undelivered message, in seconds.
    pub ttl_secs: u64,
    /// PKCS#8 file holding the VAPID key pair; generated on first start
    /// (in the state dir when unset). `$VAR` and `${VAR}` expand.
    pub key_file: Option<PathBuf>,
}

/// OTLP/HTTP (JSON) receiver for Claude Code's OpenTelemetry export. It
/// takes no token, so only loopback addresses are accepted; empty is off.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Telemetry {
    pub listen: Vec<SocketAddr>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Store {
    pub path: Option<PathBuf>,
    pub retention_days: u32,
    /// Bytes of a Bash call's output (or a failed call's error) kept with
    /// its event, for the session page's Shell view; 0 keeps none.
    pub tool_output_bytes: usize,
}

impl Default for Http {
    fn default() -> Self {
        Self {
            listen: vec![SocketAddr::from(([127, 0, 0, 1], 7837))],
            token_file: None,
            public_url: None,
            web_dir: None,
        }
    }
}

/// Windows: a hidden PowerShell calls `SetSuspendState` (sleep, not
/// hibernation), so the command returns before the machine sleeps.
const WINDOWS_SUSPEND: &str = "Start-Process -WindowStyle Hidden powershell.exe -ArgumentList \
    '-NoProfile -Command Add-Type -AssemblyName System.Windows.Forms; \
    [void][System.Windows.Forms.Application]::SetSuspendState(\
    [System.Windows.Forms.PowerState]::Suspend, $false, $false)'";

impl Default for Power {
    fn default() -> Self {
        if cfg!(windows) {
            return Self {
                suspend: false,
                suspend_command: ["powershell.exe", "-NoProfile", "-Command", WINDOWS_SUSPEND]
                    .map(String::from)
                    .to_vec(),
                lock: false,
                lock_command: ["rundll32.exe", "user32.dll,LockWorkStation"]
                    .map(String::from)
                    .to_vec(),
            };
        }
        Self {
            suspend: false,
            suspend_command: vec!["systemctl".into(), "suspend".into()],
            lock: false,
            lock_command: vec!["loginctl".into(), "lock-session".into(), "{session}".into()],
        }
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            max_bytes: 32 << 20,
        }
    }
}

impl Default for Agents {
    fn default() -> Self {
        Self {
            command: ["claude", "agents", "--json", "--all"]
                .map(String::from)
                .to_vec(),
            interval_secs: 5,
        }
    }
}

impl Default for Usage {
    fn default() -> Self {
        Self {
            command: vec!["claude".into()],
            interval_secs: 600,
        }
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self {
            web_url: None,
            delay_secs: 15,
            focused_delay_secs: 120,
            permissions: true,
            questions: true,
            plans: true,
            turns: true,
            workspaces: Vec::new(),
        }
    }
}

impl Default for Push {
    fn default() -> Self {
        Self {
            enabled: true,
            subject: None,
            ttl_secs: 3600,
            key_file: None,
        }
    }
}

impl Default for Niri {
    fn default() -> Self {
        Self {
            enable: cfg!(unix),
            terminals: [
                "com.mitchellh.ghostty",
                "foot",
                "kitty",
                "Alacritty",
                "org.wezfurlong.wezterm",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

impl Default for Procs {
    fn default() -> Self {
        Self {
            enable: true,
            interval_secs: 5,
        }
    }
}

impl Default for Launch {
    fn default() -> Self {
        Self {
            command: vec!["claude".into()],
            channel: false,
            stop: ["claude", "stop", "{id}"].map(String::from).to_vec(),
            remove: ["claude", "rm", "{id}"].map(String::from).to_vec(),
            direnv: Vec::new(),
            env: Vec::new(),
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Self {
            path: None,
            retention_days: 30,
            tool_output_bytes: 8192,
        }
    }
}

/// Loads `path`, or the default config file when it exists.
pub fn load<T: DeserializeOwned + Default>(path: Option<PathBuf>) -> Result<T> {
    let (path, explicit) = match path {
        Some(p) => (p, true),
        None => (paths::config_file(), false),
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

impl HubConfig {
    pub fn host(&self) -> String {
        self.host.clone().unwrap_or_else(crate::util::hostname)
    }

    pub fn store_path(&self) -> PathBuf {
        self.store
            .path
            .clone()
            .unwrap_or_else(|| paths::state_dir().join("ccdash.db"))
    }

    /// Where the panel's files are: `CCDASH_WEB_DIR`, else `http.web_dir`,
    /// else the directory baked in at build time, else `web/build`.
    pub fn web_dir(&self) -> PathBuf {
        if let Some(dir) = std::env::var_os("CCDASH_WEB_DIR") {
            return PathBuf::from(dir);
        }
        if let Some(dir) = &self.http.web_dir {
            return PathBuf::from(crate::util::expand_env(&dir.to_string_lossy()));
        }
        match option_env!("CCDASH_WEB_DIR_DEFAULT") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from("web/build"),
        }
    }

    /// Where Claude Code's state is kept: `sync.dir`, else `claude/` next
    /// to the database.
    pub fn sync_dir(&self) -> PathBuf {
        if let Some(dir) = &self.sync.dir {
            return PathBuf::from(crate::util::expand_env(&dir.to_string_lossy()));
        }
        let db = self.store_path();
        db.parent()
            .map_or_else(|| PathBuf::from("claude"), |dir| dir.join("claude"))
    }

    /// The session catalog (derived from the synced transcripts, rebuilt
    /// when missing): `catalog.db` next to the database.
    pub fn catalog_path(&self) -> PathBuf {
        self.store_path()
            .parent()
            .map_or_else(|| PathBuf::from("catalog.db"), |dir| dir.join("catalog.db"))
    }

    /// Where the reports' HTML files are kept: `reports.dir`, else
    /// `reports/` next to the database.
    pub fn reports_dir(&self) -> PathBuf {
        if let Some(dir) = &self.reports.dir {
            return PathBuf::from(crate::util::expand_env(&dir.to_string_lossy()));
        }
        let db = self.store_path();
        db.parent()
            .map_or_else(|| PathBuf::from("reports"), |dir| dir.join("reports"))
    }

    pub fn push_key_file(&self) -> PathBuf {
        self.push
            .key_file
            .as_ref()
            .map(|p| PathBuf::from(crate::util::expand_env(&p.to_string_lossy())))
            .unwrap_or_else(|| paths::state_dir().join("vapid.p8"))
    }

    /// The web UI's address for phones: `http.public_url`, else the LAN
    /// address and the first HTTP port.
    pub fn public_url(&self) -> String {
        if let Some(url) = &self.http.public_url {
            return url.trim_end_matches('/').to_owned();
        }
        let port = self.http.listen.first().map_or(7837, SocketAddr::port);
        let host = crate::util::lan_address().map_or_else(|| self.host(), |ip| ip.to_string());
        crate::util::base_url(&host, port)
    }
}

impl CollectorConfig {
    pub fn host(&self) -> String {
        self.host.clone().unwrap_or_else(crate::util::hostname)
    }

    /// The workspace root the workspace sync keeps in step.
    pub fn workspace_root(&self) -> Option<PathBuf> {
        let raw = self.workspace.root.as_ref()?.to_string_lossy().into_owned();
        let expanded = crate::util::expand_env(&raw);
        let home = paths::home()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(PathBuf::from(match expanded.strip_prefix("~/") {
            Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
            None if expanded == "~" => home,
            None => expanded,
        }))
    }

    /// The Claude config dir the sync keeps in step.
    pub fn sync_root(&self) -> PathBuf {
        self.sync
            .root
            .as_ref()
            .map_or_else(crate::sync::collector::default_root, |p| {
                PathBuf::from(crate::util::expand_env(&p.to_string_lossy()))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_configs_keep_defaults() {
        let hub: HubConfig = toml::from_str("[http]\nlisten = [\"[::]:9000\"]\n").unwrap();
        assert_eq!(hub.http.listen, vec!["[::]:9000".parse().unwrap()]);
        assert_eq!(hub.store.retention_days, 30);
        let collector: CollectorConfig =
            toml::from_str("[hub]\nurl = \"wss://h/collect\"\n[agents]\ninterval_secs = 9\n")
                .unwrap();
        assert_eq!(collector.hub.url, "wss://h/collect");
        assert_eq!(collector.agents.interval_secs, 9);
        assert_eq!(collector.niri.enable, cfg!(unix));
        assert_eq!(collector.launch.command, ["claude"]);
        // NOTE: no terminal or editor is assumed: Open and Edit stay off until set
        assert!(collector.attach.command.is_empty());
        assert!(collector.edit.command.is_empty());
        assert!(!collector.workspace.enable);
        let off: CollectorConfig = toml::from_str("[launch]\ncommand = []\n").unwrap();
        assert!(off.launch.command.is_empty());
        assert_eq!(off.launch.stop, ["claude", "stop", "{id}"]);
        assert!(!off.sync.enable);
        assert!(off.sync.include.iter().any(|p| p == "history.jsonl"));
        let sync: CollectorConfig =
            toml::from_str("[sync]\nenable = true\nroot = \"/c\"\n").unwrap();
        assert_eq!(sync.sync_root(), PathBuf::from("/c"));
        assert_eq!(sync.sync.exclude, ["projects/-tmp-*"]);
        let hub: HubConfig = toml::from_str("[store]\npath = \"/s/h.db\"\n").unwrap();
        assert_eq!(hub.sync_dir(), PathBuf::from("/s/claude"));
        let ws: CollectorConfig = toml::from_str(
            "[workspace]\nenable = true\nroot = \"~/PB\"\ninclude = [\"a/*\"]\nsync_ignored = [\".env\", \".devenv/state/pg\"]\n",
        )
        .unwrap();
        assert!(ws.workspace.enable);
        assert!(ws.workspace_root().unwrap().ends_with("PB"));
        let starting = ws.workspace.starting();
        assert_eq!(starting.include, ["a/*"]);
        assert!(starting.always_exclude.contains(&"node_modules".to_owned()));
        assert!(
            starting
                .sync_ignored
                .iter()
                .any(|i| i.pattern == ".devenv/state/pg" && i.quiescent)
        );
        assert!(
            toml::from_str::<HubConfig>("[workspace]\nenable = true\n")
                .unwrap()
                .workspace
                .enable
        );
        assert!(!off.power.suspend);
        #[cfg(unix)]
        assert_eq!(off.power.suspend_command, ["systemctl", "suspend"]);
        #[cfg(windows)]
        assert_eq!(off.power.suspend_command[3], WINDOWS_SUSPEND);
        let power: CollectorConfig = toml::from_str("[power]\nsuspend = true\n").unwrap();
        assert!(power.power.suspend);
        assert!(!power.power.lock);
        #[cfg(unix)]
        assert_eq!(
            power.power.lock_command,
            ["loginctl", "lock-session", "{session}"]
        );
        #[cfg(windows)]
        assert_eq!(
            power.power.lock_command,
            ["rundll32.exe", "user32.dll,LockWorkStation"]
        );
        let wake: HubConfig =
            toml::from_str("[wake.via]\nkrupikbundle = \"192.168.31.136\"\n").unwrap();
        assert_eq!(wake.wake.via["krupikbundle"], "192.168.31.136");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<HubConfig>("[http]\nlisen = []\n").is_err());
        assert!(toml::from_str::<CollectorConfig>("[store]\npath = \"x\"\n").is_err());
    }
}
