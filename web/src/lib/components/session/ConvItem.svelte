<script lang="ts">
	// One item of an exchange that is not a step: Claude's text, a reply,
	// an error, a subagent, a compaction, a start or end divider, the seam
	// where the conversation went on as a copy of an earlier session.
	import FileChartColumnIcon from '@lucide/svelte/icons/file-chart-column';
	import OctagonXIcon from '@lucide/svelte/icons/octagon-x';
	import type { Item } from '#lib/api/types.js';
	import { fmtDuration, fmtStamp, fmtTime, known, plural } from '#lib/format.js';
	import { reportHref } from '#lib/notes.js';
	import { modelName } from '#lib/past.js';
	import Prose from './Prose.svelte';

	let {
		item,
		isLastReply = false,
		own = ''
	}: {
		item: Item;
		isLastReply?: boolean;
		/** The title of the session whose chat this is. */
		own?: string;
	} = $props();

	const startText = (source: string) =>
		({ startup: 'Session started', resume: 'Session resumed', clear: 'Conversation cleared' })[
			source
		] ?? `Session started (${source})`;

	// NOTE: one model reads `claude-opus-5-5[1m]` from a hook and `Opus 5.5 (1M context)` from the statusline
	const family = (model: string) => model.replace(/\s*[([].*$/, '');
	const step = (from: string | null, to: string | null, same = from === to) =>
		from && to && !same ? `${from} → ${to}` : (to ?? from);

	/** What a copy changed: the model and the effort, each as "old → new" when it differs. */
	function ranOn(item: Extract<Item, { kind: 'continued' }>): string {
		const [from, to] = [modelName(item.from_model), modelName(item.model)];
		const model = step(from, to, Boolean(from && to && family(from) === family(to)));
		const effort = step(item.from_effort, item.effort);
		return [model, effort && `${effort} effort`].filter(Boolean).join(' · ');
	}
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
{:else if item.kind === 'continued'}
	{@const ran = ranOn(item)}
	<div class="flex items-center gap-3 py-1 text-xs text-muted-foreground" role="separator">
		<span class="h-px flex-1 bg-border"></span>
		<span class="flex max-w-[80%] flex-col items-center gap-0.5 text-center">
			<span>
				Continued as a copy of
				<a
					class="underline underline-offset-2"
					href={item.known ? `/sessions/${item.session}` : `/sessions/past?id=${item.session}`}
					title="Open the earlier session ({item.session})"
					>{item.title && item.title !== own
						? item.title
						: `session ${item.session.slice(0, 8)}`}</a
				>
				· {fmtStamp(item.at)}
			</span>
			{#if ran}<span class="font-medium text-foreground">{ran}</span>{/if}
		</span>
		<span class="h-px flex-1 bg-border"></span>
	</div>
{:else if item.kind === 'interrupted'}
	<div class="flex items-center gap-1.5 text-xs font-medium text-warning">
		<OctagonXIcon class="size-3.5" />
		Interrupted from ccdash · {fmtTime(item.at)}
	</div>
{:else if item.kind === 'report'}
	<div class="flex flex-col items-start gap-1">
		<span class="flex items-center gap-1 text-xs text-muted-foreground">
			<FileChartColumnIcon class="size-3.5 text-kind-report" />
			{item.replaced ? 'Replaced a report' : 'Published a report'} · {fmtStamp(item.at)}
		</span>
		<a
			href={reportHref(item.workspace, item.slug)}
			class="flex max-w-[90%] flex-wrap items-center gap-x-3 gap-y-1 rounded-lg border border-kind-report/40 bg-kind-report/10 px-3 py-2 text-sm hover:bg-kind-report/20"
		>
			<span class="font-medium">{item.title}</span>
			<span class="text-xs text-muted-foreground">Open in Reports ↗</span>
		</a>
	</div>
{/if}
