<script lang="ts">
	// `‹ [range] › ⊖` in the topbar on the pages that have a range: step by
	// the range's length, zoom out to twice it, or open the form (a popover,
	// a bottom sheet on the phone). The range lives in the URL.
	import { onMount } from 'svelte';
	import ChevronLeftIcon from '@lucide/svelte/icons/chevron-left';
	import ChevronRightIcon from '@lucide/svelte/icons/chevron-right';
	import ClockIcon from '@lucide/svelte/icons/clock';
	import ZoomOutIcon from '@lucide/svelte/icons/zoom-out';
	import * as Popover from '#lib/components/ui/popover/index.js';
	import * as Sheet from '#lib/components/ui/sheet/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { rangeLabel, shiftRange, zoomOut, type Range } from '#lib/range.js';
	import { rangeState } from '#lib/range.svelte.js';
	import RangeForm from './RangeForm.svelte';

	let open = $state(false);
	let touch = $state(false);

	onMount(() => {
		const mq = matchMedia('(max-width: 640px)');
		touch = mq.matches;
		const onchange = () => (touch = mq.matches);
		mq.addEventListener('change', onchange);
		return () => mq.removeEventListener('change', onchange);
	});

	const value = $derived(rangeState.current);
	const forward = $derived(shiftRange(value, 1, rangeState.now()));

	function apply(range: Range) {
		open = false;
		rangeState.set(range);
	}

	function shift(direction: 1 | -1) {
		const next = shiftRange(value, direction, rangeState.now());
		if (next) rangeState.set(next);
	}
</script>

{#if rangeState.applies}
	<div class="flex items-center gap-0.5">
		<Button
			variant="ghost"
			size="icon"
			class="size-8"
			aria-label="Previous period"
			title="Previous period"
			onclick={() => shift(-1)}
		>
			<ChevronLeftIcon />
		</Button>
		{#if touch}
			<Sheet.Root bind:open>
				<Sheet.Trigger>
					{#snippet child({ props })}
						<Button variant="outline" size="sm" class="max-w-48 gap-1 truncate" {...props}>
							<ClockIcon class="size-3 opacity-60" />
							<span class="truncate">{rangeLabel(value)}</span>
						</Button>
					{/snippet}
				</Sheet.Trigger>
				<Sheet.Content side="bottom" class="max-h-[85vh] overflow-y-auto">
					<Sheet.Header>
						<Sheet.Title>Time range</Sheet.Title>
						<Sheet.Description class="sr-only">Quick ranges or an exact span</Sheet.Description>
					</Sheet.Header>
					{#if open}
						<div class="px-4 pb-4">
							<RangeForm {value} quick={rangeState.quick} onapply={apply} />
						</div>
					{/if}
				</Sheet.Content>
			</Sheet.Root>
		{:else}
			<Popover.Root bind:open>
				<Popover.Trigger>
					{#snippet child({ props })}
						<Button variant="outline" size="sm" class="max-w-72 gap-1 truncate" {...props}>
							<ClockIcon class="size-3 opacity-60" />
							<span class="truncate">{rangeLabel(value)}</span>
						</Button>
					{/snippet}
				</Popover.Trigger>
				<Popover.Content align="start" class="w-80">
					{#if open}
						<RangeForm {value} quick={rangeState.quick} onapply={apply} />
					{/if}
				</Popover.Content>
			</Popover.Root>
		{/if}
		<Button
			variant="ghost"
			size="icon"
			class="size-8"
			aria-label="Next period"
			title="Next period"
			disabled={forward === null}
			onclick={() => shift(1)}
		>
			<ChevronRightIcon />
		</Button>
		<Button
			variant="ghost"
			size="icon"
			class="size-8"
			aria-label="Zoom out"
			title="Zoom out (twice as long)"
			onclick={() => rangeState.set(zoomOut(value, rangeState.now()))}
		>
			<ZoomOutIcon />
		</Button>
	</div>
{/if}
