<script lang="ts">
	// Workspace › New directories: what exists at unit depth and does not
	// travel (over the threshold, left out by the globs, or ignored), with
	// Include and Ignore.
	import { workspaceUnitOp, type NewDir } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { fmtBytes, fmtStamp } from '#lib/format.js';

	let { dirs, onchange }: { dirs: NewDir[]; onchange: () => void } = $props();

	let busy = $state<string | null>(null);
	let error = $state('');
	let showExcluded = $state(false);

	const STATE_TEXT: Record<string, string> = {
		waiting: 'larger than the threshold, waits for you',
		excluded: 'left out by the globs',
		ignored: 'ignored by you'
	};

	const waiting = $derived(dirs.filter((d) => d.state === 'waiting'));
	const rest = $derived(dirs.filter((d) => d.state !== 'waiting'));

	async function act(d: NewDir, op: 'include' | 'ignore') {
		busy = d.unit;
		error = '';
		try {
			await workspaceUnitOp(d.unit, { op, host: null, value: null });
			onchange();
		} catch (e) {
			error = (e as Error).message;
		} finally {
			busy = null;
		}
	}
</script>

{#snippet row(d: NewDir)}
	<li class="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md border px-2.5 py-1.5">
		<div class="flex min-w-0 flex-1 flex-col">
			<span class="font-mono text-sm">{d.unit}</span>
			<span class="text-xs text-muted-foreground">
				{d.kind} · {fmtBytes(d.size)} · on {d.hosts.join(', ')} · {STATE_TEXT[d.state] ?? d.state}
				· seen {fmtStamp(d.at)}
			</span>
		</div>
		{#if d.state !== 'ignored'}
			<Button size="xs" disabled={busy !== null} onclick={() => act(d, 'include')}>Include</Button>
		{/if}
		{#if d.state === 'waiting'}
			<Button variant="ghost" size="xs" disabled={busy !== null} onclick={() => act(d, 'ignore')}
				>Ignore</Button
			>
		{/if}
		{#if d.state === 'ignored'}
			<Button variant="outline" size="xs" disabled={busy !== null} onclick={() => act(d, 'include')}
				>Include after all</Button
			>
		{/if}
	</li>
{/snippet}

<section class="flex flex-col gap-2">
	<h2 class="text-base font-semibold">New directories</h2>
	{#if !waiting.length}
		<p class="text-sm text-muted-foreground">
			Nothing waits: a new directory under the threshold is included by itself.
		</p>
	{:else}
		<ul class="flex flex-col gap-1.5">
			{#each waiting as d (d.unit)}
				{@render row(d)}
			{/each}
		</ul>
	{/if}
	{#if rest.length}
		<button
			type="button"
			class="self-start text-xs text-muted-foreground hover:underline"
			onclick={() => (showExcluded = !showExcluded)}
		>
			{showExcluded ? 'Hide' : 'Show'}
			{rest.length} left out or ignored
		</button>
		{#if showExcluded}
			<ul class="flex flex-col gap-1.5">
				{#each rest as d (d.unit)}
					{@render row(d)}
				{/each}
			</ul>
		{/if}
	{/if}
	{#if error}<p class="text-sm text-critical" role="alert">{error}</p>{/if}
</section>
