<script lang="ts" generics="T extends { key: string; label: string }">
	// Stacked columns, one per bucket or bin (negative parts hang below the
	// baseline); the tooltip lists every part of the column under the
	// pointer; a span dragged across the columns narrows the range.
	import { BarChart, Tooltip } from 'layerchart';
	import { brushFor, type ColumnSeries, type TipLines } from '#lib/charts.js';
	import Tip from './Tip.svelte';

	let {
		data,
		series,
		fmtY,
		height = 190,
		tip,
		onbrush,
		edges
	}: {
		data: T[];
		series: ColumnSeries<T>[];
		fmtY: (v: number) => string;
		height?: number;
		/** The tooltip's lines for a column. */
		tip: (d: T) => TipLines;
		/** Brushed columns narrow the range; `edges` gives a column's `[from, to]`. */
		onbrush?: (from: number, to: number) => void;
		edges?: (d: T) => [number, number];
	} = $props();

	let width = $state(600);
	const plotW = $derived(Math.max(100, width - 52));
	/** Bars at most 24 px thick, and never thinner than 3 px. */
	const bandPadding = $derived(Math.min(0.66, Math.max(0.1, 1 - (24 * data.length) / plotW)));
	const every = $derived(Math.max(1, Math.ceil((data.length * 52) / plotW)));
	const byKey = $derived(Object.fromEntries(data.map((d) => [d.key, d])));
	const yExtent = $derived.by(() => {
		let lo = 0;
		let hi = 0;
		for (const d of data) {
			let up = 0;
			let down = 0;
			for (const s of series) {
				const v = s.value(d);
				if (v > 0) up += v;
				else down += v;
			}
			hi = Math.max(hi, up);
			lo = Math.min(lo, down);
		}
		return [lo, hi || 1] as [number, number];
	});
	const brush = $derived(
		onbrush && edges
			? brushFor(onbrush, (a, b) => [
					edges(byKey[String(a)] ?? data[0])[0],
					edges(byKey[String(b)] ?? data[data.length - 1])[1]
				])
			: false
	);
</script>

<div bind:clientWidth={width} style:height="{height}px" class="text-xs">
	<BarChart
		{data}
		x="key"
		series={series.map((s) => ({ key: s.key, label: s.label, color: s.color, value: s.value }))}
		seriesLayout="stack"
		{bandPadding}
		yDomain={yExtent}
		yNice
		yBaseline={0}
		padding={{ top: 10, right: 6, bottom: 24, left: 46 }}
		{brush}
		props={{
			xAxis: {
				ticks: data.filter((_, i) => i % every === 0).map((d) => d.key),
				format: (key: string) => byKey[key]?.label ?? key,
				tickMarks: false,
				rule: false
			},
			yAxis: { format: fmtY, ticks: 4, tickMarks: false, rule: false },
			bars: { radius: 3, strokeWidth: 0 }
		}}
	>
		{#snippet tooltip({ context })}
			<Tooltip.Root {context}>
				{#snippet children({ data: d })}
					<Tip lines={tip(d)} />
				{/snippet}
			</Tooltip.Root>
		{/snippet}
	</BarChart>
</div>
