//! How many sessions worked, or waited on you, at the same time.

use serde::Serialize;

use crate::{history::Lane, model::Activity};

/// About this many points across the range.
const POINTS: i64 = 240;
const MIN_SLOT_MS: i64 = 60_000;
/// A wait counts this long; after that the session is parked, not waiting.
pub const WAIT_CAP_MS: i64 = 30 * 60_000;

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Parallel {
    pub slot_ms: i64,
    pub wait_cap_ms: i64,
    /// `[slot start, mean working, max working, mean waiting, max waiting]`.
    pub series: Vec<(i64, f64, u32, f64, u32)>,
    /// Time with exactly 1, 2, 3, 4, 5, and 6 or more sessions working.
    pub working_time: [i64; 6],
    /// Time with at least one session working.
    pub busy_ms: i64,
    /// Working time summed over sessions.
    pub working_ms: i64,
    pub peak_working: u32,
    /// Time with at least one session waiting on you.
    pub waiting_ms: i64,
    /// Waiting time summed over sessions.
    pub queued_ms: i64,
    pub peak_waiting: u32,
    /// Time when sessions waited on you and none worked.
    pub blocked_ms: i64,
}

pub fn parallel(lanes: &[Lane], since: i64, until: i64) -> Parallel {
    let mut deltas: Vec<(i64, i32, i32)> = Vec::new();
    for lane in lanes {
        for &(from, to, activity) in &lane.segments {
            let (working, waiting) = match activity {
                Activity::Working => (1, 0),
                Activity::Permission | Activity::YourTurn => (0, 1),
                Activity::Idle | Activity::Gone => continue,
            };
            let to = if waiting > 0 {
                to.min(from + WAIT_CAP_MS)
            } else {
                to
            };
            if to <= since || from >= until {
                continue;
            }
            deltas.push((from.max(since), working, waiting));
            deltas.push((to.min(until), -working, -waiting));
        }
    }
    deltas.sort_unstable_by_key(|d| d.0);

    let slot_ms = ((until - since) / POINTS).max(MIN_SLOT_MS);
    let slots = usize::try_from((until - since + slot_ms - 1) / slot_ms).unwrap_or(0);
    let mut sums = vec![(0i64, 0u32, 0i64, 0u32); slots];
    let mut out = Parallel {
        slot_ms,
        wait_cap_ms: WAIT_CAP_MS,
        ..Parallel::default()
    };
    let (mut working, mut waiting) = (0i32, 0i32);
    let mut at = since;
    for (ts, dw, dq) in deltas {
        if ts > at {
            out.add(&mut sums, since, at, ts, working, waiting);
            at = ts;
        }
        working += dw;
        waiting += dq;
    }

    out.series = sums
        .into_iter()
        .enumerate()
        .map(|(i, (w_sum, w_max, q_sum, q_max))| {
            let start = since + i as i64 * slot_ms;
            let len = slot_ms.min(until - start).max(1) as f64;
            (start, w_sum as f64 / len, w_max, q_sum as f64 / len, q_max)
        })
        .collect();
    out
}

impl Parallel {
    /// `working` and `waiting` sessions from `from` to `to`.
    fn add(
        &mut self,
        sums: &mut [(i64, u32, i64, u32)],
        since: i64,
        from: i64,
        to: i64,
        working: i32,
        waiting: i32,
    ) {
        let (w, q) = (working.max(0) as u32, waiting.max(0) as u32);
        let ms = to - from;
        if w > 0 {
            self.busy_ms += ms;
            self.working_time[(w as usize).min(6) - 1] += ms;
            self.working_ms += i64::from(w) * ms;
            self.peak_working = self.peak_working.max(w);
        }
        if q > 0 {
            self.waiting_ms += ms;
            self.queued_ms += i64::from(q) * ms;
            self.peak_waiting = self.peak_waiting.max(q);
            if w == 0 {
                self.blocked_ms += ms;
            }
        }
        if w == 0 && q == 0 {
            return;
        }
        let mut at = from;
        while at < to {
            let slot = ((at - since) / self.slot_ms) as usize;
            let Some(sum) = sums.get_mut(slot) else { break };
            let end = (since + (slot as i64 + 1) * self.slot_ms).min(to);
            sum.0 += i64::from(w) * (end - at);
            sum.1 = sum.1.max(w);
            sum.2 += i64::from(q) * (end - at);
            sum.3 = sum.3.max(q);
            at = end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(segments: Vec<(i64, i64, Activity)>) -> Lane {
        Lane {
            id: "x".into(),
            label: "x".into(),
            cwd: "/w".into(),
            live: false,
            background: false,
            segments,
        }
    }

    #[test]
    fn counts_overlap_and_time_blocked_on_you() {
        let m = 60_000;
        let lanes = [
            lane(vec![
                (0, 10 * m, Activity::Working),
                (10 * m, 20 * m, Activity::YourTurn),
            ]),
            lane(vec![
                (5 * m, 15 * m, Activity::Working),
                (15 * m, 20 * m, Activity::Permission),
            ]),
        ];
        let p = parallel(&lanes, 0, 30 * m);
        assert_eq!(p.peak_working, 2);
        assert_eq!(p.working_time[..2], [10 * m, 5 * m]);
        assert_eq!(p.busy_ms, 15 * m);
        assert_eq!(p.working_ms, 20 * m);
        // 10–15 one waits while one works; 15–20 both wait, none work.
        assert_eq!(
            (p.waiting_ms, p.blocked_ms, p.peak_waiting),
            (10 * m, 5 * m, 2)
        );
        assert_eq!(p.series.len(), 30);
        assert_eq!(p.series[7], (7 * m, 2.0, 2, 0.0, 0));
        assert_eq!(p.series[25], (25 * m, 0.0, 0, 0.0, 0));

        let parked = [lane(vec![(0, 120 * m, Activity::YourTurn)])];
        assert_eq!(parallel(&parked, 0, 120 * m).waiting_ms, WAIT_CAP_MS);
    }
}
