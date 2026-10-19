<script lang="ts">
	// Analytics: one payload for the scope and range in the topbar, shown
	// by the section in the path; reloaded as the scope or range changes
	// and every five minutes while the range reaches now.
	import { untrack } from 'svelte';
	import { page } from '$app/state';
	import { fetchAnalytics } from '#lib/api/index.js';
	import { SECTIONS, type Section } from '#lib/analytics.js';
	import ContextSection from '#lib/components/analytics/ContextSection.svelte';
	import LimitsSection from '#lib/components/analytics/LimitsSection.svelte';
	import OverviewSection from '#lib/components/analytics/OverviewSection.svelte';
	import ParallelSection from '#lib/components/analytics/ParallelSection.svelte';
	import PermissionsSection from '#lib/components/analytics/PermissionsSection.svelte';
	import ProjectsSection from '#lib/components/analytics/ProjectsSection.svelte';
	import TokensSection from '#lib/components/analytics/TokensSection.svelte';
	import TurnsSection from '#lib/components/analytics/TurnsSection.svelte';
	import WaitsSection from '#lib/components/analytics/WaitsSection.svelte';
	import { encodeRange, rangeParams, reachesNow, type Range } from '#lib/range.js';
	import { live } from '#lib/live/store.svelte.js';
	import { rangeState } from '#lib/range.svelte.js';
	import { analyticsData } from '#lib/ranged.svelte.js';
	import { scope } from '#lib/scope.svelte.js';

	const views = {
		overview: OverviewSection,
		waits: WaitsSection,
		parallel: ParallelSection,
		limits: LimitsSection,
		projects: ProjectsSection,
		turns: TurnsSection,
		context: ContextSection,
		tokens: TokensSection,
		permissions: PermissionsSection
	};

	const section = $derived<Section>(
		SECTIONS.find(([id]) => id === page.params.section)?.[0] ?? 'overview'
	);
	const View = $derived(views[section]);

	function load(range: Range, workspace?: string, host?: string, force = false) {
		const tz = -new Date().getTimezoneOffset();
		const key = `${encodeRange(range)}|${tz}|${workspace ?? ''}|${host ?? ''}`;
		analyticsData.load(
			key,
			() => fetchAnalytics({ ...rangeParams(range, rangeState.now()), tz, workspace, host }),
			force
		);
	}

	$effect(() => {
		if (!live.ready) return;
		const { workspace, host } = scope.params();
		const range = rangeState.current;
		untrack(() => load(range, workspace, host));
		if (!reachesNow(range, rangeState.now())) return;
		const timer = setInterval(() => load(range, workspace, host, true), 300_000);
		return () => clearInterval(timer);
	});

	let tabs = $state<HTMLElement | null>(null);
	/** Tabs hidden past the right edge: the row fades there. */
	let more = $state(false);
	const measure = () => {
		if (tabs) more = tabs.scrollLeft + tabs.clientWidth < tabs.scrollWidth - 2;
	};
	$effect(() => {
		void section;
		tabs
			?.querySelector('[aria-current="page"]')
			?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
		requestAnimationFrame(measure);
	});
</script>

<svelte:window onresize={measure} />

<div class="mx-auto flex max-w-6xl flex-col gap-3 p-3 sm:p-4">
	<nav
		class="-mx-3 overflow-x-auto px-3 sm:mx-0 sm:px-0 {more ? 'mask-r-from-85%' : ''}"
		aria-label="Analytics section"
		bind:this={tabs}
		onscroll={measure}
	>
		<div class="inline-flex w-fit gap-0.5 rounded-lg bg-muted p-[3px] text-sm whitespace-nowrap">
			{#each SECTIONS as [id, label] (id)}
				<a
					href="/analytics/{id}{page.url.search}"
					aria-current={id === section ? 'page' : undefined}
					class="rounded-md px-2.5 py-1 {id === section
						? 'bg-background font-medium text-foreground shadow-sm'
						: 'text-foreground/60 hover:text-foreground'}">{label}</a
				>
			{/each}
		</div>
	</nav>
	{#if analyticsData.error}
		<p class="text-sm text-critical" role="alert">{analyticsData.error}</p>
	{:else if !analyticsData.data}
		<p class="text-sm text-muted-foreground">Loading…</p>
	{:else}
		<div class="flex flex-col gap-3 transition-opacity" class:opacity-60={analyticsData.loading}>
			<View data={analyticsData.data} />
		</div>
	{/if}
</div>
