<script lang="ts">
	// Claude Code's state on the hub: each machine's copy (Resync), the
	// conflicts with their resolutions, and where the space goes and what
	// grows. The status is polled while shown, the sizes less often.
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchSync, fetchSyncStats, type SyncStats, type SyncView } from '#lib/api/index.js';
	import Conflicts from '#lib/components/state/Conflicts.svelte';
	import Machines from '#lib/components/state/Machines.svelte';
	import Sizes from '#lib/components/state/Sizes.svelte';
	import { live } from '#lib/live/store.svelte.js';

	const POLL_MS = 4000;
	const STATS_EVERY_MS = 60_000;

	let view = $state<SyncView | null>(null);
	let stats = $state<SyncStats | null>(null);
	let error = $state('');

	function failed(e: unknown) {
		if (e instanceof Unauthorized) live.needToken = true;
		else error = (e as Error).message;
	}

	async function load() {
		try {
			view = await fetchSync();
			error = '';
		} catch (e) {
			failed(e);
		}
	}

	async function loadStats() {
		try {
			stats = await fetchSyncStats();
		} catch (e) {
			failed(e);
		}
	}

	$effect(() => {
		if (!live.ready) return;
		load();
		loadStats();
		const timers = [setInterval(load, POLL_MS), setInterval(loadStats, STATS_EVERY_MS)];
		return () => timers.forEach(clearInterval);
	});
</script>

<div class="mx-auto flex max-w-5xl flex-col gap-5 p-3 sm:p-4">
	<p class="text-sm text-muted-foreground">
		Sessions, rewind checkpoints, plans, tasks, pasted content and the prompt history of every
		machine, kept on the hub. A session ended on one machine resumes on another with
		<code class="font-mono">claude --resume</code>; one still open elsewhere takes no prompts until
		it is stopped there.
	</p>
	{#if error}<p class="text-sm text-critical" role="alert">{error}</p>{/if}
	{#if view}
		<Machines {view} onchange={load} />
		<Conflicts {view} onchange={load} />
	{:else if !error}
		<p class="text-sm text-muted-foreground">Loading…</p>
	{/if}
	{#if stats}<Sizes {stats} />{/if}
</div>
