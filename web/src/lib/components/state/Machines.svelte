<script lang="ts">
	// State › Machines: how each machine's copy stands (files, what waits,
	// the first-run progress, file watches) and Resync.
	import { resyncHost, type SyncHost, type SyncView } from '#lib/api/index.js';
	import Meter from '#lib/components/app/Meter.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { fmtBytes, fmtDuration, plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';

	let { view, onchange }: { view: SyncView; onchange: () => void } = $props();

	/** A report older than this means the machine's passes stalled. */
	const STALE_MS = 3 * 60_000;

	let busy = $state(false);
	let resyncing = $state<string | null>(null);
	let notice = $state('');
	let error = $state('');

	async function resync(host: string) {
		busy = true;
		notice = '';
		error = '';
		try {
			await resyncHost(host);
			notice = `${host} is comparing every file with the hub.`;
			onchange();
		} catch (e) {
			error = (e as Error).message;
		} finally {
			resyncing = null;
			busy = false;
		}
	}

	/** One line on where the machine's copy stands. */
	function summary(h: SyncHost): string {
		const s = h.status;
		if (!s) return h.connected ? 'not syncing (sync is off there)' : 'offline';
		const parts = [`${plural(s.files, 'file')}, ${fmtBytes(s.bytes)}`];
		if (s.pending_up) parts.push(`${s.pending_up} to send`);
		if (s.pending_down) parts.push(`${s.pending_down} to fetch`);
		if (s.held) parts.push(`${s.held} held while their session runs elsewhere or there`);
		if (!s.pending_up && !s.pending_down && !s.held && !s.error && !s.backfill)
			parts.push('in step');
		// NOTE: passes run on changes but report at most once a minute when nothing changes
		if (s.last_pass && live.now - s.last_pass > STALE_MS)
			parts.push(`last checked ${fmtDuration(live.now - s.last_pass)} ago`);
		if (!s.watching) parts.push('no file watches, scanning instead');
		if (!h.connected) parts.push('offline');
		return parts.join(' · ');
	}
</script>

<section class="flex flex-col gap-2">
	<h2 class="text-base font-semibold">Machines</h2>
	<ul class="flex flex-col gap-1.5">
		{#each view.hosts as h (h.host)}
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
					</div>
					<div class="text-xs text-muted-foreground">{summary(h)}</div>
					{#if h.status?.backfill}
						{@const [done, total] = h.status.backfill}
						<div class="flex items-center gap-2 text-xs text-muted-foreground">
							<Meter pct={total ? (done / total) * 100 : 0} level="neutral" class="w-28" />
							{done} of {total} files settled
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
					onclick={() => (resyncing = h.host)}>Resync</Button
				>
			</li>
		{/each}
	</ul>
	{#if notice}<p class="text-sm text-muted-foreground" role="status">{notice}</p>{/if}
	{#if error}<p class="text-sm text-critical" role="alert">{error}</p>{/if}
</section>

<Dialog.Root
	open={resyncing !== null}
	onOpenChange={(open) => {
		if (!open) resyncing = null;
	}}
>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Resync {resyncing ?? ''}?</Dialog.Title>
			<Dialog.Description>
				It compares every file with the hub again: it sends what the hub lacks and fetches what the
				machine lacks, files deleted there included. Nothing is lost: a file that differs on both
				sides is set aside as a conflict.
			</Dialog.Description>
		</Dialog.Header>
		<Dialog.Footer>
			<Button variant="outline" onclick={() => (resyncing = null)}>Cancel</Button>
			<Button disabled={busy} onclick={() => resyncing && resync(resyncing)}>Resync</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
