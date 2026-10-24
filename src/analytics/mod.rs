//! Trends for the analytics view: activity, prompts, turn lengths, cost,
//! lines changed and resource use, in day buckets (hour buckets for spans of
//! two days or less) of the viewer's time zone. Each section past the
//! overview has its own module.

mod context;
mod limits;
mod parallel;
mod permissions;
mod telemetry;
mod turns;
mod waiting;

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use rusqlite::{Connection, named_params};
use serde::Serialize;
use serde_json::Value;
use ts_rs::TS;

use crate::{
    history::lanes,
    model::{Activity, RateLimits, Session},
    waits,
    workspace::{IN_SCOPE, Scope, Workspaces},
};

pub use self::{
    context::ContextHealth, limits::Limits, parallel::Parallel, permissions::Permissions,
    telemetry::Telemetry, turns::Anatomy, waiting::Waiting,
};

const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;
/// Spans up to this long get hour buckets.
const HOURLY_UP_TO_MS: i64 = 2 * DAY_MS + HOUR_MS;
const MINUTE_MS: i64 = 60_000;
/// Upper bounds of the duration histogram bins; the last bin is open.
const BOUNDS_MS: [i64; 5] = [30_000, 120_000, 300_000, 900_000, 3_600_000];
/// A process sample stands for about this much time.
const PROC_SAMPLE_MS: i64 = 60_000;
const MEMORY_POINTS: i64 = 240;

#[derive(Debug, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Analytics {
    pub since: i64,
    pub until: i64,
    pub now: i64,
    /// `DAY` or `HOUR` in ms.
    pub bucket_ms: i64,
    pub buckets: Vec<Bucket>,
    /// Prompts by weekday (Monday first) and local hour.
    pub heatmap: [[u32; 24]; 7],
    pub bounds_ms: [i64; 5],
    /// How long Claude worked per prompt, binned by `bounds_ms`.
    pub turns: Histogram,
    /// How long a session waited on you before you answered: your-turn
    /// waits answered within the parking time, and permission prompts.
    pub responses: Histogram,
    pub models: Vec<ModelCost>,
    /// Per working directory, most working time first.
    pub projects: Vec<ProjectUse>,
    /// Per machine, most working time first; one row under a host filter.
    pub hosts: Vec<HostUse>,
    /// Memory of all session process trees together.
    pub memory: Vec<(i64, u64)>,
    pub sessions: usize,
    pub parallel: Parallel,
    /// Account-wide; neither the range nor the workspace applies.
    pub limits: Limits,
    /// What filled the turns: Claude, tools, permission waits.
    pub anatomy: Anatomy,
    pub context: ContextHealth,
    pub permissions: Permissions,
    pub waiting: Waiting,
    /// None until Claude Code has sent OpenTelemetry data at all.
    pub telemetry: Option<Telemetry>,
}

#[derive(Debug, Default, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Bucket {
    /// Local midnight (or the hour), as a timestamp.
    pub start: i64,
    pub working_ms: i64,
    pub permission_ms: i64,
    /// Waits on you; parked time is apart.
    pub your_turn_ms: i64,
    pub parked_ms: i64,
    /// Your-turn waits answered in the bucket, and their median.
    pub answers: u32,
    pub answer_p50_ms: Option<i64>,
    pub prompts: u32,
    pub cost_usd: f64,
    pub lines_added: i64,
    pub lines_removed: i64,
    pub cpu_secs: f64,
    pub peak_mem_bytes: u64,
    /// Finished turns started in the bucket, split by what filled them.
    pub turns: u32,
    pub turn_claude_ms: i64,
    pub turn_tools_ms: i64,
    pub turn_permission_ms: i64,
    pub compactions: u32,
    pub compactions_auto: u32,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_write: u64,
    pub api_requests: u32,
    pub api_errors: u32,
    pub commits: u32,
    pub pull_requests: u32,
    pub active_user_secs: f64,
    pub active_cli_secs: f64,
}

#[derive(Debug, Default, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Histogram {
    pub counts: [u32; 6],
    pub median_ms: Option<i64>,
}

#[derive(Debug, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct ModelCost {
    pub model: String,
    pub cost_usd: f64,
    pub sessions: usize,
}

/// Totals of one project or one host.
#[derive(Debug, Default, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Tally {
    pub sessions: u32,
    pub working_ms: i64,
    pub permission_ms: i64,
    /// Waits on you; parked time is apart.
    pub your_turn_ms: i64,
    pub parked_ms: i64,
    pub prompts: u32,
    pub cost_usd: f64,
    pub lines_added: i64,
    pub lines_removed: i64,
    pub tools: u32,
    pub tool_failures: u32,
    pub compactions: u32,
    pub tokens: u64,
    pub cpu_secs: f64,
    pub peak_mem_bytes: u64,
}

#[derive(Debug, Default, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct ProjectUse {
    pub cwd: String,
    #[serde(flatten)]
    pub tally: Tally,
}

#[derive(Debug, Default, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct HostUse {
    pub host: String,
    #[serde(flatten)]
    pub tally: Tally,
    pub api_requests: u32,
    /// Your-turn waits answered in the range on this host, and their median.
    pub answers: u32,
    pub answer_p50_ms: Option<i64>,
}

/// The per-project and per-host totals, filled side by side.
#[derive(Default)]
pub(crate) struct Uses {
    projects: BTreeMap<String, ProjectUse>,
    hosts: BTreeMap<String, HostUse>,
}

impl Uses {
    /// Applies `add` to the project's and the host's tallies.
    fn each(&mut self, cwd: &str, host: &str, mut add: impl FnMut(&mut Tally)) {
        add(&mut self.project(cwd).tally);
        add(&mut self.host(host).tally);
    }

    fn project(&mut self, cwd: &str) -> &mut ProjectUse {
        self.projects
            .entry(cwd.to_owned())
            .or_insert_with(|| ProjectUse {
                cwd: cwd.to_owned(),
                ..ProjectUse::default()
            })
    }

    fn host(&mut self, host: &str) -> &mut HostUse {
        self.hosts
            .entry(host.to_owned())
            .or_insert_with(|| HostUse {
                host: host.to_owned(),
                ..HostUse::default()
            })
    }
}

/// One recorded event of a session.
pub(crate) struct Ev {
    pub ts: i64,
    pub kind: String,
    pub data: Value,
}

/// A session's events in the scope, oldest first.
pub(crate) struct SessionEvents {
    pub id: String,
    pub host: String,
    pub cwd: String,
    pub label: String,
    pub events: Vec<Ev>,
}

/// Percentile of sorted values, nearest rank.
fn percentile(sorted: &[i64], pct: usize) -> Option<i64> {
    let last = sorted.len().checked_sub(1)?;
    sorted.get((sorted.len() * pct / 100).min(last)).copied()
}

/// Local calendar math for a fixed offset (`tz_min` minutes east of UTC).
struct Clock {
    offset_ms: i64,
    bucket_ms: i64,
    first: i64,
    count: usize,
}

impl Clock {
    fn bucket(&self, ts: i64) -> i64 {
        (ts + self.offset_ms).div_euclid(self.bucket_ms)
    }

    fn index(&self, ts: i64) -> usize {
        usize::try_from(self.bucket(ts) - self.first)
            .unwrap_or(0)
            .min(self.count.saturating_sub(1))
    }

    fn start(&self, bucket: i64) -> i64 {
        bucket * self.bucket_ms - self.offset_ms
    }

    /// `(weekday, hour)`, Monday = 0.
    fn slot(&self, ts: i64) -> (usize, usize) {
        let local = ts + self.offset_ms;
        // NOTE: 1970-01-01 was a Thursday
        let weekday = (local.div_euclid(DAY_MS) + 3).rem_euclid(7);
        let hour = local.rem_euclid(DAY_MS) / HOUR_MS;
        (weekday as usize, hour as usize)
    }
}

/// Everything the analytics view shows for `scope`. `limits` is the live
/// rate-limit reading; `workspaces` names the shares of the week's usage.
pub fn analytics(
    conn: &Connection,
    scope: &Scope,
    tz_min: i64,
    live: &[Session],
    limits: Option<&RateLimits>,
    workspaces: &Workspaces,
) -> Result<Analytics> {
    let bucket_ms = if scope.until - scope.since <= HOURLY_UP_TO_MS {
        HOUR_MS
    } else {
        DAY_MS
    };
    let offset_ms = tz_min * MINUTE_MS;
    let first = (scope.since + offset_ms).div_euclid(bucket_ms);
    let last = (scope.until - 1 + offset_ms).div_euclid(bucket_ms);
    let mut clock = Clock {
        offset_ms,
        bucket_ms,
        first,
        count: 0,
    };
    let mut buckets: Vec<Bucket> = (first..=last)
        .map(|b| Bucket {
            start: clock.start(b),
            ..Bucket::default()
        })
        .collect();
    clock.count = buckets.len();
    let mut uses = Uses::default();

    let lanes = lanes(conn, scope, live)?;
    for lane in &lanes {
        uses.each(&lane.cwd, &lane.host, |t| t.sessions += 1);
        for &(from, to, activity) in &lane.segments {
            uses.each(&lane.cwd, &lane.host, |t| match activity {
                Activity::Working => t.working_ms += to - from,
                Activity::Permission => t.permission_ms += to - from,
                Activity::YourTurn => t.your_turn_ms += to - from,
                Activity::Parked => t.parked_ms += to - from,
                Activity::Idle | Activity::Gone => {}
            });
            split_buckets(&clock, from, to, |i, ms| {
                let bucket = &mut buckets[i];
                match activity {
                    Activity::Working => bucket.working_ms += ms,
                    Activity::Permission => bucket.permission_ms += ms,
                    Activity::YourTurn => bucket.your_turn_ms += ms,
                    Activity::Parked => bucket.parked_ms += ms,
                    Activity::Idle | Activity::Gone => {}
                }
            });
        }
    }
    let (waiting, responses) =
        waiting::waiting(&lanes, (scope.since, scope.until), &clock, &mut buckets);
    let mut answers_by_host: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
    for lane in lanes.iter().filter(|l| !l.background) {
        for w in &lane.waits {
            if w.outcome == waits::Outcome::Answered && (scope.since..scope.until).contains(&w.end)
            {
                answers_by_host
                    .entry(&lane.host)
                    .or_default()
                    .push(w.end - w.start);
            }
        }
    }
    for (host, mut ms) in answers_by_host {
        ms.sort_unstable();
        let h = uses.host(host);
        h.answers = ms.len() as u32;
        h.answer_p50_ms = percentile(&ms, 50);
    }

    let mut heatmap = [[0; 24]; 7];
    let mut turn_lengths = Vec::new();
    let mut open: HashMap<String, i64> = HashMap::new();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT e.session, e.ts, e.kind FROM events e LEFT JOIN sessions x ON x.id = e.session
            WHERE e.ts >= :since AND e.ts < :until AND {IN_SCOPE}
                AND e.kind IN ('prompt', 'reply', 'error')
            ORDER BY e.ts, e.rowid"
    ))?;
    let rows = stmt.query_map(
        named_params! {":since": scope.since, ":until": scope.until, ":dirs": scope.dirs_param(), ":host": scope.host},
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    for row in rows {
        let (session, ts, kind) = row?;
        if kind == "prompt" {
            buckets[clock.index(ts)].prompts += 1;
            let (weekday, hour) = clock.slot(ts);
            heatmap[weekday][hour] += 1;
            open.insert(session, ts);
        } else if let Some(started) = open.remove(&session) {
            turn_lengths.push(ts - started);
        }
    }

    let events = session_events(conn, scope, live)?;
    for session in &events {
        for ev in &session.events {
            uses.each(&session.cwd, &session.host, |t| match ev.kind.as_str() {
                "prompt" => t.prompts += 1,
                "compact" => t.compactions += 1,
                "tool" => {
                    t.tools += 1;
                    if ev.data["ok"] == false {
                        t.tool_failures += 1;
                    }
                }
                _ => {}
            });
        }
    }
    let anatomy = turns::anatomy(&events, &clock, &mut buckets);
    let context = context::context(conn, scope, &events, &clock, &mut buckets)?;
    let telemetry = telemetry::telemetry(conn, scope, &clock, &mut buckets, &mut uses)?;
    let permissions = permissions::permissions(&events);
    let models = sample_deltas(conn, scope, &clock, &mut buckets, &mut uses)?;
    let memory = resources(conn, scope, &clock, &mut buckets, &mut uses)?;

    let busiest = |a: &Tally, b: &Tally| {
        (b.working_ms + b.permission_ms + b.your_turn_ms)
            .cmp(&(a.working_ms + a.permission_ms + a.your_turn_ms))
            .then_with(|| b.cost_usd.total_cmp(&a.cost_usd))
    };
    let mut projects: Vec<ProjectUse> = uses.projects.into_values().collect();
    projects.sort_by(|a, b| busiest(&a.tally, &b.tally));
    let mut hosts: Vec<HostUse> = uses.hosts.into_values().collect();
    hosts.sort_by(|a, b| busiest(&a.tally, &b.tally));
    Ok(Analytics {
        since: scope.since,
        until: scope.until,
        now: scope.now,
        bucket_ms,
        buckets,
        heatmap,
        bounds_ms: BOUNDS_MS,
        turns: histogram(turn_lengths),
        responses: histogram(responses),
        models,
        projects,
        hosts,
        memory,
        sessions: lanes.len(),
        parallel: parallel::parallel(&lanes, scope.since, scope.until),
        limits: limits::limits(conn, scope, limits, workspaces)?,
        anatomy,
        context,
        permissions,
        waiting,
        telemetry,
    })
}

/// The events the sections read, grouped by session; labels prefer the
/// live title.
fn session_events(
    conn: &Connection,
    scope: &Scope,
    live: &[Session],
) -> Result<Vec<SessionEvents>> {
    let titles: HashMap<&str, &str> = live
        .iter()
        .filter_map(|s| Some((s.id.as_str(), s.title.as_deref().or(s.name.as_deref())?)))
        .collect();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT e.session, coalesce(x.cwd, '?'), coalesce(x.title, x.name), e.ts, e.kind, e.data,
                coalesce(x.host, e.host)
            FROM events e LEFT JOIN sessions x ON x.id = e.session
            WHERE e.ts >= :since AND e.ts < :until AND {IN_SCOPE}
                AND e.kind IN ('prompt', 'task_notification', 'agent_message', 'reply', 'error',
                    'tool', 'permission', 'activity', 'compact', 'denied')
            ORDER BY e.session, e.ts, e.rowid"
    ))?;
    let rows = stmt.query_map(
        named_params! {":since": scope.since, ":until": scope.until, ":dirs": scope.dirs_param(), ":host": scope.host},
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        },
    )?;
    let mut out: Vec<SessionEvents> = Vec::new();
    for row in rows {
        let (id, cwd, title, ts, kind, data, host) = row?;
        let ev = Ev {
            ts,
            kind,
            data: serde_json::from_str(&data).unwrap_or(Value::Null),
        };
        match out.last_mut() {
            Some(last) if last.id == id => last.events.push(ev),
            _ => out.push(SessionEvents {
                label: titles
                    .get(id.as_str())
                    .map(|t| (*t).to_owned())
                    .or(title)
                    .unwrap_or_else(|| id.chars().take(8).collect()),
                id,
                host,
                cwd,
                events: vec![ev],
            }),
        }
    }
    Ok(out)
}

/// Calls `add(bucket index, ms)` for each bucket `from..to` overlaps.
fn split_buckets(clock: &Clock, from: i64, to: i64, mut add: impl FnMut(usize, i64)) {
    let mut at = from;
    while at < to {
        let end = clock.start(clock.bucket(at) + 1).min(to);
        add(clock.index(at), end - at);
        at = end;
    }
}

fn histogram(mut values: Vec<i64>) -> Histogram {
    let mut counts = [0; 6];
    for v in &values {
        counts[BOUNDS_MS.iter().take_while(|b| v >= b).count()] += 1;
    }
    values.sort_unstable();
    Histogram {
        counts,
        median_ms: values.get(values.len() / 2).copied(),
    }
}

/// Cost and lines from the statusline's running totals: each sample adds
/// its increase over the session's previous one.
fn sample_deltas(
    conn: &Connection,
    scope: &Scope,
    clock: &Clock,
    buckets: &mut [Bucket],
    uses: &mut Uses,
) -> Result<Vec<ModelCost>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT s.session, s.ts, s.cost_usd, s.lines_added, s.lines_removed,
                coalesce(s.model, 'unknown'), coalesce(x.started_at, 0) >= :since,
                coalesce(x.cwd, '?'), coalesce(x.host, s.host)
            FROM samples s LEFT JOIN sessions x ON x.id = s.session
            WHERE s.ts >= :since AND s.ts < :until AND {IN_SCOPE}
            ORDER BY s.session, s.ts"
    ))?;
    let params = named_params! {":since": scope.since, ":until": scope.until, ":dirs": scope.dirs_param(), ":host": scope.host};
    let rows = stmt.query_map(params, |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<f64>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, bool>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, String>(8)?,
        ))
    })?;
    let mut prev: Option<(String, Totals)> = None;
    let mut models: BTreeMap<String, (f64, HashSet<String>)> = BTreeMap::new();
    for row in rows {
        let (session, ts, cost, added, removed, model, started_in_range, cwd, host) = row?;
        let now = Totals {
            cost,
            added: added.map(|n| n as f64),
            removed: removed.map(|n| n as f64),
        };
        let then = match &prev {
            Some((id, totals)) if *id == session => *totals,
            // NOTE: a session started in range counts from zero
            _ if started_in_range => Totals {
                cost: Some(0.0),
                added: Some(0.0),
                removed: Some(0.0),
            },
            _ => now,
        };
        let bucket = &mut buckets[clock.index(ts)];
        let cost_delta = increase(now.cost, then.cost);
        let added_delta = increase(now.added, then.added) as i64;
        let removed_delta = increase(now.removed, then.removed) as i64;
        bucket.cost_usd += cost_delta;
        bucket.lines_added += added_delta;
        bucket.lines_removed += removed_delta;
        uses.each(&cwd, &host, |t| {
            t.cost_usd += cost_delta;
            t.lines_added += added_delta;
            t.lines_removed += removed_delta;
        });
        let entry = models.entry(model).or_default();
        entry.0 += cost_delta;
        entry.1.insert(session.clone());
        prev = Some((
            session,
            Totals {
                cost: now.cost.or(then.cost),
                added: now.added.or(then.added),
                removed: now.removed.or(then.removed),
            },
        ));
    }
    let mut models: Vec<ModelCost> = models
        .into_iter()
        .map(|(model, (cost_usd, sessions))| ModelCost {
            model,
            cost_usd,
            sessions: sessions.len(),
        })
        .collect();
    models.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
    Ok(models)
}

/// Statusline running totals of one session.
#[derive(Clone, Copy)]
struct Totals {
    cost: Option<f64>,
    added: Option<f64>,
    removed: Option<f64>,
}

fn increase(now: Option<f64>, then: Option<f64>) -> f64 {
    match (now, then) {
        (Some(now), Some(then)) => (now - then).max(0.0),
        _ => 0.0,
    }
}

/// CPU and memory per bucket and project; returns memory over time.
fn resources(
    conn: &Connection,
    scope: &Scope,
    clock: &Clock,
    buckets: &mut [Bucket],
    uses: &mut Uses,
) -> Result<Vec<(i64, u64)>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT p.ts, p.cpu_pct, p.mem_bytes, coalesce(x.cwd, '?'), coalesce(x.host, p.host)
            FROM proc_samples p LEFT JOIN sessions x ON x.id = p.session
            WHERE p.ts >= :since AND p.ts < :until AND {IN_SCOPE} ORDER BY p.ts"
    ))?;
    let params = named_params! {":since": scope.since, ":until": scope.until, ":dirs": scope.dirs_param(), ":host": scope.host};
    let rows = stmt.query_map(params, |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut per_minute: BTreeMap<i64, u64> = BTreeMap::new();
    for row in rows {
        let (ts, cpu_pct, rss, cwd, host) = row?;
        let rss = u64::try_from(rss).unwrap_or(0);
        let cpu_secs = cpu_pct / 100.0 * (PROC_SAMPLE_MS / 1000) as f64;
        buckets[clock.index(ts)].cpu_secs += cpu_secs;
        *per_minute.entry(ts / MINUTE_MS).or_default() += rss;
        uses.each(&cwd, &host, |t| {
            t.cpu_secs += cpu_secs;
            t.peak_mem_bytes = t.peak_mem_bytes.max(rss);
        });
    }
    for (&minute, &rss) in &per_minute {
        let bucket = &mut buckets[clock.index(minute * MINUTE_MS)];
        bucket.peak_mem_bytes = bucket.peak_mem_bytes.max(rss);
    }
    let bucket = ((scope.until - scope.since) / MEMORY_POINTS).max(MINUTE_MS);
    let mut buckets: BTreeMap<i64, (u64, u64)> = BTreeMap::new();
    for (&minute, &rss) in &per_minute {
        let entry = buckets
            .entry(minute * MINUTE_MS / bucket * bucket)
            .or_default();
        entry.0 += rss;
        entry.1 += 1;
    }
    Ok(buckets
        .into_iter()
        .map(|(ts, (sum, n))| (ts, sum / n.max(1)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SCHEMA;
    use rusqlite::params;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn event(conn: &Connection, session: &str, ts: i64, kind: &str, data: &str) {
        conn.execute(
            "INSERT INTO events VALUES (?1, 'h', ?2, ?3, ?4)",
            params![ts, session, kind, data],
        )
        .unwrap();
    }

    #[test]
    fn clock_buckets_in_local_time() {
        // UTC+2: 2026-10-11T23:30Z is Monday 01:30 local.
        let clock = Clock {
            offset_ms: 120 * MINUTE_MS,
            bucket_ms: DAY_MS,
            first: 0,
            count: usize::MAX,
        };
        let ts = 1_791_761_400_000;
        assert_eq!(clock.slot(ts), (0, 1));
        assert_eq!(clock.start(clock.bucket(ts)), 1_791_756_000_000);
    }

    #[test]
    fn splits_segments_at_midnight() {
        let clock = Clock {
            offset_ms: 0,
            bucket_ms: DAY_MS,
            first: 0,
            count: 2,
        };
        let mut parts = Vec::new();
        split_buckets(&clock, DAY_MS - 1000, DAY_MS + 500, |i, ms| {
            parts.push((i, ms))
        });
        assert_eq!(parts, [(0, 1000), (1, 500)]);
    }

    #[test]
    fn workspace_scope_and_day_buckets() {
        let conn = db();
        let t0 = 20 * DAY_MS;
        for (id, cwd) in [("a", "/ws/one/repo"), ("b", "/ws/two")] {
            conn.execute(
                "INSERT INTO sessions (id, host, cwd, name, title, kind, started_at, ended_at) VALUES (?1, 'h', ?2, NULL, NULL, 'interactive', ?3, NULL)",
                params![id, cwd, t0],
            )
            .unwrap();
            event(&conn, id, t0 + 1000, "prompt", r#"{"text":"x"}"#);
        }
        event(&conn, "a", t0 + 3 * DAY_MS, "prompt", r#"{"text":"x"}"#);
        let scope = |dir: Option<&str>| Scope {
            since: t0,
            until: t0 + 4 * DAY_MS,
            now: t0 + 4 * DAY_MS,
            dirs: dir.map(|d| vec![d.to_owned()]),
            host: None,
        };
        let all = analytics(&conn, &scope(None), 0, &[], None, &Workspaces::default()).unwrap();
        assert_eq!((all.bucket_ms, all.buckets.len()), (DAY_MS, 4));
        let prompts: Vec<u32> = all.buckets.iter().map(|b| b.prompts).collect();
        assert_eq!(prompts, [2, 0, 0, 1]);
        let one = analytics(
            &conn,
            &scope(Some("/ws/one")),
            0,
            &[],
            None,
            &Workspaces::default(),
        )
        .unwrap();
        let prompts: Vec<u32> = one.buckets.iter().map(|b| b.prompts).collect();
        assert_eq!(prompts, [1, 0, 0, 1]);
    }

    #[test]
    fn histogram_bins_and_median() {
        let h = histogram(vec![10_000, 60_000, 60_000, 4_000_000]);
        assert_eq!(h.counts, [1, 2, 0, 0, 0, 1]);
        assert_eq!(h.median_ms, Some(60_000));
    }

    #[test]
    fn turns_responses_cost_and_lines() {
        let conn = db();
        let t0 = 10 * DAY_MS;
        conn.execute(
            "INSERT INTO sessions (id, host, cwd, name, title, kind, started_at, ended_at) VALUES ('a', 'h', '/w', NULL, NULL, 'interactive', ?1, NULL)",
            [t0],
        )
        .unwrap();
        event(
            &conn,
            "a",
            t0 + 1000,
            "activity",
            r#"{"from":"idle","to":"working"}"#,
        );
        event(&conn, "a", t0 + 1000, "prompt", r#"{"text":"go"}"#);
        event(&conn, "a", t0 + 61_000, "reply", r#"{"text":"ok"}"#);
        event(
            &conn,
            "a",
            t0 + 61_000,
            "activity",
            r#"{"from":"working","to":"your_turn"}"#,
        );
        event(
            &conn,
            "a",
            t0 + 180_000,
            "activity",
            r#"{"from":"your_turn","to":"working"}"#,
        );
        event(&conn, "a", t0 + 180_000, "prompt", r#"{"text":"again"}"#);
        for (ts, cost, added, model) in [
            (t0 + 30_000, 0.5, 10, "Opus 5.5"),
            (t0 + 90_000, 1.25, 25, "Opus 5.5"),
            (t0 + 200_000, 1.5, 20, "Sonnet 5.5"),
        ] {
            conn.execute(
                "INSERT INTO samples (ts, host, session, cost_usd, model, lines_added, lines_removed)
                    VALUES (?1, 'h', 'a', ?2, ?3, ?4, 0)",
                params![ts, cost, model, added],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO proc_samples VALUES (?1, 'h', 'a', 50.0, 1048576, 3)",
            [t0 + 60_000],
        )
        .unwrap();

        let scope = Scope {
            since: t0,
            until: t0 + 300_000,
            now: t0 + 300_000,
            dirs: None,
            host: None,
        };
        let a = analytics(&conn, &scope, 0, &[], None, &Workspaces::default()).unwrap();
        assert_eq!((a.bucket_ms, a.buckets.len()), (HOUR_MS, 1));
        let day = &a.buckets[0];
        assert_eq!(day.prompts, 2);
        assert_eq!(a.turns.counts, [0, 1, 0, 0, 0, 0]);
        assert_eq!(a.responses.counts, [0, 1, 0, 0, 0, 0]);
        assert!((day.cost_usd - 1.5).abs() < 1e-9);
        assert_eq!(day.lines_added, 25);
        assert_eq!((day.working_ms, day.your_turn_ms), (60_000, 119_000));
        assert_eq!(a.models[0].model, "Opus 5.5");
        assert!((a.models[0].cost_usd - 1.25).abs() < 1e-9);
        assert!((day.cpu_secs - 30.0).abs() < 1e-9);
        assert_eq!(a.projects[0].tally.peak_mem_bytes, 1_048_576);
        assert_eq!(a.hosts.len(), 1);
        let host = &a.hosts[0];
        assert_eq!(
            (host.host.as_str(), host.tally.sessions, host.tally.prompts),
            ("h", 1, 2)
        );
        assert!((host.tally.cost_usd - 1.5).abs() < 1e-9);
        assert_eq!((host.answers, host.answer_p50_ms), (1, Some(119_000)));
        let json = serde_json::to_value(host).unwrap();
        assert_eq!(json["working_ms"], 60_000, "{json}");
    }
}
