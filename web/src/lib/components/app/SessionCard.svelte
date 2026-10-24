<script lang="ts">
	// A session in a list: state, title, where (with the branch), what it
	// is doing, claim, inbox, a resolved mark, meta, the last hour; its
	// Actions menu beside the link.
	import { resolvedNote } from '#lib/actions.js';
	import type { Lane, Session } from '#lib/api/types.js';
	import { fmtBytes, fmtDuration, fmtUsd, known, plain, plural, toolLabel } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { children, claimPaths, isAgent, label, shown } from '#lib/session.js';
	import Clock from './Clock.svelte';
	import Meter from './Meter.svelte';
	import SessionActions from './SessionActions.svelte';
	import StateBadge from './StateBadge.svelte';
	import Strip from './Strip.svelte';
	import Where from './Where.svelte';

	let {
		session: s,
		lane,
		compact = false
	}: { session: Session; lane?: Lane; compact?: boolean } = $props();

	const parent = $derived(s.parent ? live.sessions.get(s.parent) : undefined);
	const working = $derived(s.activity === 'working');
	const said = $derived.by((): [string, string] | null => {
		const [who, text] =
			working && s.message
				? ['Claude', s.message.text]
				: !working && s.reply
					? ['Claude', s.reply.text]
					: ['You', s.prompt?.text];
		const preview = plain(text);
		return preview ? [who, preview] : null;
	});
	const tasksDone = $derived(s.tasks.filter((t) => t.status === 'completed').length);
	const activeTask = $derived(s.tasks.find((t) => t.status === 'in_progress'));
	const agents = $derived(children(live.sessions.values(), s).length);
	const facts = $derived.by(() => {
		const t = s.turn;
		const out: string[] = [];
		if (t && s.activity !== 'gone') {
			if (t.ended_at) out.push(`turn took ${fmtDuration(t.ended_at - t.started_at)}`);
			if (t.tools) out.push(plural(t.tools, 'tool'));
			if (t.failures) out.push(`${t.failures} failed`);
			if (t.files.length) out.push(plural(t.files.length, 'file'));
		}
		if (s.subagents.length) out.push(plural(s.subagents.length, 'subagent'));
		if (agents) out.push(plural(agents, 'background agent'));
		return out;
	});
	const claim = $derived.by(() => {
		if (!s.claim || s.activity === 'gone') return '';
		const paths = claimPaths(s);
		return [s.claim.what, paths.length ? paths.join(', ') : null].filter(Boolean).join(' · ');
	});
	const resolved = $derived(resolvedNote(s));
	const inbox = $derived(
		s.activity !== 'gone' && s.inbox?.length ? s.inbox[s.inbox.length - 1] : null
	);
</script>

<div class="relative min-w-0">
	<a
		href="/sessions/{s.id}"
		class="flex min-w-0 flex-col gap-1.5 rounded-lg border bg-card p-3 text-left text-sm transition-colors hover:bg-accent/40"
		data-activity={shown(s)}
	>
		<span class="flex items-center justify-between gap-2 pr-7">
			<StateBadge activity={shown(s)} />
			<Clock since={s.activity_since} class="text-xs" />
		</span>
		<span class="truncate font-medium">{label(s)}</span>
		<Where session={s} />
		{#if isAgent(s)}
			<span class="text-xs text-muted-foreground"
				>{parent
					? `Started by ${label(parent)}`
					: s.parent
						? `Started by session ${s.parent.slice(0, 8)}`
						: 'Background session'}</span
			>
		{/if}
		{#if s.hooked || s.activity === 'gone'}
			{#if s.waiting_for}
				<span class="truncate font-mono text-xs text-permission">{s.waiting_for}</span>
			{:else if !compact}
				{#if working && s.tool}
					<span class="flex items-center gap-2 text-xs">
						<span class="truncate font-mono">{toolLabel(s.tool)}</span>
						<Clock since={s.tool.started_at} />
					</span>
				{/if}
				{#if said}
					<span class="line-clamp-2 text-xs"
						><span class="font-medium text-muted-foreground">{said[0]}</span> {said[1]}</span
					>
				{/if}
			{/if}
			{#if s.tasks.length && !compact}
				<span class="flex items-center gap-2 text-xs">
					<Meter pct={(100 * tasksDone) / s.tasks.length} level="neutral" label="tasks" />
					<span class="tabular-nums">{tasksDone}/{s.tasks.length}</span>
					{#if activeTask}<span class="truncate text-muted-foreground"
							>{activeTask.active_form || activeTask.subject}</span
						>{/if}
				</span>
			{/if}
			{#if facts.length && !compact}
				<span class="text-xs text-muted-foreground">{facts.join(' · ')}</span>
			{/if}
			{#if claim}
				<span class="truncate text-xs" title={claim}
					><span class="font-medium text-muted-foreground">Claims</span> {claim}</span
				>
			{/if}
			{#if resolved}
				<span class="text-xs font-medium text-your-turn">{resolved}</span>
			{/if}
			{#if inbox}
				<span class="truncate text-xs" title={inbox.text}
					><span class="font-medium text-muted-foreground"
						>{plural(s.inbox.length, 'message')} waiting</span
					>
					· from {inbox.from_label}: {plain(inbox.text)}</span
				>
			{/if}
			{#if !compact}
				<span class="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
					{#if known(s.context_pct)}
						<span class="inline-flex items-center gap-1.5"
							><Meter pct={s.context_pct} label="context" /> context {Math.round(
								s.context_pct
							)}%</span
						>
					{/if}
					{#if s.cost_usd}<span>{fmtUsd(s.cost_usd)}</span>{/if}
					{#if s.resources}
						<span title={plural(s.resources.procs, 'process', 'processes')}
							>{s.resources.cpu_pct}% CPU</span
						>
						<span>{fmtBytes(s.resources.mem_bytes)}</span>
					{/if}
				</span>
			{/if}
		{:else}
			<span class="text-xs text-muted-foreground"
				>Started before ccdash was installed, so only busy and idle are known. Resume it to see
				details.</span
			>
		{/if}
		<Strip session={s} {lane} />
	</a>
	<div class="absolute top-2 right-2"><SessionActions session={s} /></div>
</div>
