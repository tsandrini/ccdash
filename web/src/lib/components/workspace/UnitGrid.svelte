<script lang="ts">
	// Workspace › Units: the grid of units × hosts, grouped by bundle, with
	// the filters (changed today, conflicts, held, paused, a search).
	import type { UnitView, WorkspaceHost } from '#lib/api/types.js';
	import StateChip from '#lib/components/workspace/StateChip.svelte';
	import { Input } from '#lib/components/ui/input/index.js';
	import { fmtBytes, fmtStamp } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { cn } from '#lib/utils.js';
	import { bundleOf, unitHref, unitName, worst } from '#lib/workspace.js';

	let { units, hosts }: { units: UnitView[]; hosts: WorkspaceHost[] } = $props();

	type Filter = 'all' | 'today' | 'attention' | 'conflicts' | 'held' | 'paused';
	const FILTERS: { id: Filter; label: string }[] = [
		{ id: 'all', label: 'All' },
		{ id: 'today', label: 'Changed today' },
		{ id: 'attention', label: 'Needs you' },
		{ id: 'conflicts', label: 'Conflicts' },
		{ id: 'held', label: 'Held' },
		{ id: 'paused', label: 'Paused' }
	];

	let filter = $state<Filter>('all');
	let query = $state('');

	const DAY_MS = 86_400_000;

	function passes(u: UnitView): boolean {
		const q = query.trim().toLowerCase();
		if (q && !u.unit.toLowerCase().includes(q)) return false;
		const states = Object.values(u.hosts);
		switch (filter) {
			case 'today':
				return live.now - u.at < DAY_MS;
			case 'attention':
				return states.some((s) => ['conflict', 'held', 'failed', 'unadopted'].includes(s.state));
			case 'conflicts':
				return states.some((s) => s.conflicts.length > 0);
			case 'held':
				return states.some((s) => s.held !== null);
			case 'paused':
				return u.paused;
			default:
				return true;
		}
	}

	const shown = $derived(units.filter(passes));
	const bundles = $derived.by(() => {
		const groups: Record<string, UnitView[]> = {};
		for (const u of shown) {
			const b = bundleOf(u.unit);
			groups[b] = [...(groups[b] ?? []), u];
		}
		return Object.entries(groups);
	});
	const hostNames = $derived(hosts.map((h) => h.host));
</script>

<section class="flex flex-col gap-2">
	<div class="flex flex-wrap items-center gap-2">
		<h2 class="text-base font-semibold">Units</h2>
		<span class="text-xs text-muted-foreground">{shown.length} of {units.length}</span>
		<div class="ml-auto flex flex-wrap items-center gap-1">
			{#each FILTERS as f (f.id)}
				<button
					type="button"
					class={cn(
						'rounded-full border px-2 py-0.5 text-xs',
						filter === f.id && 'bg-sidebar-accent font-medium'
					)}
					aria-pressed={filter === f.id}
					onclick={() => (filter = f.id)}>{f.label}</button
				>
			{/each}
			<Input
				bind:value={query}
				class="h-7 max-w-40 text-xs"
				placeholder="Find a unit"
				aria-label="Find a unit"
			/>
		</div>
	</div>
	{#if !units.length}
		<p class="text-sm text-muted-foreground">
			No unit travels yet. Collectors report what they find at unit depth; include directories
			below, or widen the globs in the configuration.
		</p>
	{:else}
		<div class="overflow-x-auto rounded-md border">
			<table class="w-full text-sm">
				<thead class="bg-muted/50 text-left text-xs text-muted-foreground">
					<tr>
						<th class="px-2 py-1.5 font-medium">Unit</th>
						{#each hostNames as h (h)}
							<th class="px-2 py-1.5 font-medium">{h}</th>
						{/each}
						<th class="px-2 py-1.5 text-right font-medium">Last push</th>
					</tr>
				</thead>
				<tbody>
					{#each bundles as [bundle, list] (bundle)}
						<tr class="bg-muted/30">
							<td class="px-2 py-1 text-xs font-semibold" colspan={hostNames.length + 2}
								>{bundle}</td
							>
						</tr>
						{#each list as u (u.unit)}
							{@const w = worst(u)}
							<tr class="border-t hover:bg-muted/40">
								<td class="px-2 py-1">
									<a href={unitHref(u.unit)} class="flex items-center gap-2 hover:underline">
										<span class="font-mono text-xs">{unitName(u.unit)}</span>
										{#if u.kind === 'plain'}
											<span class="rounded bg-muted px-1 text-[10px] text-muted-foreground"
												>plain</span
											>
										{/if}
										{#if u.paused}
											<span class="rounded bg-muted px-1 text-[10px] text-muted-foreground"
												>paused</span
											>
										{/if}
										{#if u.size}
											<span class="text-[10px] text-muted-foreground">{fmtBytes(u.size)}</span>
										{/if}
										{#if w && ['conflict', 'held', 'failed', 'unadopted'].includes(w)}
											<StateChip state={w} class="ml-auto" />
										{/if}
									</a>
								</td>
								{#each hostNames as h (h)}
									{@const s = u.hosts[h]}
									<td class="px-2 py-1">
										<StateChip
											state={s?.state ?? null}
											detail={s?.detail ?? (s ? null : 'this host has not reported the unit')}
										/>
									</td>
								{/each}
								<td class="px-2 py-1 text-right text-xs text-muted-foreground tabular-nums">
									{#if u.version}
										{fmtStamp(u.at)}
									{:else}
										—
									{/if}
								</td>
							</tr>
						{/each}
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</section>
