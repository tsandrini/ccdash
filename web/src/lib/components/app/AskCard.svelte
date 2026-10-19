<script lang="ts">
	// An open prompt with its answers; the terminal dialog closes with it.
	// Tool calls: Allow / Deny / Always allow. Questions: options, several
	// for multi-select, or free text. Plans: Approve / Keep planning. A note
	// turns a denial into feedback Claude carries on with.
	import type { Answer, Ask } from '#lib/api/types.js';
	import { answerAsk } from '#lib/api/index.js';
	import { Unauthorized } from '#lib/api/client.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Textarea } from '#lib/components/ui/textarea/index.js';
	import { toolName } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import Clock from './Clock.svelte';
	import QuestionForm from './QuestionForm.svelte';
	import StateBadge from './StateBadge.svelte';

	let { session, ask, count = 1 }: { session: string; ask: Ask; count?: number } = $props();

	const TITLES = { tool: 'Needs permission', question: 'Claude asks', plan: 'Plan ready' };

	let busy = $state(false);
	let error = $state('');
	let noteOpen = $state(false);
	let note = $state('');

	async function send(answer: Answer) {
		busy = true;
		error = '';
		try {
			const { session: s } = await answerAsk(session, ask.id, answer);
			live.put(s);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		} finally {
			busy = false;
		}
	}
</script>

<div
	class="flex flex-col gap-2 rounded-lg border bg-card p-3 text-sm"
	role="group"
	aria-label={TITLES[ask.kind]}
>
	<div class="flex items-center gap-2">
		{#if ask.kind === 'tool'}<StateBadge activity="permission" />{/if}
		<strong class={ask.kind === 'tool' ? 'font-mono' : ''}
			>{ask.kind === 'tool' ? toolName(ask.tool) : TITLES[ask.kind]}</strong
		>
		{#if count > 1}<span class="text-xs text-muted-foreground">1 of {count}</span>{/if}
		<Clock since={ask.at} class="ml-auto text-xs" />
	</div>
	{#if ask.kind === 'question'}
		<QuestionForm
			questions={ask.questions}
			disabled={busy}
			onanswer={(answers) => send({ action: 'answer', answers })}
		/>
		<div class="flex flex-wrap items-center gap-2">
			{#if !noteOpen}
				<button
					type="button"
					class="text-xs text-muted-foreground underline"
					onclick={() => (noteOpen = true)}>Skip with a note</button
				>
			{/if}
			<Button variant="outline" size="sm" disabled={busy} onclick={() => send({ action: 'deny' })}
				>Dismiss</Button
			>
		</div>
	{:else if ask.kind === 'plan'}
		<pre
			class="max-h-72 overflow-auto rounded-md bg-muted p-2 font-mono text-xs whitespace-pre-wrap">{ask.plan}</pre>
		<div class="flex flex-wrap items-center justify-end gap-2">
			{#if !noteOpen}
				<button
					type="button"
					class="mr-auto text-xs text-muted-foreground underline"
					onclick={() => (noteOpen = true)}>Keep planning with a note</button
				>
			{/if}
			<Button variant="outline" size="sm" disabled={busy} onclick={() => send({ action: 'deny' })}
				>Keep planning</Button
			>
			<Button size="sm" disabled={busy} onclick={() => send({ action: 'allow' })}>Approve</Button>
		</div>
	{:else}
		{#if ask.description}<p>{ask.description}</p>{/if}
		{#if ask.input.length}
			<dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
				{#each ask.input as [key, value] (key)}
					<dt class="text-muted-foreground">{key}</dt>
					<dd class="max-h-40 overflow-auto font-mono break-words whitespace-pre-wrap">{value}</dd>
				{/each}
			</dl>
		{/if}
		<div class="flex flex-wrap items-center justify-end gap-2">
			{#if !noteOpen}
				<button
					type="button"
					class="mr-auto text-xs text-muted-foreground underline"
					onclick={() => (noteOpen = true)}>Deny with a note</button
				>
			{/if}
			<Button variant="outline" size="sm" disabled={busy} onclick={() => send({ action: 'deny' })}
				>Deny</Button
			>
			{#if ask.always}
				<Button
					variant="outline"
					size="sm"
					disabled={busy}
					onclick={() => send({ action: 'always' })}>Always allow</Button
				>
			{/if}
			<Button size="sm" disabled={busy} onclick={() => send({ action: 'allow' })}>Allow</Button>
		</div>
		{#if ask.always}<p class="text-xs text-muted-foreground">Always allow: {ask.always}</p>{/if}
	{/if}
	{#if noteOpen}
		<div class="flex flex-col gap-2">
			<Textarea
				bind:value={note}
				rows={2}
				maxlength={2000}
				placeholder="What should Claude do instead?"
				aria-label="Note for Claude"
			/>
			<div class="flex justify-end">
				<Button
					size="sm"
					disabled={busy || !note.trim()}
					onclick={() => send({ action: 'deny', message: note })}>Send note</Button
				>
			</div>
		</div>
	{/if}
	{#if error}<p class="text-xs text-critical" role="alert">{error}</p>{/if}
</div>
