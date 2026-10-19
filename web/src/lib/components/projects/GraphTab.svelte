<script lang="ts">
	// The graph tab: filters by kind and project, retired notes on demand,
	// a slider over creation time that plays the growth, the legend.
	import { untrack } from 'svelte';
	import PauseIcon from '@lucide/svelte/icons/pause';
	import PlayIcon from '@lucide/svelte/icons/play';
	import type { Graph, NoteKind } from '#lib/api/types.js';
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchGraph } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Toggle } from '#lib/components/ui/toggle/index.js';
	import { fmtDate, plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { KIND_COLOR, KIND_LABEL, KINDS } from '#lib/notes.js';
	import { notePanel } from '#lib/notepanel.svelte.js';
	import NoteGraph from './NoteGraph.svelte';

	const PLAY_MS = 8000;

	let { ws, project }: { ws: string; project: string | null } = $props();

	let graph = $state<Graph | null>(null);
	let error = $state('');
	let retired = $state(false);
	let kinds = $state<Record<NoteKind, boolean>>({
		memory: true,
		issue: true,
		doc: true,
		progress: true
	});
	let pickedProject = $state<string | null>(null);
	let projectSet = $state(false);
	/** Upper bound on `created_at`; null = everything. */
	let until = $state<number | null>(null);
	let playing = $state(false);

	async function load() {
		try {
			graph = await fetchGraph(ws, retired ? 'active,stale,superseded,archived' : undefined);
			error = '';
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		}
	}

	$effect(() => {
		void ws;
		void retired;
		void live.notes.n;
		untrack(load);
	});

	// The project page starts scoped to its project; the workspace page to all.
	$effect(() => {
		void project;
		untrack(() => {
			if (!projectSet) pickedProject = project;
		});
	});

	const projectsSeen = $derived(
		[...new Set((graph?.nodes ?? []).map((n) => n.project ?? ''))].sort()
	);
	const span = $derived.by(() => {
		const times = (graph?.nodes ?? []).map((n) => n.created_at);
		return times.length ? [Math.min(...times), Math.max(...times)] : [0, 0];
	});
	const nodes = $derived(
		(graph?.nodes ?? []).filter(
			(n) =>
				kinds[n.kind] &&
				(pickedProject === null || (n.project ?? '') === pickedProject) &&
				(until === null || n.created_at <= until)
		)
	);
	const ids = $derived(new Set(nodes.map((n) => n.id)));
	const edges = $derived((graph?.edges ?? []).filter((e) => ids.has(e.src) && ids.has(e.dst)));

	let frame = 0;
	function play() {
		if (playing) {
			playing = false;
			cancelAnimationFrame(frame);
			return;
		}
		playing = true;
		const [from, to] = span;
		const start = performance.now();
		const step = (now: number) => {
			const t = Math.min(1, (now - start) / PLAY_MS);
			until = from + (to - from) * t;
			if (t < 1 && playing) frame = requestAnimationFrame(step);
			else {
				playing = false;
				until = null;
			}
		};
		frame = requestAnimationFrame(step);
	}
</script>

<div class="flex flex-col gap-3">
	<div class="flex flex-wrap items-center gap-2 text-xs">
		{#each KINDS as kind (kind)}
			<Toggle
				size="sm"
				variant="outline"
				pressed={kinds[kind]}
				onPressedChange={(on) => (kinds = { ...kinds, [kind]: on })}
				aria-label={KIND_LABEL[kind]}
			>
				<i
					class="inline-block size-2 rounded-full"
					style="background: {KIND_COLOR[kind]}"
					aria-hidden="true"
				></i>
				{KIND_LABEL[kind]}
			</Toggle>
		{/each}
		<Toggle size="sm" variant="outline" bind:pressed={retired}>Retired notes</Toggle>
		{#if projectsSeen.length > 1}
			<select
				class="h-8 rounded-lg border border-input bg-transparent px-2 text-xs dark:bg-input/30"
				value={pickedProject ?? '*'}
				onchange={(e) => {
					const v = (e.currentTarget as HTMLSelectElement).value;
					pickedProject = v === '*' ? null : v;
					projectSet = true;
				}}
				aria-label="Project"
			>
				<option value="*">All projects</option>
				{#each projectsSeen as p (p)}<option value={p}>{p || 'Workspace-wide'}</option>{/each}
			</select>
		{/if}
		<span class="ml-auto text-muted-foreground"
			>{plural(nodes.length, 'note')} · {plural(edges.length, 'link')}</span
		>
	</div>
	{#if error}
		<p class="text-sm text-critical" role="alert">{error}</p>
	{:else if !graph}
		<p class="text-sm text-muted-foreground">Loading…</p>
	{:else}
		<NoteGraph {nodes} {edges} onpick={(n) => notePanel.show(ws, n.slug)} />
		{#if span[1] > span[0]}
			<div class="flex items-center gap-2 text-xs">
				<Button
					variant="outline"
					size="icon-sm"
					aria-label={playing ? 'Pause' : 'Play the growth'}
					onclick={play}
				>
					{#if playing}<PauseIcon />{:else}<PlayIcon />{/if}
				</Button>
				<span class="w-20 shrink-0 text-muted-foreground tabular-nums">{fmtDate(span[0])}</span>
				<input
					type="range"
					class="flex-1 accent-foreground"
					min={span[0]}
					max={span[1]}
					step={Math.max(1, Math.round((span[1] - span[0]) / 500))}
					value={until ?? span[1]}
					oninput={(e) => {
						playing = false;
						const v = Number((e.currentTarget as HTMLInputElement).value);
						until = v >= span[1] ? null : v;
					}}
					aria-label="Created before"
				/>
				<span class="w-20 shrink-0 text-right text-muted-foreground tabular-nums"
					>{fmtDate(until ?? span[1])}</span
				>
			</div>
		{/if}
		<p class="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
			<span>Size: recalls</span>
			<span
				><span class="inline-block w-5 border-t border-muted-foreground align-middle"></span> mentions</span
			>
			<span
				><span class="inline-block w-5 border-t border-dotted border-muted-foreground align-middle"
				></span> relates (progress on an issue)</span
			>
			<span
				><span
					class="inline-block w-5 border-t-2 border-dashed border-muted-foreground align-middle"
				></span> supersedes</span
			>
			<span
				><span class="inline-block w-5 border-t border-critical align-middle"></span> blocks</span
			>
			<span>Dashed ring: stale · faded: retired</span>
		</p>
	{/if}
</div>
