<script lang="ts">
	// A row of stat tiles that wraps.
	import type { Snippet } from 'svelte';

	let { children }: { children: Snippet } = $props();
</script>

<div class="grid grid-cols-[repeat(auto-fit,minmax(10rem,1fr))] gap-2">
	{@render children()}
</div>
