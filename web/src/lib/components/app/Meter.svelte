<script lang="ts">
	// A thin bar: context, task progress, limits.
	import { meterLevel, type Level } from '#lib/format.js';
	import { cn } from '#lib/utils.js';

	let {
		pct,
		level = 'auto',
		label,
		class: className
	}: { pct: number; level?: Level | 'auto' | 'neutral'; label?: string; class?: string } = $props();

	const fill = $derived(
		{
			normal: 'bg-working',
			warning: 'bg-warning',
			critical: 'bg-critical',
			neutral: 'bg-your-turn'
		}[level === 'auto' ? meterLevel(pct) : level]
	);
	const width = $derived(Math.min(100, Math.max(0, pct)));
</script>

<span
	class={cn(
		'inline-block h-1.5 w-14 overflow-hidden rounded-full bg-muted align-middle',
		className
	)}
	role="meter"
	aria-valuenow={Math.round(pct)}
	aria-valuemin={0}
	aria-valuemax={100}
	aria-label={label}
>
	<span class="block h-full rounded-full {fill}" style:width="{width}%"></span>
</span>
