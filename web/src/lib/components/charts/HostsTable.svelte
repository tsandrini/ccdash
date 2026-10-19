<script
	lang="ts"
	generics="T extends { host: string; permission_ms: number; your_turn_ms: number }"
>
	// A sortable per-host table under a figure, or nothing when one host
	// covers the data.
	import type { SortColumn } from '#lib/charts.js';
	import Figure from './Figure.svelte';
	import SortableTable from './SortableTable.svelte';

	let {
		hosts,
		title,
		sub,
		columns
	}: { hosts: T[]; title: string; sub: string; columns: SortColumn<Record<string, unknown>>[] } =
		$props();

	const rows = $derived(
		hosts.map((x) => {
			const r = x as T & { lines_added?: number; lines_removed?: number };
			return {
				...x,
				waited: x.permission_ms + x.your_turn_ms,
				lines: (r.lines_added ?? 0) + (r.lines_removed ?? 0)
			} as Record<string, unknown>;
		})
	);
</script>

{#if hosts.length > 1}
	<Figure {title} {sub}>
		<SortableTable {columns} {rows} sortKey="working_ms" />
	</Figure>
{/if}
