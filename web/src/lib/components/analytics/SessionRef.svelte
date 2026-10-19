<script lang="ts">
	// A session's label, linked to its page while the hub still knows it.
	import { live } from '#lib/live/store.svelte.js';

	let { id, text }: { id: string; text: string } = $props();
</script>

{#if live.sessions.has(id)}
	<a class="underline" href="/sessions/{id}">{text}</a>
{:else}
	<span>{text}</span>
{/if}
