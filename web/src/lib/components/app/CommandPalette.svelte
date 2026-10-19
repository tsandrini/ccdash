<script lang="ts">
	// Sessions, notes and pages from anywhere; notes only from the composer.
	import { goto } from '$app/navigation';
	import * as Command from '#lib/components/ui/command/index.js';
	import { fetchNotes, type Hit } from '#lib/api/index.js';
	import { live } from '#lib/live/store.svelte.js';
	import { areas } from '#lib/nav.js';
	import { palette } from '#lib/palette.svelte.js';
	import { ACTIVITY, label, shown } from '#lib/session.js';
	import { KIND_LABEL, noteHref } from '#lib/notes.js';
	import { Unauthorized } from '#lib/api/client.js';

	let hits = $state<Hit[]>([]);
	let searching = $state(false);
	let timer: ReturnType<typeof setTimeout> | null = null;

	const sessions = $derived(
		live.list.filter((s) => s.activity !== 'gone').sort((a, b) => b.last_event_at - a.last_event_at)
	);

	$effect(() => {
		const q = palette.query.trim();
		if (!palette.open) return;
		if (timer) clearTimeout(timer);
		if (q.length < 2) {
			hits = [];
			return;
		}
		timer = setTimeout(async () => {
			searching = true;
			try {
				hits = (await fetchNotes({ q, limit: 12 })).hits;
			} catch (e) {
				if (e instanceof Unauthorized) live.needToken = true;
				hits = [];
			} finally {
				searching = false;
			}
		}, 200);
	});

	function pickNote(hit: Hit) {
		const onNote = palette.onNote;
		palette.close();
		if (onNote) onNote(hit);
		else goto(noteHref(hit.workspace, hit.slug));
	}

	function go(href: string, legacy = false) {
		palette.close();
		if (legacy) location.assign(href);
		else goto(href);
	}

	function onkeydown(event: KeyboardEvent) {
		if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
			event.preventDefault();
			if (palette.open) palette.close();
			else palette.show();
		}
	}
</script>

<svelte:window {onkeydown} />

<Command.Dialog
	bind:open={palette.open}
	onOpenChange={(open) => {
		if (!open) palette.close();
	}}
	title={palette.notesOnly ? 'Insert a note link' : 'Search'}
	description="Sessions, notes and pages"
	shouldFilter={true}
>
	<Command.Input
		placeholder={palette.notesOnly ? 'Search notes…' : 'Sessions, notes, pages…'}
		bind:value={palette.query}
	/>
	<Command.List>
		<Command.Empty>{searching ? 'Searching…' : 'Nothing found.'}</Command.Empty>
		{#if !palette.notesOnly}
			<Command.Group heading="Sessions">
				{#each sessions as s (s.id)}
					<Command.Item
						value="session {label(s)} {s.id} {s.workspace ?? ''}"
						onSelect={() => go(`/sessions/${s.id}`)}
					>
						<span class="truncate">{label(s)}</span>
						<span class="ml-auto text-xs text-muted-foreground">{ACTIVITY[shown(s)].label}</span>
					</Command.Item>
				{/each}
			</Command.Group>
		{/if}
		{#if hits.length}
			<Command.Group heading="Notes">
				{#each hits as hit (hit.workspace + hit.slug)}
					<Command.Item value="note {hit.title} {hit.slug}" onSelect={() => pickNote(hit)}>
						<span class="truncate">{hit.title}</span>
						<span class="ml-auto text-xs text-muted-foreground"
							>{KIND_LABEL[hit.kind]} · {hit.workspace}</span
						>
					</Command.Item>
				{/each}
			</Command.Group>
		{/if}
		{#if !palette.notesOnly}
			<Command.Group heading="Pages">
				{#each areas as area (area.href)}
					<Command.Item value="page {area.label}" onSelect={() => go(area.href, area.legacy)}>
						<area.icon class="size-4 opacity-70" />
						{area.label}
					</Command.Item>
				{/each}
			</Command.Group>
		{/if}
	</Command.List>
</Command.Dialog>
