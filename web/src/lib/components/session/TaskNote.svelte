<script lang="ts">
	// What a background task or a subagent told Claude (a task notification,
	// a subagent's report or message), on Claude's side in its own colour:
	// what it is and its name, the outcome, a subagent's tool calls and
	// time, then its report, a monitor's event or an MCP task's result.
	import BellIcon from '@lucide/svelte/icons/bell';
	import BotIcon from '@lucide/svelte/icons/bot';
	import EyeIcon from '@lucide/svelte/icons/eye';
	import PlugIcon from '@lucide/svelte/icons/plug';
	import TerminalIcon from '@lucide/svelte/icons/terminal';
	import WorkflowIcon from '@lucide/svelte/icons/workflow';
	import type { Component } from 'svelte';
	import { fmtDuration, fmtStamp, known, plural } from '#lib/format.js';
	import type { Notice } from '#lib/session.js';
	import Prose from './Prose.svelte';

	let { notice: t, at }: { notice: Notice; at: number } = $props();

	const ICONS: Record<string, Component> = {
		agent: BotIcon,
		shell: TerminalIcon,
		monitor: EyeIcon,
		mcp: PlugIcon,
		workflow: WorkflowIcon
	};
	const Icon = $derived(ICONS[t.source] ?? BellIcon);
	const outcome = $derived(
		t.status === 'failed' || t.status === 'killed'
			? 'text-critical'
			: t.status === 'stopped'
				? 'text-warning'
				: 'text-muted-foreground'
	);
	const usage = $derived(
		[known(t.tools) ? plural(t.tools, 'tool call') : null, known(t.ms) ? fmtDuration(t.ms) : null]
			.filter(Boolean)
			.join(' · ')
	);
</script>

<div class="flex flex-col items-start gap-1">
	<span class="flex items-center gap-1 text-xs text-muted-foreground">
		<Icon class="size-3.5 text-task" />
		<span class="font-medium">{t.label}</span>
		· {fmtStamp(at)}
	</span>
	<div
		class="flex max-w-[90%] min-w-0 flex-col gap-1 rounded-lg border border-task/40 bg-task/10 px-3 py-2 text-sm"
	>
		<p class="flex flex-wrap items-baseline gap-x-2 break-words">
			{#if t.name}
				<strong>{t.name}</strong>
				{#if t.outcome}<span class={outcome}>{t.outcome}</span>{/if}
			{:else}
				<span class={outcome}>{t.outcome}</span>
			{/if}
			{#if usage}<span class="text-xs text-muted-foreground">{usage}</span>{/if}
		</p>
		{#if t.event}
			<p class="font-mono text-xs break-words whitespace-pre-wrap">{t.event}</p>
		{/if}
		{#if t.html}<Prose html={t.html} clamp />{/if}
	</div>
</div>
