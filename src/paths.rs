use std::{env, path::PathBuf};

/// `$CCDASH_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR/ccdash`; on Windows
/// `%LOCALAPPDATA%\ccdash\run` whatever `XDG_RUNTIME_DIR` says: the pipes
/// are named after this directory, and every process of the user has the
/// same `LOCALAPPDATA` (an MSYS shell sets `XDG_RUNTIME_DIR`, a logon
/// does not).
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = env::var_os("CCDASH_RUNTIME_DIR") {
        return dir.into();
    }
    if let Some(dir) = known_folder("LOCALAPPDATA") {
        return dir.join("ccdash").join("run");
    }
    match xdg("XDG_RUNTIME_DIR") {
        Some(dir) => dir.join("ccdash"),
        None => env::temp_dir().join(format!("ccdash-{}", env::var("USER").unwrap_or_default())),
    }
}

pub fn ingest_socket() -> PathBuf {
    runtime_dir().join("ingest.sock")
}

pub fn api_socket() -> PathBuf {
    runtime_dir().join("api.sock")
}

/// Whether `session` is safe to use as a file name (plain session ids are).
pub fn plain_session_id(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 64
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Where `ccdash rename` queues a title until the session's next prompt.
/// `None` for ids that are not plain session ids.
pub fn pending_title(session: &str) -> Option<PathBuf> {
    plain_session_id(session).then(|| runtime_dir().join("titles").join(session))
}

/// The collector's queue of events for the hub while it is unreachable.
pub fn outbox_file() -> PathBuf {
    state_dir().join("outbox.ndjson")
}

/// The collector's copy of the workspaces' live notes, for reads while
/// the hub is unreachable.
pub fn notes_cache() -> PathBuf {
    state_dir().join("notes-cache.json")
}

/// Where desktop tools leave generated files (pywal's colours and such).
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME").unwrap_or_else(|| home_or_root().join(".cache"))
}

/// `$XDG_STATE_HOME/ccdash`; on Windows `%LOCALAPPDATA%\ccdash\state`.
pub fn state_dir() -> PathBuf {
    match xdg("XDG_STATE_HOME") {
        Some(dir) => dir.join("ccdash"),
        None => match known_folder("LOCALAPPDATA") {
            Some(dir) => dir.join("ccdash").join("state"),
            None => home_or_root().join(".local/state").join("ccdash"),
        },
    }
}

/// `$XDG_CONFIG_HOME/ccdash/config.toml`; on Windows
/// `%APPDATA%\ccdash\config.toml`.
pub fn config_file() -> PathBuf {
    xdg("XDG_CONFIG_HOME")
        .or_else(|| known_folder("APPDATA"))
        .unwrap_or_else(|| home_or_root().join(".config"))
        .join("ccdash")
        .join("config.toml")
}

/// The user's home: `HOME`; on Windows `USERPROFILE`.
pub fn home() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env::var_os(var)
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn home_or_root() -> PathBuf {
    home().unwrap_or_else(|| "/".into())
}

fn xdg(var: &str) -> Option<PathBuf> {
    env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// A Windows known folder from its environment variable (`APPDATA`,
/// `LOCALAPPDATA`); none elsewhere.
fn known_folder(var: &str) -> Option<PathBuf> {
    if cfg!(windows) { xdg(var) } else { None }
}
