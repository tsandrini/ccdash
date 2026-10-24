<script lang="ts">
	// The session page: header (title with inline rename, state, where and
	// branch, context, cost, Open in Claude, Open on and Resolved with the
	// other actions in a menu), Chat / Shell / Details, the open asks pinned
	// above the composer; an ended session (also one only history still
	// knows) offers Continue.
	import { tick, untrack } from 'svelte';
	import { page } from '$app/state';
	import ArrowDownIcon from '@lucide/svelte/icons/arrow-down';
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import PencilIcon from '@lucide/svelte/icons/pencil';
	import { resolvedNote } from '#lib/actions.js';
	import { Unauthorized } from '#lib/api/client.js';
	import { renameSession } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import * as Tabs from '#lib/components/ui/tabs/index.js';
	import AskCard from '#lib/components/app/AskCard.svelte';
	import Clock from '#lib/components/app/Clock.svelte';
	import Meter from '#lib/components/app/Meter.svelte';
	import SessionActions from '#lib/components/app/SessionActions.svelte';
	import StateBadge from '#lib/components/app/StateBadge.svelte';
	import Where from '#lib/components/app/Where.svelte';
	import ChatTab from '#lib/components/session/ChatTab.svelte';
	import Composer from '#lib/components/session/Composer.svelte';
	import DetailsTab from '#lib/components/session/DetailsTab.svelte';
	import ShellTab from '#lib/components/session/ShellTab.svelte';
	import { Convo } from '#lib/components/session/convo.svelte.js';
	import { fmtStamp, fmtUsd, known } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { claimPaths, isAgent, label, signature, shown } from '#lib/session.js';
	import { starter } from '#lib/starter.svelte.js';

	const id = $derived(page.params.id!);
	let convo = $state<Convo>(new Convo(''));
	const session = $derived(
		live.sessions.get(id) ??
			[...live.sessions.values()].find((s) => s.id.startsWith(id)) ??
			(convo.session?.id.startsWith(id) ? convo.session : undefined)
	);
	const tab = $derived(page.url.searchParams.get('tab') ?? 'chat');

	let scroller = $state<HTMLDivElement | null>(null);
	let atEnd = $state(true);
	let editing = $state(false);
	let title = $state('');
	let renameError = $state('');

	// A new session: a new conversation, loaded from the start.
	$effect(() => {
		const current = id;
		untrack(() => {
			convo = new Convo(current);
			convo.load('initial');
			atEnd = true;
		});
	});

	// Live changes to the session refetch the newest exchanges.
	$effect(() => {
		const sig = session ? signature(session) : '';
		void sig;
		untrack(() => {
			if (session && convo.loaded) convo.scheduleRefresh();
		});
	});

	// Keep the end in view while the user is there; keep the place when older pages load.
	$effect(() => {
		const change = convo.changed;
		untrack(async () => {
			if (!scroller || !change.n || tab === 'details') return;
			const before = scroller.scrollHeight;
			await tick();
			if (change.mode === 'older') scroller.scrollTop += scroller.scrollHeight - before;
			else if (change.mode === 'initial' || atEnd) scrollToEnd();
		});
	});

	function onscroll() {
		if (!scroller) return;
		atEnd = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80;
	}

	function scrollToEnd() {
		if (scroller) scroller.scrollTop = scroller.scrollHeight;
		atEnd = true;
	}

	function setTab(value: string) {
		const url = new URL(page.url.href);
		if (value === 'chat') url.searchParams.delete('tab');
		else url.searchParams.set('tab', value);
		history.replaceState(history.state, '', url);
		if (value !== 'details') requestAnimationFrame(scrollToEnd);
	}

	async function saveTitle() {
		if (!session) return;
		try {
			live.put((await renameSession(session.id, title)).session);
			editing = false;
			renameError = '';
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else renameError = (e as Error).message;
		}
	}

	const parent = $derived(session?.parent ? live.sessions.get(session.parent) : undefined);
</script>

<svelte:head>
	<title>{session ? label(session) : 'Session'} · ccdash</title>
</svelte:head>

{#if !session}
	<div class="p-6 text-center text-sm text-muted-foreground">
		{#if convo.missing || live.ready}
			No session {id.slice(0, 8)} is known to the hub (it may have ended before the hub restarted).
			<a class="underline" href="/sessions">All sessions</a>
		{:else}
			Loading…
		{/if}
	</div>
{:else}
	{@const s = session}
	<div class="flex h-full flex-col">
		<header class="flex flex-col gap-1.5 border-b px-3 py-2 sm:px-4">
			<div class="flex flex-wrap items-center gap-x-2 gap-y-1">
				<StateBadge activity={shown(s)} />
				<Clock since={s.activity_since} class="text-xs" />
				{#if s.renaming}<span class="text-xs text-muted-foreground"
						>renamed; adopted at the next prompt</span
					>{/if}
				<span class="ml-auto flex items-center gap-2">
					{#if known(s.context_pct)}
						<span class="inline-flex items-center gap-1.5 text-xs text-muted-foreground"
							><Meter pct={s.context_pct} label="context" /> {Math.round(s.context_pct)}%</span
						>
					{/if}
					{#if known(s.lines_added) || known(s.lines_removed)}
						<span class="text-xs text-muted-foreground tabular-nums" title="Lines added and removed"
							>+{s.lines_added ?? 0} −{s.lines_removed ?? 0}</span
						>
					{/if}
					{#if s.cost_usd}<span class="text-xs text-muted-foreground">{fmtUsd(s.cost_usd)}</span
						>{/if}
					{#if s.remote_url}
						<Button
							variant="outline"
							size="xs"
							href={s.remote_url}
							target="_blank"
							rel="noopener noreferrer"
						>
							Open in Claude <ExternalLinkIcon />
						</Button>
					{/if}
				</span>
			</div>
			{#if editing}
				<form
					class="flex items-center gap-2"
					onsubmit={(e) => {
						e.preventDefault();
						saveTitle();
					}}
				>
					<Input
						bind:value={title}
						maxlength={100}
						aria-label="Session title"
						class="h-8"
						autofocus
					/>
					<Button type="submit" size="sm">Save</Button>
					<Button type="button" variant="ghost" size="sm" onclick={() => (editing = false)}
						>Cancel</Button
					>
				</form>
				{#if renameError}<p class="text-xs text-critical" role="alert">{renameError}</p>{/if}
			{:else}
				<div class="flex min-w-0 items-center gap-2">
					<h2 class="flex min-w-0 items-center gap-1 text-base font-semibold">
						<span class="truncate">{label(s)}</span>
						{#if s.activity !== 'gone'}
							<Button
								variant="ghost"
								size="icon-xs"
								aria-label="Rename session"
								title="Rename"
								onclick={() => {
									title = label(s);
									editing = true;
								}}><PencilIcon /></Button
							>
						{/if}
					</h2>
					<div class="ml-auto flex shrink-0 flex-col items-end gap-1">
						<SessionActions session={s} pin={['open', 'resolve']} omit={['continue']} labelled />
					</div>
				</div>
			{/if}
			<Where session={s} workspace />
			{#if isAgent(s)}
				<p class="text-xs text-muted-foreground">
					{#if parent}Started by <a class="underline" href="/sessions/{parent.id}"
							>{label(parent)}</a
						>{:else}Background session{/if}
				</p>
			{/if}
			{#if resolvedNote(s)}
				<p class="text-xs font-medium text-your-turn">{resolvedNote(s)}</p>
			{/if}
			{#if s.claim && s.activity !== 'gone'}
				<p class="truncate text-xs" title={claimPaths(s).join(', ')}>
					<span class="font-medium text-muted-foreground">Claims</span>
					{s.claim.what}{claimPaths(s).length ? ` · ${claimPaths(s).join(', ')}` : ''}
				</p>
			{/if}
		</header>
		<Tabs.Root value={tab} onValueChange={setTab} class="min-h-0 flex-1 gap-0">
			<Tabs.List class="mx-3 mt-2 w-fit sm:mx-4">
				<Tabs.Trigger value="chat">Chat</Tabs.Trigger>
				<Tabs.Trigger value="shell">Shell</Tabs.Trigger>
				<Tabs.Trigger value="details">Details</Tabs.Trigger>
			</Tabs.List>
			<div
				bind:this={scroller}
				{onscroll}
				class="relative min-h-0 flex-1 overflow-y-auto px-3 py-3 sm:px-4"
			>
				<div class="mx-auto max-w-3xl">
					<Tabs.Content value="chat"><ChatTab {convo} session={s} /></Tabs.Content>
					<Tabs.Content value="shell"><ShellTab {convo} /></Tabs.Content>
					<Tabs.Content value="details"><DetailsTab session={s} {convo} /></Tabs.Content>
				</div>
			</div>
			{#if !atEnd && tab !== 'details'}
				<div class="pointer-events-none relative">
					<Button
						variant="secondary"
						size="sm"
						class="pointer-events-auto absolute -top-12 right-4 shadow"
						onclick={scrollToEnd}
					>
						<ArrowDownIcon /> Latest
					</Button>
				</div>
			{/if}
		</Tabs.Root>
		{#if s.activity === 'gone'}
			<footer class="border-t bg-background px-3 py-2 sm:px-4">
				<div class="mx-auto flex w-full max-w-3xl flex-wrap items-center gap-2">
					<p class="text-xs text-muted-foreground">
						Ended {fmtStamp(s.activity_since)}.{s.workspace
							? ` Continue it in the background on ${s.host}.`
							: ' It ran outside every workspace, so it cannot be continued from here.'}
					</p>
					{#if s.workspace}
						<Button size="sm" class="ml-auto" onclick={() => starter.show({ resume: s })}
							>Continue…</Button
						>
					{/if}
				</div>
			</footer>
		{:else if s.asks.length || (s.hooked && !isAgent(s)) || s.channel}
			<footer class="flex flex-col gap-2 border-t bg-background px-3 py-2 sm:px-4">
				<div class="mx-auto flex w-full max-w-3xl flex-col gap-2">
					{#if s.asks.length}
						<AskCard session={s.id} ask={s.asks[0]} count={s.asks.length} />
					{/if}
					{#if (s.hooked && !isAgent(s)) || s.channel}
						<Composer
							session={s}
							onsent={(kind) => {
								if (kind === 'message') convo.scheduleRefresh();
								requestAnimationFrame(scrollToEnd);
							}}
						/>
					{/if}
				</div>
			</footer>
		{/if}
	</div>
{/if}
