<script lang="ts">
	// A new note: `?ws=&kind=&project=` preselect where it goes.
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import type { NoteKind } from '#lib/api/types.js';
	import NoteEditor from '#lib/components/notes/NoteEditor.svelte';
	import { live } from '#lib/live/store.svelte.js';
	import { KIND_LABEL, KINDS, noteHref } from '#lib/notes.js';
	import { scope } from '#lib/scope.svelte.js';

	const q = $derived(page.url.searchParams);
	const ws = $derived(q.get('ws') || scope.current.ws[0] || live.workspaces[0] || '');
	const kind = $derived(
		(KINDS.find((k) => k === q.get('kind')) ?? 'memory') as Exclude<NoteKind, 'progress'>
	);
	const project = $derived(q.get('project'));
</script>

<svelte:head>
	<title>New {KIND_LABEL[kind].toLowerCase()} · ccdash</title>
</svelte:head>

<div class="mx-auto flex max-w-3xl flex-col gap-3 p-3 sm:p-4">
	<h2 class="text-lg font-semibold">New {KIND_LABEL[kind].toLowerCase()}</h2>
	{#if ws}
		{#key `${ws}/${kind}/${project}`}
			<NoteEditor
				{ws}
				{kind}
				{project}
				onsaved={(note) => goto(noteHref(note.workspace, note.slug))}
				oncancel={() => history.back()}
			/>
		{/key}
	{:else}
		<p class="text-sm text-muted-foreground">Waiting for the hub's workspaces…</p>
	{/if}
</div>
