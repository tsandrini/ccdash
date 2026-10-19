<script lang="ts">
	import type { Limit, ScopedLimit } from '#lib/api/types.js';
	import { fmtDuration, meterLevel } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import Meter from './Meter.svelte';

	let { name, limit }: { name: string; limit: Limit | ScopedLimit | null | undefined } = $props();

	const pct = $derived(limit ? Math.round(limit.used_pct) : 0);
	const resetsIn = $derived(limit ? limit.resets_at * 1000 - live.now : 0);
</script>

<div class="flex flex-col gap-1 rounded-lg border bg-card p-3">
	<span class="text-xs text-muted-foreground">{name}</span>
	{#if limit}
		<span class="text-lg font-semibold tabular-nums"
			>{pct}%{meterLevel(pct) === 'normal' ? '' : ' ⚠︎'}</span
		>
		<Meter {pct} label={name} class="w-full" />
		<span class="text-xs text-muted-foreground"
			>{resetsIn > 0 ? `resets in ${fmtDuration(resetsIn)}` : ''}</span
		>
	{:else}
		<span class="text-lg text-muted-foreground">–</span>
		<span class="text-xs text-muted-foreground">no statusline data yet</span>
	{/if}
</div>
