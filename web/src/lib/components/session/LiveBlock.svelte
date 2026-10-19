<script lang="ts">
	// What the session is doing right now, below the newest exchange: the
	// state, the running tool, the streaming text, the turn's facts,
	// subagents and tasks; and the replies Claude has not taken yet.
	import type { Session } from '#lib/api/types.js';
	import { fmtDuration, plural, toolLabel } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import Clock from '#lib/components/app/Clock.svelte';
	import Meter from '#lib/components/app/Meter.svelte';
	import StateBadge from '#lib/components/app/StateBadge.svelte';
	import SubagentList from './SubagentList.svelte';
	import TaskList from './TaskList.svelte';

	const UNDELIVERED_MS = 20_000;

	let { session: s, messageShown = false }: { session: Session; messageShown?: boolean } = $props();

	const busy = $derived(s.activity === 'working' || s.activity === 'permission');
	const said = $derived(
		s.message && s.activity === 'working' && !messageShown ? s.message.text : null
	);
	const facts = $derived.by(() => {
		const t = s.turn;
		if (!t || !busy) return [];
		return [
			t.tools ? plural(t.tools, 'tool') : null,
			t.failures ? `${t.failures} failed` : null,
			t.files.length ? plural(t.files.length, 'file') : null
		].filter(Boolean);
	});
	const tasksDone = $derived(s.tasks.filter((t) => t.status === 'completed').length);
</script>

{#if s.activity !== 'gone' && s.outbox?.length}
	{#each s.outbox as m (m.at)}
		{@const stuck = !busy && live.now - m.at > UNDELIVERED_MS}
		<div class="flex flex-col items-end gap-1">
			<div
				class="max-w-[90%] rounded-lg bg-primary/80 px-3 py-2 text-sm whitespace-pre-wrap text-primary-foreground"
			>
				{m.text}
			</div>
			<time class="text-xs {stuck ? 'text-warning' : 'text-muted-foreground'}"
				>{stuck ? 'Not picked up yet: check the terminal' : 'Sent · waiting for Claude'}</time
			>
		</div>
	{/each}
{/if}
{#if s.activity !== 'gone' && s.hooked && ((busy && !(s.activity === 'permission' && s.asks.length)) || said || s.subagents.length || s.tasks.length)}
	<div
		class="flex flex-col gap-2 rounded-lg border bg-muted/40 p-3 text-sm"
		data-activity={s.activity}
	>
		{#if s.activity === 'permission' && !s.asks.length}
			<div class="flex items-center gap-2">
				<StateBadge activity="permission" />
				<span class="truncate font-mono text-xs">{s.waiting_for ?? ''}</span>
			</div>
		{:else if s.activity === 'working'}
			<div class="flex items-center gap-2">
				<StateBadge activity="working" />
				{#if s.tool}
					<span class="truncate font-mono text-xs">{toolLabel(s.tool)}</span>
					<Clock since={s.tool.started_at} class="text-xs" />
				{:else}
					<span class="text-xs text-muted-foreground">thinking</span>
				{/if}
				{#if s.turn && !s.turn.ended_at}
					<span class="ml-auto text-xs text-muted-foreground"
						>turn <Clock since={s.turn.started_at} /></span
					>
				{/if}
			</div>
		{/if}
		{#if said}
			<p class="text-sm whitespace-pre-wrap">
				<span class="font-medium text-muted-foreground">Claude</span>
				{said}
			</p>
		{/if}
		{#if facts.length}<p class="text-xs text-muted-foreground">{facts.join(' · ')}</p>{/if}
		{#if s.subagents.length}<SubagentList subagents={s.subagents} />{/if}
		{#if s.tasks.length}
			<details>
				<summary class="flex cursor-pointer items-center gap-2 text-xs select-none">
					<Meter pct={(100 * tasksDone) / s.tasks.length} level="neutral" label="tasks" />
					<span class="tabular-nums">{tasksDone}/{s.tasks.length} tasks</span>
					{#if s.tasks.find((t) => t.status === 'in_progress')}
						<span class="truncate text-muted-foreground"
							>{s.tasks.find((t) => t.status === 'in_progress')?.active_form ?? ''}</span
						>
					{/if}
				</summary>
				<TaskList tasks={s.tasks} />
			</details>
		{/if}
	</div>
{/if}
{#if s.activity !== 'gone' && !s.hooked}
	<div class="rounded-lg border bg-muted/40 p-3 text-sm">
		<strong>No hook data for this session.</strong> It started before ccdash was installed, so only
		busy and idle are known. Exit it and run <code class="font-mono">claude --resume {s.id}</code>
		in the same directory to see its conversation and progress. {#if s.activity === 'working'}It is
			busy.{:else}It is idle; {fmtDuration(live.now - s.activity_since)} so far.{/if}
	</div>
{/if}
