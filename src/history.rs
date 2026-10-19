//! Activity timeline, usage and cost over a time window, for the web UI charts.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, named_params, params};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{
    model::{Activity, Kind, Session},
    waits::{self, CLOSERS, Close, Edges, Wait},
    workspace::{IN_SCOPE, Scope},
};

#[derive(Debug, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct History {
    pub since: i64,
    pub until: i64,
    pub now: i64,
    pub bucket_ms: i64,
    pub lanes: Vec<Lane>,
    pub usage: Vec<UsagePoint>,
    pub cost_usd: f64,
    pub prompts: i64,
    /// Most used main-thread tools, by call count.
    pub tools: Vec<ToolCount>,
    /// Per machine, most working time first; one row under a host filter.
    pub hosts: Vec<HostTotals>,
}

#[derive(Debug, Default, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct HostTotals {
    pub host: String,
    pub sessions: u32,
    pub working_ms: i64,
    pub permission_ms: i64,
    /// Waits on you; parked time is apart.
    pub your_turn_ms: i64,
    pub parked_ms: i64,
    pub prompts: i64,
    pub cost_usd: f64,
}

#[derive(Debug, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct ToolCount {
    pub name: String,
    pub calls: i64,
    pub failed: i64,
}

#[derive(Debug, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Lane {
    pub id: String,
    pub label: String,
    pub host: String,
    pub cwd: String,
    pub live: bool,
    /// Started by Claude or `claude --bg`; its finished time is not a wait on you.
    pub background: bool,
    /// `[from, to, activity]`; time spent gone is not drawn. Your-turn
    /// stretches are drawn by how they ended (see `waits`).
    pub segments: Vec<(i64, i64, Activity)>,
    /// Your-turn waits overlapping the range.
    pub waits: Vec<Wait>,
}

#[derive(Debug, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct UsagePoint {
    pub ts: i64,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
}

/// Data of an `activity` event.
#[derive(Deserialize)]
pub struct Transition {
    pub from: Activity,
    pub to: Activity,
}

struct Row {
    id: String,
    host: String,
    cwd: String,
    name: Option<String>,
    title: Option<String>,
    kind: String,
    started_at: i64,
    ended_at: Option<i64>,
}

/// Sessions shorter than this that never worked (e.g. the `--resume` picker)
/// are left out.
const TRIVIAL_MS: i64 = 30_000;

pub fn history(conn: &Connection, scope: &Scope, live: &[Session]) -> Result<History> {
    let lanes = lanes(conn, scope, live)?;
    let bucket_ms = ((scope.until - scope.since) / 288).max(60_000);
    let mut hosts: BTreeMap<String, HostTotals> = BTreeMap::new();
    fn host<'a>(hosts: &'a mut BTreeMap<String, HostTotals>, name: &str) -> &'a mut HostTotals {
        hosts.entry(name.to_owned()).or_insert_with(|| HostTotals {
            host: name.to_owned(),
            ..HostTotals::default()
        })
    }
    for lane in &lanes {
        let h = host(&mut hosts, &lane.host);
        h.sessions += 1;
        for &(from, to, activity) in &lane.segments {
            match activity {
                Activity::Working => h.working_ms += to - from,
                Activity::Permission => h.permission_ms += to - from,
                Activity::YourTurn => h.your_turn_ms += to - from,
                Activity::Parked => h.parked_ms += to - from,
                Activity::Idle | Activity::Gone => {}
            }
        }
    }
    let mut prompts = 0;
    for (name, n) in prompts_by_host(conn, scope)? {
        prompts += n;
        host(&mut hosts, &name).prompts += n;
    }
    let mut cost_usd = 0.0;
    for (name, usd) in cost_by_host(conn, scope)? {
        cost_usd += usd;
        host(&mut hosts, &name).cost_usd += usd;
    }
    let mut hosts: Vec<HostTotals> = hosts.into_values().collect();
    hosts.sort_by(|a, b| {
        (b.working_ms + b.permission_ms + b.your_turn_ms)
            .cmp(&(a.working_ms + a.permission_ms + a.your_turn_ms))
            .then_with(|| a.host.cmp(&b.host))
    });
    Ok(History {
        since: scope.since,
        until: scope.until,
        now: scope.now,
        bucket_ms,
        lanes,
        usage: usage(conn, scope, bucket_ms)?,
        cost_usd,
        prompts,
        tools: tools(conn, scope)?,
        hosts,
    })
}

fn prompts_by_host(conn: &Connection, scope: &Scope) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT coalesce(x.host, e.host), count(*)
            FROM events e LEFT JOIN sessions x ON x.id = e.session
            WHERE e.kind = 'prompt' AND e.ts >= :since AND e.ts < :until AND {IN_SCOPE}
            GROUP BY 1"
    ))?;
    let rows = stmt.query_map(
        named_params! {":since": scope.since, ":until": scope.until, ":dirs": scope.dirs_param(), ":host": scope.host},
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One lane per session active in the scope, ordered by first activity.
pub fn lanes(conn: &Connection, scope: &Scope, live: &[Session]) -> Result<Vec<Lane>> {
    let live: HashMap<&str, &Session> = live.iter().map(|s| (s.id.as_str(), s)).collect();
    let (since, until) = (scope.since, scope.until);
    let mut transitions = transitions_in(conn, scope)?;
    let mut closes = closes_in(conn, scope)?;
    let mut lanes = Vec::new();
    for row in sessions_overlapping(conn, scope)? {
        if !scope.covers(&row.host, &row.cwd) {
            continue;
        }
        let current = live.get(row.id.as_str()).copied();
        let events = transitions.remove(&row.id).unwrap_or_default();
        let initial = match events.first() {
            Some((_, first)) => Some(first.from),
            None => match current {
                Some(s) if until >= scope.now => Some(s.activity),
                _ => match state_before(conn, &row.id, since)? {
                    Some(state) => Some(state),
                    None => state_after(conn, &row.id, until)?.or(current.map(|s| s.activity)),
                },
            },
        };
        let Some(initial) = initial else { continue };
        let end = match (current, row.ended_at) {
            (Some(_), _) => scope.now.min(until),
            (None, Some(ended)) => ended.min(until),
            (None, None) if state_after(conn, &row.id, until)?.is_some() => until,
            (None, None) => events.last().map_or(row.started_at, |(ts, _)| *ts),
        };
        let trivial = row.started_at >= since
            && row
                .ended_at
                .is_some_and(|ended| ended - row.started_at < TRIVIAL_MS)
            && initial != Activity::Working
            && !events.iter().any(|(_, t)| t.to == Activity::Working);
        let background = current.map_or(row.kind == "background", |s| s.kind == Kind::Background);
        let mut segments = segments(row.started_at.max(since), end, initial, &events);
        if segments.is_empty() || trivial {
            continue;
        }
        let waits = if background {
            finished_is_idle(&mut segments);
            Vec::new()
        } else {
            let edges = Edges {
                conn,
                session: &row.id,
                since,
                until,
                live: current.is_some(),
                now: scope.now,
            };
            let closes = closes.remove(&row.id).unwrap_or_default();
            waits::settle(&mut segments, &closes, &edges)?
        };
        let label = current
            .and_then(|s| s.title.clone().or_else(|| s.name.clone()))
            .or(row.title)
            .or(row.name)
            .unwrap_or_else(|| row.id.chars().take(8).collect());
        lanes.push(Lane {
            id: row.id,
            label,
            host: row.host,
            cwd: row.cwd,
            live: current.is_some(),
            background,
            segments,
            waits,
        });
    }
    lanes.sort_by_key(|lane| lane.segments[0].0);
    Ok(lanes)
}

fn tools(conn: &Connection, scope: &Scope) -> Result<Vec<ToolCount>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT json_extract(e.data, '$.name') AS tool, count(*),
                coalesce(sum(json_extract(e.data, '$.ok') = 0), 0)
            FROM events e LEFT JOIN sessions x ON x.id = e.session
            WHERE e.kind = 'tool' AND e.ts >= :since AND e.ts < :until AND {IN_SCOPE}
                AND tool IS NOT NULL
            GROUP BY tool ORDER BY 2 DESC, tool LIMIT 12"
    ))?;
    let rows = stmt.query_map(
        named_params! {":since": scope.since, ":until": scope.until, ":dirs": scope.dirs_param(), ":host": scope.host},
        |r| {
            Ok(ToolCount {
                name: r.get(0)?,
                calls: r.get(1)?,
                failed: r.get(2)?,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Splits `start..end` into spans of one activity; time spent gone is left out.
pub fn segments(
    start: i64,
    end: i64,
    initial: Activity,
    events: &[(i64, Transition)],
) -> Vec<(i64, i64, Activity)> {
    let mut out = Vec::new();
    let (mut from, mut state) = (start, initial);
    for (ts, transition) in events {
        if *ts > from && state != Activity::Gone {
            out.push((from, *ts, state));
        }
        from = from.max(*ts);
        state = transition.to;
    }
    if end > from && state != Activity::Gone {
        out.push((from, end, state));
    }
    out
}

/// Background sessions wait for their parent, not for you.
fn finished_is_idle(segments: &mut Vec<(i64, i64, Activity)>) {
    let mut merged: Vec<(i64, i64, Activity)> = Vec::with_capacity(segments.len());
    for &(from, to, activity) in segments.iter() {
        let activity = match activity {
            Activity::YourTurn | Activity::Parked => Activity::Idle,
            other => other,
        };
        match merged.last_mut() {
            Some(last) if last.2 == activity && last.1 == from => last.1 = to,
            _ => merged.push((from, to, activity)),
        }
    }
    *segments = merged;
}

fn sessions_overlapping(conn: &Connection, scope: &Scope) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, host, cwd, name, title, kind, started_at, ended_at FROM sessions
            WHERE started_at < ?2 AND (ended_at IS NULL OR ended_at >= ?1)",
    )?;
    let rows = stmt.query_map(params![scope.since, scope.until], |r| {
        Ok(Row {
            id: r.get(0)?,
            host: r.get(1)?,
            cwd: r.get(2)?,
            name: r.get(3)?,
            title: r.get(4)?,
            kind: r.get(5)?,
            started_at: r.get(6)?,
            ended_at: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn transitions_in(
    conn: &Connection,
    scope: &Scope,
) -> Result<HashMap<String, Vec<(i64, Transition)>>> {
    let mut stmt = conn.prepare_cached(
        "SELECT session, ts, data FROM events
            WHERE kind = 'activity' AND ts >= ?1 AND ts < ?2 ORDER BY ts",
    )?;
    let mut out: HashMap<String, Vec<(i64, Transition)>> = HashMap::new();
    let rows = stmt.query_map(params![scope.since, scope.until], |r| {
        Ok((r.get::<_, String>(0)?, r.get(1)?, r.get::<_, String>(2)?))
    })?;
    for row in rows {
        let (session, ts, data) = row?;
        if let Ok(transition) = serde_json::from_str(&data) {
            out.entry(session).or_default().push((ts, transition));
        }
    }
    Ok(out)
}

/// Events that can end a wait, per session, in order.
fn closes_in(conn: &Connection, scope: &Scope) -> Result<HashMap<String, Vec<Close>>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT session, kind, ts, json_extract(data, '$.ms') FROM events
            WHERE kind IN ({CLOSERS}) AND ts >= ?1 AND ts < ?2 ORDER BY ts, rowid"
    ))?;
    let mut out: HashMap<String, Vec<Close>> = HashMap::new();
    let rows = stmt.query_map(params![scope.since, scope.until], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<i64>>(3)?,
        ))
    })?;
    for row in rows {
        let (session, kind, ts, ms) = row?;
        out.entry(session)
            .or_default()
            .push(Close::new(&kind, ts, ms));
    }
    Ok(out)
}

fn state_before(conn: &Connection, session: &str, since: i64) -> Result<Option<Activity>> {
    let data: Option<String> = conn
        .prepare_cached(
            "SELECT data FROM events WHERE session = ?1 AND kind = 'activity' AND ts < ?2
                ORDER BY ts DESC LIMIT 1",
        )?
        .query_row(params![session, since], |r| r.get(0))
        .optional()?;
    Ok(data
        .and_then(|d| serde_json::from_str::<Transition>(&d).ok())
        .map(|t| t.to))
}

/// The state a session left at its first transition after `until`, for
/// sessions that did not change state inside the window.
fn state_after(conn: &Connection, session: &str, until: i64) -> Result<Option<Activity>> {
    let data: Option<String> = conn
        .prepare_cached(
            "SELECT data FROM events WHERE session = ?1 AND kind = 'activity' AND ts >= ?2
                ORDER BY ts LIMIT 1",
        )?
        .query_row(params![session, until], |r| r.get(0))
        .optional()?;
    Ok(data
        .and_then(|d| serde_json::from_str::<Transition>(&d).ok())
        .map(|t| t.from))
}

/// Rate limits are account-wide, so the workspace does not apply.
fn usage(conn: &Connection, scope: &Scope, bucket_ms: i64) -> Result<Vec<UsagePoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT (ts / ?3) * ?3 AS bucket, max(five_hour), max(seven_day) FROM samples
            WHERE ts >= ?1 AND ts < ?2 AND (five_hour IS NOT NULL OR seven_day IS NOT NULL)
            GROUP BY bucket ORDER BY bucket",
    )?;
    let rows = stmt.query_map(params![scope.since, scope.until, bucket_ms], |r| {
        Ok(UsagePoint {
            ts: r.get(0)?,
            five_hour: r.get(1)?,
            seven_day: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// API-equivalent cost accrued in the scope, per host; the statusline
/// reports a running total per session.
fn cost_by_host(conn: &Connection, scope: &Scope) -> Result<BTreeMap<String, f64>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT min(s.cost_usd), max(s.cost_usd), coalesce(x.started_at, 0),
                coalesce(x.host, s.host)
            FROM samples s LEFT JOIN sessions x ON x.id = s.session
            WHERE s.ts >= :since AND s.ts < :until AND s.cost_usd IS NOT NULL AND {IN_SCOPE}
            GROUP BY s.session"
    ))?;
    let rows = stmt.query_map(
        named_params! {":since": scope.since, ":until": scope.until, ":dirs": scope.dirs_param(), ":host": scope.host},
        |r| {
            Ok((
                r.get::<_, f64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        },
    )?;
    let mut totals = BTreeMap::new();
    for row in rows {
        let (min, max, started_at, host) = row?;
        *totals.entry(host).or_insert(0.0) += if started_at >= scope.since {
            max
        } else {
            max - min
        };
    }
    Ok(totals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SCHEMA;

    fn scope(since: i64, until: i64) -> Scope {
        Scope {
            since,
            until,
            now: until,
            dirs: None,
            host: None,
        }
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn session(conn: &Connection, id: &str, started_at: i64, ended_at: Option<i64>) {
        conn.execute(
            "INSERT INTO sessions VALUES (?1, 'h', '/w', NULL, ?2, 'interactive', ?3, ?4)",
            params![id, format!("title {id}"), started_at, ended_at],
        )
        .unwrap();
    }

    fn transition(conn: &Connection, id: &str, ts: i64, from: &str, to: &str) {
        conn.execute(
            "INSERT INTO events VALUES (?1, 'h', ?2, 'activity', ?3)",
            params![ts, id, format!(r#"{{"from":"{from}","to":"{to}"}}"#)],
        )
        .unwrap();
    }

    #[test]
    fn lanes_follow_transitions_and_clip_to_window() {
        let conn = db();
        session(&conn, "a", 0, Some(500));
        transition(&conn, "a", 50, "idle", "working");
        transition(&conn, "a", 200, "working", "your_turn");
        transition(&conn, "a", 500, "your_turn", "gone");
        // Ended before the window starts.
        session(&conn, "old", 0, Some(90));

        let h = history(&conn, &scope(100, 1_000), &[]).unwrap();
        assert_eq!(h.lanes.len(), 1);
        // Closed without another prompt: parked.
        assert_eq!(
            h.lanes[0].segments,
            vec![(100, 200, Activity::Working), (200, 500, Activity::Parked)]
        );
        assert_eq!(h.lanes[0].label, "title a");
    }

    #[test]
    fn quiet_sessions_show_in_past_windows() {
        let conn = db();
        session(&conn, "a", 0, None);
        transition(&conn, "a", 5_000, "idle", "working");
        let h = history(&conn, &scope(1_000, 2_000), &[]).unwrap();
        assert_eq!(h.lanes[0].segments, vec![(1_000, 2_000, Activity::Idle)]);
    }

    #[test]
    fn background_sessions_never_wait_on_you() {
        let conn = db();
        conn.execute(
            "INSERT INTO sessions VALUES ('bg', 'h', '/w', NULL, 't', 'background', 0, 500)",
            [],
        )
        .unwrap();
        transition(&conn, "bg", 0, "idle", "working");
        transition(&conn, "bg", 100, "working", "your_turn");
        transition(&conn, "bg", 500, "your_turn", "gone");
        // A 1 s session that never worked, like the resume picker.
        session(&conn, "stub", 600, Some(1600));
        transition(&conn, "stub", 1600, "idle", "gone");

        let h = history(&conn, &scope(0, 2_000), &[]).unwrap();
        assert_eq!(h.lanes.len(), 1);
        assert!(h.lanes[0].background);
        assert_eq!(
            h.lanes[0].segments,
            vec![(0, 100, Activity::Working), (100, 500, Activity::Idle)]
        );
    }

    #[test]
    fn usage_buckets_and_cost() {
        let conn = db();
        session(&conn, "new", 150, None);
        session(&conn, "old", 0, None);
        for (ts, session, cost, five) in [
            (120, "old", 1.0, 10.0),
            (180, "old", 1.5, 12.0),
            (160, "new", 0.2, 11.0),
            (190, "new", 0.4, 13.0),
        ] {
            conn.execute(
                "INSERT INTO samples (ts, host, session, cost_usd, five_hour, seven_day)
                    VALUES (?1, 'h', ?2, ?3, ?4, 30.0)",
                params![ts, session, cost, five],
            )
            .unwrap();
        }
        for (ts, name, ok) in [
            (150, "Bash", true),
            (160, "Bash", false),
            (170, "Read", true),
        ] {
            conn.execute(
                "INSERT INTO events VALUES (?1, 'h', 'new', 'tool', ?2)",
                params![ts, format!(r#"{{"name":"{name}","ok":{ok}}}"#)],
            )
            .unwrap();
        }
        let h = history(&conn, &scope(100, 100 + 288 * 60_000), &[]).unwrap();
        assert_eq!(
            h.tools,
            [
                ToolCount {
                    name: "Bash".into(),
                    calls: 2,
                    failed: 1
                },
                ToolCount {
                    name: "Read".into(),
                    calls: 1,
                    failed: 0
                },
            ]
        );
        assert!((h.cost_usd - 0.9).abs() < 1e-9);
        assert_eq!(h.usage.len(), 1);
        assert_eq!(h.usage[0].five_hour, Some(13.0));
        assert_eq!(h.hosts.len(), 1);
        assert_eq!(h.hosts[0].host, "h");
        assert!((h.hosts[0].cost_usd - 0.9).abs() < 1e-9);
    }

    #[test]
    fn totals_split_by_host() {
        let conn = db();
        session(&conn, "a", 0, None);
        conn.execute(
            "INSERT INTO sessions VALUES ('b', 'other', '/w', NULL, 'b', 'interactive', 0, NULL)",
            [],
        )
        .unwrap();
        transition(&conn, "a", 100, "idle", "working");
        conn.execute(
            "INSERT INTO events VALUES (100, 'other', 'b', 'activity', '{\"from\":\"idle\",\"to\":\"working\"}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events VALUES (100, 'other', 'b', 'prompt', '{\"text\":\"x\"}')",
            [],
        )
        .unwrap();
        let h = history(&conn, &scope(0, 1_000), &[]).unwrap();
        assert_eq!(h.lanes.len(), 2);
        assert_eq!(h.prompts, 1);
        let by: Vec<(&str, u32, i64)> = h
            .hosts
            .iter()
            .map(|x| (x.host.as_str(), x.sessions, x.prompts))
            .collect();
        assert_eq!(by, [("h", 1, 0), ("other", 1, 1)]);
        let one = history(
            &conn,
            &Scope {
                host: Some("other".into()),
                ..scope(0, 1_000)
            },
            &[],
        )
        .unwrap();
        assert_eq!(one.lanes.len(), 1);
        assert_eq!(one.lanes[0].host, "other");
        assert_eq!(one.hosts.len(), 1);
    }
}
