// What the Analytics sections share: the section list, bucket labels and
// columns, the bins' labels, the wait outcomes, and the limit outlook.

import type { Analytics, Bucket, Forecast } from './api/types';
import { dayFmt, shortDay } from './charts';
import { fmtTime } from './format';

export const SECTIONS = [
	['overview', 'Overview'],
	['waits', 'Waits'],
	['parallel', 'Parallel'],
	['limits', 'Limits'],
	['projects', 'Projects'],
	['turns', 'Turns'],
	['context', 'Context'],
	['tokens', 'Tokens'],
	['permissions', 'Permissions']
] as const;
export type Section = (typeof SECTIONS)[number][0];

export const hourly = (data: Analytics) => data.bucket_ms < 86_400_000;

export const bucketLabel = (data: Analytics, ts: number) =>
	hourly(data) ? fmtTime(ts) : data.buckets.length > 10 ? shortDay.format(ts) : dayFmt.format(ts);

export const bucketName = (data: Analytics, ts: number) =>
	hourly(data) ? `${dayFmt.format(ts)} ${fmtTime(ts)}` : dayFmt.format(ts);

/** A bucket as a column. */
export type Col = { key: string; label: string; b: Bucket };

export const columns = (data: Analytics): Col[] =>
	data.buckets.map((b) => ({ key: String(b.start), label: bucketLabel(data, b.start), b }));

/** A column's span, for the brush. */
export const bucketEdges =
	(data: Analytics) =>
	(d: Col): [number, number] => [d.b.start, d.b.start + data.bucket_ms];

export const sum = (data: Analytics, key: keyof Bucket) =>
	data.buckets.reduce((n, d) => n + (d[key] as number), 0);

export const BIN_LABELS = ['<30s', '30s–2m', '2–5m', '5–15m', '15m–1h', '>1h'];
export const PEAK_LABELS = ['<25%', '25–50%', '50–75%', '75–90%', '≥90%'];

/** `[key, label, what happened, counts as]`, as `waiting` tallies them. */
export const OUTCOMES = [
	['answered', 'Answered', 'You replied within the hour', 'waiting on you'],
	['waiting', 'Waiting now', 'Open, under an hour so far', 'waiting on you'],
	['came_back', 'Came back later', 'You replied after the hour', 'parked'],
	['finished', 'Finished', 'Closed without another prompt', 'parked'],
	['left_open', 'Left open', 'Open for over an hour', 'parked'],
	['resumed', 'Claude carried on', 'Claude went on by itself before you replied', 'neither']
] as const;

export const DECISION_SOURCES: Record<string, string> = {
	config: 'Settings or auto mode',
	hook: 'A hook',
	user_permanent: 'You, for good',
	user_temporary: 'You, once',
	user_reject: 'You refused',
	user_abort: 'Interrupted'
};

export function paceText(f: Forecast): string {
	const lately = f.recent_ms >= 86_400_000 ? '24 hours' : 'hour';
	if (f.pace_recent === null) {
		return f.pace_avg === null
			? 'not enough readings yet'
			: `${f.pace_avg.toFixed(1)}%/h, the average so far`;
	}
	const avg = f.pace_avg === null ? '' : `; ${f.pace_avg.toFixed(1)}%/h on average`;
	return `${f.pace_recent.toFixed(1)}%/h over the last ${lately}${avg}`;
}

/** Where a limit is heading: the recent pace, else the average so far. */
export function outlook(
	f: Forecast,
	now: number
): { at_reset: number | null; full_at: number | null } {
	if (f.pace_recent !== null) return { at_reset: f.at_reset_recent, full_at: f.full_at };
	const pace = f.pace_avg;
	const full =
		pace !== null && pace > 0 && f.used_pct < 100 && (f.at_reset_avg ?? 0) > 100
			? now + ((100 - f.used_pct) / pace) * 3_600_000
			: null;
	return { at_reset: f.at_reset_avg, full_at: full };
}
