//! Your-turn waits, from the hand-over to what ended it. A reply within
//! `PARKED_AFTER_MS` makes it a wait on you. Replying later, closing the
//! session or leaving it open makes it parked. Claude carrying on by itself
//! (a background task finished, a wakeup) means it never waited on you.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::model::{Activity, PARKED_AFTER_MS};

/// `[from, to, activity]`, as in history lanes.
pub type Segment = (i64, i64, Activity);

/// Event kinds that can end a wait, for SQL `IN (…)`.
pub const CLOSERS: &str = "'prompt', 'tool', 'permission', 'reply', 'error', 'end'";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// You prompted within `PARKED_AFTER_MS`.
    Answered,
    /// Claude carried on by itself before you prompted.
    Resumed,
    /// Still open, for less than `PARKED_AFTER_MS` so far.
    Waiting,
    /// You prompted, after more than `PARKED_AFTER_MS`.
    CameBack,
    /// The session ended without another prompt.
    Finished,
    /// Still open, for more than `PARKED_AFTER_MS`.
    LeftOpen,
}

impl Outcome {
    /// Waits on you; the rest were parked or never needed you.
    pub fn on_you(self) -> bool {
        matches!(self, Self::Answered | Self::Waiting)
    }

    pub fn parked(self) -> bool {
        matches!(self, Self::CameBack | Self::Finished | Self::LeftOpen)
    }

    /// How history draws the wait.
    fn shown(self) -> Activity {
        if self.on_you() {
            Activity::YourTurn
        } else if self.parked() {
            Activity::Parked
        } else {
            Activity::Idle
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Wait {
    /// The hand-over, possibly before the loaded range.
    pub start: i64,
    /// When it ended (now, while open), possibly after the loaded range.
    pub end: i64,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closer {
    Prompt,
    /// A tool call, permission prompt, reply or error.
    Claude,
    End,
}

/// An event that ends a wait open before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Close {
    /// When the event was recorded.
    pub ts: i64,
    /// When the wait ended: a tool call's start, otherwise `ts`.
    pub at: i64,
    pub by: Closer,
}

impl Close {
    /// From an event row of one of the `CLOSERS` kinds; `ms` is a tool call's.
    pub fn new(kind: &str, ts: i64, ms: Option<i64>) -> Self {
        let by = match kind {
            "prompt" => Closer::Prompt,
            "end" => Closer::End,
            _ => Closer::Claude,
        };
        Self {
            ts,
            at: ts - ms.unwrap_or(0).max(0),
            by,
        }
    }
}

/// One session's surroundings, to finish waits cut by the loaded range.
pub struct Edges<'a> {
    pub conn: &'a Connection,
    pub session: &'a str,
    /// Start of the loaded range; a wait open there began earlier.
    pub since: i64,
    /// End of the loaded range; a wait open there ends later.
    pub until: i64,
    /// Whether the session still runs.
    pub live: bool,
    pub now: i64,
}

/// Finds the waits in one session's `segments` and redraws them by outcome:
/// your turn, parked, or idle when Claude carried on by itself. Working
/// stretches inside a wait (an idle compaction, a stale poll) count as
/// the wait. `closes` are the session's events in the range, in order.
///
/// # Errors
///
/// Fails when a lookup past the range edges fails.
pub fn settle(segments: &mut Vec<Segment>, closes: &[Close], edges: &Edges) -> Result<Vec<Wait>> {
    let mut waits = Vec::new();
    let mut scanned = i64::MIN;
    let mut next = 0;
    let last = segments.last().map_or(edges.until, |s| s.1);
    let mut i = 0;
    // NOTE: a wait can end inside a segment; its rest starts the next one
    while let Some(&(from, to, activity)) = segments.get(i) {
        if to <= scanned || !matches!(activity, Activity::YourTurn | Activity::Parked) {
            i += 1;
            continue;
        }
        let from = from.max(scanned);
        let start = if from == edges.since {
            handed_over_before(edges)?.unwrap_or(from)
        } else {
            from
        };
        while closes.get(next).is_some_and(|c| c.ts <= from) {
            next += 1;
        }
        let close = match closes.get(next) {
            Some(c) => Some(*c),
            None => close_after(edges)?,
        };
        let long = |end: i64| end - start >= PARKED_AFTER_MS;
        let (end, outcome) = match close {
            Some(c) => {
                let end = c.at.max(from);
                let outcome = match c.by {
                    Closer::Prompt if long(end) => Outcome::CameBack,
                    Closer::Prompt => Outcome::Answered,
                    Closer::Claude => Outcome::Resumed,
                    Closer::End => Outcome::Finished,
                };
                (end, outcome)
            }
            None if edges.live && long(edges.now) => (edges.now, Outcome::LeftOpen),
            None if edges.live => (edges.now, Outcome::Waiting),
            None => (last.max(from), Outcome::Finished),
        };
        waits.push(Wait {
            start,
            end,
            outcome,
        });
        scanned = end.max(from + 1);
    }
    for wait in &waits {
        paint(segments, wait);
    }
    Ok(waits)
}

/// Redraws the part of `segments` inside `wait`.
fn paint(segments: &mut Vec<Segment>, wait: &Wait) {
    let shown = wait.outcome.shown();
    let repaint = |a: Activity| match a {
        Activity::YourTurn | Activity::Parked => true,
        Activity::Working => wait.outcome != Outcome::Resumed,
        _ => false,
    };
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len() + 2);
    for &(a, b, activity) in segments.iter() {
        let (lo, hi) = (a.max(wait.start), b.min(wait.end));
        if lo >= hi || !repaint(activity) {
            out.push((a, b, activity));
            continue;
        }
        if a < lo {
            out.push((a, lo, activity));
        }
        out.push((lo, hi, shown));
        if hi < b {
            out.push((hi, b, activity));
        }
    }
    out.dedup_by(|next, prev| {
        let joined = prev.2 == next.2 && prev.1 == next.0;
        if joined {
            prev.1 = next.1;
        }
        joined
    });
    *segments = out;
}

/// When a wait still open at `since` began: the first hand-over after the
/// last event that ended a wait (a reply that Claude sent by itself starts
/// one too).
fn handed_over_before(edges: &Edges) -> Result<Option<i64>> {
    let last: Option<(i64, String)> = edges
        .conn
        .prepare_cached(&format!(
            "SELECT ts, kind FROM events WHERE session = ?1 AND ts < ?2 AND kind IN ({CLOSERS})
                ORDER BY ts DESC, rowid DESC LIMIT 1"
        ))?
        .query_row(params![edges.session, edges.since], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let after = last.as_ref().map_or(i64::MIN, |l| l.0);
    let handed: Option<i64> = edges
        .conn
        .prepare_cached(
            "SELECT min(ts) FROM events WHERE session = ?1 AND kind = 'activity'
                AND ts >= ?2 AND ts < ?3 AND json_extract(data, '$.to') = 'your_turn'",
        )?
        .query_row(params![edges.session, after, edges.since], |r| r.get(0))?;
    Ok(handed.or_else(|| {
        last.filter(|(_, kind)| matches!(kind.as_str(), "reply" | "error"))
            .map(|(ts, _)| ts)
    }))
}

/// The first event at or after `until` that ends a wait.
fn close_after(edges: &Edges) -> Result<Option<Close>> {
    Ok(edges
        .conn
        .prepare_cached(&format!(
            "SELECT kind, ts, json_extract(data, '$.ms') FROM events
                WHERE session = ?1 AND ts >= ?2 AND kind IN ({CLOSERS})
                ORDER BY ts, rowid LIMIT 1"
        ))?
        .query_row(params![edges.session, edges.until], |r| {
            Ok(Close::new(&r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SCHEMA;

    const H: i64 = PARKED_AFTER_MS;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn event(conn: &Connection, ts: i64, kind: &str, data: &str) {
        conn.execute(
            "INSERT INTO events (ts, host, session, kind, data) VALUES (?1, 'h', 's', ?2, ?3)",
            params![ts, kind, data],
        )
        .unwrap();
    }

    fn run(
        conn: &Connection,
        segments: &mut Vec<Segment>,
        closes: &[Close],
        (since, until): (i64, i64),
        live: bool,
        now: i64,
    ) -> Vec<Wait> {
        let edges = Edges {
            conn,
            session: "s",
            since,
            until,
            live,
            now,
        };
        settle(segments, closes, &edges).unwrap()
    }

    use Activity::{Idle, Parked, Working, YourTurn};

    fn prompt(ts: i64) -> Close {
        Close::new("prompt", ts, None)
    }

    #[test]
    fn a_quick_reply_is_a_wait_on_you_and_a_late_one_is_parked() {
        let conn = db();
        let mut segments = vec![
            (0, 100, Working),
            (100, 200, YourTurn),
            (200, 300, Working),
            (300, 300 + H + 1, YourTurn),
            (300 + H + 1, 400 + H, Working),
        ];
        let closes = [prompt(200), prompt(300 + H + 1)];
        let waits = run(&conn, &mut segments, &closes, (0, 400 + H), false, 0);
        let outcomes: Vec<_> = waits.iter().map(|w| w.outcome).collect();
        assert_eq!(outcomes, [Outcome::Answered, Outcome::CameBack]);
        assert_eq!(segments[1], (100, 200, YourTurn));
        assert_eq!(segments[3], (300, 300 + H + 1, Parked));
    }

    #[test]
    fn working_without_a_prompt_stays_part_of_the_wait() {
        let conn = db();
        // An idle compaction (or a stale poll) at 150–400, the reply at 500.
        let mut segments = vec![
            (0, 100, Working),
            (100, 150, YourTurn),
            (150, 400, Working),
            (400, 500, YourTurn),
            (500, 600, Working),
        ];
        let waits = run(&conn, &mut segments, &[prompt(500)], (0, 600), false, 0);
        assert_eq!(
            waits,
            [Wait {
                start: 100,
                end: 500,
                outcome: Outcome::Answered
            }]
        );
        assert_eq!(
            segments,
            [(0, 100, Working), (100, 500, YourTurn), (500, 600, Working)]
        );
    }

    #[test]
    fn claude_carrying_on_by_itself_never_waited_on_you() {
        let conn = db();
        // Turn ends at 100; a background task wakes Claude, whose tool runs
        // 300–320; a text-only reply at 700 starts a new wait, answered at 800.
        let mut segments = vec![
            (0, 100, Working),
            (100, 300, YourTurn),
            (300, 320, Working),
            (320, 800, YourTurn),
            (800, 900, Working),
        ];
        let closes = [
            Close::new("tool", 320, Some(20)),
            Close::new("reply", 700, None),
            prompt(800),
        ];
        let waits = run(&conn, &mut segments, &closes, (0, 900), false, 0);
        let outcomes: Vec<_> = waits.iter().map(|w| (w.start, w.end, w.outcome)).collect();
        assert_eq!(
            outcomes,
            [
                (100, 300, Outcome::Resumed),
                (320, 700, Outcome::Resumed),
                (700, 800, Outcome::Answered)
            ]
        );
        assert_eq!(segments[1], (100, 300, Idle));
        assert_eq!(segments[3], (320, 700, Idle));
        assert_eq!(segments[4], (700, 800, YourTurn));
    }

    #[test]
    fn closed_or_left_open_is_parked() {
        let conn = db();
        let mut segments = vec![(0, 100, Working), (100, 150, YourTurn)];
        let closes = [Close::new("end", 150, None)];
        let waits = run(&conn, &mut segments, &closes, (0, 150), false, 0);
        assert_eq!(waits[0].outcome, Outcome::Finished);
        assert_eq!(segments[1], (100, 150, Parked));

        let open = |now| {
            let mut segments = vec![(0, 100, Working), (100, now, YourTurn)];
            run(&conn, &mut segments, &[], (0, now), true, now)[0].outcome
        };
        assert_eq!(open(100 + H - 1), Outcome::Waiting);
        assert_eq!(open(100 + H), Outcome::LeftOpen);
    }

    #[test]
    fn waits_cut_by_the_range_look_past_it() {
        let conn = db();
        event(&conn, 10, "prompt", "{}");
        event(
            &conn,
            50,
            "activity",
            r#"{"from":"working","to":"your_turn"}"#,
        );
        event(&conn, 50, "reply", "{}");
        event(
            &conn,
            50 + H,
            "activity",
            r#"{"from":"your_turn","to":"parked"}"#,
        );
        event(&conn, 60 + H, "prompt", "{}");
        // Range 1000..2000 sits inside the wait 50..60 + H.
        let mut segments = vec![(1_000, 2_000, YourTurn)];
        let waits = run(&conn, &mut segments, &[], (1_000, 2_000), true, 70 + H);
        assert_eq!(
            waits,
            [Wait {
                start: 50,
                end: 60 + H,
                outcome: Outcome::CameBack
            }]
        );
        assert_eq!(segments, [(1_000, 2_000, Parked)]);
    }
}
