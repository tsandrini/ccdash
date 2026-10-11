use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::paths;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Host name reported to clients; defaults to the kernel host name.
    pub host: Option<String>,
    pub http: Http,
    pub agents: Agents,
    pub niri: Niri,
    pub attach: Attach,
    pub store: Store,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Http {
    pub listen: Vec<SocketAddr>,
    /// File holding the bearer token; generated in the state dir when unset.
    pub token_file: Option<PathBuf>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Attach {
    /// Opens a background session; `{id}` is replaced with its short id.
    pub command: Vec<String>,
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

impl Default for Niri {
    fn default() -> Self {
        Self {
            enable: true,
            terminals: vec!["com.mitchellh.ghostty".to_owned()],
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

impl Config {
    /// Loads `path`, or the default config file when it exists.
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let (path, explicit) = match path {
            Some(p) => (p, true),
            None => (paths::config_file(), false),
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn host(&self) -> String {
        self.host.clone().unwrap_or_else(crate::util::hostname)
    }

    pub fn store_path(&self) -> PathBuf {
        self.store
            .path
            .clone()
            .unwrap_or_else(|| paths::state_dir().join("ccdash.db"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_keeps_defaults() {
        let cfg: Config = toml::from_str("[http]\nlisten = [\"[::]:9000\"]\n").unwrap();
        assert_eq!(cfg.http.listen, vec!["[::]:9000".parse().unwrap()]);
        assert_eq!(cfg.agents.interval_secs, 5);
        assert!(cfg.niri.enable);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("[http]\nlisen = []\n").is_err());
    }
}
