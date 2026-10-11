//! Day-by-day trends for the analytics view: activity, prompts, turn
//! lengths, cost, lines changed and resource use, bucketed in the viewer's
//! time zone.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use crate::{
    history::{Transition, lanes},
    model::{Activity, Session},
};

const DAY_MS: i64 = 86_400_000;
const MINUTE_MS: i64 = 60_000;
/// Upper bounds of the duration histogram bins; the last bin is open.
const BOUNDS_MS: [i64; 5] = [30_000, 120_000, 300_000, 900_000, 3_600_000];
/// A process sample stands for about this much time.
const PROC_SAMPLE_MS: i64 = 60_000;
const MEMORY_POINTS: i64 = 240;

#[derive(Debug, Serialize)]
pub struct Analytics {
    pub since: i64,
    pub now: i64,
    pub days: Vec<Day>,
    /// Prompts by weekday (Monday first) and local hour.
    pub heatmap: [[u32; 24]; 7],
    pub bounds_ms: [i64; 5],
    /// How long Claude worked per prompt, binned by `bounds_ms`.
    pub turns: Histogram,
    /// How long a session waited on you before you answered.
    pub responses: Histogram,
    pub models: Vec<ModelCost>,
    pub projects: Vec<ProjectUse>,
    /// Memory of all session process trees together.
    pub memory: Vec<(i64, u64)>,
    pub sessions: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Day {
    /// Local midnight, as a timestamp.
    pub start: i64,
    pub working_ms: i64,
    pub permission_ms: i64,
    pub your_turn_ms: i64,
    pub prompts: u32,
    pub cost_usd: f64,
    pub lines_added: i64,
    pub lines_removed: i64,
    pub cpu_secs: f64,
    pub peak_mem_bytes: u64,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Histogram {
    pub counts: [u32; 6],
    pub median_ms: Option<i64>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ModelCost {
    pub model: String,
    pub cost_usd: f64,
    pub sessions: usize,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ProjectUse {
    pub cwd: String,
    pub cpu_secs: f64,
    pub peak_mem_bytes: u64,
}

/// Local calendar math for a fixed offset (`tz_min` minutes east of UTC).
struct Clock {
    offset_ms: i64,
    first_day: i64,
}

impl Clock {
    fn day(&self, ts: i64) -> i64 {
        (ts + self.offset_ms).div_euclid(DAY_MS)
    }

    fn index(&self, ts: i64) -> usize {
        usize::try_from(self.day(ts) - self.first_day).unwrap_or(0)
    }

    fn day_start(&self, day: i64) -> i64 {
        day * DAY_MS - self.offset_ms
    }

    /// `(weekday, hour)`, Monday = 0.
    fn slot(&self, ts: i64) -> (usize, usize) {
        let local = ts + self.offset_ms;
        // NOTE: 1970-01-01 was a Thursday
        let weekday = (local.div_euclid(DAY_MS) + 3).rem_euclid(7);
        let hour = local.rem_euclid(DAY_MS) / 3_600_000;
        (weekday as usize, hour as usize)
    }
}

pub fn analytics(
    conn: &Connection,
    since: i64,
    now: i64,
    tz_min: i64,
    live: &[Session],
) -> Result<Analytics> {
    let clock = Clock {
        offset_ms: tz_min * MINUTE_MS,
        first_day: (since + tz_min * MINUTE_MS).div_euclid(DAY_MS),
    };
    let mut days: Vec<Day> = (clock.first_day..=clock.day(now))
        .map(|day| Day {
            start: clock.day_start(day),
            ..Day::default()
        })
        .collect();
    let mut out = Analytics {
        since,
        now,
        days: Vec::new(),
        heatmap: [[0; 24]; 7],
        bounds_ms: BOUNDS_MS,
        turns: Histogram::default(),
        responses: Histogram::default(),
        models: Vec::new(),
        projects: Vec::new(),
        memory: Vec::new(),
        sessions: 0,
    };

    let lanes = lanes(conn, since, now, live)?;
    out.sessions = lanes.len();
    for lane in &lanes {
        for &(from, to, activity) in &lane.segments {
            split_days(&clock, from, to, |i, ms| {
                let day = &mut days[i];
                match activity {
                    Activity::Working => day.working_ms += ms,
                    Activity::Permission => day.permission_ms += ms,
                    Activity::YourTurn => day.your_turn_ms += ms,
                    Activity::Idle | Activity::Gone => {}
                }
            });
        }
    }

    let mut turn_lengths = Vec::new();
    let mut open: HashMap<String, i64> = HashMap::new();
    let mut stmt = conn.prepare_cached(
        "SELECT session, ts, kind FROM events
            WHERE ts >= ?1 AND kind IN ('prompt', 'reply', 'error') ORDER BY ts, rowid",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (session, ts, kind) = row?;
        if kind == "prompt" {
            days[clock.index(ts)].prompts += 1;
            let (weekday, hour) = clock.slot(ts);
            out.heatmap[weekday][hour] += 1;
            open.insert(session, ts);
        } else if let Some(started) = open.remove(&session) {
            turn_lengths.push(ts - started);
        }
    }
    out.turns = histogram(turn_lengths);
    out.responses = histogram(response_times(conn, since)?);

    sample_deltas(conn, since, &clock, &mut days, &mut out)?;
    resources(conn, since, now, &clock, &mut days, &mut out)?;
    out.days = days;
    Ok(out)
}

/// Calls `add(day index, ms)` for each local day `from..to` overlaps.
fn split_days(clock: &Clock, from: i64, to: i64, mut add: impl FnMut(usize, i64)) {
    let mut at = from;
    while at < to {
        let end = clock.day_start(clock.day(at) + 1).min(to);
        add(clock.index(at), end - at);
        at = end;
    }
}

/// Waits ended by you: the time spent in `your_turn` or `permission` right
/// before a switch back to `working`.
fn response_times(conn: &Connection, since: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.session, e.ts, e.data FROM events e LEFT JOIN sessions x ON x.id = e.session
            WHERE e.kind = 'activity' AND e.ts >= ?1 AND coalesce(x.kind, '') != 'background'
            ORDER BY e.ts, e.rowid",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut entered: HashMap<String, i64> = HashMap::new();
    let mut out = Vec::new();
    for row in rows {
        let (session, ts, data) = row?;
        let Ok(t) = serde_json::from_str::<Transition>(&data) else {
            continue;
        };
        if let Some(start) = entered.insert(session, ts)
            && t.to == Activity::Working
            && matches!(t.from, Activity::YourTurn | Activity::Permission)
        {
            out.push(ts - start);
        }
    }
    Ok(out)
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
    since: i64,
    clock: &Clock,
    days: &mut [Day],
    out: &mut Analytics,
) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.session, s.ts, s.cost_usd, s.lines_added, s.lines_removed,
                coalesce(s.model, 'unknown'), coalesce(x.started_at, 0) >= ?1
            FROM samples s LEFT JOIN sessions x ON x.id = s.session
            WHERE s.ts >= ?1 ORDER BY s.session, s.ts",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<f64>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, bool>(6)?,
        ))
    })?;
    let mut prev: Option<(String, Totals)> = None;
    let mut models: BTreeMap<String, (f64, HashSet<String>)> = BTreeMap::new();
    for row in rows {
        let (session, ts, cost, added, removed, model, started_in_range) = row?;
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
        let day = &mut days[clock.index(ts)];
        let cost_delta = increase(now.cost, then.cost);
        day.cost_usd += cost_delta;
        day.lines_added += increase(now.added, then.added) as i64;
        day.lines_removed += increase(now.removed, then.removed) as i64;
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
    out.models = models
        .into_iter()
        .map(|(model, (cost_usd, sessions))| ModelCost {
            model,
            cost_usd,
            sessions: sessions.len(),
        })
        .collect();
    out.models.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
    Ok(())
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

fn resources(
    conn: &Connection,
    since: i64,
    now: i64,
    clock: &Clock,
    days: &mut [Day],
    out: &mut Analytics,
) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "SELECT p.ts, p.cpu_pct, p.mem_bytes, coalesce(x.cwd, '?')
            FROM proc_samples p LEFT JOIN sessions x ON x.id = p.session
            WHERE p.ts >= ?1 ORDER BY p.ts",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut per_minute: BTreeMap<i64, u64> = BTreeMap::new();
    let mut projects: BTreeMap<String, (f64, u64)> = BTreeMap::new();
    for row in rows {
        let (ts, cpu_pct, rss, cwd) = row?;
        let rss = u64::try_from(rss).unwrap_or(0);
        let cpu_secs = cpu_pct / 100.0 * (PROC_SAMPLE_MS / 1000) as f64;
        days[clock.index(ts)].cpu_secs += cpu_secs;
        *per_minute.entry(ts / MINUTE_MS).or_default() += rss;
        let project = projects.entry(cwd).or_default();
        project.0 += cpu_secs;
        project.1 = project.1.max(rss);
    }
    for (&minute, &rss) in &per_minute {
        let day = &mut days[clock.index(minute * MINUTE_MS)];
        day.peak_mem_bytes = day.peak_mem_bytes.max(rss);
    }
    let bucket = ((now - since) / MEMORY_POINTS).max(MINUTE_MS);
    let mut buckets: BTreeMap<i64, (u64, u64)> = BTreeMap::new();
    for (&minute, &rss) in &per_minute {
        let entry = buckets
            .entry(minute * MINUTE_MS / bucket * bucket)
            .or_default();
        entry.0 += rss;
        entry.1 += 1;
    }
    out.memory = buckets
        .into_iter()
        .map(|(ts, (sum, n))| (ts, sum / n.max(1)))
        .collect();
    out.projects = projects
        .into_iter()
        .map(|(cwd, (cpu_secs, peak_mem_bytes))| ProjectUse {
            cwd,
            cpu_secs,
            peak_mem_bytes,
        })
        .collect();
    out.projects
        .sort_by(|a, b| b.cpu_secs.total_cmp(&a.cpu_secs));
    Ok(())
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
            first_day: 0,
        };
        let ts = 1_791_761_400_000;
        assert_eq!(clock.slot(ts), (0, 1));
        assert_eq!(clock.day_start(clock.day(ts)), 1_791_756_000_000);
    }

    #[test]
    fn splits_segments_at_midnight() {
        let clock = Clock {
            offset_ms: 0,
            first_day: 0,
        };
        let mut parts = Vec::new();
        split_days(&clock, DAY_MS - 1000, DAY_MS + 500, |i, ms| {
            parts.push((i, ms))
        });
        assert_eq!(parts, [(0, 1000), (1, 500)]);
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
            "INSERT INTO sessions VALUES ('a', 'h', '/w', NULL, NULL, 'interactive', ?1, NULL)",
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

        let a = analytics(&conn, t0, t0 + 300_000, 0, &[]).unwrap();
        assert_eq!(a.days.len(), 1);
        let day = &a.days[0];
        assert_eq!(day.prompts, 2);
        assert_eq!(a.turns.counts, [0, 1, 0, 0, 0, 0]);
        assert_eq!(a.responses.counts, [0, 1, 0, 0, 0, 0]);
        assert!((day.cost_usd - 1.5).abs() < 1e-9);
        assert_eq!(day.lines_added, 25);
        assert_eq!((day.working_ms, day.your_turn_ms), (60_000, 119_000));
        assert_eq!(a.models[0].model, "Opus 5.5");
        assert!((a.models[0].cost_usd - 1.25).abs() < 1e-9);
        assert!((day.cpu_secs - 30.0).abs() < 1e-9);
        assert_eq!(a.projects[0].peak_mem_bytes, 1_048_576);
    }
}
