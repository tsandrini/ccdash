<script lang="ts">
	// The range picker's form: quick ranges, then an exact from–to in local
	// time. A "to" at the current minute makes the range reach now.
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import {
		fromInputValue,
		MINUTE,
		PROBLEM_TEXT,
		quickLabel,
		rangeProblem,
		resolveRange,
		sameRange,
		toInputValue,
		untilNow,
		type Range
	} from '#lib/range.js';
	import { rangeState } from '#lib/range.svelte.js';

	let { value, quick, onapply }: { value: Range; quick: Range[]; onapply: (r: Range) => void } =
		$props();

	// the form opens on the current range and is edited from there
	// svelte-ignore state_referenced_locally
	const span = resolveRange(value, rangeState.now());
	let from = $state(toInputValue(span.from));
	let to = $state(toInputValue(span.to));
	let error = $state('');
	const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;

	function submit(event: SubmitEvent) {
		event.preventDefault();
		const now = rangeState.now();
		const f = fromInputValue(from);
		const t = fromInputValue(to);
		const problem = rangeProblem(f, t, now);
		if (problem || f === null || t === null) {
			error = PROBLEM_TEXT[problem ?? 'incomplete'];
			return;
		}
		onapply(
			t >= Math.floor(now / MINUTE) * MINUTE
				? untilNow(f, now)
				: { kind: 'absolute', from: f, to: t }
		);
	}
</script>

<div class="flex flex-col gap-3">
	<section class="flex flex-col gap-1.5">
		<h3 class="text-xs font-medium text-muted-foreground">Quick ranges</h3>
		<div class="grid grid-cols-2 gap-1">
			{#each quick as r (quickLabel(r))}
				{@const on = sameRange(r, value)}
				<Button
					variant={on ? 'secondary' : 'ghost'}
					size="sm"
					class="justify-start font-normal"
					aria-pressed={on}
					onclick={() => onapply(r)}>{quickLabel(r)}</Button
				>
			{/each}
		</div>
	</section>
	<form onsubmit={submit} class="flex flex-col gap-2 border-t pt-3">
		<h3 class="text-xs font-medium text-muted-foreground">Absolute range</h3>
		<label class="flex items-center gap-2 text-xs">
			<span class="w-10">From</span>
			<Input
				type="datetime-local"
				bind:value={from}
				oninput={() => (error = '')}
				class="h-8 text-xs"
			/>
		</label>
		<label class="flex items-center gap-2 text-xs">
			<span class="w-10">To</span>
			<Input
				type="datetime-local"
				bind:value={to}
				oninput={() => (error = '')}
				class="h-8 text-xs"
			/>
		</label>
		<p class="text-xs text-muted-foreground">Local time ({zone})</p>
		{#if error}<p class="text-xs text-critical" role="alert">{error}</p>{/if}
		<div class="flex justify-end gap-2">
			<Button
				type="button"
				variant="ghost"
				size="sm"
				onclick={() => {
					to = toInputValue(Math.ceil(rangeState.now() / MINUTE) * MINUTE);
					error = '';
				}}>Until now</Button
			>
			<Button type="submit" size="sm">Apply</Button>
		</div>
	</form>
</div>
