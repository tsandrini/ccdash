//! Where the 5-hour and 7-day limits, and the weekly limits of one model
//! (Fable's, say), are heading. Rate limits are account-wide, so only the
//! 5-hour window peaks follow the selected range.

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::{
    model::{Limit, RateLimits},
    workspace::{Scope, Workspaces},
};

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const FIVE_HOURS_MS: i64 = 5 * HOUR_MS;
const SEVEN_DAYS_MS: i64 = 7 * 24 * HOUR_MS;
/// How far back the recent pace looks, per window.
const RECENT_FIVE_HOUR_MS: i64 = HOUR_MS;
const RECENT_SEVEN_DAY_MS: i64 = 24 * HOUR_MS;
/// A drop of this many points between readings starts a new 5-hour window.
const RESET_DROP: f64 = 3.0;

#[derive(Debug, Default, Serialize)]
pub struct Limits {
    pub five_hour: Option<Forecast>,
    pub seven_day: Option<Forecast>,
    /// 7-day readings since the current window opened, hourly peaks.
    pub week: Vec<(i64, f64)>,
    /// Each 5-hour window seen in the range, with its highest reading.
    pub windows: Vec<Window>,
    /// API-equivalent cost per workspace since the 7-day window opened: a
    /// stand-in for each workspace's share of the limit. With a workspace
    /// filter, the workspaces it leaves out share one row.
    pub shares: Vec<Share>,
    /// Weekly limits of one model or surface, from `/usage`.
    pub scoped: Vec<ScopedForecast>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ScopedForecast {
    /// The server's label, e.g. `Fable`.
    pub label: String,
    /// The server's rating: `normal`, `warning` or `critical`.
    pub severity: String,
    pub forecast: Option<Forecast>,
    /// Readings since its window opened, hourly peaks.
    pub week: Vec<(i64, f64)>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Forecast {
    pub used_pct: f64,
    /// When the window resets, in ms.
    pub resets_at: i64,
    pub window_ms: i64,
    /// Percent per hour since the window opened.
    pub pace_avg: Option<f64>,
    /// Percent per hour over the last `recent_ms`.
    pub pace_recent: Option<f64>,
    pub recent_ms: i64,
    /// Where each pace leaves the window when it resets; may pass 100.
    pub at_reset_avg: Option<f64>,
    pub at_reset_recent: Option<f64>,
    /// When the recent pace reaches 100 %, if before the reset.
    pub full_at: Option<i64>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Window {
    pub start: i64,
    pub end: i64,
    pub peak: f64,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Share {
    /// None for sessions outside every workspace, or for `other`.
    pub workspace: Option<String>,
    /// Everything the workspace filter leaves out.
    pub other: bool,
    pub cost_usd: f64,
}

pub fn limits(
    conn: &Connection,
    scope: &Scope,
    live: Option<&RateLimits>,
    workspaces: &Workspaces,
) -> Result<Limits> {
    let now = scope.now;
    let recent = readings(conn, now - SEVEN_DAYS_MS, now)?;
    let five: Vec<(i64, f64)> = recent.iter().filter_map(|r| Some((r.0, r.1?))).collect();
    let seven: Vec<(i64, f64)> = recent.iter().filter_map(|r| Some((r.0, r.2?))).collect();
    let mut out = Limits::default();
    if let Some(live) = live {
        out.five_hour = live.five_hour.as_ref().and_then(|l| {
            forecast(
                l,
                live.updated_at,
                now,
                FIVE_HOURS_MS,
                RECENT_FIVE_HOUR_MS,
                &five,
            )
        });
        out.seven_day = live.seven_day.as_ref().and_then(|l| {
            forecast(
                l,
                live.updated_at,
                now,
                SEVEN_DAYS_MS,
                RECENT_SEVEN_DAY_MS,
                &seven,
            )
        });
    }
    let window_start =
        |f: Option<&Forecast>| f.map_or(now - SEVEN_DAYS_MS, |f| f.resets_at - f.window_ms);
    let week_start = window_start(out.seven_day.as_ref());
    out.week = hourly_peaks(&seven, week_start);
    for limit in live.map_or(&[][..], |l| &l.scoped) {
        let readings = scoped_readings(conn, &limit.label, now - SEVEN_DAYS_MS, now)?;
        // NOTE: scoped rows are weekly ones (`weekly_scoped` in `/usage`)
        let forecast = forecast(
            &Limit {
                used_pct: limit.used_pct,
                resets_at: limit.resets_at,
            },
            limit.read_at,
            now,
            SEVEN_DAYS_MS,
            RECENT_SEVEN_DAY_MS,
            &readings,
        );
        out.scoped.push(ScopedForecast {
            label: limit.label.clone(),
            severity: limit.severity.clone(),
            week: hourly_peaks(&readings, window_start(forecast.as_ref())),
            forecast,
        });
    }
    let in_range: Vec<(i64, f64)> = readings(conn, scope.since, scope.until)?
        .into_iter()
        .filter_map(|r| Some((r.0, r.1?)))
        .collect();
    out.windows = windows(&in_range);
    out.shares = shares(conn, scope, week_start, workspaces)?;
    Ok(out)
}

/// Highest reading per hour from `from` on.
fn hourly_peaks(readings: &[(i64, f64)], from: i64) -> Vec<(i64, f64)> {
    let mut peaks: BTreeMap<i64, f64> = BTreeMap::new();
    for &(ts, v) in readings.iter().filter(|r| r.0 >= from) {
        let peak = peaks.entry(ts / HOUR_MS * HOUR_MS).or_insert(v);
        *peak = peak.max(v);
    }
    peaks.into_iter().collect()
}

/// `(minute, percent)` of one scoped limit, highest per minute.
fn scoped_readings(
    conn: &Connection,
    label: &str,
    since: i64,
    until: i64,
) -> Result<Vec<(i64, f64)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ts / ?4 * ?4 AS minute, max(used_pct) FROM limit_samples
            WHERE label = ?1 AND ts >= ?2 AND ts < ?3 GROUP BY minute ORDER BY minute",
    )?;
    let rows = stmt.query_map(params![label, since, until, MINUTE_MS], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `(minute, 5-hour, 7-day)`, highest per minute.
type Reading = (i64, Option<f64>, Option<f64>);

fn readings(conn: &Connection, since: i64, until: i64) -> Result<Vec<Reading>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ts / ?3 * ?3 AS minute, max(five_hour), max(seven_day) FROM samples
            WHERE ts >= ?1 AND ts < ?2 AND (five_hour IS NOT NULL OR seven_day IS NOT NULL)
            GROUP BY minute ORDER BY minute",
    )?;
    let rows = stmt.query_map(params![since, until, MINUTE_MS], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `limit` read at `updated_at`; `readings` of the same limit, oldest first.
fn forecast(
    limit: &Limit,
    updated_at: i64,
    now: i64,
    window_ms: i64,
    recent_ms: i64,
    readings: &[(i64, f64)],
) -> Option<Forecast> {
    let resets_at = limit.resets_at * 1000;
    // NOTE: a reading whose window already reset says nothing about the new one
    if resets_at <= now {
        return None;
    }
    let hours = |ms: i64| ms as f64 / HOUR_MS as f64;
    let used = limit.used_pct;
    let start = resets_at - window_ms;
    let pace_avg = (now - start >= 10 * MINUTE_MS).then(|| used / hours(now - start));
    let from = start.max(now - recent_ms);
    let pace_recent = readings
        .iter()
        .find(|(ts, _)| *ts >= from)
        .filter(|(ts, _)| updated_at - ts >= recent_ms / 4)
        .map(|&(ts, v)| ((used - v) / hours(updated_at - ts)).max(0.0));
    let left = hours(resets_at - now);
    let at_reset = |pace: Option<f64>| pace.map(|p| used + p * left);
    let full_at = pace_recent
        .filter(|p| *p > 0.0 && used < 100.0 && used + p * left > 100.0)
        .map(|p| now + ((100.0 - used) / p * HOUR_MS as f64) as i64);
    Some(Forecast {
        used_pct: used,
        resets_at,
        window_ms,
        pace_avg,
        pace_recent,
        recent_ms,
        at_reset_avg: at_reset(pace_avg),
        at_reset_recent: at_reset(pace_recent),
        full_at,
    })
}

/// Splits 5-hour readings into windows: a new one starts after a drop or a
/// gap longer than a window.
fn windows(readings: &[(i64, f64)]) -> Vec<Window> {
    let mut out: Vec<Window> = Vec::new();
    let mut prev: Option<(i64, f64)> = None;
    for &(ts, v) in readings {
        let fresh = prev.is_none_or(|(pts, pv)| v + RESET_DROP < pv || ts - pts > FIVE_HOURS_MS);
        match out.last_mut() {
            Some(w) if !fresh => {
                w.end = ts;
                w.peak = w.peak.max(v);
            }
            _ => out.push(Window {
                start: ts,
                end: ts,
                peak: v,
            }),
        }
        prev = Some((ts, v));
    }
    out
}

fn shares(
    conn: &Connection,
    scope: &Scope,
    since: i64,
    workspaces: &Workspaces,
) -> Result<Vec<Share>> {
    let mut stmt = conn.prepare_cached(
        "SELECT min(s.cost_usd), max(s.cost_usd), coalesce(x.started_at, 0), coalesce(x.cwd, '')
            FROM samples s LEFT JOIN sessions x ON x.id = s.session
            WHERE s.ts >= ?1 AND s.ts < ?2 AND s.cost_usd IS NOT NULL
            GROUP BY s.session",
    )?;
    let rows = stmt.query_map(params![since, scope.now], |r| {
        Ok((
            r.get::<_, f64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut by: BTreeMap<(bool, Option<String>), f64> = BTreeMap::new();
    for row in rows {
        let (min, max, started_at, cwd) = row?;
        let cost = if started_at >= since { max } else { max - min };
        let key = if scope.contains(&cwd) {
            (false, workspaces.of(&cwd))
        } else {
            (true, None)
        };
        *by.entry(key).or_default() += cost.max(0.0);
    }
    let mut out: Vec<Share> = by
        .into_iter()
        .filter(|(_, cost)| *cost > 0.0)
        .map(|((other, workspace), cost_usd)| Share {
            workspace,
            other,
            cost_usd,
        })
        .collect();
    out.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forecasts_both_paces() {
        let now = 100 * HOUR_MS;
        // Opened 2 h ago, resets in 3 h; 20 % used, 10 points in the last hour.
        let limit = Limit {
            used_pct: 20.0,
            resets_at: (now + 3 * HOUR_MS) / 1000,
        };
        let readings = [(now - 2 * HOUR_MS, 0.0), (now - HOUR_MS, 10.0)];
        let f = forecast(&limit, now, now, FIVE_HOURS_MS, HOUR_MS, &readings).unwrap();
        assert_eq!(f.pace_avg, Some(10.0));
        assert_eq!(f.pace_recent, Some(10.0));
        assert_eq!(f.at_reset_recent, Some(50.0));
        assert_eq!(f.full_at, None);

        let hot = Limit {
            used_pct: 80.0,
            ..limit
        };
        let readings = [(now - HOUR_MS, 40.0)];
        let f = forecast(&hot, now, now, FIVE_HOURS_MS, HOUR_MS, &readings).unwrap();
        assert_eq!(f.pace_recent, Some(40.0));
        assert_eq!(f.full_at, Some(now + 30 * MINUTE_MS));

        let stale = Limit {
            used_pct: 5.0,
            resets_at: (now - 1000) / 1000,
        };
        assert!(forecast(&stale, now, now, FIVE_HOURS_MS, HOUR_MS, &[]).is_none());
    }

    #[test]
    fn a_workspace_filter_folds_the_rest_into_one_share() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();
        let now = 1_000 * HOUR_MS;
        for (id, cwd, cost) in [
            ("a", "/ws/one/repo", 3.0),
            ("b", "/ws/two", 2.0),
            ("c", "/elsewhere", 1.0),
        ] {
            conn.execute(
                "INSERT INTO sessions VALUES (?1, 'h', ?2, NULL, NULL, 'interactive', ?3, NULL)",
                params![id, cwd, now - HOUR_MS],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO samples (ts, host, session, cost_usd) VALUES (?1, 'h', ?2, ?3)",
                params![now - 1000, id, cost],
            )
            .unwrap();
        }
        let ws = Workspaces::new(&["/ws/one".into(), "/ws/two".into()], "/home/u");
        let scope = |dirs: Option<Vec<String>>| Scope {
            since: now - HOUR_MS,
            until: now,
            now,
            dirs,
        };
        let rows = |dirs| {
            shares(&conn, &scope(dirs), now - 2 * HOUR_MS, &ws)
                .unwrap()
                .into_iter()
                .map(|s| (s.workspace, s.other, s.cost_usd))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            rows(None),
            [
                (Some("one".into()), false, 3.0),
                (Some("two".into()), false, 2.0),
                (None, false, 1.0),
            ]
        );
        assert_eq!(
            rows(Some(vec!["/ws/one".into()])),
            [(Some("one".into()), false, 3.0), (None, true, 3.0)]
        );
    }

    #[test]
    fn scoped_limits_get_a_forecast_and_a_week() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();
        let now = 1_000 * HOUR_MS;
        // Opened 2 days ago, resets in 5; 40 % after 2 days, 30 % a day ago.
        let resets_at = now + 5 * 24 * HOUR_MS;
        for (ts, pct) in [(now - 24 * HOUR_MS, 30.0), (now, 40.0)] {
            conn.execute(
                "INSERT INTO limit_samples VALUES (?1, 'Fable', ?2, ?3)",
                params![ts, pct, resets_at / 1000],
            )
            .unwrap();
        }
        let live = RateLimits {
            scoped: vec![crate::model::ScopedLimit {
                label: "Fable".into(),
                used_pct: 40.0,
                resets_at: resets_at / 1000,
                severity: "normal".into(),
                read_at: now,
            }],
            ..RateLimits::default()
        };
        let scope = Scope {
            since: now - 24 * HOUR_MS,
            until: now,
            now,
            dirs: None,
        };
        let l = limits(&conn, &scope, Some(&live), &Workspaces::default()).unwrap();
        let fable = &l.scoped[0];
        let f = fable.forecast.as_ref().unwrap();
        assert_eq!(
            (f.pace_avg, f.pace_recent),
            (Some(40.0 / 48.0), Some(10.0 / 24.0))
        );
        assert_eq!(f.at_reset_recent, Some(90.0));
        assert_eq!(
            fable.week.len(),
            1,
            "the reading at `now` is past the range"
        );
    }

    #[test]
    fn windows_split_on_resets_and_gaps() {
        let h = HOUR_MS;
        let w = windows(&[
            (0, 10.0),
            (h, 60.0),
            (2 * h, 5.0),
            (3 * h, 30.0),
            (10 * h, 40.0),
        ]);
        let peaks: Vec<(i64, f64)> = w.iter().map(|w| (w.start, w.peak)).collect();
        assert_eq!(peaks, [(0, 60.0), (2 * h, 30.0), (10 * h, 40.0)]);
    }
}
