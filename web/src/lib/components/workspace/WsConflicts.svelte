<script lang="ts">
	// Workspace › Conflicts: files two hosts changed at once. Text files
	// carry markers in the tree; binaries kept the local version. Keep mine
	// or take theirs rewrites the file on the host that holds it; Dismiss
	// hides the entry (the markers stay for you to fix).
	import LoaderCircleIcon from '@lucide/svelte/icons/loader-circle';
	import {
		fetchConflictBlob,
		workspaceConflict,
		type Side,
		type WorkspaceConflict
	} from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { fmtStamp } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { unitHref } from '#lib/workspace.js';

	let {
		conflicts,
		onchange,
		compact = false
	}: { conflicts: WorkspaceConflict[]; onchange: () => void; compact?: boolean } = $props();

	let busy = $state(false);
	let error = $state('');
	let open = $state<number | null>(null);
	let versions = $state<Record<number, { mine: string; theirs: string }>>({});

	async function act(c: WorkspaceConflict, action: 'mine' | 'theirs' | 'dismiss') {
		busy = true;
		error = '';
		try {
			await workspaceConflict(c.id, action);
			onchange();
		} catch (e) {
			error = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	async function show(c: WorkspaceConflict) {
		if (open === c.id) {
			open = null;
			return;
		}
		open = c.id;
		if (versions[c.id]) return;
		const read = async (side: Side) => {
			try {
				return await fetchConflictBlob(c.id, side);
			} catch (e) {
				return `(${(e as Error).message})`;
			}
		};
		const [mine, theirs] = await Promise.all([
			c.ours ? read('mine') : Promise.resolve('(deleted on this host)'),
			c.theirs ? read('theirs') : Promise.resolve(`(deleted on ${c.other_host})`)
		]);
		versions = { ...versions, [c.id]: { mine, theirs } };
	}
</script>

<section class="flex flex-col gap-2">
	{#if !compact}<h2 class="text-base font-semibold">Conflicts</h2>{/if}
	{#if !conflicts.length}
		<p class="text-sm text-muted-foreground">
			None: every merge went clean, or you fixed the markers.
		</p>
	{:else}
		{#if !compact}
			<p class="text-sm text-muted-foreground">
				These files changed on two machines at once. A text file holds both versions between markers
				on the machine named (fix them there, or choose a side); a binary kept that machine's
				version and the other one is here.
			</p>
		{/if}
		<ul class="flex flex-col gap-2">
			{#each conflicts as c (c.id)}
				{@const online = live.connected(c.host)}
				<li class="flex flex-col gap-1.5 rounded-md border px-3 py-2">
					<div class="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
						<span class="font-mono text-sm font-medium">{c.path}</span>
						<a href={unitHref(c.unit)} class="text-xs text-muted-foreground hover:underline"
							>{c.unit}</a
						>
						<span class="rounded bg-muted px-1 text-[10px] text-muted-foreground">{c.kind}</span>
					</div>
					<div class="text-xs text-muted-foreground">
						{c.kind === 'binary' ? `${c.host} kept its version` : `markers on ${c.host}`} · the other
						side came from {c.other_host} · {fmtStamp(c.at)}
					</div>
					{#if c.error}<p class="text-sm text-critical">{c.error}</p>{/if}
					<div class="flex flex-wrap items-center gap-2">
						{#if c.pending}
							<span class="flex items-center gap-1.5 text-sm text-muted-foreground" role="status">
								<LoaderCircleIcon class="size-4 animate-spin" />
								Writing {c.pending === 'mine' ? `${c.host}'s` : `${c.other_host}'s`} version on {c.host}…
							</span>
						{:else}
							<Button size="xs" disabled={busy || !online} onclick={() => act(c, 'mine')}
								>Keep {c.host}'s</Button
							>
							<Button
								variant="outline"
								size="xs"
								disabled={busy || !online}
								onclick={() => act(c, 'theirs')}>Take {c.other_host}'s</Button
							>
							<Button variant="ghost" size="xs" disabled={busy} onclick={() => act(c, 'dismiss')}
								>Dismiss</Button
							>
							{#if c.kind !== 'binary'}
								<Button variant="ghost" size="xs" onclick={() => show(c)}
									>{open === c.id ? 'Hide versions' : 'Show versions'}</Button
								>
							{/if}
							{#if !online}<span class="text-xs text-muted-foreground"
									>{c.host} is offline: it holds the file.</span
								>{/if}
						{/if}
					</div>
					{#if open === c.id}
						{@const v = versions[c.id]}
						{#if v}
							<div class="grid gap-2 sm:grid-cols-2">
								<div>
									<div class="mb-1 text-xs font-medium">{c.host}</div>
									<pre
										class="max-h-64 overflow-auto rounded bg-muted p-2 font-mono text-[11px] leading-snug">{v.mine}</pre>
								</div>
								<div>
									<div class="mb-1 text-xs font-medium">{c.other_host}</div>
									<pre
										class="max-h-64 overflow-auto rounded bg-muted p-2 font-mono text-[11px] leading-snug">{v.theirs}</pre>
								</div>
							</div>
						{:else}
							<p class="text-xs text-muted-foreground">Loading…</p>
						{/if}
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
	{#if error}<p class="text-sm text-critical" role="alert">{error}</p>{/if}
</section>
