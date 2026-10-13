//! Receiver for Claude Code's OpenTelemetry export
//! (code.claude.com/docs/en/monitoring-usage): OTLP over HTTP with JSON
//! bodies, `POST /v1/metrics` and `/v1/logs`. It takes no token, so it
//! only listens on loopback. Keeps what the analytics view reads and drops
//! everything else, prompts and tool inputs included.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tracing::warn;

use crate::{state::StoreMsg, store::Store, util::now_ms};

const MAX_BODY: usize = 16 << 20;
/// Cumulative series remembered for deltas; cleared past this.
const MAX_SERIES: usize = 10_000;
const PREFIX: &str = "claude_code.";

#[derive(Clone)]
struct Receiver {
    store: Store,
    /// Last value of each cumulative series, by name and attributes.
    last: Arc<Mutex<HashMap<String, f64>>>,
}

pub async fn serve(listener: TcpListener, store: Store) {
    let app = Router::new()
        .route("/v1/metrics", post(metrics))
        .route("/v1/logs", post(logs))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(Receiver {
            store,
            last: Arc::default(),
        });
    if let Err(e) = axum::serve(listener, app).await {
        warn!("telemetry: {e}");
    }
}

async fn metrics(State(r): State<Receiver>, headers: HeaderMap, body: Bytes) -> Response {
    let body = match parse(&headers, &body) {
        Ok(body) => body,
        Err(e) => return e.into_response(),
    };
    let records = {
        let mut last = r.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.len() > MAX_SERIES {
            last.clear();
        }
        metric_records(&body, &mut last)
    };
    for record in records {
        r.store.send(record);
    }
    Json(json!({})).into_response()
}

async fn logs(State(r): State<Receiver>, headers: HeaderMap, body: Bytes) -> Response {
    let body = match parse(&headers, &body) {
        Ok(body) => body,
        Err(e) => return e.into_response(),
    };
    for record in log_records(&body, now_ms()) {
        r.store.send(record);
    }
    Json(json!({})).into_response()
}

fn parse(headers: &HeaderMap, body: &[u8]) -> Result<Value, (StatusCode, String)> {
    let header = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    };
    if !header(header::CONTENT_TYPE).starts_with("application/json")
        || !matches!(header(header::CONTENT_ENCODING), "" | "identity")
    {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "ccdash takes uncompressed JSON: OTEL_EXPORTER_OTLP_PROTOCOL=http/json".into(),
        ));
    }
    serde_json::from_slice(body).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

/// Commits, pull requests, edit decisions and active time; delta values,
/// or increases of cumulative ones.
fn metric_records(body: &Value, last: &mut HashMap<String, f64>) -> Vec<StoreMsg> {
    let mut out = Vec::new();
    for rm in array(&body["resourceMetrics"]) {
        let resource = attributes(&rm["resource"]["attributes"]);
        for m in array(&rm["scopeMetrics"]).flat_map(|sm| array(&sm["metrics"])) {
            let Some(name) = m["name"].as_str() else {
                continue;
            };
            let kind = match name.strip_prefix(PREFIX) {
                Some("commit.count") => "commit",
                Some("pull_request.count") => "pull_request",
                Some("code_edit_tool.decision") => "edit_decision",
                Some("active_time.total") => "active_time",
                _ => continue,
            };
            let sum = &m["sum"];
            // NOTE: 2 is AGGREGATION_TEMPORALITY_CUMULATIVE
            let cumulative = matches!(
                &sum["aggregationTemporality"],
                Value::Number(n) if n.as_u64() == Some(2)
            ) || sum["aggregationTemporality"]
                == "AGGREGATION_TEMPORALITY_CUMULATIVE";
            for point in array(&sum["dataPoints"]) {
                let mut attrs = resource.clone();
                attrs.extend(attributes(&point["attributes"]));
                let Some(session) = session(&attrs) else {
                    continue;
                };
                let Some(mut value) =
                    number(&point["asDouble"]).or_else(|| number(&point["asInt"]))
                else {
                    continue;
                };
                if cumulative {
                    let series = format!("{name} {}", Value::Object(attrs.clone()));
                    value -= last.insert(series, value).unwrap_or(0.0);
                }
                if value <= 0.0 {
                    continue;
                }
                let text = |key: &str| attrs.get(key).cloned().unwrap_or(Value::Null);
                let data = match kind {
                    "edit_decision" => json!({
                        "tool": text("tool_name"),
                        "decision": text("decision"),
                        "source": text("source"),
                        "language": text("language"),
                        "n": value,
                    }),
                    "active_time" => json!({ "type": text("type"), "secs": value }),
                    _ => json!({ "n": value }),
                };
                out.push(StoreMsg::Telemetry {
                    ts: nanos_ms(&point["timeUnixNano"]).unwrap_or_else(now_ms),
                    session,
                    name: kind,
                    data,
                });
            }
        }
    }
    out
}

/// API requests and errors, and tool decisions.
fn log_records(body: &Value, now: i64) -> Vec<StoreMsg> {
    let mut out = Vec::new();
    for rl in array(&body["resourceLogs"]) {
        let resource = attributes(&rl["resource"]["attributes"]);
        for record in array(&rl["scopeLogs"]).flat_map(|sl| array(&sl["logRecords"])) {
            let mut attrs = resource.clone();
            attrs.extend(attributes(&record["attributes"]));
            let event = attrs
                .get("event.name")
                .and_then(Value::as_str)
                .or_else(|| record["body"]["stringValue"].as_str())
                .unwrap_or_default();
            let event = event.strip_prefix(PREFIX).unwrap_or(event);
            let Some(session) = session(&attrs) else {
                continue;
            };
            let text = |key: &str| attrs.get(key).cloned().unwrap_or(Value::Null);
            let num = |key: &str| attrs.get(key).and_then(number);
            let (name, data) = match event {
                "api_request" => (
                    "api_request",
                    json!({
                        "model": text("model"),
                        "source": text("query_source"),
                        "ms": num("duration_ms"),
                        "ttft": num("ttft_ms"),
                        "cost": num("cost_usd"),
                        "input": num("input_tokens"),
                        "output": num("output_tokens"),
                        "cache_read": num("cache_read_tokens"),
                        "cache_write": num("cache_creation_tokens"),
                    }),
                ),
                "api_error" => (
                    "api_error",
                    json!({
                        "model": text("model"),
                        "status": attrs.get("status_code").map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        }),
                        "ms": num("duration_ms"),
                    }),
                ),
                "tool_decision" => (
                    "tool_decision",
                    json!({
                        "tool": text("tool_name"),
                        "decision": text("decision"),
                        "source": text("source"),
                    }),
                ),
                _ => continue,
            };
            let ts = nanos_ms(&record["timeUnixNano"])
                .or_else(|| nanos_ms(&record["observedTimeUnixNano"]))
                .unwrap_or(now);
            out.push(StoreMsg::Telemetry {
                ts,
                session,
                name,
                data,
            });
        }
    }
    out
}

fn array(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

/// OTLP `KeyValue` lists as plain JSON values.
fn attributes(list: &Value) -> Map<String, Value> {
    array(list)
        .filter_map(|kv| Some((kv["key"].as_str()?.to_owned(), any_value(&kv["value"]))))
        .collect()
}

fn any_value(value: &Value) -> Value {
    if let Some(s) = value.get("stringValue") {
        s.clone()
    } else if let Some(n) = value.get("intValue").and_then(number) {
        json!(n as i64)
    } else if let Some(n) = value.get("doubleValue") {
        n.clone()
    } else if let Some(b) = value.get("boolValue") {
        b.clone()
    } else {
        Value::Null
    }
}

/// A number, or a string holding one (OTLP JSON writes 64-bit ints as strings).
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn nanos_ms(value: &Value) -> Option<i64> {
    let nanos = match value {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.parse().ok()?,
        _ => return None,
    };
    (nanos > 0).then_some((nanos / 1_000_000) as i64)
}

/// The Claude Code session id; anything else is not ours to keep.
fn session(attrs: &Map<String, Value>) -> Option<String> {
    let id = attrs.get("session.id")?.as_str()?;
    let uuid = id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    uuid.then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "fc41a266-bacb-4661-8fdb-dcf6642e28b6";

    fn kv(key: &str, value: Value) -> Value {
        json!({ "key": key, "value": value })
    }

    #[test]
    fn api_requests_and_decisions_from_logs() {
        let body = json!({"resourceLogs": [{
            "resource": {"attributes": [kv("service.name", json!({"stringValue": "claude-code"}))]},
            "scopeLogs": [{"logRecords": [
                {
                    "timeUnixNano": "1791889474282000000",
                    "body": {"stringValue": "claude_code.api_request"},
                    "attributes": [
                        kv("event.name", json!({"stringValue": "api_request"})),
                        kv("session.id", json!({"stringValue": SESSION})),
                        kv("model", json!({"stringValue": "claude-opus-5-5"})),
                        kv("input_tokens", json!({"stringValue": "120"})),
                        kv("cache_read_tokens", json!({"intValue": "48000"})),
                        kv("duration_ms", json!({"doubleValue": 2300.0})),
                        kv("prompt", json!({"stringValue": "secret"})),
                    ]
                },
                {
                    "timeUnixNano": "0",
                    "observedTimeUnixNano": "1791889475000000000",
                    "attributes": [
                        kv("event.name", json!({"stringValue": "claude_code.tool_decision"})),
                        kv("session.id", json!({"stringValue": SESSION})),
                        kv("decision", json!({"stringValue": "accept"})),
                        kv("source", json!({"stringValue": "config"})),
                        kv("tool_name", json!({"stringValue": "Bash"})),
                    ]
                },
                {
                    "attributes": [
                        kv("event.name", json!({"stringValue": "user_prompt"})),
                        kv("session.id", json!({"stringValue": SESSION})),
                    ]
                }
            ]}]
        }]});
        let records = log_records(&body, 7);
        assert_eq!(records.len(), 2);
        let StoreMsg::Telemetry { ts, name, data, .. } = &records[0] else {
            panic!()
        };
        assert_eq!((*ts, *name), (1_791_889_474_282, "api_request"));
        assert_eq!(data["input"], 120.0);
        assert_eq!(data["cache_read"], 48000.0);
        assert_eq!(data["ms"], 2300.0);
        assert!(data.get("prompt").is_none());
        let StoreMsg::Telemetry { ts, data, .. } = &records[1] else {
            panic!()
        };
        assert_eq!(
            (*ts, &data["source"]),
            (1_791_889_475_000, &json!("config"))
        );
    }

    #[test]
    fn metric_deltas_and_cumulative_increases() {
        let body = |temporality: u64, value: &str| {
            json!({"resourceMetrics": [{
                "resource": {"attributes": [kv("session.id", json!({"stringValue": SESSION}))]},
                "scopeMetrics": [{"metrics": [
                    {"name": "claude_code.commit.count", "sum": {
                        "aggregationTemporality": temporality,
                        "dataPoints": [{"timeUnixNano": "1791889474282000000", "asInt": value}]
                    }},
                    {"name": "claude_code.token.usage", "sum": {
                        "aggregationTemporality": temporality,
                        "dataPoints": [{"asDouble": 5.0}]
                    }}
                ]}]
            }]})
        };
        let mut last = HashMap::new();
        let n = |records: Vec<StoreMsg>| match records.as_slice() {
            [StoreMsg::Telemetry { data, .. }] => data["n"].as_f64(),
            [] => None,
            _ => panic!(),
        };
        assert_eq!(n(metric_records(&body(1, "2"), &mut last)), Some(2.0));
        assert_eq!(n(metric_records(&body(2, "3"), &mut last)), Some(3.0));
        assert_eq!(n(metric_records(&body(2, "5"), &mut last)), Some(2.0));
        assert_eq!(n(metric_records(&body(2, "5"), &mut last)), None);
    }
}
