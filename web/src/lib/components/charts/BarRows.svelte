<script lang="ts" generics="T extends { label: string; total: number; parts: [number, string][] }">
	// Horizontal bars, one row per entry: the label above the bar, the value
	// after it; `parts` are stacked `[value, colour]` pairs.
	import { Chart, Rect, Svg, Text, Tooltip } from 'layerchart';
	import { ellipsize, type TipLines } from '#lib/charts.js';
	import Tip from './Tip.svelte';

	let {
		rows,
		valueText,
		tip
	}: {
		rows: T[];
		valueText: (row: T) => string;
		/** The tooltip's lines for one part of a row. */
		tip: (row: T, value: number, part: number) => TipLines;
	} = $props();

	const ROW_H = 34;
	const BAR_H = 12;

	type Bar = {
		row: T;
		i: number;
		part: number;
		value: number;
		color: string;
		x: number;
		w: number;
	};

	let width = $state(600);
	/** Room for the longest value label after its bar. */
	const valueW = $derived(Math.max(92, 7 * Math.max(0, ...rows.map((r) => valueText(r).length))));
	const plotW = $derived(Math.max(80, width - valueW));
	const max = $derived(Math.max(...rows.map((r) => r.total), 1));
	const bars = $derived(
		rows.flatMap((row, i) => {
			let x = 0;
			return row.parts
				.map(([value, color], part) => {
					const w = (Math.max(0, value) / max) * plotW;
					const bar: Bar = { row, i, part, value, color, x, w };
					x += w;
					return bar;
				})
				.filter((b) => b.value > 0);
		})
	);
	const ends = $derived(
		rows.map((r) => (r.parts.reduce((n, [v]) => n + Math.max(0, v), 0) / max) * plotW)
	);
	const maxChars = $derived(Math.floor(width / 7.2));
</script>

<div bind:clientWidth={width} style:height="{rows.length * ROW_H}px" class="text-xs">
	<Chart data={bars} padding={0} tooltipContext={{ mode: 'manual' }}>
		{#snippet children({ context })}
			<Svg>
				{#each rows as row, i (i)}
					<Text
						x={0}
						y={i * ROW_H + 12}
						value={ellipsize(row.label, maxChars)}
						class="fill-muted-foreground"
					/>
					<Text x={ends[i] + 6} y={i * ROW_H + 27} value={valueText(row)} class="fill-foreground" />
				{/each}
				{#each bars as b (`${b.i}-${b.part}`)}
					<Rect
						x={b.x}
						y={b.i * ROW_H + 17}
						width={Math.max(1, b.w - 2)}
						height={BAR_H}
						rx={Math.min(3, b.w / 2)}
						fill={b.color}
						class="hover:brightness-110"
						onpointermove={(e) => context.tooltip.show(e, b)}
						onpointerleave={() => context.tooltip.hide()}
					/>
				{/each}
			</Svg>
			<Tooltip.Root {context}>
				{#snippet children({ data })}
					<Tip lines={tip(data.row, data.value, data.part)} />
				{/snippet}
			</Tooltip.Root>
		{/snippet}
	</Chart>
</div>
