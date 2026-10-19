<script lang="ts">
	// The stream's state, in the topbar.
	import { live } from '#lib/live/store.svelte.js';

	const colour = $derived(
		{
			live: 'bg-your-turn',
			connecting: 'bg-warning',
			reconnecting: 'bg-warning animate-pulse',
			offline: 'bg-critical'
		}[live.conn.kind]
	);
</script>

<span class="inline-flex items-center gap-1.5 text-xs text-muted-foreground" role="status">
	<i class="inline-block size-2 rounded-full {colour}" aria-hidden="true"></i>
	<span class="hidden sm:inline">{live.conn.text}</span>
</span>
