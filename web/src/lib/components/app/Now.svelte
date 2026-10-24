<script lang="ts">
	// What needs the owner, with the answer inline and the session's
	// actions (Resolved and Kill pinned); then what is working, the hosts
	// and the limits, and the last progress entries. "Start a session"
	// opens the start sheet in the scope's workspace.
	import PlayIcon from '@lucide/svelte/icons/play';
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchNotes, type Hit } from '#lib/api/index.js';
	import { fmtDuration, fmtStamp, place, plain, plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { scope } from '#lib/scope.svelte.js';
	import { busy, group, label, shown } from '#lib/session.js';
	import { starter } from '#lib/starter.svelte.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import AskCard from './AskCard.svelte';
	import Clock from './Clock.svelte';
	import LimitTile from './LimitTile.svelte';
	import QuickReply from './QuickReply.svelte';
	import SessionActions from './SessionActions.svelte';
	import SessionCard from './SessionCard.svelte';
	import StateBadge from './StateBadge.svelte';

	const UNDELIVERED_MS = 20_000;

	const groups = $derived(group(live.list.filter((s) => scope.covers(s))));
	const holding = $derived(
		groups.all.filter(
			(s) =>
				s.activity !== 'gone' &&
				!groups.needs.includes(s) &&
				((s.outbox?.length &&
					!busy(s) &&
					live.now - s.outbox[s.outbox.length - 1].at > UNDELIVERED_MS) ||
					s.inbox?.length)
		)
	);
	const longest = $derived(
		groups.needs.length
			? live.now - groups.needs.reduce((m, s) => Math.min(m, s.activity_since), Infinity)
			: 0
	);
	const hostCounts = $derived.by(() => {
		const counts: Record<string, number> = {};
		for (const s of groups.all)
			if (s.activity !== 'gone') counts[s.host] = (counts[s.host] ?? 0) + 1;
		return counts;
	});

	let progress = $state<Hit[]>([]);
	let progressTimer: ReturnType<typeof setTimeout> | null = null;

	async function loadProgress() {
		try {
			progress = (await fetchNotes({ kind: 'progress', limit: 8, ...scope.params() })).hits;
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
		}
	}

	$effect(() => {
		void live.notes.n;
		void scope.current.ws;
		if (!live.ready) return;
		if (progressTimer) clearTimeout(progressTimer);
		progressTimer = setTimeout(loadProgress, 300);
	});
</script>

<div class="mx-auto flex max-w-5xl flex-col gap-6 p-3 sm:p-4">
	<section class="flex flex-col gap-3">
		<div class="flex flex-wrap items-center gap-2">
			<h2 class="flex items-baseline gap-2 text-base font-semibold">
				Needs you
				<span class="text-sm font-normal text-muted-foreground">
					{#if groups.needs.length}{plural(groups.needs.length, 'session')} · longest wait {fmtDuration(
							longest
						)}{:else if live.ready}nothing is waiting on you{/if}
				</span>
			</h2>
			<Button
				size="sm"
				class="ml-auto"
				disabled={!live.hosts.some((h) => h.connected)}
				title={live.hosts.some((h) => h.connected) ? undefined : 'No host is connected'}
				onclick={() => starter.show({ ws: scope.current.ws[0] ?? null })}
			>
				<PlayIcon /> Start a session
			</Button>
		</div>
		{#each groups.needs as s (s.id)}
			<div class="flex flex-col gap-2 rounded-lg border bg-card p-3" data-activity={shown(s)}>
				<a href="/sessions/{s.id}" class="flex items-center gap-2 text-sm">
					<StateBadge activity={shown(s)} />
					<span class="truncate font-medium">{label(s)}</span>
					<span class="truncate text-xs text-muted-foreground"
						>{[live.multiHost ? s.host : null, place(s.cwd)].filter(Boolean).join(' · ')}</span
					>
					<Clock since={s.activity_since} class="ml-auto text-xs" />
				</a>
				{#if s.asks.length}
					<AskCard session={s.id} ask={s.asks[0]} count={s.asks.length} />
				{:else}
					{#if s.waiting_for}
						<p class="font-mono text-xs text-permission">{s.waiting_for}</p>
					{:else if s.reply}
						<p class="line-clamp-3 text-sm">
							<span class="font-medium text-muted-foreground">Claude</span>
							{plain(s.reply.text)}
						</p>
					{/if}
					{#if s.hooked && s.kind !== 'background'}
						<QuickReply session={s} />
					{/if}
				{/if}
				<div class="flex justify-end">
					<SessionActions session={s} pin={['resolve', 'kill']} />
				</div>
			</div>
		{/each}
	</section>

	{#if holding.length}
		<section class="flex flex-col gap-2">
			<h2 class="text-base font-semibold">Holding messages</h2>
			{#each holding as s (s.id)}
				<a
					href="/sessions/{s.id}"
					class="flex flex-col gap-1 rounded-lg border bg-card p-3 text-sm"
				>
					<span class="flex items-center gap-2">
						<StateBadge activity={shown(s)} />
						<span class="truncate font-medium">{label(s)}</span>
					</span>
					{#if s.outbox?.length}
						<span class="text-xs text-warning"
							>Reply not picked up yet: check the terminal · {plain(
								s.outbox[s.outbox.length - 1].text
							)}</span
						>
					{/if}
					{#if s.inbox?.length}
						<span class="text-xs text-muted-foreground"
							>{plural(s.inbox.length, 'message')} waiting for its next prompt</span
						>
					{/if}
				</a>
			{/each}
		</section>
	{/if}

	<section class="flex flex-col gap-2">
		<h2 class="flex items-baseline gap-2 text-base font-semibold">
			Working <span class="text-sm font-normal text-muted-foreground">{groups.working.length}</span>
		</h2>
		{#if groups.working.length}
			<div class="grid grid-cols-1 gap-2 sm:grid-cols-2 lg:grid-cols-3">
				{#each groups.working as s (s.id)}
					<SessionCard session={s} compact />
				{/each}
			</div>
		{:else if live.ready}
			<p class="text-sm text-muted-foreground">Nothing is working.</p>
		{/if}
	</section>

	<section class="flex flex-col gap-2">
		<h2 class="text-base font-semibold">Hosts and limits</h2>
		<div class="grid grid-cols-1 gap-2 sm:grid-cols-2 lg:grid-cols-4">
			{#each live.hosts as h (h.name)}
				<div class="flex flex-col gap-1 rounded-lg border bg-card p-3">
					<span class="flex items-center gap-2 text-sm font-medium">
						<i
							class="inline-block size-2 rounded-full {h.connected
								? 'bg-your-turn'
								: 'bg-critical'}"
							aria-hidden="true"
						></i>
						{h.name}
					</span>
					<span class="text-xs text-muted-foreground"
						>{h.connected ? 'connected' : 'offline'} for {fmtDuration(live.now - h.since)} · {plural(
							hostCounts[h.name] ?? 0,
							'session'
						)}</span
					>
				</div>
			{/each}
			<LimitTile name="5-hour limit" limit={live.rateLimits?.five_hour} />
			<LimitTile name="7-day limit" limit={live.rateLimits?.seven_day} />
			{#each live.rateLimits?.scoped ?? [] as l (l.label)}
				<LimitTile name="{l.label} limit" limit={l} />
			{/each}
		</div>
	</section>

	{#if progress.length}
		<section class="flex flex-col gap-2">
			<h2 class="text-base font-semibold">Recent progress</h2>
			<ul class="divide-y rounded-lg border bg-card text-sm">
				{#each progress as p (p.workspace + p.slug)}
					<li class="flex flex-col gap-0.5 px-3 py-2">
						<span class="line-clamp-2">{p.summary || p.excerpt || p.title}</span>
						<span class="text-xs text-muted-foreground"
							>{fmtStamp(p.updated_at)} · {p.workspace}{p.project ? ` / ${p.project}` : ''}</span
						>
					</li>
				{/each}
			</ul>
		</section>
	{/if}
</div>
