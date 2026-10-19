<script lang="ts">
	// The Projects overview: per workspace in scope, the flagged notes and
	// a card per project; a search box over every note with kind, sort
	// and retired-notes toggles.
	import { untrack } from 'svelte';
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import PlusIcon from '@lucide/svelte/icons/plus';
	import type { Hit, NoteKind } from '#lib/api/types.js';
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchNotes } from '#lib/api/index.js';
	import NoteRow from '#lib/components/notes/NoteRow.svelte';
	import ProjectCard from '#lib/components/projects/ProjectCard.svelte';
	import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { KIND_LABEL, KINDS, newNoteHref, projectHref } from '#lib/notes.js';
	import { notePanel } from '#lib/notepanel.svelte.js';
	import { projects } from '#lib/projects.svelte.js';
	import { scope } from '#lib/scope.svelte.js';

	type Sort = 'relevance' | 'updated' | 'recalls';
	const SORTS: { id: Sort; label: string }[] = [
		{ id: 'relevance', label: 'Best match' },
		{ id: 'updated', label: 'Newest' },
		{ id: 'recalls', label: 'Most recalled' }
	];

	let query = $state('');
	let kind = $state<NoteKind | ''>('');
	let sort = $state<Sort>('relevance');
	let retired = $state(true);
	let hits = $state<Hit[] | null>(null);
	let searching = $state(false);
	let timer: ReturnType<typeof setTimeout> | null = null;

	$effect(() => {
		void live.notes.n;
		void projects.claimsSignature;
		const { workspace } = scope.params();
		if (live.ready) projects.schedule(workspace);
	});

	$effect(() => {
		const q = query.trim();
		const { workspace } = scope.params();
		const [k, r] = [kind, retired];
		if (timer) clearTimeout(timer);
		if (!q && !k) {
			hits = null;
			return;
		}
		timer = setTimeout(() => untrack(() => search(q, workspace, k, r)), 250);
	});

	async function search(q: string, workspace: string | undefined, k: NoteKind | '', r: boolean) {
		searching = true;
		const m = q.match(/^kind:(\w+)\s*/);
		try {
			hits = (
				await fetchNotes({
					q: (m ? q.slice(m[0].length) : q) || undefined,
					kind: m?.[1] ?? (k || undefined),
					workspace,
					status: r ? 'active,stale,archived,superseded' : 'active,stale',
					limit: 60
				})
			).hits;
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			hits = [];
		} finally {
			searching = false;
		}
	}

	const workspaces = $derived((projects.data?.workspaces ?? []).filter((w) => w.total > 0));
	/** The hits in the picked order (the hub's is by match). */
	const shown = $derived(
		hits
			? sort === 'relevance'
				? hits
				: [...hits].sort((a, b) =>
						sort === 'updated' ? b.updated_at - a.updated_at : b.recalls - a.recalls
					)
			: null
	);
</script>

<div class="mx-auto flex max-w-6xl flex-col gap-5 p-3 sm:p-4">
	<div class="flex flex-col gap-2">
		<Input
			type="search"
			bind:value={query}
			placeholder="Search notes…"
			aria-label="Search notes"
			class="max-w-md"
		/>
		<div class="flex flex-wrap items-center gap-1.5 text-xs">
			<Button
				variant={kind === '' ? 'secondary' : 'ghost'}
				size="xs"
				aria-pressed={kind === ''}
				onclick={() => (kind = '')}>All kinds</Button
			>
			{#each KINDS as k (k)}
				<Button
					variant={kind === k ? 'secondary' : 'ghost'}
					size="xs"
					aria-pressed={kind === k}
					onclick={() => (kind = kind === k ? '' : k)}>{KIND_LABEL[k]}</Button
				>
			{/each}
			<span class="mx-1 h-4 border-l" aria-hidden="true"></span>
			{#each SORTS as s (s.id)}
				<Button
					variant={sort === s.id ? 'secondary' : 'ghost'}
					size="xs"
					aria-pressed={sort === s.id}
					onclick={() => (sort = s.id)}>{s.label}</Button
				>
			{/each}
			<span class="mx-1 h-4 border-l" aria-hidden="true"></span>
			<label class="inline-flex items-center gap-1.5 text-muted-foreground">
				<input type="checkbox" bind:checked={retired} class="accent-primary" /> retired too
			</label>
		</div>
	</div>
	{#if shown}
		<section class="flex flex-col gap-2">
			<h2 class="text-base font-semibold">
				{searching ? 'Searching…' : `Results (${shown.length})`}
			</h2>
			{#if shown.length}
				<ul class="divide-y rounded-lg border bg-card">
					{#each shown as hit (hit.workspace + hit.slug)}
						<li>
							<NoteRow
								card={hit}
								kind
								workspace
								project
								snippet={hit.snippet}
								onclick={() => notePanel.show(hit.workspace, hit.slug)}
							/>
						</li>
					{/each}
				</ul>
			{:else if !searching}
				<p class="text-sm text-muted-foreground">Nothing matches.</p>
			{/if}
		</section>
	{:else if projects.error}
		<p class="text-sm text-critical" role="alert">{projects.error}</p>
	{:else if !projects.data}
		<p class="text-sm text-muted-foreground">Loading…</p>
	{:else if !workspaces.length}
		<p class="text-sm text-muted-foreground">
			No notes yet. Sessions write them with the ccdash tools; <code>ccdash import memory</code> brings
			in memory files.
		</p>
	{:else}
		{#each workspaces as w (w.name)}
			<section class="flex flex-col gap-2">
				<div class="flex flex-wrap items-baseline gap-2">
					<a href={projectHref(w.name)} class="text-base font-semibold hover:underline">{w.name}</a>
					<span class="text-xs text-muted-foreground"
						>{plural(w.total, 'note')}{w.flagged.length
							? ` · ${w.flagged.length} flagged`
							: ''}</span
					>
					<DropdownMenu.Root>
						<DropdownMenu.Trigger>
							{#snippet child({ props })}
								<Button variant="ghost" size="xs" class="ml-auto" {...props}>
									<PlusIcon /> New <ChevronDownIcon class="size-3 opacity-60" />
								</Button>
							{/snippet}
						</DropdownMenu.Trigger>
						<DropdownMenu.Content align="end">
							<DropdownMenu.Item>
								{#snippet child({ props })}
									<a href={newNoteHref(w.name, 'issue')} {...props}>Issue</a>
								{/snippet}
							</DropdownMenu.Item>
							<DropdownMenu.Item>
								{#snippet child({ props })}
									<a href={newNoteHref(w.name, 'memory')} {...props}>Memory</a>
								{/snippet}
							</DropdownMenu.Item>
							<DropdownMenu.Item>
								{#snippet child({ props })}
									<a href={newNoteHref(w.name, 'doc')} {...props}>Document</a>
								{/snippet}
							</DropdownMenu.Item>
						</DropdownMenu.Content>
					</DropdownMenu.Root>
				</div>
				{#if w.flagged.length}
					<details class="rounded-lg border border-warning/50 bg-card">
						<summary class="cursor-pointer px-3 py-2 text-sm select-none">
							Flagged <span class="text-muted-foreground">{w.flagged.length}</span>
							<span class="text-xs text-muted-foreground">
								· stale memories: verify, rewrite or archive</span
							>
						</summary>
						<ul class="divide-y border-t">
							{#each w.flagged as c (c.slug)}
								<li>
									<NoteRow
										card={c}
										kind
										project
										reason
										onclick={() => notePanel.show(w.name, c.slug)}
									/>
								</li>
							{/each}
						</ul>
					</details>
				{/if}
				<div class="grid gap-2 sm:grid-cols-2 lg:grid-cols-3">
					{#each w.projects as p (p.project ?? '')}
						<ProjectCard ws={w.name} project={p} />
					{/each}
				</div>
			</section>
		{/each}
	{/if}
</div>
