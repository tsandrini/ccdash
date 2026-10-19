<script lang="ts">
	// A note read in full: metadata, rendered body, links and backlinks,
	// the actions (edit, verify, close or reopen, archive); the editor
	// takes its place inline. Reloads when the note changes on the hub.
	import { untrack } from 'svelte';
	import PencilIcon from '@lucide/svelte/icons/pencil';
	import type { LinkRef } from '#lib/api/types.js';
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchNote, noteOp, putNote, type NotePage } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import Prose from '#lib/components/session/Prose.svelte';
	import { fmtDate, fmtStamp, plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { priorityOf, projectHref } from '#lib/notes.js';
	import KindBadge from './KindBadge.svelte';
	import NoteEditor from './NoteEditor.svelte';
	import StatusBadge from './StatusBadge.svelte';

	let {
		ws,
		slug,
		onnavigate,
		edit = false,
		ontitle
	}: {
		ws: string;
		slug: string;
		/** A link to another note of the workspace was followed. */
		onnavigate: (ws: string, slug: string) => void;
		/** Start in the editor. */
		edit?: boolean;
		/** The title once loaded (the slug when the note is missing). */
		ontitle?: (title: string) => void;
	} = $props();

	let view = $state<NotePage | null>(null);
	let error = $state('');
	let editing = $state(false);
	let actionError = $state('');
	let busy = $state(false);

	async function load() {
		const target = `${ws}/${slug}`;
		try {
			const page = await fetchNote(ws, slug);
			if (`${ws}/${slug}` !== target) return;
			view = page;
			error = '';
			ontitle?.(page.title);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else {
				error = (e as Error).message;
				ontitle?.(slug);
			}
		}
	}

	$effect(() => {
		void ws;
		void slug;
		untrack(() => {
			view = null;
			error = '';
			editing = edit;
			actionError = '';
			load();
		});
	});

	$effect(() => {
		const changed = live.notes;
		untrack(() => {
			if (changed.n && changed.workspace === ws && !editing) load();
		});
	});

	async function run(action: () => Promise<unknown>) {
		busy = true;
		actionError = '';
		try {
			await action();
			await load();
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else actionError = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	const verify = () => run(() => noteOp(ws, slug, { op: 'verify', slug }));
	const toggleClosed = () =>
		run(() =>
			noteOp(ws, slug, {
				op: 'issue_update',
				slug,
				status: view?.status === 'archived' ? 'open' : 'closed',
				title: null,
				body: null,
				priority: null,
				comment: null
			})
		);
	const archive = () =>
		run(() => {
			const v = view!;
			return putNote(ws, slug, {
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

	const meta = $derived.by(() => {
		const v = view;
		if (!v) return [];
		return [
			`v${v.version}`,
			`updated ${fmtStamp(v.updated_at)}`,
			v.verified_at ? `verified ${fmtDate(v.verified_at)}` : null,
			v.kind === 'memory' ? plural(v.recalls, 'recall') : null,
			v.kind === 'issue' ? `priority ${priorityOf(v)}` : null,
			v.created_by ? `by ${v.created_by.slice(0, 8)}` : null
		].filter((x): x is string => Boolean(x));
	});
</script>

{#snippet refs(heading: string, list: LinkRef[])}
	{#if list.length}
		<section>
			<h4 class="mb-1 text-xs font-semibold text-muted-foreground uppercase">{heading}</h4>
			<ul class="divide-y rounded-lg border">
				{#each list as l (l.slug + l.kind)}
					<li>
						{#if l.title}
							<button
								type="button"
								class="flex w-full flex-wrap items-center gap-2 px-3 py-1.5 text-left text-sm hover:bg-accent/40"
								onclick={() => onnavigate(ws, l.slug)}
							>
								{#if l.note_kind}<KindBadge kind={l.note_kind} />{/if}
								<span class="font-medium">{l.title}</span>
								<span class="text-xs text-muted-foreground">{l.kind}</span>
								{#if l.status && l.note_kind}<StatusBadge
										note={{ kind: l.note_kind, status: l.status }}
									/>{/if}
							</button>
						{:else}
							<span class="flex items-center gap-2 px-3 py-1.5 text-sm text-muted-foreground"
								><code class="font-mono text-xs">{l.slug}</code> (no such note yet)</span
							>
						{/if}
					</li>
				{/each}
			</ul>
		</section>
	{/if}
{/snippet}

<div class="flex flex-col gap-4">
	{#if error}
		<p class="text-sm text-critical" role="alert">{error}</p>
	{:else if !view}
		<p class="text-sm text-muted-foreground">Loading…</p>
	{:else}
		{@const v = view}
		<header class="flex flex-col gap-1">
			<div class="flex flex-wrap items-center gap-2">
				<KindBadge kind={v.kind} />
				<StatusBadge note={v} />
				<a
					class="text-xs text-muted-foreground hover:underline"
					href={projectHref(v.workspace, v.project)}
					>{v.workspace}{v.project ? ` / ${v.project}` : ''}</a
				>
			</div>
			<h2 class="text-lg leading-tight font-semibold">{v.title}</h2>
			<p class="flex flex-wrap gap-x-2 text-xs text-muted-foreground">
				<code class="font-mono">{v.slug}</code>
				{#each meta as m (m)}<span>· {m}</span>{/each}
			</p>
		</header>
		{#if editing}
			<NoteEditor
				{ws}
				{view}
				onsaved={() => {
					editing = false;
					load();
				}}
				oncancel={() => (editing = false)}
			/>
		{:else}
			{#if v.status === 'stale'}
				<p class="rounded-md border border-warning/60 bg-warning/10 px-3 py-2 text-sm">
					Flagged stale: {v.stale_reason || 'lifecycle'}. Verify it, rewrite it or archive it.
				</p>
			{:else if v.status === 'superseded'}
				<p class="rounded-md border bg-muted px-3 py-2 text-sm">
					Superseded by a newer note (see the backlinks).
				</p>
			{/if}
			{#if v.summary}<p class="text-sm text-muted-foreground italic">{v.summary}</p>{/if}
			<Prose html={v.html} onnote={(target) => onnavigate(ws, target)} />
			{@render refs('Links', v.links)}
			{@render refs('Backlinks', v.backlinks)}
			<footer class="flex flex-wrap items-center gap-2 border-t pt-3">
				<Button size="sm" variant="outline" onclick={() => (editing = true)} disabled={busy}>
					<PencilIcon /> Edit
				</Button>
				{#if v.kind === 'memory'}
					<Button
						size="sm"
						variant="outline"
						onclick={verify}
						disabled={busy}
						title="It is still true: resets the stale flag and the verification clock"
						>Verify</Button
					>
				{/if}
				{#if v.kind === 'issue'}
					<Button size="sm" variant="outline" onclick={toggleClosed} disabled={busy}
						>{v.status === 'archived' ? 'Reopen' : 'Close'}</Button
					>
				{:else if v.status !== 'archived'}
					<Button size="sm" variant="ghost" onclick={archive} disabled={busy}>Archive</Button>
				{/if}
				{#if actionError}<span class="text-xs text-critical" role="alert">{actionError}</span>{/if}
			</footer>
		{/if}
	{/if}
</div>
