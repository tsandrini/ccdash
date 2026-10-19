<script lang="ts">
	// The History timeline: one lane per session, its stretches coloured by
	// state; a span dragged across it narrows the range.
	import { Chart, Rect, Rule, Svg, Text, Tooltip } from 'layerchart';
	import type { Activity, History, Lane } from '#lib/api/types.js';
	import { brushFor, COLOR, ellipsize, hostTag, tickLabel, timeTicks } from '#lib/charts.js';
	import { fmtDuration, fmtTime, place } from '#lib/format.js';
	import { snapMinute } from '#lib/range.js';
	import { ACTIVITY } from '#lib/session.js';
	import Tip from './Tip.svelte';

	let { data, onbrush }: { data: History; onbrush: (from: number, to: number) => void } = $props();

	const PAD_TOP = 18;
	const LANE_H = 36;
	const BAR_H = 12;

	type Seg = { lane: Lane; i: number; from: number; to: number; activity: Activity };

	let width = $state(600);
	const segs = $derived(
		data.lanes.flatMap((lane, i) =>
			lane.segments.map(([from, to, activity]): Seg => ({
				lane,
				i,
				from: Math.max(from, data.since),
				to,
				activity
			}))
		)
	);
	const height = $derived(PAD_TOP + data.lanes.length * LANE_H);
	const ticks = $derived(timeTicks(data.since, data.until, width));
	const maxChars = $derived(Math.floor(width / 7.2));
	const laneText = (lane: Lane) =>
		`${lane.background ? 'bg · ' : ''}${lane.label} · ${place(lane.cwd)}${hostTag(data, lane.host)}`;
	const brush = $derived(
		brushFor(onbrush, (a, b) => [snapMinute(Number(a)), snapMinute(Number(b))])
	);
</script>

<div bind:clientWidth={width} style:height="{height}px" class="text-xs">
	<Chart
		data={segs}
		x="from"
		xDomain={[data.since, data.until]}
		padding={{ top: PAD_TOP }}
		tooltipContext={{ mode: 'manual' }}
		{brush}
	>
		{#snippet children({ context })}
			<Svg>
				{#each ticks.ticks as t (t)}
					{@const x = context.xScale(t)}
					<Rule x={t} class="stroke-border" />
					{#if x > width - 70}
						<Text
							x={x - 3}
							y={-7}
							value={tickLabel(t, ticks.daily)}
							textAnchor="end"
							class="fill-muted-foreground"
						/>
					{:else}
						<Text
							x={x + 3}
							y={-7}
							value={tickLabel(t, ticks.daily)}
							class="fill-muted-foreground"
						/>
					{/if}
				{/each}
				{#each data.lanes as lane, i (lane.id)}
					<Text
						x={0}
						y={i * LANE_H + 13}
						value={ellipsize(laneText(lane), maxChars)}
						class="fill-muted-foreground"
					/>
				{/each}
				{#each segs as seg, i (i)}
					{@const x0 = context.xScale(seg.from)}
					{@const w = Math.max(1, context.xScale(seg.to) - x0 - 2)}
					<Rect
						x={x0}
						y={seg.i * LANE_H + 18}
						width={w}
						height={BAR_H}
						rx={Math.min(3, w / 2)}
						fill={COLOR[seg.activity]}
						class="hover:brightness-110"
						onpointermove={(e) => context.tooltip.show(e, seg)}
						onpointerleave={() => context.tooltip.hide()}
					/>
				{/each}
			</Svg>
			<Tooltip.Root {context}>
				{#snippet children({ data })}
					{@const seg = data as Seg}
					<Tip
						lines={[
							fmtDuration(seg.to - seg.from),
							`${ACTIVITY[seg.activity].label} · ${fmtTime(seg.from)}–${fmtTime(seg.to)}`,
							seg.lane.label
						]}
					/>
				{/snippet}
			</Tooltip.Root>
		{/snippet}
	</Chart>
</div>
