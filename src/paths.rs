use std::{env, path::PathBuf};

pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = env::var_os("CCDASH_RUNTIME_DIR") {
        return dir.into();
    }
    match env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("ccdash"),
        None => env::temp_dir().join(format!("ccdash-{}", env::var("USER").unwrap_or_default())),
    }
}

pub fn ingest_socket() -> PathBuf {
    runtime_dir().join("ingest.sock")
}

pub fn api_socket() -> PathBuf {
    runtime_dir().join("api.sock")
}

pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("ccdash")
}

pub fn config_file() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("ccdash/config.toml")
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("HOME").unwrap_or_else(|| "/".into())).join(fallback)
        })
}
