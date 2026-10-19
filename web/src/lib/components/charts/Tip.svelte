<script lang="ts">
	// Tooltip content: the value first, then its context in muted lines.
	import type { TipLines } from '#lib/charts.js';

	let { lines }: { lines: TipLines } = $props();

	const shown = $derived(lines.filter((l): l is string => Boolean(l)));
</script>

<div class="whitespace-nowrap">
	{#each shown as line, i (i)}
		{#if i === 0}
			<strong class="block text-[15px] font-semibold">{line}</strong>
		{:else}
			<div class="text-xs text-muted-foreground">{line}</div>
		{/if}
	{/each}
</div>
