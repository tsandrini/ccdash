// Shared by the History and Analytics charts: series colours (the theme's
// tokens), tick placement, formatters, and the brush that narrows the range.

import type { Activity } from './api/types';
import { fmtTime } from './format';
import { ACTIVITY } from './session';

/**
 * Series colours: the session states, then slots 4–6 of the validated
 * palette (the theme's own variables: Tailwind only emits the `--color-*`
 * ones a utility class uses).
 */
export const COLOR = {
	working: 'var(--working)',
	permission: 'var(--permission)',
	your_turn: 'var(--your-turn)',
	parked: 'var(--parked)',
	idle: 'var(--idle)',
	gone: 'transparent',
	tools: 'var(--series-4)',
	input: 'var(--series-4)',
	cache_write: 'var(--series-5)',
	output: 'var(--series-6)',
	/** Single-series bars that mean no state. */
	neutral: 'var(--muted-foreground)'
} as const;
export type Role = keyof typeof COLOR;

export type LegendItem = { label: string; color: string; line?: boolean };

export const STATES: Activity[] = ['working', 'permission', 'your_turn'];

export const stateLegend = (states: Activity[]): LegendItem[] =>
	states.map((s) => ({ label: ACTIVITY[s].label, color: COLOR[s] }));

export const roleLegend = (items: [Role, string][], line = false): LegendItem[] =>
	items.map(([role, label]) => ({ label, color: COLOR[role], line }));

/** One stacked part of a column: how it is read off a row, and its colour. */
export type ColumnSeries<T> = {
	key: string;
	label: string;
	color: string;
	value: (d: T) => number;
};

export type TipLines = (string | null | undefined | false)[];

export const dayFmt = new Intl.DateTimeFormat(undefined, { weekday: 'short', day: 'numeric' });
export const shortDay = new Intl.DateTimeFormat(undefined, { day: 'numeric', month: 'numeric' });
export const compact = new Intl.NumberFormat(undefined, {
	notation: 'compact',
	maximumFractionDigits: 1
});

export const fmtTokens = (n: number) => compact.format(n);
export const pct = (v: number) => `${Math.round(v)}%`;
/** Axis labels for counts: whole numbers only. */
export const fmtCount = (v: number) => (Number.isInteger(v) ? String(v) : '');
/** Hours on an axis: `2h`, `30m`. */
export const fmtHoursAxis = (v: number) =>
	v === 0 ? '0' : v >= 1 ? `${v}h` : `${Math.round(v * 60)}m`;
export const fmtCpu = (secs: number) =>
	secs >= 3600 ? `${(secs / 3600).toFixed(1)} core-h` : `${Math.round(secs / 60)} core-min`;
export const fmtSigned = (v: number) => (v > 0 ? `+${v}` : v < 0 ? `−${-v}` : '0');

/** A moment with its day unless it is today. */
export const fmtWhen = (ts: number, now: number) =>
	new Date(ts).toDateString() === new Date(now).toDateString()
		? fmtTime(ts)
		: `${dayFmt.format(ts)} ${fmtTime(ts)}`;

/** Byte points rescaled to whole MB or GB for a readable axis. */
export function byteSeries(points: [number, number][]) {
	const peak = Math.max(0, ...points.map(([, v]) => v));
	const [scale, unit] = peak >= 2 ** 30 ? [2 ** 30, 'GB'] : [2 ** 20, 'MB'];
	return {
		points: points.map(([t, v]) => [t, v / scale] as [number, number]),
		fmt: (v: number) => `${Number(v.toFixed(2))} ${unit}`,
		floorMax: unit === 'GB' ? 1 : 256
	};
}

const TICK_STEPS = [15, 30, 60, 120, 180, 360, 720, 1440, 2880, 4320, 10080].map((m) => m * 60_000);

/** Tick times across a span, from a round hour or midnight, about one per 110 px. */
export function timeTicks(
	since: number,
	until: number,
	width: number
): { ticks: number[]; daily: boolean } {
	const maxTicks = Math.max(3, Math.floor(width / 110));
	const step =
		TICK_STEPS.find((st) => (until - since) / st <= maxTicks) ?? TICK_STEPS[TICK_STEPS.length - 1];
	const first = new Date(since);
	first.setMinutes(0, 0, 0);
	if (step >= 86_400_000) first.setHours(0);
	else if (step > 3_600_000)
		first.setHours(first.getHours() - (first.getHours() % (step / 3_600_000)));
	const ticks: number[] = [];
	for (let t = first.getTime(); t <= until; t += step) if (t >= since) ticks.push(t);
	return { ticks, daily: step >= 86_400_000 };
}

export const tickLabel = (t: number, daily: boolean) => (daily ? dayFmt.format(t) : fmtTime(t));

/** Round tick values covering `lo..hi` (both inclusive of 0). */
export function niceTicks(lo: number, hi: number, count = 4): number[] {
	const span = hi - lo || 1;
	const raw = span / count;
	const mag = 10 ** Math.floor(Math.log10(raw));
	const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((st) => st >= raw) ?? mag;
	const ticks: number[] = [];
	for (let t = Math.floor(lo / step) * step; t <= hi + step / 2; t += step)
		ticks.push(Number(t.toFixed(10)));
	if (ticks[ticks.length - 1] < hi) ticks.push(ticks[ticks.length - 1] + step);
	return ticks;
}

export const ellipsize = (text: string, max: number) =>
	text.length > max ? `${text.slice(0, max - 1)}…` : text;

/** Whether the payload spans more than one machine (merged view). */
export const spansHosts = (data: { hosts: { host: string }[] }) => data.hosts.length > 1;
/** ` · host` for per-session rows of a merged view. */
export const hostTag = (data: { hosts: { host: string }[] }, host: string) =>
	spansHosts(data) ? ` · ${host}` : '';

/** Brushing is for a mouse or pen: a finger keeps scrolling the page. */
export const coarsePointer = () =>
	typeof matchMedia === 'function' && matchMedia('(pointer: coarse)').matches;

type BrushEnd = { brush: { active: boolean | undefined; x: unknown[]; reset: () => void } };

/**
 * The chart's `brush` prop: a span dragged along x picks a range. `edges`
 * turns the selection's ends into times (bucket keys into their bounds).
 */
export function brushFor(
	onselect: (from: number, to: number) => void,
	edges: (a: unknown, b: unknown) => [number, number] = (a, b) => [Number(a), Number(b)]
) {
	if (coarsePointer()) return false;
	return {
		axis: 'x' as const,
		// NOTE: the simplified charts zoom to the selection unless told not to
		zoomOnBrush: false,
		onBrushEnd({ brush }: BrushEnd) {
			if (!brush.active || brush.x[0] == null || brush.x[1] == null) return;
			const [from, to] = edges(brush.x[0], brush.x[1]);
			brush.reset();
			if (to > from) onselect(from, to);
		}
	};
}

/** A sortable table's column: text columns sort by name, ascending first. */
export type SortColumn<T> = {
	key: string;
	label: string;
	fmt?: (value: unknown, row: T) => string;
	text?: boolean;
};
