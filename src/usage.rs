//! Polls Claude Code's `/usage` report for the weekly limits of one model or
//! surface, such as Fable's, which the statusline doesn't carry. Runs
//! `claude -p /usage` in safe mode: no model request, no hooks, plugins or
//! memory, nothing saved. The report is `SDKUsageReport` in the Agent SDK
//! reference (experimental).

use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::{process::Command, sync::mpsc};
use tracing::warn;

use crate::{
    collector::LocalCmd,
    model::ScopedLimit,
    util::{now_ms, parse_rfc3339},
};

const ARGS: [&str; 6] = [
    "-p",
    "/usage",
    "--output-format",
    "stream-json",
    "--verbose",
    "--no-session-persistence",
];

pub async fn run(command: Vec<String>, every: Duration, tx: mpsc::Sender<LocalCmd>) {
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_error = None;
    loop {
        ticker.tick().await;
        match poll(&command).await {
            Ok(limits) => {
                last_error = None;
                if tx.send(LocalCmd::Usage(limits, now_ms())).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                let message = format!("{e:#}");
                if last_error.as_ref() != Some(&message) {
                    warn!("claude /usage: {message}");
                    last_error = Some(message);
                }
            }
        }
    }
}

async fn poll(command: &[String]) -> Result<Vec<ScopedLimit>> {
    let (program, args) = command.split_first().context("empty usage command")?;
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new(program)
            .args(args)
            .args(ARGS)
            .current_dir(std::env::temp_dir())
            .env("CLAUDE_CODE_SAFE_MODE", "1")
            .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")
            .env_remove("CLAUDECODE")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
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
    parse(&output.stdout, now_ms())
}

/// The scoped rows of the usage report in stream-json `stdout`; none when
/// the plan has no usage rows (an API key, say).
pub fn parse(stdout: &[u8], now: i64) -> Result<Vec<ScopedLimit>> {
    let report = stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find_map(|message| message.get("usage_report").cloned())
        .context("no usage report in the output")?;
    let Some(rows) = report
        .pointer("/rate_limits/limits")
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };
    Ok(rows
        .iter()
        .filter_map(|row| {
            let scope = row.get("scope")?;
            let label = scope
                .pointer("/model/display_name")
                .or_else(|| scope.pointer("/surface/display_name"))?
                .as_str()?;
            Some(ScopedLimit {
                label: label.to_owned(),
                used_pct: row.get("percent")?.as_f64()?,
                resets_at: row
                    .get("resets_at")
                    .and_then(Value::as_str)
                    .and_then(parse_rfc3339)
                    .map_or(0, |ms| ms / 1000),
                severity: row
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("normal")
                    .to_owned(),
                read_at: now,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_scoped_rows() {
        let row = |kind: &str, scope: &str, pct: u32| {
            format!(
                r#"{{"kind":"{kind}","group":"weekly","percent":{pct},"resets_at":"2026-10-15T00:59:59.823+00:00","scope":{scope},"severity":"critical","is_active":true}}"#
            )
        };
        let report = format!(
            r#"{{"type":"assistant","usage_report":{{"session":{{}},"rate_limits":{{"limits":[{},{},{}]}}}}}}"#,
            row("session", "null", 4),
            row("weekly_all", "null", 55),
            row(
                "weekly_scoped",
                r#"{"model":{"display_name":"Fable"},"surface":null}"#,
                91
            ),
        );
        let stdout = format!("{{\"type\":\"system\",\"subtype\":\"init\"}}\n{report}\n");
        let limits = parse(stdout.as_bytes(), 7).unwrap();
        assert_eq!(
            limits,
            [ScopedLimit {
                label: "Fable".into(),
                used_pct: 91.0,
                resets_at: 1_792_025_999,
                severity: "critical".into(),
                read_at: 7,
            }]
        );

        let no_plan = r#"{"type":"assistant","usage_report":{"session":{},"rate_limits":null}}"#;
        assert!(parse(no_plan.as_bytes(), 7).unwrap().is_empty());
        assert!(parse(b"{}", 7).is_err());
    }
}
