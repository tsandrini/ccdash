<script lang="ts">
	// Tools or commands by total time, with calls, failures and percentiles.
	import type { ToolStat } from '#lib/api/types.js';
	import { COLOR } from '#lib/charts.js';
	import { fmtDuration, plural } from '#lib/format.js';
	import BarRows from '../charts/BarRows.svelte';
	import Figure from '../charts/Figure.svelte';

	let {
		title,
		sub,
		stats,
		label
	}: { title: string; sub: string; stats: ToolStat[]; label: (name: string) => string } = $props();

	const rows = $derived(
		stats.map((t) => ({
			...t,
			label: label(t.name),
			total: t.total_ms,
			parts: [[t.total_ms, COLOR.neutral]] as [number, string][]
		}))
	);
</script>

<Figure
	{title}
	{sub}
	empty={!stats.length && 'Nothing ran in this range.'}
	table={{
		headers: ['Name', 'Calls', 'Failed', 'Total', 'Median', '95%', 'Longest'],
		rows: stats.map((t) => [
			label(t.name),
			t.calls,
			t.failed,
			fmtDuration(t.total_ms),
			fmtDuration(t.p50_ms ?? 0),
			fmtDuration(t.p95_ms ?? 0),
			fmtDuration(t.max_ms ?? 0)
		])
	}}
>
	<BarRows
		{rows}
		valueText={(row) => `${fmtDuration(row.total_ms)} · ${plural(row.calls, 'call')}`}
		tip={(row) => [
			fmtDuration(row.total_ms),
			row.label,
			`median ${fmtDuration(row.p50_ms ?? 0)}, 95% under ${fmtDuration(row.p95_ms ?? 0)}`,
			row.failed ? `${row.failed} failed` : null
		]}
	/>
</Figure>
