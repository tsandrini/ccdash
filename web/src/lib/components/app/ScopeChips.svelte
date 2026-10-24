<script lang="ts">
	// Workspace and host filters, kept on the device (`scope`).
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { live } from '#lib/live/store.svelte.js';
	import { scope } from '#lib/scope.svelte.js';

	const current = $derived(scope.current);
	const wsLabel = $derived(
		!current.ws.length
			? 'All workspaces'
			: current.ws.length === 1
				? current.ws[0]
				: `${current.ws.length} workspaces`
	);
	const hostLabel = $derived(current.host ? live.hostLabel(current.host) : 'All hosts');

	function toggleWs(name: string, on: boolean) {
		const rest = current.ws.filter((n) => n !== name);
		scope.set({ ws: on ? live.workspaces.filter((n) => n === name || rest.includes(n)) : rest });
	}
</script>

{#if live.workspaces.length || current.ws.length}
	<DropdownMenu.Root>
		<DropdownMenu.Trigger>
			{#snippet child({ props })}
				<Button variant="outline" size="sm" class="max-w-40 gap-1 truncate" {...props}>
					<span class="truncate">{wsLabel}</span>
					<ChevronDownIcon class="size-3 opacity-60" />
				</Button>
			{/snippet}
		</DropdownMenu.Trigger>
		<DropdownMenu.Content align="start">
			<DropdownMenu.CheckboxItem
				checked={!current.ws.length}
				onCheckedChange={() => scope.set({ ws: [] })}>All</DropdownMenu.CheckboxItem
			>
			<DropdownMenu.Separator />
			{#each live.workspaces as name (name)}
				<DropdownMenu.CheckboxItem
					checked={current.ws.includes(name)}
					closeOnSelect={false}
					onCheckedChange={(on) => toggleWs(name, on)}>{name}</DropdownMenu.CheckboxItem
				>
			{/each}
		</DropdownMenu.Content>
	</DropdownMenu.Root>
{/if}
{#if live.multiHost || current.host}
	<DropdownMenu.Root>
		<DropdownMenu.Trigger>
			{#snippet child({ props })}
				<Button variant="outline" size="sm" class="max-w-36 gap-1 truncate" {...props}>
					<span class="truncate">{hostLabel}</span>
					<ChevronDownIcon class="size-3 opacity-60" />
				</Button>
			{/snippet}
		</DropdownMenu.Trigger>
		<DropdownMenu.Content align="start">
			<DropdownMenu.RadioGroup value={current.host} onValueChange={(host) => scope.set({ host })}>
				<DropdownMenu.RadioItem value="">All</DropdownMenu.RadioItem>
				{#each live.hosts as h (h.name)}
					<DropdownMenu.RadioItem value={h.name}>{live.hostLabel(h.name)}</DropdownMenu.RadioItem>
				{/each}
			</DropdownMenu.RadioGroup>
		</DropdownMenu.Content>
	</DropdownMenu.Root>
{/if}
