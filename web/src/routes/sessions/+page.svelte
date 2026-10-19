<script lang="ts">
	// Every session in scope, grouped: needs you, working, parked, idle,
	// background agents, ended.
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchHistory, type Lane, type Session } from '#lib/api/index.js';
	import SessionCard from '#lib/components/app/SessionCard.svelte';
	import { live } from '#lib/live/store.svelte.js';
	import { scope } from '#lib/scope.svelte.js';
	import { group } from '#lib/session.js';

	const STRIP_MS = 60 * 60_000;

	const groups = $derived(group(live.list.filter((s) => scope.covers(s))));

	let lanes = $state(new Map<string, Lane>());

	async function loadRecent() {
		if (!live.ready) return;
		try {
			const data = await fetchHistory({ since: Math.round(live.now - STRIP_MS) });
			lanes = new Map(data.lanes.map((lane) => [lane.id, lane]));
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
		}
	}

	$effect(() => {
		if (!live.ready) return;
		loadRecent();
		const timer = setInterval(loadRecent, 60_000);
		return () => clearInterval(timer);
	});

	const sections = $derived<[string, Session[], string | null, boolean][]>([
		['Needs you', groups.needs, null, false],
		['Working', groups.working, null, false],
		[
			'Parked',
			groups.parked,
			'Your turn for over an hour: most likely done, so not counted as waiting on you.',
			false
		],
		['Idle', groups.idle, null, false],
		[
			'Background agents',
			groups.agents,
			'Started by Claude or with claude --bg; they report to the session that started them.',
			false
		],
		['Ended', groups.gone, null, true]
	]);
</script>

<div class="mx-auto flex max-w-6xl flex-col gap-5 p-3 sm:p-4">
	{#if live.ready && !groups.all.length}
		<p class="p-6 text-center text-sm text-muted-foreground">
			No Claude Code sessions are running.
		</p>
	{/if}
	{#each sections as [title, sessions, note, collapsed] (title)}
		{#if sessions.length}
			<details class="group" open={!collapsed}>
				<summary
					class="flex cursor-pointer items-baseline gap-2 text-base font-semibold select-none"
				>
					{title}
					<span class="text-sm font-normal text-muted-foreground">{sessions.length}</span>
				</summary>
				{#if note}<p class="mt-1 text-xs text-muted-foreground">{note}</p>{/if}
				<div class="mt-2 grid gap-2 sm:grid-cols-2 lg:grid-cols-3">
					{#each sessions as s (s.id)}
						<SessionCard session={s} lane={lanes.get(s.id)} />
					{/each}
				</div>
			</details>
		{/if}
	{/each}
	<p class="text-xs text-muted-foreground">Bars show the last hour.</p>
</div>
