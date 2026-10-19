// The time range History and Analytics show: quick ranges (relative to
// now, or whole local days), or an absolute span; kept in the URL query
// as `range` and resolved against the hub's clock when used.

export type Range =
	| { kind: 'relative'; minutes: number }
	| { kind: 'days'; days: number }
	| { kind: 'absolute'; from: number; to: number };

export const MINUTE = 60_000;
const DAY = 86_400_000;
/** Retention is 30 days; the API serves 31. */
export const MAX_RANGE_MINUTES = 31 * 1440;

export const HISTORY_QUICK: Range[] = [
	{ kind: 'relative', minutes: 60 },
	{ kind: 'relative', minutes: 180 },
	{ kind: 'relative', minutes: 360 },
	{ kind: 'relative', minutes: 720 },
	{ kind: 'relative', minutes: 1440 },
	{ kind: 'days', days: 1 },
	{ kind: 'relative', minutes: 2880 },
	{ kind: 'days', days: 7 }
];

export const ANALYTICS_QUICK: Range[] = [
	{ kind: 'days', days: 1 },
	{ kind: 'relative', minutes: 1440 },
	{ kind: 'days', days: 2 },
	{ kind: 'days', days: 3 },
	{ kind: 'days', days: 7 },
	{ kind: 'days', days: 14 },
	{ kind: 'days', days: 30 }
];

/** The page's quick ranges and its range when the URL names none. */
export function rangeSetup(path: string): { quick: Range[]; fallback: Range } | null {
	if (path === '/history' || path.startsWith('/history/'))
		return { quick: HISTORY_QUICK, fallback: { kind: 'days', days: 1 } };
	if (path === '/analytics' || path.startsWith('/analytics/'))
		return { quick: ANALYTICS_QUICK, fallback: { kind: 'days', days: 7 } };
	return null;
}

export function resolveRange(range: Range, t: number): { from: number; to: number } {
	if (range.kind === 'relative') return { from: t - range.minutes * MINUTE, to: t };
	if (range.kind === 'days') {
		const start = new Date(t);
		start.setHours(0, 0, 0, 0);
		start.setDate(start.getDate() - range.days + 1);
		return { from: start.getTime(), to: t };
	}
	return { from: range.from, to: range.to };
}

/** New data can still arrive in the range. */
export const reachesNow = (range: Range, t: number) =>
	range.kind !== 'absolute' || range.to >= t - MINUTE;

/** Moves a range by its own length; forward never passes now (null then). */
export function shiftRange(range: Range, direction: 1 | -1, t: number): Range | null {
	const { from, to } = resolveRange(range, t);
	const span = to - from;
	if (direction === 1) {
		if (to >= t - MINUTE) return null;
		const end = Math.min(t, to + span);
		return { kind: 'absolute', from: end - span, to: end };
	}
	return { kind: 'absolute', from: from - span, to: to - span };
}

/** Twice the span around the same middle, never past now or retention. */
export function zoomOut(range: Range, t: number): Range {
	const { from, to } = resolveRange(range, t);
	const span = Math.min((to - from) * 2, MAX_RANGE_MINUTES * MINUTE);
	const end = Math.min(t, (from + to) / 2 + span / 2);
	return { kind: 'absolute', from: end - span, to: end };
}

/** From `from` to now: "last N days" when it starts at a local midnight. */
export function untilNow(from: number, t: number): Range {
	const start = new Date(from);
	const today = new Date(t);
	today.setHours(0, 0, 0, 0);
	if (start.getHours() || start.getMinutes()) return { kind: 'absolute', from, to: t };
	return { kind: 'days', days: Math.round((today.getTime() - start.getTime()) / DAY) + 1 };
}

const pad2 = (n: number) => String(n).padStart(2, '0');

/** A moment as `<input type="datetime-local">` text, local time. */
export function toInputValue(time: number): string {
	const d = new Date(time);
	return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}T${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

/** The moment a `datetime-local` value names; null when it names none. */
export function fromInputValue(text: string): number | null {
	const m = /^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})$/.exec(text.trim());
	if (!m) return null;
	const [year, month, day, hour, minute] = m.slice(1).map(Number);
	const time = new Date(year, month - 1, day, hour, minute).getTime();
	// NOTE: Date rolls 31 February over instead of refusing it
	return toInputValue(time).startsWith(`${m[1]}-${m[2]}-${m[3]}`) ? time : null;
}

export type RangeProblem = 'incomplete' | 'backwards' | 'future' | 'too_old';

export function rangeProblem(
	from: number | null,
	to: number | null,
	t: number
): RangeProblem | null {
	if (from === null || to === null) return 'incomplete';
	if (from >= to) return 'backwards';
	if (from > t) return 'future';
	if (from < t - MAX_RANGE_MINUTES * MINUTE) return 'too_old';
	return null;
}

export const PROBLEM_TEXT: Record<RangeProblem, string> = {
	incomplete: 'Fill in both times.',
	backwards: 'The start has to be before the end.',
	future: 'The range starts in the future.',
	too_old: 'ccdash keeps 30 days of history.'
};

export function spanText(from: number, to: number): string {
	const start = toInputValue(from).replace('T', ' ');
	const end = toInputValue(to).replace('T', ' ');
	return start.slice(0, 10) === end.slice(0, 10)
		? `${start} – ${end.slice(11)}`
		: `${start} – ${end}`;
}

export function quickLabel(range: Range): string {
	if (range.kind === 'days') return range.days === 1 ? 'Today' : `Last ${range.days} days`;
	if (range.kind === 'absolute') return spanText(range.from, range.to);
	const m = range.minutes;
	if (m % 1440 === 0) return m === 1440 ? 'Last 24 hours' : `Last ${m / 1440} days`;
	if (m % 60 === 0) return m === 60 ? 'Last hour' : `Last ${m / 60} hours`;
	return `Last ${m} minutes`;
}

export const rangeLabel = quickLabel;

export const sameRange = (a: Range, b: Range) => encodeRange(a) === encodeRange(b);

/** `since` and `until` for the API; ranges that reach now leave `until` to the server. */
export function rangeParams(range: Range, t: number): { since: number; until?: number } {
	const { from, to } = resolveRange(range, t);
	return reachesNow(range, t)
		? { since: Math.round(from) }
		: { since: Math.round(from), until: Math.round(to) };
}

/** `6h`, `90m`, `7d` (whole local days) or `<from>-<to>` in ms, for the URL. */
export function encodeRange(range: Range): string {
	if (range.kind === 'days') return `${range.days}d`;
	if (range.kind === 'absolute') return `${Math.round(range.from)}-${Math.round(range.to)}`;
	return range.minutes % 60 === 0 ? `${range.minutes / 60}h` : `${range.minutes}m`;
}

export function decodeRange(text: string): Range | null {
	const quick = /^(\d+)([mhd])$/.exec(text);
	if (quick) {
		const n = Number(quick[1]);
		if (!n) return null;
		if (quick[2] === 'd') return { kind: 'days', days: Math.min(n, 31) };
		const minutes = quick[2] === 'h' ? n * 60 : n;
		return { kind: 'relative', minutes: Math.min(minutes, MAX_RANGE_MINUTES) };
	}
	const abs = /^(\d+)-(\d+)$/.exec(text);
	if (abs) {
		const [from, to] = [Number(abs[1]), Number(abs[2])];
		return from < to ? { kind: 'absolute', from, to } : null;
	}
	return null;
}

export const snapMinute = (t: number) => Math.round(t / MINUTE) * MINUTE;
