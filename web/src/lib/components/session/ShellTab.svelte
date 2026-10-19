<script lang="ts">
	// The Bash calls of the loaded exchanges as a terminal transcript:
	// `$ command`, its output, failures highlighted, a copy button each.
	import CheckIcon from '@lucide/svelte/icons/check';
	import CopyIcon from '@lucide/svelte/icons/copy';
	import type { Item } from '#lib/api/types.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { fmtDuration, fmtStamp, fmtTime, known } from '#lib/format.js';
	import type { Convo } from './convo.svelte.js';

	let { convo }: { convo: Convo } = $props();

	type Call = Extract<Item, { kind: 'tool' }>;

	const groups = $derived(
		convo.exchanges
			.map((ex) => ({
				at: ex.at,
				prompt: ex.prompt,
				calls: ex.items.filter((i): i is Call => i.kind === 'tool' && i.name === 'Bash')
			}))
			.filter((g) => g.calls.length)
	);
	const total = $derived(groups.reduce((n, g) => n + g.calls.length, 0));

	let copied = $state<number | null>(null);

	async function copy(call: Call) {
		const text = [call.command ?? call.summary, call.output ?? ''].filter(Boolean).join('\n');
		await navigator.clipboard?.writeText(text);
		copied = call.at;
		setTimeout(() => (copied = null), 1500);
	}
</script>

<div class="flex flex-col gap-4">
	{#if convo.more}
		<div class="text-center">
			<Button
				variant="outline"
				size="sm"
				disabled={convo.loading}
				onclick={() => convo.load('older')}>Show earlier exchanges</Button
			>
		</div>
	{/if}
	{#if convo.loaded && !total}
		<p class="text-center text-sm text-muted-foreground">
			No shell commands in the loaded exchanges. Outputs are kept for calls made since the hub
			started storing them.
		</p>
	{/if}
	{#each groups as g (g.at)}
		<section class="flex flex-col gap-1.5">
			<p class="truncate text-xs text-muted-foreground" title={g.prompt ?? ''}>
				{fmtStamp(g.at)}{g.prompt ? ` · ${g.prompt.split('\n')[0]}` : ''}
			</p>
			{#each g.calls as call (call.at)}
				<div
					class="group relative overflow-hidden rounded-md border bg-neutral-950 font-mono text-xs text-neutral-200 {call.ok
						? ''
						: 'border-critical/60'}"
				>
					<div class="flex items-start gap-2 px-3 pt-2">
						<span class="text-your-turn select-none">$</span>
						<pre class="min-w-0 flex-1 break-words whitespace-pre-wrap">{call.command ??
								call.summary}</pre>
						<span class="shrink-0 text-muted-foreground tabular-nums"
							>{fmtTime(call.at)}{known(call.ms) && call.ms >= 1000
								? ` · ${fmtDuration(call.ms)}`
								: ''}</span
						>
						<Button
							variant="ghost"
							size="icon-xs"
							class="text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100"
							aria-label="Copy"
							onclick={() => copy(call)}
						>
							{#if copied === call.at}<CheckIcon />{:else}<CopyIcon />{/if}
						</Button>
					</div>
					{#if call.output}
						<pre
							class="max-h-80 overflow-auto px-3 py-2 break-words whitespace-pre-wrap {call.ok
								? 'text-neutral-300'
								: 'text-critical'}">{call.output}</pre>
					{:else}
						<p class="px-3 pb-2 text-neutral-500">{call.ok ? 'no output' : 'failed'}</p>
					{/if}
				</div>
			{/each}
		</section>
	{/each}
</div>
