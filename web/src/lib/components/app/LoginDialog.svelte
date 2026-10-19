<script lang="ts">
	// Asks for the access token when none is stored or the hub rejected it.
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { setToken, token, tokenFrom } from '#lib/api/client.js';
	import { live } from '#lib/live/store.svelte.js';
	import { pwa } from '#lib/pwa.svelte.js';

	let value = $state('');
	const rejected = $derived(live.needToken && Boolean(token()));

	function submit(event: SubmitEvent) {
		event.preventDefault();
		const t = tokenFrom(value);
		if (!t) return;
		setToken(t);
		value = '';
		live.run();
		pwa.syncSubscription();
	}
</script>

<Dialog.Root open={live.needToken} onOpenChange={() => {}}>
	<Dialog.Content
		showCloseButton={false}
		interactOutsideBehavior="ignore"
		escapeKeydownBehavior="ignore"
	>
		<Dialog.Header>
			<Dialog.Title>Access token</Dialog.Title>
			<Dialog.Description>
				Run <code class="font-mono">ccdash url --hub --qr</code> on the hub and scan the code, or paste
				the token or the printed link here.
			</Dialog.Description>
		</Dialog.Header>
		<form class="flex flex-col gap-3" onsubmit={submit}>
			{#if rejected}
				<p class="text-sm text-critical" role="alert">That token was not accepted.</p>
			{/if}
			<Input bind:value autocomplete="off" spellcheck={false} required aria-label="Token" />
			<Dialog.Footer>
				<Button type="submit">Connect</Button>
			</Dialog.Footer>
		</form>
	</Dialog.Content>
</Dialog.Root>
