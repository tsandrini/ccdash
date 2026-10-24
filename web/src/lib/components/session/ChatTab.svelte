<script lang="ts">
	// The exchanges (prompt, grouped steps, Claude's text, subagents,
	// compactions), messages between sessions at their time, and the live
	// block for what is happening now.
	import type { Item, Session } from '#lib/api/types.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { fmtStamp } from '#lib/format.js';
	import { busy } from '#lib/session.js';
	import ConvItem from './ConvItem.svelte';
	import LiveBlock from './LiveBlock.svelte';
	import MessageItem from './MessageItem.svelte';
	import Steps from './Steps.svelte';
	import type { Convo } from './convo.svelte.js';

	const MESSAGE_SLACK_MS = 3000;
	const GROUPED = new Set(['tool', 'permission', 'answered', 'task']);

	let { convo, session: s }: { convo: Convo; session: Session } = $props();

	type Part =
		| { kind: 'steps'; key: string; items: Item[]; omitted: number }
		| { kind: 'item'; key: string; item: Item };

	/** Steps grouped into runs between Claude's messages. */
	function parts(items: Item[], omitted: number): Part[] {
		const out: Part[] = [];
		let run: Item[] = [];
		let left = omitted;
		const flush = () => {
			if (!run.length) return;
			out.push({ kind: 'steps', key: `s${run[0].id}`, items: run, omitted: left });
			run = [];
			left = 0;
		};
		for (const item of items) {
			if (GROUPED.has(item.kind)) {
				run.push(item);
				continue;
			}
			flush();
			out.push({ kind: 'item', key: `i${item.id}`, item });
		}
		flush();
		return out;
	}

	/** Loaded messages in timeline order: sent ones when sent, received ones when delivered. */
	const queue = $derived(
		convo.messages
			.filter((m) => m.from_session === s.id || m.delivered_at !== null)
			.map((m) => ({ at: m.from_session === s.id ? m.sent_at : m.delivered_at!, m }))
			.sort((a, b) => a.at - b.at)
	);

	type Row =
		| { kind: 'message'; key: string; at: number; m: (typeof queue)[number]['m'] }
		| { kind: 'exchange'; key: string; at: number; ex: Convo['exchanges'][number] };

	// NOTE: a message delivered at a prompt lands a moment after it
	const rows = $derived.by(() => {
		const out: Row[] = [];
		let next = 0;
		const flushBefore = (t: number) => {
			while (next < queue.length && queue[next].at <= t + MESSAGE_SLACK_MS) {
				const q = queue[next++];
				out.push({ kind: 'message', key: `m${q.m.id}`, at: q.at, m: q.m });
			}
		};
		for (const ex of convo.exchanges) {
			flushBefore(ex.at);
			out.push({ kind: 'exchange', key: `e${ex.id}`, at: ex.at, ex });
		}
		flushBefore(Infinity);
		return out;
	});

	const lastReply = $derived(convo.lastReplyId);
	const messageShown = $derived(
		Boolean(
			s.message &&
			convo.exchanges[convo.exchanges.length - 1]?.items.some(
				(it) => it.kind === 'message' && it.at >= s.message!.at
			)
		)
	);
	const lastExchange = $derived(convo.exchanges[convo.exchanges.length - 1]?.id);
</script>

<div class="flex flex-col gap-3">
	{#if convo.more}
		<div class="text-center">
			<Button
				variant="outline"
				size="sm"
				disabled={convo.loading}
				onclick={() => convo.load('older')}>Show earlier messages</Button
			>
		</div>
	{/if}
	{#if convo.loaded && !convo.exchanges.length && s.hooked}
		<p class="text-center text-sm text-muted-foreground">No messages yet.</p>
	{/if}
	{#each rows as row (row.key)}
		{#if row.kind === 'message'}
			<MessageItem message={row.m} session={s} />
		{:else}
			{@const ex = row.ex}
			<section class="flex flex-col gap-2">
				{#if ex.prompt !== null}
					<div class="flex flex-col items-end gap-1">
						<div
							class="max-w-[90%] rounded-lg bg-primary px-3 py-2 text-sm whitespace-pre-wrap text-primary-foreground"
						>
							{ex.prompt}
						</div>
						<time class="text-xs text-muted-foreground"
							>{fmtStamp(ex.at)}{ex.via === 'ccdash' ? ' · sent from ccdash' : ''}</time
						>
					</div>
				{/if}
				{#each parts(ex.items, ex.omitted) as part, i (part.key)}
					{#if part.kind === 'steps'}
						<Steps
							items={part.items}
							omitted={part.omitted}
							open={ex.id === lastExchange &&
								busy(s) &&
								i === parts(ex.items, ex.omitted).length - 1}
						/>
					{:else}
						<ConvItem item={part.item} isLastReply={part.item.id === lastReply} />
					{/if}
				{/each}
			</section>
		{/if}
	{/each}
	{#if s.activity !== 'gone' && s.inbox?.length}
		{#each s.inbox as m (m.id)}
			<MessageItem message={m} session={s} pending />
		{/each}
	{/if}
	<LiveBlock session={s} {messageShown} />
</div>
