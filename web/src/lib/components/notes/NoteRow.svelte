<script lang="ts">
	// A note in a list: kind, title, where, status, one line. A button when
	// it opens the panel, a link otherwise.
	import type { Card } from '#lib/api/types.js';
	import { fmtStamp } from '#lib/format.js';
	import { line } from '#lib/notes.js';
	import KindBadge from './KindBadge.svelte';
	import StatusBadge from './StatusBadge.svelte';

	let {
		card,
		href,
		onclick,
		kind = false,
		workspace = false,
		project = false,
		date = false,
		reason = false,
		snippet
	}: {
		card: Card;
		href?: string;
		onclick?: () => void;
		kind?: boolean;
		workspace?: boolean;
		project?: boolean;
		date?: boolean;
		reason?: boolean;
		/** An FTS snippet with `[term]` marks, instead of the summary. */
		snippet?: string;
	} = $props();

	const where = $derived(
		[workspace ? card.workspace : null, project ? card.project : null].filter(Boolean).join(' / ')
	);
	const text = $derived(reason && card.stale_reason ? card.stale_reason : line(card));
	const parts = $derived(snippet ? snippet.split(/\[([^\]]+)\]/) : null);
</script>

{#snippet inner()}
	<span class="flex flex-wrap items-center gap-x-2 gap-y-0.5">
		{#if kind}<KindBadge kind={card.kind} />{/if}
		{#if date}<span class="font-mono text-xs text-muted-foreground"
				>{fmtStamp(card.updated_at)}</span
			>{/if}
		{#if card.kind === 'issue' && card.priority !== 'normal'}
			<span
				class="text-[10px] uppercase {card.priority === 'high'
					? 'text-permission'
					: 'text-muted-foreground'}">{card.priority}</span
			>
		{/if}
		<span class="font-medium">{card.title}</span>
		<StatusBadge note={card} />
		{#if where}<span class="text-xs text-muted-foreground">{where}</span>{/if}
	</span>
	{#if parts}
		<span class="line-clamp-2 text-xs text-muted-foreground">
			{#each parts as part, i (i)}{#if i % 2}<mark
						class="rounded bg-warning/30 px-0.5 text-foreground">{part}</mark
					>{:else}{part}{/if}{/each}
		</span>
	{:else if text}
		<span class="line-clamp-2 text-xs text-muted-foreground">{text}</span>
	{/if}
{/snippet}

{#if onclick}
	<button
		type="button"
		class="flex w-full flex-col gap-0.5 px-3 py-2 text-left text-sm hover:bg-accent/40"
		{onclick}>{@render inner()}</button
	>
{:else}
	<a class="flex flex-col gap-0.5 px-3 py-2 text-sm hover:bg-accent/40" {href}>{@render inner()}</a>
{/if}
