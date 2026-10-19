<script lang="ts">
	// Text for the session: Send starts a turn through the channel, Next
	// prompt leaves it as context for the next prompt (also for sessions
	// without the channel). Quick replies send at once; the field (history,
	// templates, note links, fence, preview) is `PromptField`; the draft
	// survives reloads.
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchPrompts, messageTo, replyTo, type Session } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { live } from '#lib/live/store.svelte.js';
	import { prefs } from '#lib/prefs.svelte.js';
	import PromptField from './PromptField.svelte';

	let { session: s, onsent }: { session: Session; onsent?: (kind: 'reply' | 'message') => void } =
		$props();

	// svelte-ignore state_referenced_locally
	let text = $state(prefs.draft(s.id));
	let field = $state<PromptField | null>(null);
	let busy = $state(false);
	let error = $state('');

	const canReply = $derived(s.channel && s.activity !== 'gone');

	$effect(() => {
		prefs.setDraft(s.id, text);
	});

	async function prompts() {
		const own = (await fetchPrompts({ session: s.id, limit: 50 })).prompts;
		const ws = s.workspace
			? (await fetchPrompts({ workspace: s.workspace, limit: 50 })).prompts
			: [];
		return [...own, ...ws];
	}

	async function submit(kind: 'reply' | 'message') {
		const body = text.trim();
		if (!body || busy) return;
		busy = true;
		error = '';
		try {
			if (kind === 'reply') live.put((await replyTo(s.id, body)).session);
			else await messageTo(s.id, body);
			text = '';
			field?.remember(body);
			onsent?.(kind);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		} finally {
			busy = false;
		}
	}
</script>

<div class="flex flex-col gap-1.5">
	{#if prefs.quick.length}
		<div class="flex gap-1.5 overflow-x-auto pb-0.5">
			{#each prefs.quick as q (q)}
				<Button
					variant="outline"
					size="xs"
					class="shrink-0"
					disabled={busy}
					onclick={() => {
						text = q;
						submit(canReply ? 'reply' : 'message');
					}}>{q}</Button
				>
			{/each}
		</div>
	{/if}
	<PromptField
		bind:this={field}
		bind:text
		{prompts}
		historyNote="This session's prompts, then the workspace's."
		session={s.id}
		placeholder={canReply ? 'Message Claude…' : 'Note for the next prompt…'}
		label="Message to Claude"
		disabled={busy}
		onsubmit={() => submit(canReply ? 'reply' : 'message')}
	>
		{#snippet actions()}
			{#if canReply}
				<Button type="submit" size="sm" disabled={busy || !text.trim()}>Send</Button>
			{/if}
			<Button
				type="button"
				size="sm"
				variant={canReply ? 'outline' : 'default'}
				disabled={busy || !text.trim()}
				title="Hand the text to Claude as context with its next prompt, without starting a turn"
				onclick={() => submit('message')}>Next prompt</Button
			>
		{/snippet}
	</PromptField>
	{#if error}<p class="text-xs text-critical" role="alert">{error}</p>{/if}
	{#if !s.channel && s.activity !== 'gone'}
		<p class="text-xs text-muted-foreground">
			To start a turn from here, the session needs the ccdash channel: exit it and run
			<code class="font-mono">claude --resume {s.id}</code>. A note still reaches it with its next
			prompt.
		</p>
	{/if}
</div>
