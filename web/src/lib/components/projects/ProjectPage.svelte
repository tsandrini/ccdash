<script lang="ts">
	// A project (or a whole workspace) with its tabs: Board, Now, Progress,
	// Docs, Memories, Graph. The tab lives in the URL query.
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import Grid3x3Icon from '@lucide/svelte/icons/grid-3x3';
	import PlusIcon from '@lucide/svelte/icons/plus';
	import type { Card, Doc, Issue } from '#lib/api/types.js';
	import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js';
	import * as Tabs from '#lib/components/ui/tabs/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { newNoteHref, PRIORITY_RANK, projectHref } from '#lib/notes.js';
	import { prefs } from '#lib/prefs.svelte.js';
	import { projects } from '#lib/projects.svelte.js';
	import BoardTab from './BoardTab.svelte';
	import DocsTab from './DocsTab.svelte';
	import GraphTab from './GraphTab.svelte';
	import MemoriesTab from './MemoriesTab.svelte';
	import NowTab from './NowTab.svelte';
	import ProgressTab from './ProgressTab.svelte';

	const TABS = ['board', 'now', 'progress', 'docs', 'memories', 'graph'] as const;

	let { ws, project }: { ws: string; project: string | null } = $props();

	const tab = $derived(TABS.find((t) => t === page.url.searchParams.get('tab')) ?? 'board');

	function setTab(value: string) {
		const url = new URL(page.url.href);
		if (value === 'board') url.searchParams.delete('tab');
		else url.searchParams.set('tab', value);
		goto(`${url.pathname}${url.search}`, { replace: true });
	}

	$effect(() => {
		void live.notes.n;
		void projects.claimsSignature;
		if (live.ready) projects.schedule(ws);
	});

	const data = $derived(projects.workspace(ws));
	const parts = $derived(
		(data?.projects ?? []).filter((p) => project === null || p.project === project)
	);
	const issues = $derived(
		parts
			.flatMap((p) => p.issues)
			.sort(
				(a: Issue, b: Issue) =>
					PRIORITY_RANK[a.priority] - PRIORITY_RANK[b.priority] || b.updated_at - a.updated_at
			)
	);
	const closed = $derived(
		parts.flatMap((p) => p.closed).sort((a, b) => (b.closed_at ?? 0) - (a.closed_at ?? 0))
	);
	const docs = $derived(
		parts
			.flatMap((p) => p.docs)
			.sort((a: Doc, b: Doc) => Number(b.hot) - Number(a.hot) || b.updated_at - a.updated_at)
	);
	const progress = $derived(
		parts.flatMap((p) => p.progress).sort((a: Card, b: Card) => b.updated_at - a.updated_at)
	);
	const memories = $derived(
		parts
			.flatMap((p) => p.memories)
			.sort(
				(a: Card, b: Card) =>
					Number(a.status !== 'active') - Number(b.status !== 'active') ||
					b.recalls - a.recalls ||
					b.updated_at - a.updated_at
			)
	);
	const inProgress = $derived(issues.filter((i) => i.in_progress).length);
	const facts = $derived(
		[
			plural(issues.length, 'open issue'),
			inProgress ? `${inProgress} in progress` : null,
			plural(memories.length, 'memory', 'memories'),
			plural(docs.length, 'doc')
		].filter(Boolean)
	);
</script>

<svelte:head>
	<title>{project ?? ws} · Projects · ccdash</title>
</svelte:head>

<div class="min-h-full" class:board-grid={prefs.grid}>
	<div class="mx-auto flex max-w-6xl flex-col gap-3 p-3 sm:p-4">
		<header class="flex flex-wrap items-center gap-2">
			<nav class="flex min-w-0 items-baseline gap-1.5 text-sm" aria-label="Breadcrumb">
				<a class="text-muted-foreground hover:underline" href="/projects">Projects</a>
				<span class="text-muted-foreground">/</span>
				{#if project}
					<a class="text-muted-foreground hover:underline" href={projectHref(ws)}>{ws}</a>
					<span class="text-muted-foreground">/</span>
					<h2 class="truncate text-base font-semibold">{project}</h2>
				{:else}
					<h2 class="truncate text-base font-semibold">{ws}</h2>
				{/if}
			</nav>
			<span class="text-xs text-muted-foreground">{facts.join(' · ')}</span>
			<DropdownMenu.Root>
				<DropdownMenu.Trigger>
					{#snippet child({ props })}
						<Button variant="outline" size="sm" class="ml-auto" {...props}>
							<PlusIcon /> New <ChevronDownIcon class="size-3 opacity-60" />
						</Button>
					{/snippet}
				</DropdownMenu.Trigger>
				<DropdownMenu.Content align="end">
					<DropdownMenu.Item>
						{#snippet child({ props })}
							<a href={newNoteHref(ws, 'issue', project)} {...props}>Issue</a>
						{/snippet}
					</DropdownMenu.Item>
					<DropdownMenu.Item>
						{#snippet child({ props })}
							<a href={newNoteHref(ws, 'memory', project)} {...props}>Memory</a>
						{/snippet}
					</DropdownMenu.Item>
					<DropdownMenu.Item>
						{#snippet child({ props })}
							<a href={newNoteHref(ws, 'doc', project)} {...props}>Document</a>
						{/snippet}
					</DropdownMenu.Item>
				</DropdownMenu.Content>
			</DropdownMenu.Root>
			<Button
				variant={prefs.grid ? 'secondary' : 'ghost'}
				size="icon"
				aria-label="Grid behind the board"
				aria-pressed={prefs.grid}
				title="Grid"
				onclick={() => prefs.setGrid(!prefs.grid)}
			>
				<Grid3x3Icon />
			</Button>
		</header>
		{#if projects.error}
			<p class="text-sm text-critical" role="alert">{projects.error}</p>
		{/if}
		<Tabs.Root value={tab} onValueChange={setTab} class="gap-3">
			<div class="-mx-3 overflow-x-auto px-3 sm:mx-0 sm:px-0">
				<Tabs.List class="w-fit">
					<Tabs.Trigger value="board">Board</Tabs.Trigger>
					<Tabs.Trigger value="now">Now</Tabs.Trigger>
					<Tabs.Trigger value="progress">Progress</Tabs.Trigger>
					<Tabs.Trigger value="docs">Docs</Tabs.Trigger>
					<Tabs.Trigger value="memories">Memories</Tabs.Trigger>
					<Tabs.Trigger value="graph">Graph</Tabs.Trigger>
				</Tabs.List>
			</div>
			{#if !data && !projects.error}
				<p class="text-sm text-muted-foreground">Loading…</p>
			{:else if data}
				<!-- only the shown tab does its work (fetches, the force layout) -->
				<Tabs.Content value="board">
					{#if tab === 'board'}<BoardTab
							{ws}
							{issues}
							{closed}
							showProject={project === null}
						/>{/if}
				</Tabs.Content>
				<Tabs.Content value="now"
					>{#if tab === 'now'}<NowTab {ws} {project} />{/if}</Tabs.Content
				>
				<Tabs.Content value="progress">
					{#if tab === 'progress'}<ProgressTab
							{ws}
							{progress}
							showProject={project === null}
						/>{/if}
				</Tabs.Content>
				<Tabs.Content value="docs">
					{#if tab === 'docs'}<DocsTab {ws} {docs} showProject={project === null} />{/if}
				</Tabs.Content>
				<Tabs.Content value="memories">
					{#if tab === 'memories'}<MemoriesTab
							{ws}
							{memories}
							showProject={project === null}
						/>{/if}
				</Tabs.Content>
				<Tabs.Content value="graph"
					>{#if tab === 'graph'}<GraphTab {ws} {project} />{/if}</Tabs.Content
				>
			{/if}
		</Tabs.Root>
	</div>
</div>
