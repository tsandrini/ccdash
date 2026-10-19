<script lang="ts">
	// A project in the overview: open issues by priority, in progress, the
	// live sessions with their claims, the last progress, hot documents.
	import type { Project } from '#lib/api/types.js';
	import Clock from '#lib/components/app/Clock.svelte';
	import StateBadge from '#lib/components/app/StateBadge.svelte';
	import { fmtStamp, plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { noteHref, projectHref } from '#lib/notes.js';
	import { notePanel } from '#lib/notepanel.svelte.js';
	import { label, shown } from '#lib/session.js';

	let { ws, project: p }: { ws: string; project: Project } = $props();

	const sessions = $derived(
		live.list.filter(
			(s) => s.activity !== 'gone' && s.workspace === ws && (s.project ?? null) === p.project
		)
	);
	const inProgress = $derived(p.issues.filter((i) => i.in_progress).length);
	const high = $derived(p.issues.filter((i) => i.priority === 'high').length);
	const low = $derived(p.issues.filter((i) => i.priority === 'low').length);
	const hot = $derived(p.docs.filter((d) => d.hot).slice(0, 3));
	const last = $derived(p.progress[0]);
	const facts = $derived(
		[
			p.closed.length ? `${p.closed.length} closed this week` : null,
			p.memories.length ? plural(p.memories.length, 'memory', 'memories') : null,
			p.docs.length ? plural(p.docs.length, 'doc') : null
		].filter(Boolean)
	);
</script>

<article class="flex flex-col gap-2 rounded-lg border bg-card p-3 text-sm">
	<a href={projectHref(ws, p.project)} class="flex items-baseline gap-2 hover:underline">
		<h3 class="truncate font-semibold">{p.project ?? 'Workspace-wide'}</h3>
		{#if !p.project}<span class="text-xs text-muted-foreground">notes without a project</span>{/if}
	</a>
	<div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
		<span
			><strong class="tabular-nums">{p.issues.length}</strong> open{#if high}
				· <span class="text-permission">{high} high</span>{/if}{#if low}
				· <span class="text-muted-foreground">{low} low</span>{/if}</span
		>
		{#if inProgress}<span class="text-working">{inProgress} in progress</span>{/if}
		{#each facts as f (f)}<span class="text-muted-foreground">{f}</span>{/each}
	</div>
	{#if sessions.length}
		<ul class="flex flex-col gap-1">
			{#each sessions as s (s.id)}
				<li class="flex items-center gap-2 text-xs">
					<StateBadge activity={shown(s)} />
					<a class="truncate font-medium hover:underline" href="/sessions/{s.id}">{label(s)}</a>
					{#if s.claim}<span class="truncate text-muted-foreground" title={s.claim.what}
							>{s.claim.what}</span
						>{/if}
					<Clock since={s.activity_since} class="ml-auto" />
				</li>
			{/each}
		</ul>
	{/if}
	{#if last}
		<button
			type="button"
			class="flex flex-col gap-0.5 text-left text-xs hover:underline"
			onclick={() => notePanel.show(ws, last.slug)}
		>
			<span class="text-muted-foreground">Last progress · {fmtStamp(last.updated_at)}</span>
			<span class="line-clamp-2">{last.title}</span>
		</button>
	{/if}
	{#if hot.length}
		<div class="flex flex-wrap items-center gap-1 text-xs">
			<span class="text-muted-foreground">Hot docs</span>
			{#each hot as d (d.slug)}
				<a class="rounded-full border px-2 py-0.5 hover:bg-accent" href={noteHref(ws, d.slug)}
					>{d.title}</a
				>
			{/each}
		</div>
	{/if}
</article>
