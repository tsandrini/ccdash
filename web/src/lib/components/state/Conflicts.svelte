<script lang="ts">
	// State › Conflicts: files that changed on a machine and on the hub at
	// once. The hub's version is in place everywhere; the machine kept its
	// own as a copy. Keep the current version (the copy goes) or use the
	// copy (it replaces the hub's everywhere, and the replaced version
	// becomes the copy), or hide the conflict.
	import LoaderCircleIcon from '@lucide/svelte/icons/loader-circle';
	import {
		dismissConflict,
		resolveConflict,
		type Keep,
		type SyncConflict,
		type SyncView
	} from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { fmtBytes, fmtStamp, plural } from '#lib/format.js';

	let { view, onchange }: { view: SyncView; onchange: () => void } = $props();

	let asking = $state<{ c: SyncConflict; keep: Keep } | null>(null);
	let busy = $state(false);
	let error = $state('');
	let copied = $state<number | null>(null);

	const connected = $derived(new Set(view.hosts.filter((h) => h.connected).map((h) => h.host)));

	const fileName = (path: string) => path.split('/').pop() ?? path;
	const transcript = (c: SyncConflict) => c.path.endsWith('.jsonl') && c.session !== null;

	async function resolve() {
		if (!asking) return;
		busy = true;
		error = '';
		try {
			await resolveConflict(asking.c.id, asking.keep);
			onchange();
		} catch (e) {
			error = (e as Error).message;
		} finally {
			asking = null;
			busy = false;
		}
	}

	async function hide(id: number) {
		error = '';
		try {
			await dismissConflict(id);
			onchange();
		} catch (e) {
			error = (e as Error).message;
		}
	}

	async function copyCommand(c: SyncConflict) {
		await navigator.clipboard?.writeText(`claude --resume ~/.claude/${c.copy}`);
		copied = c.id;
		setTimeout(() => (copied = null), 1500);
	}
</script>

<section class="flex flex-col gap-2">
	<h2 class="text-base font-semibold">Conflicts</h2>
	{#if !view.conflicts.length}
		<p class="text-sm text-muted-foreground">None: every machine agrees with the hub.</p>
	{:else}
		<p class="text-sm text-muted-foreground">
			These files changed on a machine and on the hub at once. Every machine has the current
			version; the machine named kept its own as a copy in its Claude config dir. Nothing is deleted
			until you choose.
		</p>
		<ul class="flex flex-col gap-2">
			{#each view.conflicts as c (c.id)}
				{@const d = c.details}
				{@const online = connected.has(c.host)}
				<li class="flex flex-col gap-1.5 rounded-md border px-3 py-2">
					<div class="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
						{#if c.session}
							<a href="/sessions/{c.session}" class="font-medium hover:underline">
								{c.title ?? `Session ${c.session.slice(0, 8)}`}
							</a>
							<span class="text-xs text-muted-foreground">{fileName(c.path)}</span>
						{:else}
							<span class="font-medium">{c.path}</span>
						{/if}
					</div>
					<div class="text-xs text-muted-foreground">
						{c.host} · found {fmtStamp(c.at)} · <code class="font-mono">{c.path}</code>
					</div>
					{#if c.former_hub}
						<p class="text-sm">
							The hub has {c.host}'s version now, as you chose; the copy is the version it replaced.
						</p>
					{/if}
					{#if d}
						<dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-sm">
							<dt class="text-muted-foreground">Both start with</dt>
							<dd>{fmtBytes(d.shared)}</dd>
							<dt class="text-muted-foreground">Current version</dt>
							<dd>
								{fmtBytes(d.current_size)}, {plural(d.current_lines, 'line')} after that · written
								{fmtStamp(d.current_mtime)}
							</dd>
							<dt class="text-muted-foreground">
								{c.former_hub ? 'Copy (the former version)' : `Copy (${c.host}'s)`}
							</dt>
							<dd>
								{fmtBytes(d.copy_size)}, {plural(d.copy_lines, 'line')} after that · written
								{fmtStamp(d.copy_mtime)}
							</dd>
						</dl>
					{:else}
						<p class="text-xs text-muted-foreground">
							Copy {fmtBytes(c.size)}; {c.host} describes it at its next full scan.
						</p>
					{/if}
					{#if !transcript(c)}
						<div class="text-xs text-muted-foreground">
							Copy at <code class="font-mono">~/.claude/{c.copy}</code>
						</div>
					{/if}
					{#if transcript(c)}
						<div class="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
							Open the copy on {c.host}:
							<code class="rounded bg-muted px-1 font-mono">claude --resume ~/.claude/{c.copy}</code
							>
							<Button variant="ghost" size="xs" onclick={() => copyCommand(c)}
								>{copied === c.id ? 'Copied' : 'Copy'}</Button
							>
						</div>
					{/if}
					{#if c.error}<p class="text-sm text-critical">{c.error}</p>{/if}
					<div class="flex flex-wrap items-center gap-2">
						{#if c.pending}
							<span class="flex items-center gap-1.5 text-sm text-muted-foreground" role="status">
								<LoaderCircleIcon class="size-4 animate-spin" />
								{c.pending === 'hub' ? 'Deleting the copy' : 'Putting the copy in place'} on {c.host}…
							</span>
						{:else}
							<Button size="xs" disabled={!online} onclick={() => (asking = { c, keep: 'hub' })}
								>Keep the current version</Button
							>
							<Button
								variant="outline"
								size="xs"
								disabled={!online}
								onclick={() => (asking = { c, keep: 'copy' })}>Use the copy</Button
							>
							<Button variant="ghost" size="xs" onclick={() => hide(c.id)}>Hide</Button>
							{#if !online}<span class="text-xs text-muted-foreground"
									>{c.host} is offline: it holds the copy.</span
								>{/if}
						{/if}
					</div>
				</li>
			{/each}
		</ul>
	{/if}
	{#if error}<p class="text-sm text-critical" role="alert">{error}</p>{/if}
</section>

<Dialog.Root
	open={asking !== null}
	onOpenChange={(open) => {
		if (!open) asking = null;
	}}
>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>
				{asking?.keep === 'hub' ? 'Keep the current version?' : 'Use the copy?'}
			</Dialog.Title>
			<Dialog.Description>
				{#if asking?.keep === 'hub'}
					{asking.c.host} deletes its copy (<code class="font-mono">~/.claude/{asking.c.copy}</code
					>). Every machine keeps the version it has now.
				{:else if asking}
					The copy replaces the current version on every machine. {asking.c.host} keeps the version it
					replaces as the new copy, so you can still go back. The session must not be open on any machine.
				{/if}
			</Dialog.Description>
		</Dialog.Header>
		<Dialog.Footer>
			<Button variant="outline" onclick={() => (asking = null)}>Cancel</Button>
			<Button
				variant={asking?.keep === 'hub' ? 'destructive' : 'default'}
				disabled={busy}
				onclick={resolve}>{asking?.keep === 'hub' ? 'Delete the copy' : 'Use the copy'}</Button
			>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
