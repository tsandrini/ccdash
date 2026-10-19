<script lang="ts">
	// History: the sessions of the range in the topbar, reloaded as the
	// scope or range changes and every minute while the range reaches now.
	import { untrack } from 'svelte';
	import { fetchHistory } from '#lib/api/index.js';
	import HistoryView from '#lib/components/history/HistoryView.svelte';
	import { encodeRange, rangeParams, reachesNow, type Range } from '#lib/range.js';
	import { live } from '#lib/live/store.svelte.js';
	import { rangeState } from '#lib/range.svelte.js';
	import { historyData } from '#lib/ranged.svelte.js';
	import { scope } from '#lib/scope.svelte.js';

	function load(range: Range, workspace?: string, host?: string, force = false) {
		const key = `${encodeRange(range)}|${workspace ?? ''}|${host ?? ''}`;
		historyData.load(
			key,
			() => fetchHistory({ ...rangeParams(range, rangeState.now()), workspace, host }),
			force
		);
	}

	$effect(() => {
		if (!live.ready) return;
		const { workspace, host } = scope.params();
		const range = rangeState.current;
		untrack(() => load(range, workspace, host));
		if (!reachesNow(range, rangeState.now())) return;
		const timer = setInterval(() => load(range, workspace, host, true), 60_000);
		return () => clearInterval(timer);
	});
</script>

<div
	class="mx-auto flex max-w-6xl flex-col gap-3 p-3 transition-opacity sm:p-4"
	class:opacity-60={historyData.loading && historyData.data}
>
	{#if historyData.error}
		<p class="text-sm text-critical" role="alert">{historyData.error}</p>
	{:else if !historyData.data}
		<p class="text-sm text-muted-foreground">Loading…</p>
	{:else}
		<HistoryView data={historyData.data} />
	{/if}
</div>
