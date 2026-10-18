//! Tokens, cache use, API latency, commits, pull requests, edit decisions
//! and active time, from Claude Code's OpenTelemetry export (see `otlp`).

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use rusqlite::{Connection, named_params};
use serde::Serialize;
use serde_json::Value;

use super::{Bucket, Clock, Uses, percentile};
use crate::workspace::{IN_SCOPE, Scope};

#[derive(Debug, Default, Serialize)]
pub struct Telemetry {
    /// Newest record of any range, to tell an idle range from a dead export.
    pub last_at: i64,
    pub requests: u32,
    pub errors: u32,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_write: u64,
    pub cost_usd: f64,
    pub commits: u32,
    pub pull_requests: u32,
    pub active_user_secs: f64,
    pub active_cli_secs: f64,
    /// Most tokens first.
    pub models: Vec<ModelUse>,
    /// `main`, `subagent`, `auxiliary`.
    pub sources: Vec<SourceUse>,
    pub errors_by_status: Vec<(String, u32)>,
    pub edits: Vec<EditUse>,
    /// How tool calls were let through or refused.
    pub decisions: Vec<DecisionUse>,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct ModelUse {
    pub model: String,
    pub requests: u32,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_write: u64,
    pub cost_usd: f64,
    pub p50_ms: Option<i64>,
    pub p95_ms: Option<i64>,
    /// Time to the first token, median.
    pub ttft_p50_ms: Option<i64>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct SourceUse {
    pub source: String,
    pub requests: u32,
    pub tokens: u64,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct EditUse {
    pub language: String,
    pub accepted: u32,
    pub rejected: u32,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct DecisionUse {
    pub source: String,
    pub decision: String,
    pub count: u32,
}

/// None when Claude Code never sent anything.
pub fn telemetry(
    conn: &Connection,
    scope: &Scope,
    clock: &Clock,
    buckets: &mut [Bucket],
    uses: &mut Uses,
) -> Result<Option<Telemetry>> {
    let last_at: Option<i64> = conn.query_row("SELECT max(ts) FROM telemetry", [], |r| r.get(0))?;
    let Some(last_at) = last_at else {
        return Ok(None);
    };
    let mut out = Telemetry {
        last_at,
        ..Telemetry::default()
    };
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT t.ts, t.name, t.data, coalesce(x.cwd, '?'), coalesce(x.host, t.host)
            FROM telemetry t LEFT JOIN sessions x ON x.id = t.session
            WHERE t.ts >= :since AND t.ts < :until AND {IN_SCOPE}
            ORDER BY t.ts"
    ))?;
    let rows = stmt.query_map(
        named_params! {":since": scope.since, ":until": scope.until, ":dirs": scope.dirs_param(), ":host": scope.host},
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        },
    )?;
    let mut models: HashMap<String, (ModelUse, Vec<i64>, Vec<i64>)> = HashMap::new();
    let mut sources: BTreeMap<String, (u32, u64)> = BTreeMap::new();
    let mut statuses: BTreeMap<String, u32> = BTreeMap::new();
    let mut edits: BTreeMap<String, EditUse> = BTreeMap::new();
    let mut decisions: BTreeMap<(String, String), u32> = BTreeMap::new();
    for row in rows {
        let (ts, name, data, cwd, host) = row?;
        let Ok(data) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let text = |key: &str| data[key].as_str().unwrap_or("unknown").to_owned();
        let count = |key: &str| data[key].as_f64().unwrap_or(0.0).max(0.0);
        let bucket = &mut buckets[clock.index(ts)];
        match name.as_str() {
            "api_request" => {
                let tokens = [
                    count("input"),
                    count("output"),
                    count("cache_read"),
                    count("cache_write"),
                ]
                .map(|n| n as u64);
                let [input, output, cache_read, cache_write] = tokens;
                let total: u64 = tokens.iter().sum();
                bucket.api_requests += 1;
                bucket.tokens_input += input;
                bucket.tokens_output += output;
                bucket.tokens_cache_read += cache_read;
                bucket.tokens_cache_write += cache_write;
                out.requests += 1;
                out.tokens_input += input;
                out.tokens_output += output;
                out.tokens_cache_read += cache_read;
                out.tokens_cache_write += cache_write;
                out.cost_usd += count("cost");
                uses.each(&cwd, &host, |t| t.tokens += total);
                uses.host(&host).api_requests += 1;
                let model = text("model");
                let (m, latencies, ttfts) = models.entry(model.clone()).or_insert_with(|| {
                    (
                        ModelUse {
                            model,
                            ..ModelUse::default()
                        },
                        Vec::new(),
                        Vec::new(),
                    )
                });
                m.requests += 1;
                m.tokens_input += input;
                m.tokens_output += output;
                m.tokens_cache_read += cache_read;
                m.tokens_cache_write += cache_write;
                m.cost_usd += count("cost");
                if let Some(ms) = data["ms"].as_f64() {
                    latencies.push(ms as i64);
                }
                if let Some(ms) = data["ttft"].as_f64() {
                    ttfts.push(ms as i64);
                }
                let source = sources.entry(text("source")).or_default();
                source.0 += 1;
                source.1 += total;
            }
            "api_error" => {
                bucket.api_errors += 1;
                out.errors += 1;
                *statuses.entry(text("status")).or_default() += 1;
            }
            "tool_decision" => {
                *decisions
                    .entry((text("source"), text("decision")))
                    .or_default() += 1;
            }
            "commit" => {
                let n = count("n") as u32;
                bucket.commits += n;
                out.commits += n;
            }
            "pull_request" => {
                let n = count("n") as u32;
                bucket.pull_requests += n;
                out.pull_requests += n;
            }
            "edit_decision" => {
                let n = count("n") as u32;
                let edit = edits.entry(text("language")).or_default();
                if data["decision"] == "reject" {
                    edit.rejected += n;
                } else {
                    edit.accepted += n;
                }
            }
            "active_time" => {
                let secs = count("secs");
                if data["type"] == "user" {
                    bucket.active_user_secs += secs;
                    out.active_user_secs += secs;
                } else {
                    bucket.active_cli_secs += secs;
                    out.active_cli_secs += secs;
                }
            }
            _ => {}
        }
    }
    let total = |m: &ModelUse| {
        m.tokens_input + m.tokens_output + m.tokens_cache_read + m.tokens_cache_write
    };
    out.models = models
        .into_values()
        .map(|(mut m, mut latencies, mut ttfts)| {
            latencies.sort_unstable();
            ttfts.sort_unstable();
            m.p50_ms = percentile(&latencies, 50);
            m.p95_ms = percentile(&latencies, 95);
            m.ttft_p50_ms = percentile(&ttfts, 50);
            m
        })
        .collect();
    out.models.sort_by_key(|m| std::cmp::Reverse(total(m)));
    out.sources = sources
        .into_iter()
        .map(|(source, (requests, tokens))| SourceUse {
            source,
            requests,
            tokens,
        })
        .collect();
    out.sources.sort_by_key(|s| std::cmp::Reverse(s.tokens));
    out.errors_by_status = statuses.into_iter().collect();
    out.edits = edits
        .into_iter()
        .map(|(language, e)| EditUse { language, ..e })
        .collect();
    out.edits
        .sort_by_key(|e| std::cmp::Reverse(e.accepted + e.rejected));
    out.decisions = decisions
        .into_iter()
        .map(|((source, decision), count)| DecisionUse {
            source,
            decision,
            count,
        })
        .collect();
    out.decisions.sort_by_key(|d| std::cmp::Reverse(d.count));
    Ok(Some(out))
}
