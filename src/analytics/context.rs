//! How full sessions got: compactions, and the highest context use each
//! session reached.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{Connection, named_params};
use serde::Serialize;

use super::{Bucket, Clock, SessionEvents};
use crate::workspace::{IN_SCOPE, Scope};

/// Upper bounds of the peak-context bins, in percent; the last bin is open.
pub const PEAK_BOUNDS: [f64; 4] = [25.0, 50.0, 75.0, 90.0];
const TOP_SESSIONS: usize = 8;

#[derive(Debug, Default, Serialize)]
pub struct ContextHealth {
    pub compactions: u32,
    pub compactions_auto: u32,
    /// Sessions that compacted at least once.
    pub compacted_sessions: u32,
    pub peak_bounds: [f64; 4],
    /// Sessions by the highest context use they reached, binned.
    pub peaks: [u32; 5],
    pub median_peak: Option<f64>,
    /// Most compactions first, then the fullest.
    pub sessions: Vec<ContextSession>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ContextSession {
    pub id: String,
    pub label: String,
    pub cwd: String,
    pub compactions: u32,
    pub peak_pct: Option<f64>,
}

pub fn context(
    conn: &Connection,
    scope: &Scope,
    sessions: &[SessionEvents],
    clock: &Clock,
    buckets: &mut [Bucket],
) -> Result<ContextHealth> {
    let mut out = ContextHealth {
        peak_bounds: PEAK_BOUNDS,
        ..ContextHealth::default()
    };
    let mut rows: HashMap<&str, ContextSession> = HashMap::new();
    for session in sessions {
        let compactions: Vec<_> = session
            .events
            .iter()
            .filter(|e| e.kind == "compact")
            .collect();
        for ev in &compactions {
            let bucket = &mut buckets[clock.index(ev.ts)];
            bucket.compactions += 1;
            out.compactions += 1;
            if ev.data["trigger"] == "auto" {
                bucket.compactions_auto += 1;
                out.compactions_auto += 1;
            }
        }
        if !compactions.is_empty() {
            out.compacted_sessions += 1;
        }
        rows.insert(
            &session.id,
            ContextSession {
                id: session.id.clone(),
                label: session.label.clone(),
                cwd: session.cwd.clone(),
                compactions: compactions.len() as u32,
                peak_pct: None,
            },
        );
    }

    let mut stmt = conn.prepare_cached(&format!(
        "SELECT s.session, max(s.context_pct) FROM samples s LEFT JOIN sessions x ON x.id = s.session
            WHERE s.ts >= :since AND s.ts < :until AND s.context_pct IS NOT NULL AND {IN_SCOPE}
            GROUP BY s.session"
    ))?;
    let peaks = stmt.query_map(
        named_params! {":since": scope.since, ":until": scope.until, ":dir": scope.dir},
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)),
    )?;
    let mut values = Vec::new();
    for row in peaks {
        let (id, peak) = row?;
        out.peaks[PEAK_BOUNDS.iter().take_while(|b| peak >= **b).count()] += 1;
        values.push(peak);
        if let Some(row) = rows.get_mut(id.as_str()) {
            row.peak_pct = Some(peak);
        }
    }
    values.sort_by(f64::total_cmp);
    out.median_peak = values.get(values.len() / 2).copied();

    let mut sessions: Vec<ContextSession> = rows
        .into_values()
        .filter(|r| r.compactions > 0 || r.peak_pct.is_some())
        .collect();
    sessions.sort_by(|a, b| {
        b.compactions.cmp(&a.compactions).then_with(|| {
            b.peak_pct
                .unwrap_or(0.0)
                .total_cmp(&a.peak_pct.unwrap_or(0.0))
        })
    });
    sessions.truncate(TOP_SESSIONS);
    out.sessions = sessions;
    Ok(out)
}
