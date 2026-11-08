//! `report_publish`: the one knowledge tool the collector carries out
//! itself. The report's HTML is read here, on the session's machine, and
//! sent with the note's metadata to the hub's `/reports/-/upload` over
//! HTTP with this machine's token; the hub keeps the file, writes the
//! `report` note and answers with the panel's address. Not queued while
//! the hub is unreachable: a report is megabytes, and the file stays where
//! it is for another try.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::{
    config::CollectorConfig,
    protocol::Who,
    report,
    sync::collector::{base_url, hub_http_config},
    util::expand_env,
};

pub const TOOL: &str = "report_publish";
/// Past this the file is refused here, before the hub's own cap.
const MAX_BYTES: usize = 64 * 1024 * 1024;
const REPLY_LIMIT: u64 = 1024 * 1024;

/// The tool's answer: whether it worked, and the text Claude reads.
pub async fn publish(cfg: &CollectorConfig, who: Who, args: &Value) -> (bool, String) {
    match run(cfg, who, args).await {
        Ok(text) => (true, text),
        Err(e) => (false, format!("{e:#}")),
    }
}

async fn run(cfg: &CollectorConfig, who: Who, args: &Value) -> Result<String> {
    let text = |key: &str| -> Option<String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let path = text("path").context("path is required: the HTML file to publish")?;
    let body = text("body")
        .context("body is required: the abstract (markdown), what the report answers and found")?;
    let file = resolve(&path, who.cwd.as_deref());
    let read = file.clone();
    let html = tokio::task::spawn_blocking(move || std::fs::read(&read))
        .await
        .context("reading")?
        .with_context(|| format!("reading {}", file.display()))?;
    if html.len() > MAX_BYTES {
        bail!(
            "{} is {:.1} MB; a report stays under {} MB",
            file.display(),
            html.len() as f64 / 1_048_576.0,
            MAX_BYTES / 1_048_576
        );
    }
    if !report::is_html(&html) {
        bail!(
            "{} is not an HTML document: build the report with `ccdash report build` first",
            file.display()
        );
    }
    let html = String::from_utf8(html).context("the file is not UTF-8")?;
    let Some(token_file) = &cfg.hub.token_file else {
        bail!("this collector has no hub.token_file: it cannot publish to the hub");
    };
    let token_file = PathBuf::from(expand_env(&token_file.to_string_lossy()));
    let url = format!("{}/reports/-/upload", base_url(&cfg.hub.url));
    let payload = json!({
        "session": who.session,
        "pid": who.pid,
        "cwd": who.cwd,
        "title": text("title"),
        "slug": text("slug"),
        "summary": text("summary"),
        "body": body,
        "project": text("project"),
        "date": text("date"),
        "path": file.to_string_lossy(),
        "html": html,
    });
    let shown = file.display().to_string();
    let reply = tokio::task::spawn_blocking(move || send(&url, &token_file, &payload))
        .await
        .context("publishing")??;
    let w = &reply["written"];
    let bytes = reply["bytes"].as_u64().unwrap_or(0);
    let size = if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    };
    Ok(format!(
        "published \"{}\" as `{}` ({}, v{}, {size}): {}\nThe file stays at {shown}; the hub keeps its copy.",
        w["title"].as_str().unwrap_or_default(),
        reply["slug"].as_str().unwrap_or_default(),
        if w["created"].as_bool().unwrap_or(false) {
            "new"
        } else {
            "replaced"
        },
        w["version"].as_u64().unwrap_or(0),
        reply["url"].as_str().unwrap_or_default(),
    ))
}

/// `path` as given when absolute, else under the session's directory.
fn resolve(path: &str, cwd: Option<&str>) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_owned();
    }
    match cwd {
        Some(cwd) if !cwd.is_empty() => Path::new(cwd).join(path),
        _ => std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_owned()),
    }
}

fn send(url: &str, token_file: &Path, payload: &Value) -> Result<Value> {
    let token = std::fs::read_to_string(token_file)
        .with_context(|| format!("reading {}", token_file.display()))?;
    let agent: ureq::Agent = hub_http_config(Duration::from_secs(120)).into();
    let response = agent
        .put(url)
        .header("Authorization", &format!("Bearer {}", token.trim()))
        .header("Content-Type", "application/json")
        .send(&serde_json::to_vec(payload)?[..])
        .map_err(|e| {
            anyhow!(
                "the hub is unreachable ({e}); the report stays on this machine, \
                    publish it again once the link is back"
            )
        })?;
    let status = response.status();
    // NOTE: a proxy answers for a hub that is down
    if matches!(status.as_u16(), 502..=504) {
        bail!(
            "the hub is unreachable ({status} from the proxy); the report stays on this \
                machine, publish it again once the link is back"
        );
    }
    let body = response
        .into_body()
        .with_config()
        .limit(REPLY_LIMIT)
        .read_to_vec()
        .context("reading the hub's answer")?;
    if !status.is_success() {
        let message = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_owned))
            .unwrap_or_else(|| {
                let text = String::from_utf8_lossy(&body);
                if text.trim_start().starts_with('<') {
                    String::new()
                } else {
                    crate::util::truncate(text.trim(), 300)
                }
            });
        bail!("the hub refused the report ({status}): {message}");
    }
    serde_json::from_slice(&body).context("parsing the hub's answer")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_under_the_session_directory() {
        assert_eq!(resolve("/a/b.html", Some("/x")), PathBuf::from("/a/b.html"));
        assert_eq!(resolve("b.html", Some("/x")), PathBuf::from("/x/b.html"));
        assert_eq!(
            resolve("out/b.html", Some("/x/y")),
            PathBuf::from("/x/y/out/b.html")
        );
    }

    #[tokio::test]
    async fn a_missing_file_or_a_non_report_is_refused_before_the_hub() {
        let cfg = CollectorConfig::default();
        let who = Who {
            session: None,
            pid: None,
            cwd: Some("/nonexistent".into()),
        };
        let (ok, text) = publish(&cfg, who.clone(), &json!({ "body": "x" })).await;
        assert!(!ok && text.contains("path is required"), "{text}");
        let (ok, text) = publish(
            &cfg,
            who.clone(),
            &json!({ "path": "nope.html", "body": "x" }),
        )
        .await;
        assert!(
            !ok && text.contains("reading /nonexistent") && text.contains("nope.html"),
            "{text}"
        );
        let dir = std::env::temp_dir();
        let file = dir.join(format!("ccdash-not-a-report-{}.html", std::process::id()));
        std::fs::write(&file, "# markdown").unwrap();
        let (ok, text) = publish(
            &cfg,
            who,
            &json!({ "path": file.to_string_lossy(), "body": "x" }),
        )
        .await;
        assert!(!ok && text.contains("not an HTML document"), "{text}");
        std::fs::remove_file(file).unwrap();
    }
}
