<script lang="ts">
	// What can be done to a session from here: Stop (its host connected),
	// Open on the desktop (a background session on a connected host),
	// Forget (its host offline), Continue (ended). Stop and Forget ask
	// first, naming the session and the host. Buttons on the session page,
	// a menu on the cards.
	import EllipsisIcon from '@lucide/svelte/icons/ellipsis';
	import { Unauthorized } from '#lib/api/client.js';
	import { forgetSession, openSession, stopSession, type Session } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js';
	import { live } from '#lib/live/store.svelte.js';
	import { isAgent, label } from '#lib/session.js';
	import { starter } from '#lib/starter.svelte.js';

	let {
		session: s,
		menu = false,
		resume = true
	}: { session: Session; menu?: boolean; resume?: boolean } = $props();

	let confirming = $state<'stop' | 'forget' | null>(null);
	let busy = $state(false);
	let error = $state('');
	let note = $state('');

	const ended = $derived(s.activity === 'gone');
	const online = $derived(live.connected(s.host));
	const canStop = $derived(!ended && online);
	const canOpen = $derived(!ended && online && isAgent(s));
	const canForget = $derived(!ended && !online);
	const canContinue = $derived(resume && ended && Boolean(s.workspace));
	const any = $derived(canStop || canOpen || canForget || canContinue);

	async function act(what: 'stop' | 'forget' | 'open') {
		busy = true;
		error = '';
		note = '';
		try {
			const call = { stop: stopSession, forget: forgetSession, open: openSession }[what];
			live.put((await call(s.id)).session);
			confirming = null;
			if (what === 'open') note = `Opening on ${s.host}.`;
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	// Outside a dialog the outcome shows for a moment.
	$effect(() => {
		if (!(error || note) || confirming) return;
		const timer = setTimeout(() => {
			error = '';
			note = '';
		}, 6000);
		return () => clearTimeout(timer);
	});

	const cont = () => starter.show({ resume: s });
</script>

{#if any}
	{#if menu}
		<DropdownMenu.Root>
			<DropdownMenu.Trigger>
				{#snippet child({ props })}
					<Button variant="ghost" size="icon-xs" aria-label="Session actions" {...props}>
						<EllipsisIcon />
					</Button>
				{/snippet}
			</DropdownMenu.Trigger>
			<DropdownMenu.Content align="end">
				{#if canOpen}
					<DropdownMenu.Item onSelect={() => act('open')}>Open on {s.host}</DropdownMenu.Item>
				{/if}
				{#if canContinue}
					<DropdownMenu.Item onSelect={cont}>Continue…</DropdownMenu.Item>
				{/if}
				{#if canStop}
					<DropdownMenu.Item variant="destructive" onSelect={() => (confirming = 'stop')}
						>Stop…</DropdownMenu.Item
					>
				{/if}
				{#if canForget}
					<DropdownMenu.Item onSelect={() => (confirming = 'forget')}>Forget…</DropdownMenu.Item>
				{/if}
			</DropdownMenu.Content>
		</DropdownMenu.Root>
	{:else}
		<span class="flex flex-wrap items-center gap-1.5">
			{#if canOpen}
				<Button variant="outline" size="xs" disabled={busy} onclick={() => act('open')}
					>Open on {s.host}</Button
				>
			{/if}
			{#if canContinue}
				<Button size="xs" onclick={cont}>Continue</Button>
			{/if}
			{#if canStop}
				<Button variant="outline" size="xs" onclick={() => (confirming = 'stop')}>Stop</Button>
			{/if}
			{#if canForget}
				<Button variant="outline" size="xs" onclick={() => (confirming = 'forget')}>Forget</Button>
			{/if}
		</span>
	{/if}
	{#if (error || note) && !confirming}
		<p
			class="text-xs {error ? 'text-critical' : 'text-muted-foreground'} {menu
				? 'absolute top-7 right-0 z-10 w-max max-w-60 rounded-md border bg-popover p-1.5 shadow'
				: ''}"
			role="status"
		>
			{error || note}
		</p>
	{/if}
{/if}

<Dialog.Root
	open={confirming !== null}
	onOpenChange={(open) => {
		if (!open) {
			confirming = null;
			error = '';
		}
	}}
>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>
				{confirming === 'stop' ? 'Stop' : 'Forget'} “{label(s)}” on {s.host}?
			</Dialog.Title>
			<Dialog.Description>
				{#if confirming === 'forget'}
					{s.host} is offline, so nothing can stop the session from here. Forgetting marks it ended on
					the hub; it comes back if {s.host} reports it running again.
				{:else if isAgent(s)}
					Runs <code class="font-mono">claude stop {s.short_id ?? s.id.slice(0, 8)}</code> on {s.host}.
					The conversation is kept: continue it later from its page.
				{:else}
					Sends SIGTERM to its Claude process{s.pid ? ` (pid ${s.pid})` : ''} on {s.host}: the
					session in that terminal ends. The conversation is kept: continue it later from its page.
				{/if}
			</Dialog.Description>
		</Dialog.Header>
		{#if error}<p class="text-sm text-critical" role="alert">{error}</p>{/if}
		<Dialog.Footer>
			<Button variant="outline" onclick={() => (confirming = null)}>Cancel</Button>
			<Button variant="destructive" disabled={busy} onclick={() => confirming && act(confirming)}
				>{confirming === 'stop' ? 'Stop' : 'Forget'}</Button
			>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
