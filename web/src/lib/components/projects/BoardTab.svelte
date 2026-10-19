<script lang="ts">
	// The board: Open (by priority), In progress, Done (last 7 days). No
	// drag and drop: the state derives from what sessions do; only Close
	// and Reopen are the owner's.
	import type { Issue } from '#lib/api/types.js';
	import { Unauthorized } from '#lib/api/client.js';
	import { noteOp } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { fmtStamp } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { PRIORITY_RANK } from '#lib/notes.js';
	import { notePanel } from '#lib/notepanel.svelte.js';
	import { projects } from '#lib/projects.svelte.js';

	let {
		ws,
		issues,
		closed,
		showProject
	}: { ws: string; issues: Issue[]; closed: Issue[]; showProject: boolean } = $props();

	const byPriority = (a: Issue, b: Issue) =>
		PRIORITY_RANK[a.priority] - PRIORITY_RANK[b.priority] || b.updated_at - a.updated_at;
	const open = $derived(issues.filter((i) => !i.in_progress).sort(byPriority));
	const inProgress = $derived(issues.filter((i) => i.in_progress).sort(byPriority));
	/** Imports close many at once; the column shows the latest. */
	const DONE_SHOWN = 20;
	const done = $derived(closed.slice(0, DONE_SHOWN));

	let error = $state('');
	let busy = $state('');

	async function toggle(issue: Issue) {
		busy = issue.slug;
		error = '';
		try {
			await noteOp(ws, issue.slug, {
				op: 'issue_update',
				slug: issue.slug,
				status: issue.status === 'archived' ? 'open' : 'closed',
				title: null,
				body: null,
				priority: null,
				comment: null
			});
			projects.schedule(ws);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		} finally {
			busy = '';
		}
	}
</script>

{#snippet column(title: string, list: Issue[], hint: string | null, total = list.length)}
	<section class="flex min-w-0 flex-col gap-2">
		<h3 class="flex items-baseline gap-2 text-sm font-semibold">
			{title} <span class="font-normal text-muted-foreground">{total}</span>
		</h3>
		{#if hint && !list.length}<p class="text-xs text-muted-foreground">{hint}</p>{/if}
		{#each list as issue (issue.slug)}
			<article class="flex flex-col gap-1 rounded-lg border bg-card p-2.5 text-sm">
				<button
					type="button"
					class="flex flex-col gap-0.5 text-left"
					onclick={() => notePanel.show(ws, issue.slug)}
				>
					<span class="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-xs">
						{#if issue.priority !== 'normal'}
							<span
								class="uppercase {issue.priority === 'high'
									? 'text-permission'
									: 'text-muted-foreground'}">{issue.priority}</span
							>
						{/if}
						{#if showProject && issue.project}<span class="text-muted-foreground"
								>{issue.project}</span
							>{/if}
						{#if issue.closed_at}<span class="text-muted-foreground"
								>closed {fmtStamp(issue.closed_at)}</span
							>{/if}
					</span>
					<span class="font-medium">{issue.title}</span>
					{#if issue.last_progress}
						<span class="line-clamp-2 text-xs text-muted-foreground"
							>{fmtStamp(issue.last_progress.updated_at)} · {issue.last_progress.title}</span
						>
					{/if}
				</button>
				<div class="flex items-center gap-2 text-xs">
					{#if issue.working}
						{@const w = issue.working}
						<a
							class="flex min-w-0 items-center gap-1.5 hover:underline"
							href="/sessions/{w.session}"
						>
							<i class="inline-block size-2 shrink-0 rounded-full bg-working" aria-hidden="true"
							></i>
							<span class="truncate">{w.label}</span>
							<span class="shrink-0 text-muted-foreground"
								>{w.claimed ? 'claims it' : 'wrote today'}{live.multiHost
									? ` · ${w.host}`
									: ''}</span
							>
						</a>
					{/if}
					<Button
						variant="ghost"
						size="xs"
						class="ml-auto"
						disabled={busy === issue.slug}
						onclick={() => toggle(issue)}>{issue.status === 'archived' ? 'Reopen' : 'Close'}</Button
					>
				</div>
			</article>
		{/each}
		{#if total > list.length}
			<p class="text-xs text-muted-foreground">{total - list.length} more closed this week.</p>
		{/if}
	</section>
{/snippet}

{#if error}<p class="mb-2 text-xs text-critical" role="alert">{error}</p>{/if}
<div class="grid gap-4 md:grid-cols-3">
	{@render column('Open', open, 'No open issues.')}
	{@render column(
		'In progress',
		inProgress,
		'An issue is in progress while a live session claims it, or it got progress or a comment in the last day.'
	)}
	{@render column('Done', done, 'Nothing closed in the last 7 days.', closed.length)}
</div>
