<script lang="ts">
	// A small line chart of one measure over the last hour (LayerChart).
	import { LineChart } from 'layerchart';
	import { fmtTime } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';

	let {
		title,
		sub,
		points,
		max,
		format
	}: {
		title: string;
		sub: string;
		points: { ts: number; v: number }[];
		max?: number;
		format: (v: number) => string;
	} = $props();

	const data = $derived(points.map((p) => ({ ts: new Date(p.ts), v: p.v })));
	const top = $derived(Math.max(max ?? 0, ...points.map((p) => p.v)) || 1);
	const since = $derived(new Date(live.now - 3_600_000));
	const until = $derived(new Date(live.now));
</script>

<figure class="rounded-lg border bg-card p-3">
	<figcaption class="mb-2">
		<span class="text-sm font-medium">{title}</span>
		<span class="block text-xs text-muted-foreground">{sub}</span>
	</figcaption>
	<div class="h-40 text-xs">
		<LineChart
			{data}
			x="ts"
			y="v"
			xDomain={[since, until]}
			yDomain={[0, top]}
			series={[{ key: 'v', color: 'var(--color-working)' }]}
			props={{
				xAxis: { format: (d: Date) => fmtTime(d.getTime()), ticks: 4 },
				yAxis: { format, ticks: 3 },
				tooltip: { header: { format: (d: Date) => fmtTime(d.getTime()) } }
			}}
		/>
	</div>
</figure>
