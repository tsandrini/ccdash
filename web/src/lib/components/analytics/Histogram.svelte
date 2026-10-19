<script lang="ts">
	// Cases by duration bin, with the median in the subtitle.
	import type { Histogram } from '#lib/api/types.js';
	import { BIN_LABELS } from '#lib/analytics.js';
	import { COLOR, type Role } from '#lib/charts.js';
	import { fmtDuration, plural } from '#lib/format.js';
	import Columns from '../charts/Columns.svelte';
	import Figure from '../charts/Figure.svelte';

	let { title, hist, role }: { title: string; hist: Histogram; role: Role } = $props();

	const total = $derived(hist.counts.reduce((n, c) => n + c, 0));
	const bins = $derived(
		hist.counts.map((count, i) => ({ key: BIN_LABELS[i], label: BIN_LABELS[i], count }))
	);
</script>

<Figure
	{title}
	sub={total
		? `${plural(total, 'case')}, median ${fmtDuration(hist.median_ms ?? 0)}`
		: 'No data in this range yet'}
	empty={!total && ' '}
	table={{ headers: ['Duration', 'Count'], rows: bins.map((b) => [b.label, b.count]) }}
>
	<Columns
		data={bins}
		series={[{ key: 'count', label: 'Cases', color: COLOR[role], value: (d) => d.count }]}
		fmtY={String}
		height={170}
		tip={(d) => [plural(d.count, 'case'), d.label]}
	/>
</Figure>
