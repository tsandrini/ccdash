//! Activity timeline, usage and cost over a time window, for the web UI charts.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::model::{Activity, Session};

#[derive(Debug, Serialize)]
pub struct History {
    pub since: i64,
    pub now: i64,
    pub bucket_ms: i64,
    pub lanes: Vec<Lane>,
    pub usage: Vec<UsagePoint>,
    pub cost_usd: f64,
    pub prompts: i64,
    /// Most used main-thread tools, by call count.
    pub tools: Vec<ToolCount>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ToolCount {
    pub name: String,
    pub calls: i64,
    pub failed: i64,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Lane {
    pub id: String,
    pub label: String,
    pub cwd: String,
    pub live: bool,
    /// `[from, to, activity]`; time spent gone is not drawn.
    pub segments: Vec<(i64, i64, Activity)>,
}

#[derive(Debug, PartialEq, Serialize)]
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
    cwd: String,
    name: Option<String>,
    title: Option<String>,
    started_at: i64,
    ended_at: Option<i64>,
}

pub fn history(conn: &Connection, since: i64, now: i64, live: &[Session]) -> Result<History> {
    let live: HashMap<&str, &Session> = live.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut transitions = transitions_since(conn, since)?;
    let mut lanes = Vec::new();
    for row in sessions_overlapping(conn, since)? {
        let current = live.get(row.id.as_str()).copied();
        let events = transitions.remove(&row.id).unwrap_or_default();
        let initial = match events.first() {
            Some((_, first)) => Some(first.from),
            None => match current {
                Some(s) => Some(s.activity),
                None => state_before(conn, &row.id, since)?,
            },
        };
        let Some(initial) = initial else { continue };
        let end = match (current, row.ended_at) {
            (Some(_), _) => now,
            (None, Some(ended)) => ended,
            (None, None) => events.last().map_or(row.started_at, |(ts, _)| *ts),
        };
        let segments = segments(row.started_at.max(since), end, initial, &events);
        if segments.is_empty() {
            continue;
        }
        let label = current
            .and_then(|s| s.title.clone().or_else(|| s.name.clone()))
            .or(row.title)
            .or(row.name)
            .unwrap_or_else(|| row.id.chars().take(8).collect());
        lanes.push(Lane {
            id: row.id,
            label,
            cwd: row.cwd,
            live: current.is_some(),
            segments,
        });
    }
    lanes.sort_by_key(|lane| lane.segments[0].0);
    let bucket_ms = ((now - since) / 288).max(60_000);
    Ok(History {
        since,
        now,
        bucket_ms,
        lanes,
        usage: usage(conn, since, bucket_ms)?,
        cost_usd: cost(conn, since)?,
        prompts: conn.query_row(
            "SELECT count(*) FROM events WHERE kind = 'prompt' AND ts >= ?1",
            [since],
            |r| r.get(0),
        )?,
        tools: tools(conn, since)?,
    })
}

fn tools(conn: &Connection, since: i64) -> Result<Vec<ToolCount>> {
    let mut stmt = conn.prepare_cached(
        "SELECT json_extract(data, '$.name') AS name, count(*),
                coalesce(sum(json_extract(data, '$.ok') = 0), 0)
            FROM events WHERE kind = 'tool' AND ts >= ?1 AND name IS NOT NULL
            GROUP BY name ORDER BY 2 DESC, name LIMIT 12",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok(ToolCount {
            name: r.get(0)?,
            calls: r.get(1)?,
            failed: r.get(2)?,
        })
    })?;
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

fn sessions_overlapping(conn: &Connection, since: i64) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, cwd, name, title, started_at, ended_at FROM sessions
            WHERE ended_at IS NULL OR ended_at >= ?1",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok(Row {
            id: r.get(0)?,
            cwd: r.get(1)?,
            name: r.get(2)?,
            title: r.get(3)?,
            started_at: r.get(4)?,
            ended_at: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn transitions_since(
    conn: &Connection,
    since: i64,
) -> Result<HashMap<String, Vec<(i64, Transition)>>> {
    let mut stmt = conn.prepare_cached(
        "SELECT session, ts, data FROM events WHERE kind = 'activity' AND ts >= ?1 ORDER BY ts",
    )?;
    let mut out: HashMap<String, Vec<(i64, Transition)>> = HashMap::new();
    let rows = stmt.query_map([since], |r| {
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

fn usage(conn: &Connection, since: i64, bucket_ms: i64) -> Result<Vec<UsagePoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT (ts / ?2) * ?2 AS bucket, max(five_hour), max(seven_day) FROM samples
            WHERE ts >= ?1 AND (five_hour IS NOT NULL OR seven_day IS NOT NULL)
            GROUP BY bucket ORDER BY bucket",
    )?;
    let rows = stmt.query_map(params![since, bucket_ms], |r| {
        Ok(UsagePoint {
            ts: r.get(0)?,
            five_hour: r.get(1)?,
            seven_day: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// API-equivalent cost accrued since `since`; the statusline reports a
/// running total per session.
fn cost(conn: &Connection, since: i64) -> Result<f64> {
    let mut stmt = conn.prepare_cached(
        "SELECT min(s.cost_usd), max(s.cost_usd), coalesce(x.started_at, 0)
            FROM samples s LEFT JOIN sessions x ON x.id = s.session
            WHERE s.ts >= ?1 AND s.cost_usd IS NOT NULL
            GROUP BY s.session",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            r.get::<_, f64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut total = 0.0;
    for row in rows {
        let (min, max, started_at) = row?;
        total += if started_at >= since { max } else { max - min };
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SCHEMA;

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

        let h = history(&conn, 100, 1_000, &[]).unwrap();
        assert_eq!(h.lanes.len(), 1);
        assert_eq!(
            h.lanes[0].segments,
            vec![
                (100, 200, Activity::Working),
                (200, 500, Activity::YourTurn)
            ]
        );
        assert_eq!(h.lanes[0].label, "title a");
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
                "INSERT INTO samples VALUES (?1, 'h', ?2, NULL, ?3, ?4, 30.0)",
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
        let h = history(&conn, 100, 100 + 288 * 60_000, &[]).unwrap();
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
    }
}
