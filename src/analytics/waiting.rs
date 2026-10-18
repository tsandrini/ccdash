//! Your-turn waits by how they ended (see `crate::waits`): waits on you,
//! parked sessions, and turns Claude carried on by itself. Plus how fast you
//! answered, per bucket, and the sessions you came back to late.

use serde::Serialize;

use super::{Bucket, Clock, percentile};
use crate::{
    history::Lane,
    model::{Activity, PARKED_AFTER_MS},
    waits::Outcome,
};

const TOP_LATE: usize = 10;

#[derive(Debug, Default, Serialize)]
pub struct Waiting {
    pub parked_after_ms: i64,
    /// Waits overlapping the range by outcome; time inside the range.
    pub answered: Tally,
    pub waiting: Tally,
    pub resumed: Tally,
    pub came_back: Tally,
    pub finished: Tally,
    pub left_open: Tally,
    /// From the hand-over to your prompt, for waits answered in the range.
    pub answer_p50_ms: Option<i64>,
    pub answer_p90_ms: Option<i64>,
    /// Waits you came back to in the range, longest first.
    pub late: Vec<LateWait>,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Tally {
    pub waits: u32,
    pub ms: i64,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct LateWait {
    pub session: String,
    pub label: String,
    pub host: String,
    pub cwd: String,
    pub start: i64,
    pub ms: i64,
}

/// Returns the summary and the times you took to answer a wait or a
/// permission prompt (for the response histogram); fills the buckets'
/// answer counts and medians.
pub fn waiting(
    lanes: &[Lane],
    (since, until): (i64, i64),
    clock: &Clock,
    buckets: &mut [Bucket],
) -> (Waiting, Vec<i64>) {
    let mut out = Waiting {
        parked_after_ms: PARKED_AFTER_MS,
        ..Waiting::default()
    };
    let mut answers = Vec::new();
    let mut per_bucket: Vec<Vec<i64>> = vec![Vec::new(); buckets.len()];
    let mut responses = Vec::new();
    for lane in lanes.iter().filter(|l| !l.background) {
        for w in &lane.waits {
            let tally = match w.outcome {
                Outcome::Answered => &mut out.answered,
                Outcome::Waiting => &mut out.waiting,
                Outcome::Resumed => &mut out.resumed,
                Outcome::CameBack => &mut out.came_back,
                Outcome::Finished => &mut out.finished,
                Outcome::LeftOpen => &mut out.left_open,
            };
            tally.waits += 1;
            tally.ms += (w.end.min(until) - w.start.max(since)).max(0);
            let ms = w.end - w.start;
            let ended_here = (since..until).contains(&w.end);
            if w.outcome == Outcome::Answered && ended_here {
                answers.push(ms);
                per_bucket[clock.index(w.end)].push(ms);
            }
            if w.outcome == Outcome::CameBack && ended_here {
                out.late.push(LateWait {
                    session: lane.id.clone(),
                    label: lane.label.clone(),
                    host: lane.host.clone(),
                    cwd: lane.cwd.clone(),
                    start: w.start,
                    ms,
                });
            }
        }
        for pair in lane.segments.windows(2) {
            if let [
                (from, to, Activity::Permission),
                (next, _, Activity::Working),
            ] = *pair
                && to == next
            {
                responses.push(to - from);
            }
        }
    }
    answers.sort_unstable();
    out.answer_p50_ms = percentile(&answers, 50);
    out.answer_p90_ms = percentile(&answers, 90);
    for (bucket, mut ms) in buckets.iter_mut().zip(per_bucket) {
        ms.sort_unstable();
        bucket.answers = ms.len() as u32;
        bucket.answer_p50_ms = percentile(&ms, 50);
    }
    out.late.sort_by_key(|l| std::cmp::Reverse(l.ms));
    out.late.truncate(TOP_LATE);
    responses.extend(answers);
    (out, responses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::waits::Wait;

    #[test]
    fn waits_split_by_outcome() {
        let h = PARKED_AFTER_MS;
        let wait = |start, end, outcome| Wait {
            start,
            end,
            outcome,
        };
        let lane = Lane {
            id: "a".into(),
            label: "a".into(),
            host: "h".into(),
            cwd: "/w".into(),
            live: true,
            background: false,
            segments: vec![
                (0, 10, Activity::Permission),
                (10, 20, Activity::Working),
                (20, 80, Activity::YourTurn),
            ],
            waits: vec![
                wait(20, 80, Outcome::Answered),
                wait(100, 100 + 2 * h, Outcome::CameBack),
                wait(-h, 50, Outcome::Finished),
            ],
        };
        let clock = Clock {
            offset_ms: 0,
            bucket_ms: 10 * h,
            first: 0,
            count: 1,
        };
        let mut buckets = vec![Bucket::default()];
        let (w, responses) = waiting(&[lane], (0, 10 * h), &clock, &mut buckets);
        assert_eq!(w.answered, Tally { waits: 1, ms: 60 });
        // Clipped to the range.
        assert_eq!(w.finished, Tally { waits: 1, ms: 50 });
        assert_eq!(w.late[0].ms, 2 * h);
        assert_eq!(w.answer_p50_ms, Some(60));
        assert_eq!(
            (buckets[0].answers, buckets[0].answer_p50_ms),
            (1, Some(60))
        );
        assert_eq!(responses, [10, 60]);
    }
}
