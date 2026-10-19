<script lang="ts">
	// The project's live sessions with their claims and the files they are
	// editing, and the messages between them in the last hour.
	import { untrack } from 'svelte';
	import type { Message, Session } from '#lib/api/types.js';
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchSession } from '#lib/api/index.js';
	import Clock from '#lib/components/app/Clock.svelte';
	import StateBadge from '#lib/components/app/StateBadge.svelte';
	import { fmtStamp, place, plain, toolLabel } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { notePanel } from '#lib/notepanel.svelte.js';
	import { claimPaths, label, shown } from '#lib/session.js';

	const HOUR_MS = 3_600_000;

	let { ws, project }: { ws: string; project: string | null } = $props();

	const sessions = $derived(
		live.list
			.filter(
				(s) =>
					s.activity !== 'gone' &&
					s.workspace === ws &&
					(project === null || (s.project ?? null) === project)
			)
			.sort((a, b) => b.last_event_at - a.last_event_at)
	);
	const ids = $derived(
		sessions
			.map((s) => s.id)
			.sort()
			.join()
	);

	let messages = $state<Message[]>([]);

	async function loadMessages(list: Session[]) {
		const seen: Record<number, Message> = {};
		await Promise.all(
			list.map(async (s) => {
				try {
					const page = await fetchSession(s.id, { limit: 1 });
					for (const m of page.messages ?? []) seen[m.id] = m;
				} catch (e) {
					if (e instanceof Unauthorized) live.needToken = true;
				}
			})
		);
		const since = live.now - HOUR_MS;
		messages = Object.values(seen)
			.filter((m) => m.sent_at >= since)
			.sort((a, b) => b.sent_at - a.sent_at);
	}

	$effect(() => {
		void ids;
		const list = untrack(() => sessions);
		loadMessages(list);
		const timer = setInterval(() => loadMessages(list), 60_000);
		return () => clearInterval(timer);
	});

	const who = (id: string | null, fallback: string) => {
		const s = id ? live.sessions.get(id) : undefined;
		return s ? label(s) : fallback;
	};
</script>

<div class="flex flex-col gap-5">
	<section class="flex flex-col gap-2">
		<h3 class="text-sm font-semibold">
			Live sessions <span class="font-normal text-muted-foreground">{sessions.length}</span>
		</h3>
		{#if !sessions.length}
			<p class="text-sm text-muted-foreground">No session is running here.</p>
		{/if}
		<div class="grid gap-2 sm:grid-cols-2">
			{#each sessions as s (s.id)}
				<article class="flex flex-col gap-1 rounded-lg border bg-card p-3 text-sm">
					<span class="flex items-center gap-2">
						<StateBadge activity={shown(s)} />
						<Clock since={s.activity_since} class="ml-auto text-xs" />
					</span>
					<a class="truncate font-medium hover:underline" href="/sessions/{s.id}">{label(s)}</a>
					<span class="truncate text-xs text-muted-foreground"
						>{[live.multiHost ? s.host : null, place(s.cwd), s.model]
							.filter(Boolean)
							.join(' · ')}</span
					>
					{#if s.activity === 'working' && s.tool}
						<span class="truncate font-mono text-xs">{toolLabel(s.tool)}</span>
					{/if}
					{#if s.claim}
						<span class="text-xs">
							<span class="font-medium text-muted-foreground">Claims</span>
							{s.claim.what}
							{#if claimPaths(s).length}<span class="text-muted-foreground"
									>· {claimPaths(s).join(', ')}</span
								>{/if}
							{#if s.claim.issue}
								{@const issue = s.claim.issue}
								·
								<button
									type="button"
									class="font-mono text-muted-foreground hover:underline"
									onclick={() => notePanel.show(ws, issue)}>{issue}</button
								>
							{/if}
						</span>
					{/if}
					{#if s.turn?.files.length}
						<span class="text-xs text-muted-foreground"
							>Editing: <span class="font-mono">{s.turn.files.slice(0, 4).join(', ')}</span
							>{#if s.turn.files.length > 4}
								+{s.turn.files.length - 4}{/if}</span
						>
					{/if}
				</article>
			{/each}
		</div>
	</section>
	<section class="flex flex-col gap-2">
		<h3 class="text-sm font-semibold">Messages in the last hour</h3>
		{#if !messages.length}
			<p class="text-sm text-muted-foreground">No messages between these sessions.</p>
		{:else}
			<ul class="divide-y rounded-lg border bg-card text-sm">
				{#each messages as m (m.id)}
					<li class="flex flex-col gap-0.5 px-3 py-2">
						<span class="text-xs text-muted-foreground">
							{who(m.from_session, m.from_label)} → {who(m.to_session, m.to_session.slice(0, 8))} · {fmtStamp(
								m.sent_at
							)} · {m.read_at
								? 'read'
								: m.delivered_at
									? 'delivered'
									: 'waiting for its next prompt'}
						</span>
						<span class="line-clamp-3">{plain(m.text)}</span>
					</li>
				{/each}
			</ul>
		{/if}
	</section>
</div>
