<script lang="ts">
	// One item of an exchange that is not a step: Claude's text, a reply,
	// an error, a subagent, a compaction, a start or end divider.
	import type { Item } from '#lib/api/types.js';
	import { fmtDuration, fmtStamp, fmtTime, known, plural } from '#lib/format.js';
	import Prose from './Prose.svelte';

	let { item, isLastReply = false }: { item: Item; isLastReply?: boolean } = $props();

	const startText = (source: string) =>
		({ startup: 'Session started', resume: 'Session resumed', clear: 'Conversation cleared' })[
			source
		] ?? `Session started (${source})`;
</script>

{#if item.kind === 'message'}
	<div class="text-sm"><Prose html={item.html} clamp /></div>
{:else if item.kind === 'reply'}
	<div class="flex flex-col gap-1">
		<div class="rounded-lg border bg-card p-3 text-sm">
			<Prose html={item.html} clamp={!isLastReply} />
		</div>
		<time class="text-xs text-muted-foreground">{fmtStamp(item.at)}</time>
	</div>
{:else if item.kind === 'error'}
	<div class="rounded-md border border-current/30 p-2 text-sm text-critical">
		<strong>Turn failed: </strong>{item.error}
	</div>
{:else if item.kind === 'subagent'}
	<details class="rounded-md border bg-muted/40 text-sm">
		<summary class="flex cursor-pointer flex-wrap items-center gap-2 px-2.5 py-1.5 select-none">
			<span class="rounded bg-muted px-1.5 text-xs">{item.agent_type || 'Subagent'}</span>
			<span>{item.description || 'finished'}</span>
			<span class="text-xs text-muted-foreground"
				>{[
					known(item.tools) ? plural(item.tools, 'tool call') : null,
					known(item.ms) ? fmtDuration(item.ms) : null
				]
					.filter(Boolean)
					.join(' · ')}</span
			>
		</summary>
		<div class="px-2.5 pb-2">
			{#if item.html}<Prose html={item.html} />{:else}<p class="text-xs text-muted-foreground">
					No final message.
				</p>{/if}
		</div>
	</details>
{:else if item.kind === 'compact'}
	<details class="text-xs text-muted-foreground">
		<summary class="cursor-pointer select-none"
			>Context compacted{item.trigger ? ` (${item.trigger})` : ''} · {fmtTime(item.at)}</summary
		>
		{#if item.html}<div class="mt-1"><Prose html={item.html} /></div>{/if}
	</details>
{:else if item.kind === 'start' && item.source !== 'compact'}
	<div class="text-center text-xs text-muted-foreground">
		{startText(item.source)} · {fmtStamp(item.at)}
	</div>
{:else if item.kind === 'end'}
	<div class="text-center text-xs text-muted-foreground">
		Session ended ({item.reason}) · {fmtStamp(item.at)}
	</div>
{/if}
