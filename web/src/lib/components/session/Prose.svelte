<script lang="ts">
	// Server-rendered markdown (src/markdown.rs, raw HTML escaped), folded
	// when long with a button to unfold; `note:` links (`[[slug]]` in notes)
	// go to `onnote` instead of a new tab.
	import { Button } from '#lib/components/ui/button/index.js';

	let {
		html,
		clamp = false,
		onnote
	}: { html: string; clamp?: boolean; onnote?: (slug: string) => void } = $props();

	let el = $state<HTMLDivElement | null>(null);
	// svelte-ignore state_referenced_locally
	let folded = $state(clamp);
	let overflows = $state(false);

	$effect(() => {
		if (!el) return;
		for (const a of el.querySelectorAll('a')) {
			const note = a.getAttribute('href')?.match(/^note:(.+)$/);
			if (note && onnote) {
				a.onclick = (event) => {
					event.preventDefault();
					onnote(note[1]);
				};
				continue;
			}
			a.target = '_blank';
			a.rel = 'noopener noreferrer';
		}
		overflows = folded && el.scrollHeight > el.clientHeight + 8;
		if (folded && !overflows) folded = false;
	});
</script>

<div bind:this={el} class="prose-chat {folded ? 'clamp' : ''}">
	<!-- eslint-disable-next-line svelte/no-at-html-tags -->
	{@html html}
</div>
{#if folded && overflows}
	<Button variant="link" size="xs" class="h-auto px-0" onclick={() => (folded = false)}
		>Show all</Button
	>
{/if}
