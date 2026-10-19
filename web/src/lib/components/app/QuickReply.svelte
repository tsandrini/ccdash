<script lang="ts">
	// A one-line reply from a list: the quick replies as chips, a field,
	// Send (through the channel) or Next prompt.
	import { goto } from '$app/navigation';
	import { Unauthorized } from '#lib/api/client.js';
	import { messageTo, replyTo } from '#lib/api/index.js';
	import type { Session } from '#lib/api/types.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { live } from '#lib/live/store.svelte.js';
	import { prefs } from '#lib/prefs.svelte.js';

	let { session: s }: { session: Session } = $props();

	let text = $state('');
	let busy = $state(false);
	let error = $state('');
	let sent = $state('');

	async function send(kind: 'reply' | 'message', value = text) {
		const body = value.trim();
		if (!body) return;
		busy = true;
		error = '';
		try {
			if (kind === 'reply') live.put((await replyTo(s.id, body)).session);
			else await messageTo(s.id, body);
			text = '';
			sent = kind === 'reply' ? 'Sent.' : 'Left for the next prompt.';
			setTimeout(() => (sent = ''), 4000);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		} finally {
			busy = false;
		}
	}
</script>

<div class="flex flex-col gap-1.5">
	<div class="flex flex-wrap gap-1.5">
		{#each prefs.quick as q (q)}
			<Button
				variant="outline"
				size="xs"
				disabled={busy}
				onclick={() => send(s.channel ? 'reply' : 'message', q)}>{q}</Button
			>
		{/each}
	</div>
	<form
		class="flex gap-1.5"
		onsubmit={(e) => {
			e.preventDefault();
			send(s.channel ? 'reply' : 'message');
		}}
	>
		<Input
			bind:value={text}
			placeholder={s.channel ? 'Reply…' : 'Note for the next prompt…'}
			aria-label="Reply to {s.title ?? s.id}"
			disabled={busy}
			class="h-8"
		/>
		{#if s.channel}
			<Button type="submit" size="sm" disabled={busy || !text.trim()}>Send</Button>
		{/if}
		<Button
			type="button"
			variant={s.channel ? 'outline' : 'default'}
			size="sm"
			disabled={busy || !text.trim()}
			title="Hand the text to Claude as context with its next prompt, without starting a turn"
			onclick={() => send('message')}>Next prompt</Button
		>
		<Button type="button" variant="ghost" size="sm" onclick={() => goto(`/sessions/${s.id}`)}
			>Open</Button
		>
	</form>
	{#if error}<p class="text-xs text-critical" role="alert">{error}</p>{/if}
	{#if sent}<p class="text-xs text-muted-foreground" role="status">{sent}</p>{/if}
</div>
