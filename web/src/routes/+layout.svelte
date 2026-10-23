<script lang="ts">
	// The shell: sidebar (a drawer on the phone; on wide screens the full
	// list or, collapsed, a rail of icons), topbar with the scope,
	// the range where it applies, the connection state, the palette and the
	// theme; the live store and
	// the worker start here. `#token=`, `#session=` and `#pair` links from
	// pairing and old notifications are honoured.
	import './layout.css';
	import { onMount } from 'svelte';
	import { browser } from '$app/env';
	import { afterNavigate, goto } from '$app/navigation';
	import { page } from '$app/state';
	import MenuIcon from '@lucide/svelte/icons/menu';
	import PanelLeftIcon from '@lucide/svelte/icons/panel-left';
	import SearchIcon from '@lucide/svelte/icons/search';
	import * as Sheet from '#lib/components/ui/sheet/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { TooltipProvider } from '#lib/components/ui/tooltip/index.js';
	import CommandPalette from '#lib/components/app/CommandPalette.svelte';
	import ConnState from '#lib/components/app/ConnState.svelte';
	import LoginDialog from '#lib/components/app/LoginDialog.svelte';
	import Nav from '#lib/components/app/Nav.svelte';
	import NotePanel from '#lib/components/notes/NotePanel.svelte';
	import RangePicker from '#lib/components/app/RangePicker.svelte';
	import ScopeChips from '#lib/components/app/ScopeChips.svelte';
	import StartSheet from '#lib/components/app/StartSheet.svelte';
	import ThemeToggle from '#lib/components/app/ThemeToggle.svelte';
	import { setToken } from '#lib/api/client.js';
	import { live } from '#lib/live/store.svelte.js';
	import { palette } from '#lib/palette.svelte.js';
	import { prefs } from '#lib/prefs.svelte.js';
	import { pwa } from '#lib/pwa.svelte.js';
	import { areas } from '#lib/nav.js';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();
	let drawer = $state(false);
	let mainEl = $state<HTMLElement | null>(null);

	// NOTE: SvelteKit resets the window's scroll; the page scrolls inside `main`
	afterNavigate(({ type, from, to }) => {
		if (type !== 'popstate' && from?.url?.pathname !== to?.url?.pathname) mainEl?.scrollTo(0, 0);
	});

	const title = $derived(areas.find((a) => a.match(page.url.pathname))?.label ?? 'ccdash');

	/** What a link's hash asks: pairing (`#token=`), a notification's
	 * session (`#session=`), `#pair`. */
	function parseHash(hash: string) {
		const token = hash.match(/token=([^&]+)/)?.[1];
		const session = hash.match(/session=([^&]+)/)?.[1];
		const pair = /(^#|&)pair(&|$)/.test(hash);
		if (!token && !session && !pair) return null;
		return {
			token: token && decodeURIComponent(token),
			session: session && decodeURIComponent(session),
			pair
		};
	}

	/** Honours the link's hash and drops it from the address. */
	function takeHash() {
		const link = parseHash(location.hash);
		if (!link) return;
		history.replaceState(null, '', location.pathname + location.search);
		if (link.token) {
			setToken(link.token);
			live.run();
			pwa.syncSubscription();
		}
		if (link.session) goto(`/sessions/${link.session}`);
		else if (link.pair) goto('/settings');
	}

	// NOTE: before the pages mount: their first fetch must carry a pairing link's token
	if (browser) {
		const token = parseHash(location.hash)?.token;
		if (token) setToken(token);
	}

	onMount(() => {
		prefs.applyTheme();
		matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => prefs.applyTheme());
		takeHash();
		addEventListener('hashchange', takeHash);
		live.start();
		pwa.start((id) => goto(`/sessions/${id}`));
	});
</script>

<svelte:head>
	<title>{title} · ccdash</title>
</svelte:head>

<TooltipProvider delayDuration={300}>
	<div class="flex h-full">
		<aside
			class="hidden h-full shrink-0 flex-col overflow-y-auto border-r border-sidebar-border bg-sidebar text-sidebar-foreground md:flex {prefs.sidebar
				? 'w-52'
				: 'w-14'}"
		>
			<a
				href="/"
				class="flex items-center gap-2 py-3 font-semibold {prefs.sidebar
					? 'px-4'
					: 'justify-center px-2'}"
				title={prefs.sidebar ? undefined : 'ccdash'}
			>
				<img src="/icon.svg" alt="" width="20" height="20" />
				{#if prefs.sidebar}
					ccdash
					<span class="truncate text-xs font-normal text-muted-foreground">{live.host}</span>
				{:else}
					<span class="sr-only">ccdash</span>
				{/if}
			</a>
			<Nav compact={!prefs.sidebar} />
		</aside>
		<div class="flex h-full min-w-0 flex-1 flex-col">
			<header
				class="z-20 flex items-center gap-2 border-b bg-background/90 px-2 py-1.5 backdrop-blur sm:px-3"
			>
				<Sheet.Root bind:open={drawer}>
					<Sheet.Trigger>
						{#snippet child({ props })}
							<Button variant="ghost" size="icon" class="md:hidden" aria-label="Menu" {...props}>
								<MenuIcon />
							</Button>
						{/snippet}
					</Sheet.Trigger>
					<Sheet.Content side="left" class="w-64 gap-0 p-0">
						<Sheet.Header class="px-4 py-3">
							<Sheet.Title class="flex items-center gap-2">
								<img src="/icon.svg" alt="" width="20" height="20" /> ccdash
							</Sheet.Title>
							<Sheet.Description class="sr-only">Areas</Sheet.Description>
						</Sheet.Header>
						<Nav onnavigate={() => (drawer = false)} />
					</Sheet.Content>
				</Sheet.Root>
				<Button
					variant="ghost"
					size="icon"
					class="hidden md:inline-flex"
					aria-label={prefs.sidebar ? 'Narrow the sidebar to icons' : 'Widen the sidebar'}
					aria-pressed={!prefs.sidebar}
					title="Sidebar"
					onclick={() => prefs.setSidebar(!prefs.sidebar)}
				>
					<PanelLeftIcon />
				</Button>
				<h1 class="truncate text-sm font-semibold {prefs.sidebar ? 'md:hidden' : ''}">{title}</h1>
				<div class="hidden items-center gap-1.5 sm:flex">
					<ScopeChips />
					<RangePicker />
				</div>
				<div class="ml-auto flex items-center gap-1">
					<ConnState />
					<Button
						variant="ghost"
						size="icon"
						aria-label="Search (Ctrl K)"
						title="Search (Ctrl K)"
						onclick={() => palette.show()}
					>
						<SearchIcon />
					</Button>
					<ThemeToggle />
				</div>
			</header>
			<div class="flex flex-wrap items-center gap-1.5 border-b px-2 py-1.5 sm:hidden">
				<ScopeChips />
				<RangePicker />
			</div>
			<!-- NOTE: the page scrolls here, not the window: the sidebar and the bar stay put -->
			<main class="min-h-0 flex-1 overflow-y-auto" bind:this={mainEl}>
				{@render children()}
			</main>
		</div>
	</div>
</TooltipProvider>

<LoginDialog />
<CommandPalette />
<NotePanel />
<StartSheet />
