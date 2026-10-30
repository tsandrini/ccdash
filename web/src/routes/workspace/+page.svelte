<script lang="ts">
	// The Workspace page: the workspace root as one logical tree on every
	// machine. Machines, the units × hosts grid with filters, new
	// directories, held snapshots, conflicts, the configuration.
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchWorkspace, type WorkspaceView } from '#lib/api/index.js';
	import StatTile from '#lib/components/charts/StatTile.svelte';
	import Tiles from '#lib/components/charts/Tiles.svelte';
	import ConfigEditor from '#lib/components/workspace/ConfigEditor.svelte';
	import HeldList from '#lib/components/workspace/HeldList.svelte';
	import HostsStrip from '#lib/components/workspace/HostsStrip.svelte';
	import NewDirs from '#lib/components/workspace/NewDirs.svelte';
	import UnitGrid from '#lib/components/workspace/UnitGrid.svelte';
	import WsConflicts from '#lib/components/workspace/WsConflicts.svelte';
	import { fmtBytes } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { bundleOf } from '#lib/workspace.js';

	const POLL_MS = 3000;

	let view = $state<WorkspaceView | null>(null);
	let error = $state('');
	let off = $state(false);

	async function load() {
		try {
			view = await fetchWorkspace();
			error = '';
			off = false;
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else if ((e as { status?: number }).status === 404) off = true;
			else error = (e as Error).message;
		}
	}

	$effect(() => {
		if (!live.ready) return;
		load();
		const timer = setInterval(load, POLL_MS);
		return () => clearInterval(timer);
	});

	const inStep = $derived(
		view
			? view.units.filter((u) => {
					const states = Object.values(u.hosts);
					return states.length > 0 && states.every((s) => s.state === 'in_step');
				}).length
			: 0
	);
	const bundles = $derived(
		view ? [...new Set(view.units.map((u) => bundleOf(u.unit)))].sort() : []
	);
	const today = $derived(view?.growth.at(-1)?.[1] ?? 0);
</script>

<div class="mx-auto flex max-w-6xl flex-col gap-5 p-3 sm:p-4">
	<p class="text-sm text-muted-foreground">
		Every repository and plain directory of the workspace root, snapshotted on each machine as it
		changes and kept in step through the hub: a file saved here is there within a second, a branch
		switch, a staged file and a stash follow, two edits of one hunk end as an ordinary git conflict
		marked below.
	</p>
	{#if off}
		<p class="text-sm text-muted-foreground">
			The workspace sync is off on this hub (<code class="font-mono">workspace.enable</code>).
		</p>
	{:else if error}
		<p class="text-sm text-critical" role="alert">{error}</p>
	{:else if view}
		<Tiles>
			<StatTile name="Units" value={view.units.length} sub="{inStep} in step on every machine" />
			<StatTile
				name="Machines"
				value={view.hosts.filter((h) => h.status).length}
				sub="{view.hosts.filter((h) => h.connected).length} connected"
			/>
			<StatTile
				name="Needs you"
				value={view.conflicts.length +
					view.held.length +
					view.new_dirs.filter((d) => d.state === 'waiting').length}
				sub="{view.conflicts.length} conflicts, {view.held.length} held, {view.new_dirs.filter(
					(d) => d.state === 'waiting'
				).length} new"
			/>
			<StatTile name="On the hub" value={fmtBytes(view.bytes)} sub="+{fmtBytes(today)} today" />
		</Tiles>
		<HostsStrip hosts={view.hosts} onchange={load} />
		<UnitGrid units={view.units} hosts={view.hosts.filter((h) => h.status)} />
		<NewDirs dirs={view.new_dirs} onchange={load} />
		<HeldList held={view.held} onchange={load} />
		<WsConflicts conflicts={view.conflicts} onchange={load} />
		<ConfigEditor config={view.config} version={view.config_version} {bundles} onchange={load} />
	{:else}
		<p class="text-sm text-muted-foreground">Loading…</p>
	{/if}
</div>
