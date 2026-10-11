use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::{hub::Hub, model::Kind, niri};

/// Focuses the session's window, or attaches to a background session in a new
/// terminal when it has none.
pub async fn open(hub: &Hub, attach_command: &[String], key: &str) -> Result<String> {
    let session = hub
        .resolve(key)
        .await?
        .with_context(|| format!("no session matches {key:?}"))?;
    if let Some(window) = &session.window {
        niri::focus(window.id).await?;
        return Ok(format!("focused window {}", window.id));
    }
    let short_id = match (session.kind, &session.short_id) {
        (Kind::Background, Some(id)) => id.clone(),
        _ => bail!("no known window for session {}", session.id),
    };
    let argv: Vec<String> = attach_command
        .iter()
        .map(|arg| arg.replace("{id}", &short_id))
        .collect();
    let (program, args) = argv.split_first().context("empty attach command")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("running {program}"))?;
    tokio::spawn(async move { child.wait().await });
    Ok(format!("attaching to {short_id}"))
}
