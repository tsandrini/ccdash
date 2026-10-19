<script lang="ts">
	// Lines (with a soft area when asked) over time on one scale: a
	// crosshair finds the nearest moment, long gaps break a line, the end
	// of a single series is labelled; a span dragged along it narrows the range.
	import type { Snippet } from 'svelte';
	import {
		Area,
		Axis,
		Chart,
		Circle,
		Highlight,
		Rule,
		Spline,
		Svg,
		Text,
		Tooltip
	} from 'layerchart';
	import { brushFor, niceTicks, tickLabel, timeTicks, type TipLines } from '#lib/charts.js';
	import { snapMinute } from '#lib/range.js';
	import Tip from './Tip.svelte';

	export type LineSeries = {
		key: string;
		label: string;
		color: string;
		/** `[ts, value]`. */
		points: [number, number][];
		area?: boolean;
	};

	let {
		series,
		since,
		until,
		fmt,
		max = null,
		floorMax = 0,
		height = 170,
		endLabel = false,
		tip,
		onbrush,
		extra
	}: {
		series: LineSeries[];
		since: number;
		until: number;
		fmt: (v: number) => string;
		/** Fixed top of the scale (values above it are clamped). */
		max?: number | null;
		/** The scale reaches at least this far. */
		floorMax?: number;
		height?: number;
		/** Mark and label the last point (single series). */
		endLabel?: boolean;
		/** The tooltip's lines for the moment under the pointer. */
		tip: (ts: number) => TipLines;
		onbrush?: (from: number, to: number) => void;
		/** More marks, drawn with the scales. */
		extra?: Snippet<[{ x: (t: number) => number; y: (v: number) => number }]>;
	} = $props();

	const PAD = { top: 8, right: 52, bottom: 22, left: 46 };
	type Pt = { ts: number; v: number | null };

	let width = $state(600);
	const peak = $derived(Math.max(0, ...series.flatMap((s) => s.points.map(([, v]) => v))));
	const yTicks = $derived(niceTicks(0, max ?? Math.max(peak || 1, floorMax * 0.05), 3));
	const top = $derived(yTicks[yTicks.length - 1]);
	const xTicks = $derived(timeTicks(since, until, Math.max(100, width - PAD.left - PAD.right)));
	/** Every moment any series has, for the crosshair. */
	const rows = $derived(
		[...new Set(series.flatMap((s) => s.points.map(([t]) => t)))]
			.sort((a, b) => a - b)
			.map((ts) => ({ ts }))
	);
	/** Each series' points with a gap marker where the line breaks, and its lone points. */
	const drawn = $derived(
		series.map((s) => {
			const steps = s.points
				.slice(1)
				.map(([t], i) => t - s.points[i][0])
				.sort((a, b) => a - b);
			const maxGap = 3 * (steps[steps.length >> 1] || 60_000);
			const pts: Pt[] = [];
			const runs: Pt[][] = [];
			for (const [ts, raw] of s.points) {
				const v = max === null ? raw : Math.min(raw, max);
				const last = pts[pts.length - 1];
				if (last && last.v !== null && ts - last.ts > maxGap) {
					pts.push({ ts: last.ts + 1, v: null });
					runs.push([]);
				}
				if (!runs.length) runs.push([]);
				pts.push({ ts, v });
				runs[runs.length - 1].push({ ts, v });
			}
			const lone = runs.filter((r) => r.length === 1).map((r) => r[0]);
			const last = pts.filter((p) => p.v !== null).at(-1) ?? null;
			return { ...s, pts, lone, last };
		})
	);
	const brush = $derived(
		onbrush ? brushFor(onbrush, (a, b) => [snapMinute(Number(a)), snapMinute(Number(b))]) : false
	);
</script>

<div bind:clientWidth={width} style:height="{height}px" class="text-xs">
	<Chart
		data={rows}
		x="ts"
		xDomain={[since, until]}
		y={() => 0}
		yDomain={[0, top]}
		padding={PAD}
		tooltipContext={{ mode: 'bisect-x' }}
		{brush}
	>
		{#snippet children({ context })}
			{@const x = (t: number) => context.xScale(t) as number}
			{@const y = (v: number) => context.yScale(v) as number}
			<Svg>
				<Axis
					placement="left"
					ticks={yTicks}
					format={fmt}
					grid={{ class: 'stroke-border' }}
					tickMarks={false}
					rule={false}
				/>
				<Rule y={0} class="stroke-border" />
				<Axis
					placement="bottom"
					ticks={xTicks.ticks}
					format={(t: number) => tickLabel(t, xTicks.daily)}
					tickMarks={false}
					rule={false}
				/>
				{#each drawn as s (s.key)}
					{#if s.area}
						<Area
							data={s.pts}
							x="ts"
							y1="v"
							defined={(d: Pt) => d.v !== null}
							fill={s.color}
							fillOpacity={0.1}
						/>
					{/if}
					<Spline
						data={s.pts}
						x="ts"
						y="v"
						defined={(d: Pt) => d.v !== null}
						stroke={s.color}
						strokeWidth={2}
						class="[stroke-linecap:round] [stroke-linejoin:round]"
					/>
					{#each s.lone as p (p.ts)}
						<Circle cx={x(p.ts)} cy={y(p.v ?? 0)} r={3} fill={s.color} />
					{/each}
				{/each}
				{#if endLabel && drawn[0]?.last}
					{@const p = drawn[0].last}
					<Circle
						cx={x(p.ts)}
						cy={y(p.v ?? 0)}
						r={4}
						fill={drawn[0].color}
						class="stroke-card"
						strokeWidth={2}
					/>
					<Text x={x(p.ts) + 8} y={y(p.v ?? 0) + 4} value={fmt(p.v ?? 0)} class="fill-foreground" />
				{/if}
				{@render extra?.({ x, y })}
				{#if rows.length}
					<Highlight axis="x" lines={{ class: 'stroke-border' }} />
				{/if}
			</Svg>
			{#if rows.length}
				<Tooltip.Root {context}>
					{#snippet children({ data: d })}
						<Tip lines={tip(d.ts)} />
					{/snippet}
				</Tooltip.Root>
			{/if}
		{/snippet}
	</Chart>
</div>
