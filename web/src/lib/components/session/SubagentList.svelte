<script lang="ts">
	import type { Subagent } from '#lib/api/types.js';
	import { plural, toolLabel } from '#lib/format.js';
	import Clock from '#lib/components/app/Clock.svelte';

	let { subagents }: { subagents: Subagent[] } = $props();
</script>

<ul class="flex flex-col gap-1 text-xs">
	{#each subagents as a (a.id)}
		<li class="flex flex-wrap items-center gap-2">
			<i class="inline-block size-2 animate-pulse rounded-full bg-working" aria-hidden="true"></i>
			<span class="rounded bg-muted px-1.5">{a.agent_type || 'Subagent'}</span>
			<span>{a.description ?? ''}</span>
			<span class="text-muted-foreground">
				{#if a.tool}<span class="font-mono">{toolLabel(a.tool)}</span> ·
				{/if}{#if a.tools}{plural(a.tools, 'tool')} ·
				{/if}<Clock since={a.started_at} />
			</span>
		</li>
	{/each}
</ul>
