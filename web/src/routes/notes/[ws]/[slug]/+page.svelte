<script lang="ts">
	// A note as a page; `?edit=1` opens the editor.
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import NoteView from '#lib/components/notes/NoteView.svelte';
	import { noteHref } from '#lib/notes.js';

	const ws = $derived(page.params.ws!);
	const slug = $derived(page.params.slug!);
	const edit = $derived(page.url.searchParams.get('edit') === '1');
	let title = $state('');
</script>

<svelte:head>
	<title>{title || 'Note'} · ccdash</title>
</svelte:head>

<div class="mx-auto max-w-3xl p-3 sm:p-4">
	<NoteView
		{ws}
		{slug}
		{edit}
		onnavigate={(w, s) => goto(noteHref(w, s))}
		ontitle={(t) => (title = t)}
	/>
</div>
