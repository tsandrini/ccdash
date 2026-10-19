<script lang="ts">
	// The areas, from the registry; a count where the area has one.
	import { page } from '$app/state';
	import { areas } from '#lib/nav.js';
	import { live } from '#lib/live/store.svelte.js';
	import { scope } from '#lib/scope.svelte.js';
	import { cn } from '#lib/utils.js';

	let { onnavigate }: { onnavigate?: () => void } = $props();

	const inScope = $derived(live.list.filter((s) => scope.covers(s)));
	const search = $derived(page.url.search);
</script>

<nav class="flex flex-col gap-0.5 p-2" aria-label="Areas">
	{#each areas as area (area.href)}
		{@const active = area.match(page.url.pathname)}
		{@const badge = area.badge?.(inScope)}
		<a
			href={`${area.href}${search}`}
			aria-current={active ? 'page' : undefined}
			class={cn(
				'flex items-center gap-2.5 rounded-md px-2.5 py-2 text-sm hover:bg-sidebar-accent hover:text-sidebar-accent-foreground',
				active && 'bg-sidebar-accent font-medium text-sidebar-accent-foreground'
			)}
			onclick={onnavigate}
		>
			<area.icon class="size-4 shrink-0 opacity-80" />
			<span class="flex-1">{area.label}</span>
			{#if badge}
				<span class="rounded-full bg-muted px-1.5 text-xs text-muted-foreground tabular-nums"
					>{badge}</span
				>
			{/if}
		</a>
	{/each}
</nav>
