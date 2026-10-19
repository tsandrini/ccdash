<script lang="ts">
	// Two figures side by side when there is room.
	import type { Snippet } from 'svelte';

	let { children }: { children: Snippet } = $props();
</script>

<div class="grid grid-cols-[repeat(auto-fit,minmax(min(100%,21rem),1fr))] gap-3">
	{@render children()}
</div>
