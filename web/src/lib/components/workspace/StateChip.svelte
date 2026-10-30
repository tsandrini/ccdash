<script lang="ts">
	// A unit's state on one host (or across hosts) as a dot and a word.
	import type { UnitState } from '#lib/api/types.js';
	import { cn } from '#lib/utils.js';
	import { STATE } from '#lib/workspace.js';

	let {
		state,
		detail,
		class: className
	}: { state: UnitState | null; detail?: string | null; class?: string } = $props();

	const info = $derived(state ? STATE[state] : null);
</script>

<span
	class={cn(
		'inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-xs whitespace-nowrap',
		className
	)}
	title={detail ?? undefined}
	data-state={state}
>
	<i
		class={cn(
			'inline-block size-2 rounded-full',
			info?.dot ?? 'bg-idle opacity-50',
			info?.busy && 'animate-pulse'
		)}
		aria-hidden="true"
	></i>
	{info?.label ?? 'not here'}
</span>
