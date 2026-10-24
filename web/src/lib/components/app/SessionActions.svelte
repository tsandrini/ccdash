<script lang="ts">
	// A session's actions (#lib/actions): those in `pin` as buttons, the
	// others in an Actions menu, `omit`ted ones nowhere. One blocked shows
	// disabled in the menu with why; those that ask first open a dialog
	// naming the session and the host.
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import EllipsisIcon from '@lucide/svelte/icons/ellipsis';
	import { ACTIONS, type ActionId, type SessionAction } from '#lib/actions.js';
	import { Unauthorized } from '#lib/api/client.js';
	import type { Session } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js';
	import { live } from '#lib/live/store.svelte.js';

	let {
		session: s,
		pin = [],
		omit = [],
		labelled = false
	}: {
		session: Session;
		pin?: ActionId[];
		omit?: ActionId[];
		/** The menu's trigger reads "Actions" rather than "…". */
		labelled?: boolean;
	} = $props();

	let confirming = $state<SessionAction | null>(null);
	let busy = $state(false);
	let error = $state('');
	let note = $state('');

	const offered = $derived(ACTIONS.filter((a) => !omit.includes(a.id) && a.offered(s)));
	const pinned = $derived(offered.filter((a) => pin.includes(a.id) && !a.blocked?.(s)));
	const rest = $derived(offered.filter((a) => !pinned.includes(a)));
	const prompt = $derived(confirming?.confirm?.(s) ?? null);
	// Outside a row of buttons (a card's corner) the outcome floats.
	const floating = $derived(!pinned.length && !labelled);

	function start(a: SessionAction) {
		error = '';
		note = '';
		if (a.confirm) confirming = a;
		else run(a);
	}

	async function run(a: SessionAction) {
		busy = true;
		try {
			const said = await a.run(s);
			confirming = null;
			if (said) note = said;
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
</script>

{#if offered.length}
	<span class="relative flex flex-wrap items-center gap-1.5">
		{#each pinned as a (a.id)}
			<Button
				variant={a.id === 'resolve' || a.id === 'continue' ? 'default' : 'outline'}
				size="xs"
				disabled={busy}
				onclick={() => start(a)}
			>
				<a.icon />
				{a.label(s)}
			</Button>
		{/each}
		{#if rest.length}
			<DropdownMenu.Root>
				<DropdownMenu.Trigger>
					{#snippet child({ props })}
						{#if labelled || pinned.length}
							<Button variant="outline" size="xs" aria-label="Session actions" {...props}>
								{#if labelled}Actions <ChevronDownIcon />{:else}<EllipsisIcon />{/if}
							</Button>
						{:else}
							<Button variant="ghost" size="icon-xs" aria-label="Session actions" {...props}>
								<EllipsisIcon />
							</Button>
						{/if}
					{/snippet}
				</DropdownMenu.Trigger>
				<DropdownMenu.Content align="end" class="max-w-72">
					{#each rest as a, i (a.id)}
						{@const why = a.blocked?.(s) ?? null}
						{#if a.destructive && i > 0}<DropdownMenu.Separator />{/if}
						<DropdownMenu.Item
							variant={a.destructive ? 'destructive' : 'default'}
							disabled={Boolean(why)}
							onSelect={() => start(a)}
						>
							<a.icon />
							<span class="flex flex-col">
								<span>{a.label(s)}{a.confirm && !a.label(s).endsWith('…') ? '…' : ''}</span>
								{#if why}<span class="text-xs text-muted-foreground">{why}</span>{/if}
							</span>
						</DropdownMenu.Item>
					{/each}
				</DropdownMenu.Content>
			</DropdownMenu.Root>
		{/if}
		{#if (error || note) && !confirming}
			<span
				class="text-xs {error ? 'text-critical' : 'text-muted-foreground'} {floating
					? 'absolute top-7 right-0 z-10 w-max max-w-60 rounded-md border bg-popover p-1.5 shadow'
					: 'basis-full text-right'}"
				role="status"
			>
				{error || note}
			</span>
		{/if}
	</span>
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
		{#if confirming && prompt}
			<Dialog.Header>
				<Dialog.Title>{prompt.title}</Dialog.Title>
				<Dialog.Description>{prompt.body}</Dialog.Description>
			</Dialog.Header>
			{#if error}<p class="text-sm text-critical" role="alert">{error}</p>{/if}
			<Dialog.Footer>
				<Button variant="outline" onclick={() => (confirming = null)}>Cancel</Button>
				<Button
					variant={confirming.destructive || confirming.id === 'forget' ? 'destructive' : 'default'}
					disabled={busy}
					onclick={() => confirming && run(confirming)}>{prompt.button}</Button
				>
			</Dialog.Footer>
		{/if}
	</Dialog.Content>
</Dialog.Root>
