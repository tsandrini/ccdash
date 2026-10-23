<script lang="ts">
	// Settings › Claude Code state: how each machine's copy stands (files,
	// what waits, the first-run progress), Resync, and the conflicts the
	// collectors set aside. Polled while shown.
	import { Unauthorized } from '#lib/api/client.js';
	import {
		dismissConflict,
		fetchSync,
		resyncHost,
		type SyncHost,
		type SyncView
	} from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { fmtBytes, fmtDuration, fmtStamp, plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import Meter from './Meter.svelte';

	const POLL_MS = 5000;
	/** A report older than this means the machine's passes stalled. */
	const STALE_MS = 3 * 60_000;

	let view = $state<SyncView | null>(null);
	let error = $state('');
	let busy = $state(false);
	let resyncing = $state<string | null>(null);
	let notice = $state('');

	async function load() {
		try {
			view = await fetchSync();
			error = '';
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		}
	}

	$effect(() => {
		if (!live.ready) return;
		load();
		const timer = setInterval(load, POLL_MS);
		return () => clearInterval(timer);
	});

	async function resync(host: string) {
		busy = true;
		notice = '';
		try {
			await resyncHost(host);
			notice = `${host} is comparing every file with the hub.`;
			resyncing = null;
			await load();
		} catch (e) {
			resyncing = null;
			error = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	async function dismiss(id: number) {
		try {
			await dismissConflict(id);
			await load();
		} catch (e) {
			error = (e as Error).message;
		}
	}

	/** One line on where the machine's copy stands. */
	function summary(h: SyncHost): string {
		const s = h.status;
		if (!s) return h.connected ? 'not syncing (sync is off there)' : 'offline';
		const parts = [`${plural(s.files, 'file')}, ${fmtBytes(s.bytes)}`];
		if (s.pending_up) parts.push(`${s.pending_up} to send`);
		if (s.pending_down) parts.push(`${s.pending_down} to fetch`);
		if (s.held) parts.push(`${s.held} held while their session runs there`);
		if (!s.pending_up && !s.pending_down && !s.held && !s.error && !s.backfill)
			parts.push('in step');
		// NOTE: passes run every few seconds but report at most once a minute when nothing changes
		if (s.last_pass && live.now - s.last_pass > STALE_MS)
			parts.push(`last checked ${fmtDuration(live.now - s.last_pass)} ago`);
		if (!s.watching) parts.push(`no file watches, scanning instead`);
		if (!h.connected) parts.push('offline');
		return parts.join(' · ');
	}
	const total = $derived(view ? ` (${plural(view.files, 'file')}, ${fmtBytes(view.bytes)})` : '');
</script>

<section class="flex flex-col gap-2">
	<h2 class="text-base font-semibold">Claude Code state</h2>
	<p class="text-sm text-muted-foreground">
		Sessions, rewind checkpoints, plans, tasks, pasted content and the prompt history of every
		machine, kept on the hub{total}. A session ended on one machine resumes on another with
		<code class="font-mono">claude --resume</code>; one that is still open elsewhere takes no
		prompts until it is stopped there.
	</p>
	{#if view}
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
							<div class="text-xs break-words text-critical">{h.status.error}</div>
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
		{#if view.conflicts.length}
			<h3 class="mt-2 text-sm font-medium">Conflicts</h3>
			<p class="text-xs text-muted-foreground">
				These files changed on a machine and on the hub at once. The hub's version is in place; the
				machine kept its own under the path shown, in its Claude config dir. A transcript there
				still resumes with <code class="font-mono">claude --resume &lt;path&gt;</code>.
			</p>
			<ul class="flex flex-col gap-1.5">
				{#each view.conflicts as c (c.id)}
					<li class="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md border px-2.5 py-1.5">
						<div class="flex min-w-0 flex-1 flex-col gap-0.5">
							<div class="text-sm break-all">{c.path}</div>
							<div class="text-xs break-all text-muted-foreground">
								{c.host} · {fmtStamp(c.at)} · {fmtBytes(c.size)} · kept as
								<code class="font-mono">~/.claude/{c.copy}</code>
							</div>
						</div>
						<Button variant="outline" size="xs" onclick={() => dismiss(c.id)}>Dismiss</Button>
					</li>
				{/each}
			</ul>
		{/if}
	{/if}
	{#if error}<p class="text-sm text-critical">{error}</p>{/if}
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
