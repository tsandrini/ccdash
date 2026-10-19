<script lang="ts">
	// Memories: active, then the ones the lifecycle pass flagged; verify
	// and archive from the list.
	import type { Card } from '#lib/api/types.js';
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchNote, noteOp, putNote } from '#lib/api/index.js';
	import NoteRow from '#lib/components/notes/NoteRow.svelte';
	import { Button } from '#lib/components/ui/button/index.js';
	import { plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { notePanel } from '#lib/notepanel.svelte.js';
	import { projects } from '#lib/projects.svelte.js';

	let { ws, memories, showProject }: { ws: string; memories: Card[]; showProject: boolean } =
		$props();

	const active = $derived(memories.filter((m) => m.status === 'active'));
	const flagged = $derived(memories.filter((m) => m.status !== 'active'));

	let busy = $state('');
	let error = $state('');

	async function run(slug: string, action: () => Promise<unknown>) {
		busy = slug;
		error = '';
		try {
			await action();
			projects.schedule(ws);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		} finally {
			busy = '';
		}
	}

	const verify = (c: Card) => run(c.slug, () => noteOp(ws, c.slug, { op: 'verify', slug: c.slug }));
	const archive = (c: Card) =>
		run(c.slug, async () => {
			const v = await fetchNote(ws, c.slug);
			await putNote(ws, c.slug, {
				kind: v.kind,
				title: v.title,
				body: v.body,
				summary: v.summary,
				project: v.project,
				status: 'archived',
				extra: v.extra,
				base_version: v.version
			});
		});
</script>

{#snippet list(title: string, items: Card[], hint: string | null)}
	{#if items.length}
		<section>
			<h3 class="mb-1 text-sm font-semibold">
				{title} <span class="font-normal text-muted-foreground">{items.length}</span>
			</h3>
			{#if hint}<p class="mb-1 text-xs text-muted-foreground">{hint}</p>{/if}
			<ul class="divide-y rounded-lg border bg-card">
				{#each items as c (c.slug)}
					<li class="flex items-start">
						<div class="min-w-0 flex-1">
							<NoteRow
								card={c}
								project={showProject}
								reason={c.status === 'stale'}
								onclick={() => notePanel.show(ws, c.slug)}
							/>
						</div>
						<div class="flex shrink-0 flex-col items-end gap-1 py-2 pr-2">
							<span class="text-xs text-muted-foreground tabular-nums"
								>{plural(c.recalls, 'recall')}</span
							>
							<span class="flex gap-1">
								<Button
									variant="ghost"
									size="xs"
									disabled={busy === c.slug}
									onclick={() => verify(c)}
									title="It is still true">Verify</Button
								>
								<Button
									variant="ghost"
									size="xs"
									disabled={busy === c.slug}
									onclick={() => archive(c)}>Archive</Button
								>
							</span>
						</div>
					</li>
				{/each}
			</ul>
		</section>
	{/if}
{/snippet}

{#if error}<p class="mb-2 text-xs text-critical" role="alert">{error}</p>{/if}
{#if !memories.length}
	<p class="text-sm text-muted-foreground">No memories yet.</p>
{/if}
<div class="flex flex-col gap-4">
	{@render list('Flagged', flagged, 'Stale memories: verify, rewrite or archive them.')}
	{@render list('Active', active, null)}
</div>
