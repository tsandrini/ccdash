<script lang="ts">
	// The editor for a note: new (workspace, kind, slug) or existing. Saves
	// with the version it was based on; when the note moved on meanwhile,
	// the owner chooses between loading theirs and overwriting with this.
	import type { Note, NoteKind, Priority, PutNote, Status } from '#lib/api/types.js';
	import { ApiError, Unauthorized } from '#lib/api/client.js';
	import { fetchNote, putNote, type NotePage } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Textarea } from '#lib/components/ui/textarea/index.js';
	import { live } from '#lib/live/store.svelte.js';
	import { KIND_LABEL, priorityOf, slugify, validSlug } from '#lib/notes.js';

	let {
		ws,
		view = null,
		kind: newKind = 'memory',
		project: newProject = null,
		onsaved,
		oncancel
	}: {
		ws: string;
		/** The note being edited; null for a new one. */
		view?: NotePage | null;
		kind?: NoteKind;
		project?: string | null;
		onsaved: (note: Note) => void;
		oncancel: () => void;
	} = $props();

	const isNew = $derived(!view);
	const kind = $derived(view?.kind ?? newKind);

	let workspace = $state('');
	let title = $state('');
	let slug = $state('');
	let project = $state('');
	let summary = $state('');
	let priority = $state<Priority>('normal');
	let status = $state<Status>('active');
	let body = $state('');
	let base = $state<number | undefined>(undefined);
	let error = $state('');
	let conflict = $state<NotePage | null>(null);
	let busy = $state(false);

	function fill(v: NotePage | null) {
		workspace = v?.workspace ?? ws;
		title = v?.title ?? '';
		slug = v?.slug ?? '';
		project = v?.project ?? newProject ?? '';
		summary = v?.summary ?? '';
		priority = v ? priorityOf(v) : 'normal';
		status = v?.status ?? 'active';
		body = v?.body ?? '';
		base = v?.version;
	}

	// svelte-ignore state_referenced_locally
	fill(view);

	const statuses = $derived<[Status, string][]>(
		kind === 'issue'
			? [
					['active', 'open'],
					['archived', 'closed']
				]
			: [
					['active', 'active'],
					['stale', 'stale'],
					['superseded', 'superseded'],
					['archived', 'archived']
				]
	);

	async function save(baseVersion = base) {
		error = '';
		conflict = null;
		const theSlug = isNew ? slug.trim() || slugify(title) : view!.slug;
		if (!theSlug) return void (error = 'The title gives no slug; set one.');
		if (!validSlug(theSlug))
			return void (error = `${theSlug} is not a slug (lowercase words joined with -).`);
		if (!title.trim()) return void (error = 'A title is needed.');
		const extra = { ...(view?.extra ?? {}) };
		if (kind === 'issue') extra.priority = priority;
		const put: PutNote = {
			kind,
			title: title.trim(),
			body,
			summary: summary.trim() || null,
			project: project.trim() || null,
			status,
			extra,
			base_version: baseVersion ?? null
		};
		busy = true;
		try {
			const { note } = await putNote(workspace, theSlug, put);
			onsaved(note);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else if (e instanceof ApiError && e.status === 409) {
				error = e.message;
				try {
					conflict = await fetchNote(workspace, theSlug);
				} catch {
					// the message is enough
				}
			} else error = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	function onkeydown(event: KeyboardEvent) {
		if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') {
			event.preventDefault();
			save();
		}
	}
</script>

<form
	class="flex flex-col gap-3"
	onsubmit={(e) => {
		e.preventDefault();
		save();
	}}
>
	<div class="grid gap-3 sm:grid-cols-2">
		{#if isNew}
			<label class="flex flex-col gap-1 text-xs text-muted-foreground">
				Workspace
				<select
					class="h-8 rounded-lg border border-input bg-transparent px-2 text-sm text-foreground dark:bg-input/30"
					bind:value={workspace}
				>
					{#each live.workspaces as w (w)}<option value={w}>{w}</option>{/each}
				</select>
			</label>
		{/if}
		<label class="flex flex-col gap-1 text-xs text-muted-foreground {isNew ? '' : 'sm:col-span-2'}">
			Title
			<Input bind:value={title} required placeholder="Title" class="text-foreground" />
		</label>
		{#if isNew}
			<label class="flex flex-col gap-1 text-xs text-muted-foreground">
				Slug
				<Input
					bind:value={slug}
					placeholder={slugify(title) || 'from the title when empty'}
					class="font-mono text-foreground"
					pattern="[a-z0-9][a-z0-9._-]*"
				/>
			</label>
		{/if}
		<label class="flex flex-col gap-1 text-xs text-muted-foreground">
			Project
			<Input
				bind:value={project}
				placeholder="repo directory; empty = workspace-wide"
				class="text-foreground"
			/>
		</label>
		{#if kind === 'issue'}
			<label class="flex flex-col gap-1 text-xs text-muted-foreground">
				Priority
				<select
					class="h-8 rounded-lg border border-input bg-transparent px-2 text-sm text-foreground dark:bg-input/30"
					bind:value={priority}
				>
					<option value="low">low</option>
					<option value="normal">normal</option>
					<option value="high">high</option>
				</select>
			</label>
		{/if}
		<label class="flex flex-col gap-1 text-xs text-muted-foreground">
			Status
			<select
				class="h-8 rounded-lg border border-input bg-transparent px-2 text-sm text-foreground dark:bg-input/30"
				bind:value={status}
			>
				{#each statuses as [value, label] (value)}<option {value}>{label}</option>{/each}
			</select>
		</label>
		{#if kind === 'memory' || kind === 'doc'}
			<label class="flex flex-col gap-1 text-xs text-muted-foreground sm:col-span-2">
				Summary
				<Input
					bind:value={summary}
					placeholder="one line for indexes and session-start injections"
					class="text-foreground"
				/>
			</label>
		{/if}
	</div>
	<label class="flex flex-col gap-1 text-xs text-muted-foreground">
		Body
		<Textarea
			bind:value={body}
			rows={16}
			placeholder="Markdown; link notes as [[slug]]"
			class="min-h-64 font-mono text-[0.8rem] text-foreground"
			{onkeydown}
		/>
	</label>
	{#if error}
		<div class="flex flex-col gap-2 rounded-md border border-critical/50 p-2 text-sm" role="alert">
			<span class="text-critical">{error}</span>
			{#if conflict}
				<span class="text-xs text-muted-foreground"
					>Theirs is version {conflict.version}, updated by {conflict.updated_by ?? 'someone'}. Your
					text stays in the editor until you choose.</span
				>
				<span class="flex flex-wrap gap-2">
					<Button
						type="button"
						size="sm"
						variant="outline"
						onclick={() => {
							fill(conflict);
							error = '';
							conflict = null;
						}}>Load theirs (drop mine)</Button
					>
					<Button
						type="button"
						size="sm"
						variant="destructive"
						onclick={() => save(conflict?.version)}>Overwrite with mine</Button
					>
				</span>
			{/if}
		</div>
	{/if}
	<div class="flex items-center gap-2">
		<Button type="submit" size="sm" disabled={busy}
			>{isNew ? `Create ${KIND_LABEL[kind].toLowerCase()}` : 'Save'}</Button
		>
		<Button type="button" size="sm" variant="ghost" onclick={oncancel}>Cancel</Button>
		<span class="ml-auto text-xs text-muted-foreground">Ctrl Enter saves</span>
	</div>
</form>
