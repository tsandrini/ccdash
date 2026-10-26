<script lang="ts">
	// Facts, the current turn and its files, tasks, subagents, processes
	// (with the last hour's CPU and memory), the claim, the messages sent
	// and received, and the session's totals.
	import type { Session } from '#lib/api/types.js';
	import { fmtBytes, fmtDuration, fmtStamp, fmtUsd, known } from '#lib/format.js';
	import ProjectPick from './ProjectPick.svelte';
	import { live } from '#lib/live/store.svelte.js';
	import { children, claimPaths, label, shown } from '#lib/session.js';
	import StateBadge from '#lib/components/app/StateBadge.svelte';
	import Clock from '#lib/components/app/Clock.svelte';
	import MessageItem from './MessageItem.svelte';
	import ResourceChart from './ResourceChart.svelte';
	import SubagentList from './SubagentList.svelte';
	import TaskList from './TaskList.svelte';
	import type { Convo } from './convo.svelte.js';

	let { session: s, convo }: { session: Session; convo: Convo } = $props();

	const agents = $derived(children(live.sessions.values(), s));
	const tasksDone = $derived(s.tasks.filter((t) => t.status === 'completed').length);
	const stats = $derived(convo.stats);
	const messages = $derived([...convo.messages].sort((a, b) => b.sent_at - a.sent_at));

	type Row = [string, string | number | null | undefined];
	const rows = (list: Row[]) => list.filter(([, v]) => known(v) && v !== '');
	const facts = $derived(
		rows([
			['Model', s.model],
			['Effort', s.effort],
			['Permission mode', s.permission_mode],
			['Context', known(s.context_pct) ? `${Math.round(s.context_pct)}%` : null],
			['Cost', s.cost_usd ? `${fmtUsd(s.cost_usd)} (API-equivalent)` : null],
			['Lines', known(s.lines_added) ? `+${s.lines_added} −${s.lines_removed ?? 0}` : null],
			['Kind', s.kind === 'background' ? `background (${s.short_id})` : 'interactive'],
			['Window', s.window ? `niri #${s.window.id}` : null],
			['Started', `${fmtStamp(s.started_at)} (${fmtDuration(live.now - s.started_at)} ago)`],
			['Directory', s.cwd],
			['Workspace', s.workspace],
			[
				'Project',
				s.workspace && (shown(s) === 'gone' || !live.connected(s.host)) ? (s.project ?? '—') : null
			],
			['Host', s.host],
			['Session id', s.id]
		])
	);
</script>

{#snippet dl(list: Row[])}
	<dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
		{#each rows(list) as [k, v] (k)}
			<dt class="text-muted-foreground">{k}</dt>
			<dd class="min-w-0 break-words">{v}</dd>
		{/each}
	</dl>
{/snippet}

<div class="flex flex-col gap-5">
	{#if s.workspace && shown(s) !== 'gone' && live.connected(s.host)}
		<section>
			<h3 class="mb-1 text-sm font-semibold">Project</h3>
			<ProjectPick session={s} />
		</section>
	{/if}
	{#if s.waiting_for}
		<section>
			<h3 class="mb-1 text-sm font-semibold">Waiting for</h3>
			<p class="font-mono text-sm">{s.waiting_for}</p>
		</section>
	{/if}
	{#if s.turn}
		{@const t = s.turn}
		<section>
			<h3 class="mb-1 text-sm font-semibold">{t.ended_at ? 'Last turn' : 'Current turn'}</h3>
			{@render dl([
				['Started', fmtStamp(t.started_at)],
				[
					'Duration',
					t.ended_at
						? fmtDuration(t.ended_at - t.started_at)
						: `${fmtDuration(live.now - t.started_at)} so far`
				],
				['Tool calls', `${t.tools}${t.failures ? ` (${t.failures} failed)` : ''}`],
				['Subagent tool calls', t.subagent_tools || null]
			])}
			{#if t.files.length}
				<h4 class="mt-2 mb-1 text-xs text-muted-foreground">Files edited ({t.files.length})</h4>
				<ul class="font-mono text-xs">
					{#each t.files as f (f)}<li class="break-all">{f}</li>{/each}
				</ul>
			{/if}
		</section>
	{/if}
	{#if s.claim}
		<section>
			<h3 class="mb-1 text-sm font-semibold">Working on</h3>
			{@render dl([
				['What', s.claim.what],
				['Paths', claimPaths(s).join(', ') || null],
				['Since', fmtStamp(s.claim.since)],
				['Lapses', s.claim.until ? fmtStamp(s.claim.until) : null]
			])}
		</section>
	{/if}
	{#if agents.length}
		<section>
			<h3 class="mb-1 text-sm font-semibold">Background agents ({agents.length})</h3>
			<ul class="flex flex-col gap-1 text-sm">
				{#each agents as c (c.id)}
					<li class="flex items-center gap-2">
						<a class="underline" href="/sessions/{c.id}">{label(c)}</a>
						<StateBadge activity={shown(c)} />
						<Clock since={c.activity_since} class="text-xs" />
					</li>
				{/each}
			</ul>
		</section>
	{/if}
	{#if s.subagents.length}
		<section>
			<h3 class="mb-1 text-sm font-semibold">Running subagents ({s.subagents.length})</h3>
			<SubagentList subagents={s.subagents} />
		</section>
	{/if}
	{#if s.tasks.length}
		<section>
			<h3 class="mb-1 text-sm font-semibold">Tasks ({tasksDone}/{s.tasks.length} done)</h3>
			<TaskList tasks={s.tasks} />
		</section>
	{/if}
	{#if s.resources}
		{@const r = s.resources}
		<section class="flex flex-col gap-2">
			<h3 class="text-sm font-semibold">Processes</h3>
			{@render dl([
				['CPU', `${r.cpu_pct}% of one core`],
				['Memory', fmtBytes(r.mem_bytes)],
				['Processes', r.procs]
			])}
			<table class="w-full text-xs">
				<thead class="text-left text-muted-foreground">
					<tr
						><th class="py-1 font-normal">Process</th><th class="font-normal">CPU</th><th
							class="font-normal">Memory</th
						><th class="font-normal">Running</th></tr
					>
				</thead>
				<tbody>
					{#each r.top as p (p.pid)}
						<tr class="border-t align-top">
							<td class="max-w-0 py-1 pr-2">
								<strong>{p.name}</strong>
								{#if p.cmd}<div class="truncate font-mono text-muted-foreground" title={p.cmd}>
										{p.cmd}
									</div>{/if}
							</td>
							<td class="pr-2 tabular-nums">{p.cpu_pct}%</td>
							<td class="pr-2 tabular-nums">{fmtBytes(p.mem_bytes)}</td>
							<td class="tabular-nums">{fmtDuration(live.now - p.started_at)}</td>
						</tr>
					{/each}
				</tbody>
			</table>
			{#if convo.resources && convo.resources.length > 1}
				<div class="grid gap-3 sm:grid-cols-2">
					<ResourceChart
						title="CPU, last hour"
						sub="Average per minute, % of one core"
						points={convo.resources.map(([ts, cpu]) => ({ ts, v: cpu }))}
						max={100}
						format={(v) => `${Math.round(v)}%`}
					/>
					<ResourceChart
						title="Memory, last hour"
						sub="Peak per minute (PSS)"
						points={convo.resources.map(([ts, , mem]) => ({ ts, v: mem }))}
						format={fmtBytes}
					/>
				</div>
			{/if}
		</section>
	{/if}
	{#if messages.length}
		<section class="flex flex-col gap-2">
			<h3 class="text-sm font-semibold">Messages ({messages.length})</h3>
			{#each messages as m (m.id)}
				<MessageItem message={m} session={s} />
			{/each}
		</section>
	{/if}
	{#if stats}
		<section>
			<h3 class="mb-1 text-sm font-semibold">Session totals</h3>
			{@render dl([
				['Prompts', stats.prompts],
				['Tool calls', `${stats.tools}${stats.failures ? ` (${stats.failures} failed)` : ''}`],
				['Files edited', stats.files],
				['Subagents run', stats.subagents || null],
				['Compactions', stats.compactions || null],
				['Working', fmtDuration(stats.working_ms)],
				['Waited on you', fmtDuration(stats.permission_ms + stats.your_turn_ms)],
				['Parked', stats.parked_ms ? fmtDuration(stats.parked_ms) : null]
			])}
		</section>
	{/if}
	<section>
		<h3 class="mb-1 text-sm font-semibold">Session</h3>
		{@render dl(facts)}
	</section>
</div>
