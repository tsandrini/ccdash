<script lang="ts">
	// The note side panel (a sheet from the right, full width on the
	// phone), with back through the notes opened from it and a link to
	// the note's own page.
	import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left';
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import * as Sheet from '#lib/components/ui/sheet/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { noteHref } from '#lib/notes.js';
	import { notePanel } from '#lib/notepanel.svelte.js';
	import NoteView from './NoteView.svelte';

	let title = $state('');
</script>

<Sheet.Root
	bind:open={notePanel.open}
	onOpenChange={(open) => {
		if (!open) notePanel.close();
	}}
>
	<Sheet.Content
		side="right"
		class="gap-0 p-0 data-[side=right]:w-full data-[side=right]:sm:max-w-xl"
		showCloseButton={true}
	>
		<Sheet.Header class="flex-row items-center gap-1 border-b py-2 pr-12 pl-2">
			{#if notePanel.stack.length}
				<Button variant="ghost" size="icon-sm" aria-label="Back" onclick={() => notePanel.back()}>
					<ArrowLeftIcon />
				</Button>
			{/if}
			<Sheet.Title class="truncate text-sm font-medium">{title || 'Note'}</Sheet.Title>
			<Sheet.Description class="sr-only">A note from the ccdash knowledge base</Sheet.Description>
			{#if notePanel.current}
				<Button
					variant="ghost"
					size="icon-sm"
					class="ml-auto"
					href={noteHref(notePanel.current.ws, notePanel.current.slug)}
					aria-label="Open as a page"
					title="Open as a page"
					onclick={() => notePanel.close()}
				>
					<ExternalLinkIcon />
				</Button>
			{/if}
		</Sheet.Header>
		<div class="min-h-0 flex-1 overflow-y-auto p-4">
			{#if notePanel.current}
				<NoteView
					ws={notePanel.current.ws}
					slug={notePanel.current.slug}
					onnavigate={(ws, slug) => notePanel.show(ws, slug)}
					ontitle={(t) => (title = t)}
				/>
			{/if}
		</div>
	</Sheet.Content>
</Sheet.Root>
