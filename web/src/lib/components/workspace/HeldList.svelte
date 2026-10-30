<script lang="ts">
	// Workspace › Held: snapshots the guard stopped (thousands of files
	// added or deleted in one settle), with Apply and Drop.
	import { workspaceHeld, type HeldView } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { fmtStamp, plural } from '#lib/format.js';
	import { unitHref } from '#lib/workspace.js';

	let { held, onchange }: { held: HeldView[]; onchange: () => void } = $props();

	let dropping = $state<HeldView | null>(null);
	let busy = $state(false);
	let error = $state('');

	async function act(h: HeldView, action: 'apply' | 'drop') {
		busy = true;
		error = '';
		try {
			await workspaceHeld(h.host, h.unit, action, h.held.commit);
			onchange();
		} catch (e) {
			error = (e as Error).message;
		} finally {
			dropping = null;
			busy = false;
		}
	}
</script>

<section class="flex flex-col gap-2">
	<h2 class="text-base font-semibold">Held snapshots</h2>
	{#if !held.length}
		<p class="text-sm text-muted-foreground">
			None. A snapshot that adds or deletes more than 2,000 files, or a fifth of a unit, in one go
			waits here instead of travelling.
		</p>
	{:else}
		<ul class="flex flex-col gap-1.5">
			{#each held as h (`${h.host}/${h.unit}`)}
				<li class="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md border px-2.5 py-1.5">
					<div class="flex min-w-0 flex-1 flex-col">
						<a href={unitHref(h.unit)} class="font-mono text-sm hover:underline">{h.unit}</a>
						<span class="text-xs text-muted-foreground">
							on {h.host} · {plural(h.held.added, 'file')} added, {h.held.deleted} deleted of
							{h.held.tracked} tracked · {fmtStamp(h.held.at)}
						</span>
					</div>
					<Button size="xs" disabled={busy} onclick={() => act(h, 'apply')}>Apply</Button>
					<Button variant="outline" size="xs" disabled={busy} onclick={() => (dropping = h)}
						>Drop</Button
					>
				</li>
			{/each}
		</ul>
	{/if}
	{#if error}<p class="text-sm text-critical" role="alert">{error}</p>{/if}
</section>

<Dialog.Root
	open={dropping !== null}
	onOpenChange={(open) => {
		if (!open) dropping = null;
	}}
>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Drop the held snapshot?</Dialog.Title>
			<Dialog.Description>
				{#if dropping}
					{dropping.host} puts the working tree of {dropping.unit} back to the previous snapshot: the
					{dropping.held.added} added files go, the {dropping.held.deleted} deleted ones come back. Nothing
					travels.
				{/if}
			</Dialog.Description>
		</Dialog.Header>
		<Dialog.Footer>
			<Button variant="outline" onclick={() => (dropping = null)}>Cancel</Button>
			<Button
				variant="destructive"
				disabled={busy}
				onclick={() => dropping && act(dropping, 'drop')}>Drop</Button
			>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
