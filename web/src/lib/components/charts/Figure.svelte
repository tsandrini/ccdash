<script lang="ts">
	// A chart card: title, subtitle, an optional legend, the chart, and the
	// table view every chart carries (the values without hovering).
	import type { Snippet } from 'svelte';
	import type { LegendItem } from '#lib/charts.js';
	import { cn } from '#lib/utils.js';
	import ChartLegend from './ChartLegend.svelte';
	import TableView from './TableView.svelte';

	let {
		title,
		sub,
		legend,
		empty,
		table,
		class: className,
		children
	}: {
		title: string;
		sub?: string;
		legend?: LegendItem[];
		/** Shown instead of the chart when there is nothing to draw. */
		empty?: string | null | false;
		table?: { headers: string[]; rows: (string | number)[][] } | null;
		class?: string;
		children?: Snippet;
	} = $props();
</script>

<figure class={cn('flex min-w-0 flex-col gap-2 rounded-lg border bg-card p-3', className)}>
	<figcaption>
		<div class="text-sm font-medium">{title}</div>
		{#if sub}<div class="text-xs text-muted-foreground">{sub}</div>{/if}
	</figcaption>
	{#if legend?.length}<ChartLegend items={legend} />{/if}
	{#if empty}
		<p class="text-sm text-muted-foreground">{empty}</p>
	{:else}
		{@render children?.()}
		{#if table}<TableView headers={table.headers} rows={table.rows} />{/if}
	{/if}
</figure>
