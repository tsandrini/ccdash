<script lang="ts">
	// Swatches for the series a chart draws (a short stroke for lines).
	import type { LegendItem } from '#lib/charts.js';

	let { items }: { items: LegendItem[] } = $props();
</script>

<div class="flex flex-wrap gap-x-3 gap-y-1 text-xs text-muted-foreground">
	{#each items as item (item.label)}
		<span class="inline-flex items-center gap-1.5">
			<i
				class={item.line ? 'h-0.5 w-3 rounded' : 'size-2.5 rounded-sm'}
				style:background={item.color}
				aria-hidden="true"
			></i>{item.label}
		</span>
	{/each}
</div>
