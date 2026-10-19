<script lang="ts">
	// A session state with its colour dot.
	import { ACTIVITY, type Shown } from '#lib/session.js';
	import { cn } from '#lib/utils.js';

	let { activity, class: className }: { activity: Shown; class?: string } = $props();

	const dot: Record<Shown, string> = {
		permission: 'bg-permission',
		your_turn: 'bg-your-turn',
		working: 'bg-working',
		parked: 'bg-parked',
		idle: 'bg-idle',
		gone: 'bg-idle opacity-60',
		finished: 'bg-your-turn opacity-70'
	};
</script>

<span
	class={cn(
		'inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-xs whitespace-nowrap',
		className
	)}
	data-activity={activity}
>
	<i
		class={cn(
			'inline-block size-2 rounded-full',
			dot[activity],
			activity === 'working' && 'animate-pulse'
		)}
		aria-hidden="true"
	></i>
	{ACTIVITY[activity].label}
</span>
