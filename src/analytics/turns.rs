//! What fills a turn: Claude thinking and writing, tools running, or a
//! permission prompt waiting on you. Plus which tools and commands take the
//! time, the slowest calls, and the commands that fail most.

use std::collections::HashMap;

use serde::Serialize;
use ts_rs::TS;

use super::{Bucket, Clock, SessionEvents, percentile};
use crate::{command, history::Transition, model::Activity};

const TOP_TOOLS: usize = 15;
const TOP_COMMANDS: usize = 15;
const TOP_SLOWEST: usize = 10;

#[derive(Debug, Default, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Anatomy {
    pub turns: u32,
    pub claude_ms: i64,
    pub tools_ms: i64,
    pub permission_ms: i64,
    /// By total time.
    pub tools: Vec<ToolStat>,
    /// Bash commands by total time.
    pub commands: Vec<ToolStat>,
    /// Bash commands with the most failures.
    pub failing: Vec<ToolStat>,
    pub slowest: Vec<SlowCall>,
}

#[derive(Debug, Default, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct ToolStat {
    /// Tool name, or the command key for Bash commands.
    pub name: String,
    pub calls: u32,
    pub failed: u32,
    pub total_ms: i64,
    pub p50_ms: Option<i64>,
    pub p95_ms: Option<i64>,
    pub max_ms: Option<i64>,
    pub last_failed_at: Option<i64>,
}

#[derive(Debug, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct SlowCall {
    pub ts: i64,
    pub session: String,
    pub label: String,
    pub host: String,
    pub cwd: String,
    pub tool: String,
    pub summary: String,
    pub ms: i64,
    pub ok: bool,
}

#[derive(Default)]
struct Calls {
    durations: Vec<i64>,
    failed: u32,
    last_failed_at: Option<i64>,
}

impl Calls {
    fn add(&mut self, ts: i64, ms: i64, ok: bool) {
        self.durations.push(ms);
        if !ok {
            self.failed += 1;
            self.last_failed_at = Some(ts);
        }
    }

    fn stat(mut self, name: String) -> ToolStat {
        self.durations.sort_unstable();
        ToolStat {
            name,
            calls: self.durations.len() as u32,
            failed: self.failed,
            total_ms: self.durations.iter().sum(),
            p50_ms: percentile(&self.durations, 50),
            p95_ms: percentile(&self.durations, 95),
            max_ms: self.durations.last().copied(),
            last_failed_at: self.last_failed_at,
        }
    }
}

/// A turn still open: when it began, and the spans spent in tools and in
/// permission prompts.
struct Turn {
    start: i64,
    tools: Vec<(i64, i64)>,
    asks: Vec<(i64, i64)>,
}

pub fn anatomy(sessions: &[SessionEvents], clock: &Clock, buckets: &mut [Bucket]) -> Anatomy {
    let mut out = Anatomy::default();
    let mut tools: HashMap<String, Calls> = HashMap::new();
    let mut commands: HashMap<String, Calls> = HashMap::new();
    let mut slowest: Vec<SlowCall> = Vec::new();
    for session in sessions {
        let mut turn: Option<Turn> = None;
        let mut asked_at: Option<i64> = None;
        for ev in &session.events {
            let ts = ev.ts;
            match ev.kind.as_str() {
                "prompt" => {
                    if let Some(t) = turn.take() {
                        out.close(t, ts, clock, buckets);
                    }
                    turn = Some(Turn {
                        start: ts,
                        tools: Vec::new(),
                        asks: Vec::new(),
                    });
                }
                "reply" | "error" => {
                    if let Some(t) = turn.take() {
                        out.close(t, ts, clock, buckets);
                    }
                }
                "activity" => {
                    let Ok(t) = serde_json::from_value::<Transition>(ev.data.clone()) else {
                        continue;
                    };
                    if t.to == Activity::Permission {
                        asked_at = Some(ts);
                    } else if t.from == Activity::Permission
                        && let Some(from) = asked_at.take()
                        && let Some(turn) = &mut turn
                    {
                        turn.asks.push((from, ts));
                    }
                }
                "tool" => {
                    let name = ev.data["name"].as_str().unwrap_or_default();
                    let ms = ev.data["ms"].as_i64().unwrap_or(0).max(0);
                    let ok = ev.data["ok"] != false;
                    let summary = ev.data["summary"].as_str().unwrap_or_default();
                    if let Some(turn) = &mut turn {
                        turn.tools.push((ts - ms, ts));
                    }
                    tools.entry(name.to_owned()).or_default().add(ts, ms, ok);
                    if name == "Bash" {
                        let key = command::parse(summary).key;
                        if !key.is_empty() {
                            commands.entry(key).or_default().add(ts, ms, ok);
                        }
                    }
                    if slowest.len() < TOP_SLOWEST || slowest.last().is_some_and(|s| ms > s.ms) {
                        let call = SlowCall {
                            ts,
                            session: session.id.clone(),
                            label: session.label.clone(),
                            host: session.host.clone(),
                            cwd: session.cwd.clone(),
                            tool: name.to_owned(),
                            summary: summary.to_owned(),
                            ms,
                            ok,
                        };
                        let at = slowest.partition_point(|s| s.ms >= ms);
                        slowest.insert(at, call);
                        slowest.truncate(TOP_SLOWEST);
                    }
                }
                _ => {}
            }
        }
    }
    out.slowest = slowest;
    out.tools = top(tools, TOP_TOOLS, |s| s.total_ms);
    let commands: Vec<ToolStat> = commands.into_iter().map(|(k, c)| c.stat(k)).collect();
    let mut failing: Vec<&ToolStat> = commands.iter().filter(|c| c.failed > 0).collect();
    failing.sort_by(|a, b| b.failed.cmp(&a.failed).then_with(|| a.name.cmp(&b.name)));
    out.failing = failing
        .into_iter()
        .take(TOP_COMMANDS)
        .map(|c| ToolStat {
            name: c.name.clone(),
            ..*c
        })
        .collect();
    let mut commands = commands;
    commands.sort_by(|a, b| {
        b.total_ms
            .cmp(&a.total_ms)
            .then_with(|| a.name.cmp(&b.name))
    });
    commands.truncate(TOP_COMMANDS);
    out.commands = commands;
    out
}

fn top(map: HashMap<String, Calls>, n: usize, key: impl Fn(&ToolStat) -> i64) -> Vec<ToolStat> {
    let mut stats: Vec<ToolStat> = map.into_iter().map(|(k, c)| c.stat(k)).collect();
    stats.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| a.name.cmp(&b.name)));
    stats.truncate(n);
    stats
}

impl Anatomy {
    /// Splits a finished turn: permission waits first, then tool time
    /// outside them; the rest is Claude.
    fn close(&mut self, turn: Turn, end: i64, clock: &Clock, buckets: &mut [Bucket]) {
        let length = end - turn.start;
        if length <= 0 {
            return;
        }
        let clip = |spans: &[(i64, i64)]| -> Vec<(i64, i64)> {
            spans
                .iter()
                .map(|&(a, b)| (a.max(turn.start), b.min(end)))
                .filter(|(a, b)| b > a)
                .collect()
        };
        let asks = clip(&turn.asks);
        let mut both = clip(&turn.tools);
        both.extend(&asks);
        let asked = covered(asks);
        let busy = covered(both);
        let bucket = &mut buckets[clock.index(turn.start)];
        bucket.turns += 1;
        bucket.turn_permission_ms += asked;
        bucket.turn_tools_ms += busy - asked;
        bucket.turn_claude_ms += length - busy;
        self.turns += 1;
        self.permission_ms += asked;
        self.tools_ms += busy - asked;
        self.claude_ms += length - busy;
    }
}

/// Length of the union of `spans`.
fn covered(mut spans: Vec<(i64, i64)>) -> i64 {
    spans.sort_unstable();
    let mut total = 0;
    let mut reach = i64::MIN;
    for (a, b) in spans {
        let a = a.max(reach);
        if b > a {
            total += b - a;
        }
        reach = reach.max(b);
    }
    total
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::analytics::Ev;

    fn ev(ts: i64, kind: &str, data: serde_json::Value) -> Ev {
        Ev {
            ts,
            kind: kind.into(),
            data,
        }
    }

    #[test]
    fn union_of_spans() {
        assert_eq!(covered(vec![(0, 10), (5, 15), (20, 25)]), 20);
        assert_eq!(covered(vec![(0, 10), (2, 3)]), 10);
        assert_eq!(covered(vec![]), 0);
    }

    #[test]
    fn turns_split_into_claude_tools_and_permission() {
        let clock = Clock {
            offset_ms: 0,
            bucket_ms: 1_000_000,
            first: 0,
            count: 1,
        };
        let mut buckets = vec![Bucket::default()];
        let session = SessionEvents {
            id: "a".into(),
            host: "h".into(),
            cwd: "/w".into(),
            label: "a".into(),
            events: vec![
                ev(0, "prompt", json!({})),
                // Two parallel reads, 10–30 and 20–40.
                ev(
                    30,
                    "tool",
                    json!({"name": "Read", "summary": "x", "ms": 20, "ok": true}),
                ),
                ev(
                    40,
                    "tool",
                    json!({"name": "Read", "summary": "y", "ms": 20, "ok": true}),
                ),
                ev(
                    50,
                    "activity",
                    json!({"from": "working", "to": "permission"}),
                ),
                ev(
                    70,
                    "activity",
                    json!({"from": "permission", "to": "working"}),
                ),
                ev(
                    90,
                    "tool",
                    json!({
                        "name": "Bash", "summary": "direnv exec . cargo test", "ms": 20, "ok": false
                    }),
                ),
                ev(100, "reply", json!({})),
            ],
        };
        let a = anatomy(&[session], &clock, &mut buckets);
        assert_eq!(a.turns, 1);
        // Tools 10–40 and 70–90 (50 ms), permission 50–70, Claude the rest.
        assert_eq!((a.claude_ms, a.tools_ms, a.permission_ms), (30, 50, 20));
        assert_eq!(buckets[0].turn_tools_ms, 50);
        assert_eq!(a.failing[0].name, "cargo test");
        assert_eq!(a.commands[0].failed, 1);
        assert_eq!(a.tools[0].name, "Read");
        assert_eq!(a.slowest.len(), 3);
    }
}
