<script lang="ts">
	// Workspace › Machines: each host's collector (units, in step, pending,
	// a seed or adoption in progress, errors) and Resync.
	import { workspaceResync, type WorkspaceHost } from '#lib/api/index.js';
	import Meter from '#lib/components/app/Meter.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import { fmtDuration } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';

	let { hosts, onchange }: { hosts: WorkspaceHost[]; onchange: () => void } = $props();

	/** A report older than this means the collector's passes stalled. */
	const STALE_MS = 10 * 60_000;

	let busy = $state(false);
	let error = $state('');

	async function resync(host: string) {
		busy = true;
		error = '';
		try {
			await workspaceResync(host);
			onchange();
		} catch (e) {
			error = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	function summary(h: WorkspaceHost): string {
		const s = h.status;
		if (!s) return h.connected ? 'not syncing its workspace' : 'offline';
		const parts = [`${s.units} units`, `${s.in_step} in step`];
		if (s.pending) parts.push(`${s.pending} moving`);
		if (!s.watching) parts.push('no file watches, sweeping instead');
		if (s.last_pass && live.now - s.last_pass > STALE_MS)
			parts.push(`last pass ${fmtDuration(live.now - s.last_pass)} ago`);
		if (!h.connected) parts.push('offline');
		return parts.join(' · ');
	}
</script>

<section class="flex flex-col gap-2">
	<h2 class="text-base font-semibold">Machines</h2>
	<ul class="flex flex-col gap-1.5">
		{#each hosts as h (h.host)}
			<li class="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md border px-2.5 py-1.5">
				<div class="flex min-w-0 flex-1 flex-col gap-0.5">
					<div class="flex items-center gap-2 text-sm">
						<i
							class="inline-block size-2 shrink-0 rounded-full {h.connected
								? 'bg-your-turn'
								: 'bg-critical'}"
							aria-hidden="true"
						></i>
						<span class="font-medium">{h.host}</span>
						{#if h.status}
							<span class="truncate font-mono text-xs text-muted-foreground">{h.status.root}</span>
						{/if}
					</div>
					<div class="text-xs text-muted-foreground">{summary(h)}</div>
					{#if h.status?.progress}
						{@const [done, total] = h.status.progress}
						<div class="flex items-center gap-2 text-xs text-muted-foreground">
							<Meter pct={total ? (done / total) * 100 : 0} level="neutral" class="w-28" />
							{done} of {total} units seeded or created
						</div>
					{/if}
					{#if h.status?.error}
						<div class="text-xs text-critical">{h.status.error}</div>
					{/if}
				</div>
				<Button
					variant="outline"
					size="xs"
					disabled={busy || !h.connected || !h.status}
					onclick={() => resync(h.host)}>Resync</Button
				>
			</li>
		{/each}
	</ul>
	{#if error}<p class="text-sm text-critical" role="alert">{error}</p>{/if}
</section>
