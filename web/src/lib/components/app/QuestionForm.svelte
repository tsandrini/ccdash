<script lang="ts">
	// One block per question: option buttons (toggles for multi-select) and
	// a free-text "Other"; Answer sends once every question has one.
	import type { Question } from '#lib/api/types.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { cn } from '#lib/utils.js';

	let {
		questions,
		disabled = false,
		onanswer
	}: {
		questions: Question[];
		disabled?: boolean;
		onanswer: (answers: Record<string, string>) => void;
	} = $props();

	// svelte-ignore state_referenced_locally
	let picked = $state<string[][]>(questions.map(() => []));
	// svelte-ignore state_referenced_locally
	let other = $state<string[]>(questions.map(() => ''));

	const complete = $derived(questions.every((_, i) => picked[i].length || other[i].trim()));

	function toggle(i: number, q: Question, label: string) {
		const on = !picked[i].includes(label);
		if (!q.multi_select) {
			picked[i] = [];
			other[i] = '';
		}
		picked[i] = on ? [...picked[i], label] : picked[i].filter((l) => l !== label);
	}

	function submit() {
		const answers: Record<string, string> = {};
		questions.forEach((q, i) => {
			const parts = q.options.map((o) => o.label).filter((l) => picked[i].includes(l));
			if (other[i].trim()) parts.push(other[i].trim());
			answers[q.question] = parts.join(', ');
		});
		onanswer(answers);
	}
</script>

<div class="flex flex-col gap-3">
	{#each questions as q, i (q.question)}
		<div class="flex flex-col gap-1.5">
			<p class="text-sm">
				{#if q.header}<span class="mr-1.5 rounded bg-muted px-1.5 py-0.5 text-xs">{q.header}</span
					>{/if}
				{q.question}
				{#if q.multi_select}<span class="text-xs text-muted-foreground"> · pick any</span>{/if}
			</p>
			<div class="grid gap-1.5 sm:grid-cols-2" role="group" aria-label={q.header || q.question}>
				{#each q.options as o (o.label)}
					<button
						type="button"
						{disabled}
						aria-pressed={picked[i].includes(o.label)}
						class={cn(
							'flex flex-col items-start rounded-md border px-2.5 py-1.5 text-left text-sm hover:bg-accent',
							picked[i].includes(o.label) && 'border-primary bg-accent'
						)}
						onclick={() => toggle(i, q, o.label)}
					>
						<span class="font-medium">{o.label}</span>
						{#if o.description}<span class="text-xs text-muted-foreground">{o.description}</span
							>{/if}
					</button>
				{/each}
			</div>
			<Input
				bind:value={other[i]}
				{disabled}
				maxlength={2000}
				placeholder={q.multi_select ? 'Something else too…' : 'Something else…'}
				aria-label="Other answer to: {q.question}"
				oninput={() => {
					if (!q.multi_select && other[i].trim()) picked[i] = [];
				}}
			/>
		</div>
	{/each}
	<div class="flex justify-end">
		<Button size="sm" disabled={disabled || !complete} onclick={submit}>Answer</Button>
	</div>
</div>
