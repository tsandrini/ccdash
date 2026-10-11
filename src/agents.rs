//! Polls `claude agents --json` for liveness and busy/idle status.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::{process::Command, sync::mpsc};
use tracing::warn;

use crate::{
    hub::{Cmd, Input},
    state::AgentEntry,
    util::now_ms,
};

pub async fn run(command: Vec<String>, every: Duration, tx: mpsc::Sender<Cmd>) {
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_error = None;
    loop {
        ticker.tick().await;
        match poll(&command).await {
            Ok(list) => {
                last_error = None;
                if tx
                    .send(Cmd::Input(Input::Agents(list, now_ms())))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(e) => {
                let message = format!("{e:#}");
                if last_error.as_ref() != Some(&message) {
                    warn!("claude agents: {message}");
                    last_error = Some(message);
                }
            }
        }
    }
}

async fn poll(command: &[String]) -> Result<Vec<AgentEntry>> {
    let (program, args) = command.split_first().context("empty agents command")?;
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new(program).args(args).kill_on_drop(true).output(),
    )
    .await
    .context("timed out")??;
    if !output.status.success() {
        bail!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("parsing output")
}
