<script lang="ts">
	// The progress feed grouped by day, each entry linked to its issue.
	import type { Card } from '#lib/api/types.js';
	import { fmtDate, fmtTime } from '#lib/format.js';
	import { dayKey } from '#lib/notes.js';
	import { notePanel } from '#lib/notepanel.svelte.js';

	let { ws, progress, showProject }: { ws: string; progress: Card[]; showProject: boolean } =
		$props();

	const days = $derived.by(() => {
		const out: { key: string; day: string; items: Card[] }[] = [];
		for (const c of progress) {
			const key = dayKey(c.updated_at);
			const last = out.at(-1);
			if (last && last.key === key) last.items.push(c);
			else out.push({ key, day: fmtDate(c.updated_at), items: [c] });
		}
		return out;
	});
</script>

{#if !progress.length}
	<p class="text-sm text-muted-foreground">No progress logged yet.</p>
{/if}
<div class="flex flex-col gap-4">
	{#each days as group (group.key)}
		<section>
			<h3 class="mb-1 text-xs font-semibold text-muted-foreground uppercase">{group.day}</h3>
			<ul class="divide-y rounded-lg border bg-card text-sm">
				{#each group.items as c (c.slug)}
					<li class="flex gap-3 px-3 py-2">
						<span class="shrink-0 font-mono text-xs text-muted-foreground tabular-nums"
							>{fmtTime(c.updated_at)}</span
						>
						<div class="flex min-w-0 flex-1 flex-col gap-0.5">
							<button
								type="button"
								class="line-clamp-3 text-left hover:underline"
								onclick={() => notePanel.show(ws, c.slug)}>{c.title}</button
							>
							<span class="flex flex-wrap gap-x-2 text-xs text-muted-foreground">
								{#if showProject && c.project}<span>{c.project}</span>{/if}
								{#if c.issue}
									{@const issue = c.issue}
									<button
										type="button"
										class="font-mono hover:underline"
										onclick={() => notePanel.show(ws, issue)}>{issue}</button
									>
								{/if}
							</span>
						</div>
					</li>
				{/each}
			</ul>
		</section>
	{/each}
</div>
