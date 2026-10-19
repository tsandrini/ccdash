<script lang="ts">
	// Most used main-thread tools, by calls.
	import type { History } from '#lib/api/types.js';
	import { COLOR } from '#lib/charts.js';
	import { plural, toolName } from '#lib/format.js';
	import BarRows from '../charts/BarRows.svelte';
	import Figure from '../charts/Figure.svelte';

	let { data }: { data: History } = $props();

	const rows = $derived(
		data.tools.map((t) => ({
			...t,
			label: toolName(t.name),
			total: t.calls,
			parts: [[t.calls, COLOR.neutral]] as [number, string][]
		}))
	);
</script>

<Figure
	title="Tools"
	sub="Most used tools in the main conversation, by calls"
	empty={!rows.length && 'No tool calls in this range.'}
	table={{
		headers: ['Tool', 'Calls', 'Failed'],
		rows: rows.map((r) => [r.label, r.calls, r.failed])
	}}
>
	<BarRows
		{rows}
		valueText={(row) => `${row.calls}${row.failed ? ` · ${row.failed} failed` : ''}`}
		tip={(row) => [
			plural(row.calls, 'call'),
			row.label,
			row.failed ? `${row.failed} failed` : null
		]}
	/>
</Figure>
