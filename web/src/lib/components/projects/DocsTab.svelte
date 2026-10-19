<script lang="ts">
	// Documents: hot first (written or recalled lately), then the rest.
	import type { Doc } from '#lib/api/types.js';
	import NoteRow from '#lib/components/notes/NoteRow.svelte';
	import { notePanel } from '#lib/notepanel.svelte.js';

	let { ws, docs, showProject }: { ws: string; docs: Doc[]; showProject: boolean } = $props();

	const hot = $derived(docs.filter((d) => d.hot));
	const rest = $derived(docs.filter((d) => !d.hot));
</script>

{#snippet list(title: string, items: Doc[])}
	{#if items.length}
		<section>
			<h3 class="mb-1 text-sm font-semibold">
				{title} <span class="font-normal text-muted-foreground">{items.length}</span>
			</h3>
			<ul class="divide-y rounded-lg border bg-card">
				{#each items as d (d.slug)}
					<li>
						<NoteRow
							card={d}
							project={showProject}
							date
							onclick={() => notePanel.show(ws, d.slug)}
						/>
					</li>
				{/each}
			</ul>
		</section>
	{/if}
{/snippet}

{#if !docs.length}
	<p class="text-sm text-muted-foreground">No documents yet.</p>
{/if}
<div class="flex flex-col gap-4">
	{@render list('Hot', hot)}
	{@render list(hot.length ? 'Others' : 'Documents', rest)}
</div>
