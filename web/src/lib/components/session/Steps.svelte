<script lang="ts">
	// A run of tool calls, permission requests, answers and task updates
	// between Claude's messages, folded into one line with its counts.
	import type { Item } from '#lib/api/types.js';
	import { fmtDuration, fmtTime, known, plural, toolName } from '#lib/format.js';

	const VISIBLE = 15;

	let {
		items,
		omitted = 0,
		open = false
	}: { items: Item[]; omitted?: number; open?: boolean } = $props();

	let showAll = $state(false);

	const tools = $derived(items.filter((i) => i.kind === 'tool'));
	const summary = $derived.by(() => {
		const failed = tools.filter((t) => t.kind === 'tool' && !t.ok).length;
		const asks = items.filter((i) => i.kind === 'permission').length;
		const tasks = items.filter((i) => i.kind === 'task').length;
		const span = items[items.length - 1].at - items[0].at;
		return [
			tools.length + omitted ? plural(tools.length + omitted, 'tool call') : null,
			failed ? `${failed} failed` : null,
			asks ? plural(asks, 'permission request') : null,
			tasks ? plural(tasks, 'task update') : null,
			span >= 1000 ? fmtDuration(span) : null
		]
			.filter(Boolean)
			.join(' · ');
	});
	const top = $derived.by(() => {
		const counts: Record<string, number> = {};
		for (const t of tools) if (t.kind === 'tool') counts[t.name] = (counts[t.name] ?? 0) + 1;
		return Object.entries(counts)
			.sort((a, b) => b[1] - a[1])
			.slice(0, 3)
			.map(([name, n]) => `${toolName(name)}${n > 1 ? ` ×${n}` : ''}`)
			.join(', ');
	});
	const hidden = $derived(showAll ? 0 : Math.max(0, items.length - VISIBLE));
	const shownItems = $derived(items.slice(hidden));

	const answered = (item: Extract<Item, { kind: 'answered' }>) =>
		({
			allow: item.tool === 'ExitPlanMode' ? 'Approved the plan' : 'Allowed',
			always: 'Always allowed',
			deny: item.tool === 'ExitPlanMode' ? 'Kept planning' : 'Denied',
			answer: 'Answered'
		})[item.action] ?? 'Answered';
	const taskVerb = (status: string) =>
		({ pending: 'added', in_progress: 'started', completed: 'completed', deleted: 'removed' })[
			status
		] ?? status;
</script>

<details class="group rounded-md border bg-muted/40 text-xs" {open}>
	<summary class="flex cursor-pointer flex-wrap items-center gap-x-2 px-2.5 py-1.5 select-none">
		<span class="text-muted-foreground">{summary}</span>
		{#if top}<span class="truncate font-mono">{top}</span>{/if}
	</summary>
	<ol class="flex flex-col gap-0.5 px-2.5 pb-2">
		{#if omitted}<li class="text-muted-foreground">
				{plural(omitted, 'earlier tool call')} not shown
			</li>{/if}
		{#if hidden}
			<li>
				<button
					type="button"
					class="text-muted-foreground underline"
					onclick={() => (showAll = true)}>Show {plural(hidden, 'earlier step')}</button
				>
			</li>
		{/if}
		{#each shownItems as item (item.id)}
			<li class="flex items-start gap-2 {item.kind === 'tool' && !item.ok ? 'text-critical' : ''}">
				<time class="shrink-0 text-muted-foreground tabular-nums">{fmtTime(item.at)}</time>
				{#if item.kind === 'permission'}
					<span class="shrink-0">?</span>
					<span
						>Asked permission: {toolName(item.tool)}{item.summary ? ` ${item.summary}` : ''}</span
					>
				{:else if item.kind === 'answered'}
					<span class="shrink-0">✓</span>
					<span
						>{answered(item)} from {item.via === 'notification' ? 'a notification' : 'ccdash'}</span
					>
				{:else if item.kind === 'task'}
					<span class="shrink-0">{item.status === 'completed' ? '✓' : '☐'}</span>
					<span>Task {taskVerb(item.status)}: {item.subject}</span>
				{:else if item.kind === 'tool'}
					<span class="w-2 shrink-0">{item.ok ? '' : '✗'}</span>
					<span class="min-w-0 break-words"
						><strong>{toolName(item.name)}</strong>{#if item.summary}
							<span class="font-mono"> {item.summary}</span>{/if}</span
					>
					{#if known(item.ms) && item.ms >= 1000}<span
							class="ml-auto shrink-0 text-muted-foreground">{fmtDuration(item.ms)}</span
						>{/if}
				{/if}
			</li>
		{/each}
	</ol>
</details>
