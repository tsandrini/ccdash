<script lang="ts">
	// "3m 12s" since a moment, ticking with the store's clock.
	import { fmtDuration } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';

	let {
		since,
		prefix = '',
		class: className = ''
	}: { since: number; prefix?: string; class?: string } = $props();
</script>

<span class="text-muted-foreground tabular-nums {className}"
	>{prefix}{fmtDuration(live.now - since)}</span
>
