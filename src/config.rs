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
    pub knowledge: Knowledge,
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
    pub telemetry: Telemetry,
    pub usage: Usage,
    pub queue: Queue,
    pub inject: Inject,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Attach {
    /// Opens a background session; `{id}` is replaced with its short id.
    pub command: Vec<String>,
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

/// Phone notifications through an ntfy server; prompt notifications carry
/// buttons that answer them. Off while `url` is empty.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notify {
    /// The ntfy server, e.g. `https://ntfy.example.org`.
    pub url: String,
    pub topic: String,
    /// File holding an ntfy access token that may publish to `topic`;
    /// `$VAR` and `${VAR}` expand.
    pub token_file: Option<PathBuf>,
    /// Where button and tap links point; defaults to `http.public_url`.
    pub web_url: Option<String>,
    /// Seconds a prompt or a finished turn waits before it notifies, so
    /// one answered at the desk never does.
    pub delay_secs: u64,
    /// The wait while the session's window has focus.
    pub focused_delay_secs: u64,
    pub permissions: bool,
    pub questions: bool,
    pub plans: bool,
    /// Finished turns of interactive sessions.
    pub turns: bool,
    /// Workspace names to notify for; empty notifies for every session.
    pub workspaces: Vec<String>,
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
}

impl Default for Http {
    fn default() -> Self {
        Self {
            listen: vec![SocketAddr::from(([127, 0, 0, 1], 7837))],
            token_file: None,
            public_url: None,
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
            command: ["claude", "agents", "--json"].map(String::from).to_vec(),
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
            url: String::new(),
            topic: "ccdash".into(),
            token_file: None,
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

impl Default for Niri {
    fn default() -> Self {
        Self {
            enable: true,
            terminals: vec!["com.mitchellh.ghostty".to_owned()],
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

impl Default for Attach {
    fn default() -> Self {
        Self {
            command: ["ghostty", "-e", "claude", "attach", "{id}"]
                .map(String::from)
                .to_vec(),
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Self {
            path: None,
            retention_days: 30,
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
        assert!(collector.niri.enable);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<HubConfig>("[http]\nlisen = []\n").is_err());
        assert!(toml::from_str::<CollectorConfig>("[store]\npath = \"x\"\n").is_err());
    }
}
