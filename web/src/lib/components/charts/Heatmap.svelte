<script lang="ts">
	// Weekday × hour counts, Monday first, on the one-hue ramp.
	import { Chart, Rect, Svg, Text, Tooltip } from 'layerchart';
	import { plural } from '#lib/format.js';
	import Tip from './Tip.svelte';

	let { matrix, unit = 'prompt' }: { matrix: number[][]; unit?: string } = $props();

	const NAMES = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
	const PAD = { top: 16, left: 34 };
	const LEVELS = 6;
	const pad2 = (n: number) => String(n).padStart(2, '0');

	let width = $state(600);
	const cell = $derived(Math.max(9, Math.min(28, Math.floor((width - PAD.left) / 24))));
	const height = $derived(PAD.top + 7 * cell);
	const max = $derived(Math.max(0, ...matrix.flat()));
	const level = (n: number) => (n ? Math.max(1, Math.ceil((n / max) * LEVELS)) : 0);
	const cells = $derived(matrix.flatMap((row, day) => row.map((n, hour) => ({ day, hour, n }))));
	const hourStep = $derived(cell < 16 ? 6 : 3);
</script>

<div bind:clientWidth={width} class="flex flex-col gap-2 text-xs">
	<div class="overflow-x-auto">
		<div style:width="{PAD.left + 24 * cell}px" style:height="{height}px">
			<Chart data={cells} padding={0} tooltipContext={{ mode: 'manual' }}>
				{#snippet children({ context })}
					<Svg>
						{#each Array.from({ length: 24 / hourStep }, (_, i) => i * hourStep) as hour (hour)}
							<Text
								x={PAD.left + hour * cell + 1}
								y={11}
								value={pad2(hour)}
								class="fill-muted-foreground"
							/>
						{/each}
						{#each NAMES as name, day (name)}
							<Text
								x={0}
								y={PAD.top + day * cell + cell / 2 + 4}
								value={name}
								class="fill-muted-foreground"
							/>
						{/each}
						{#each cells as c (c.day * 24 + c.hour)}
							<Rect
								x={PAD.left + c.hour * cell}
								y={PAD.top + c.day * cell}
								width={cell - 2}
								height={cell - 2}
								rx={2}
								fill={c.n ? `var(--seq-${level(c.n)})` : 'var(--muted)'}
								class="hover:stroke-foreground"
								onpointermove={(e) => context.tooltip.show(e, c)}
								onpointerleave={() => context.tooltip.hide()}
							/>
						{/each}
					</Svg>
					<Tooltip.Root {context}>
						{#snippet children({ data: c })}
							<Tip
								lines={[
									plural(c.n, unit),
									`${NAMES[c.day]} ${pad2(c.hour)}:00–${pad2(c.hour + 1)}:00`
								]}
							/>
						{/snippet}
					</Tooltip.Root>
				{/snippet}
			</Chart>
		</div>
	</div>
	<div class="flex items-center gap-1 text-muted-foreground">
		<span>Fewer</span>
		{#each Array.from({ length: LEVELS }, (_, i) => i + 1) as l (l)}
			<i class="h-2.5 w-3.5 rounded-sm" style:background="var(--seq-{l})" aria-hidden="true"></i>
		{/each}
		<span>More (max {max})</span>
	</div>
</div>
