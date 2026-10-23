<script lang="ts">
	// Pairing (a token per link), this device (install, push, its token),
	// the devices with a token, theme, the composer's quick replies and
	// templates, a pointer to the State page, hosts.
	import { clearToken, setToken, tokenFrom, Unauthorized } from '#lib/api/client.js';
	import {
		fetchPush,
		fetchTokens,
		mintToken,
		relabelToken,
		revokeToken,
		testPush,
		type Devices,
		type Pairing,
		type PushInfo,
		type Token
	} from '#lib/api/index.js';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Textarea } from '#lib/components/ui/textarea/index.js';
	import { fmtDuration, fmtStamp } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { DEFAULT_QUICK, prefs, SCHEMES, type Theme } from '#lib/prefs.svelte.js';
	import { pwa } from '#lib/pwa.svelte.js';

	let label = $state('Phone');
	let pairing = $state<Pairing | null>(null);
	let pairError = $state('');
	let copied = $state(false);
	let devices = $state<Devices | null>(null);
	let devicesError = $state('');
	let editId = $state<number | null>(null);
	let editLabel = $state('');
	let editRef = $state<HTMLInputElement | null>(null);
	let revoking = $state<Token | null>(null);
	let push = $state<PushInfo | null>(null);
	let pushError = $state('');
	let sub = $state<PushSubscription | null>(null);
	let status = $state('');
	let busy = $state(false);

	let quickText = $state(prefs.quick.join('\n'));
	let templates = $state(prefs.templates.map((t) => ({ ...t })));

	const current = $derived(devices?.tokens.find((t) => t.current) ?? null);

	function failed(e: unknown, set: (message: string) => void) {
		if (e instanceof Unauthorized) live.needToken = true;
		else set((e as Error).message);
	}

	async function loadDevices() {
		try {
			devices = await fetchTokens();
			devicesError = '';
		} catch (e) {
			failed(e, (m) => (devicesError = m));
		}
	}

	async function load() {
		await loadDevices();
		if (!pwa.pushSupported) return;
		try {
			push = await fetchPush();
			sub = await pwa.subscription();
		} catch (e) {
			failed(e, (m) => (pushError = m));
		}
	}

	$effect(() => {
		if (live.ready) load();
	});

	$effect(() => {
		editRef?.select();
	});

	/** A fresh token for one more device, shown this once. */
	async function newLink() {
		busy = true;
		pairError = '';
		copied = false;
		try {
			pairing = await mintToken(label.trim() || 'New device');
			await loadDevices();
		} catch (e) {
			failed(e, (m) => (pairError = m));
		} finally {
			busy = false;
		}
	}

	/** Swaps the shared token for one of this device's own; the push
	 * subscription follows it. */
	async function ownToken() {
		busy = true;
		status = '';
		try {
			const minted = await mintToken(device(navigator.userAgent));
			setToken(tokenFrom(minted.url));
			await pwa.syncSubscription();
			await load();
		} catch (e) {
			failed(e, (m) => (status = m));
		} finally {
			busy = false;
		}
	}

	function edit(t: Token) {
		editId = t.id;
		editLabel = t.label;
	}

	async function saveLabel() {
		const id = editId;
		if (id === null) return;
		editId = null;
		const value = editLabel.trim();
		const was = devices?.tokens.find((t) => t.id === id);
		if (!value || !was || value === was.label) return;
		try {
			const { token } = await relabelToken(id, value);
			if (devices) devices.tokens = devices.tokens.map((t) => (t.id === id ? token : t));
		} catch (e) {
			failed(e, (m) => (devicesError = m));
		}
	}

	async function revoke(t: Token) {
		busy = true;
		try {
			await revokeToken(t.id);
			revoking = null;
			if (t.current) {
				clearToken();
				live.signOut();
				return;
			}
			await load();
		} catch (e) {
			failed(e, (m) => (devicesError = m));
		} finally {
			busy = false;
		}
	}

	async function togglePush() {
		if (!push?.key) return;
		busy = true;
		status = '';
		try {
			if (sub) {
				await pwa.disablePush(sub);
				sub = null;
			} else {
				sub = await pwa.enablePush(push.key);
			}
			push = await fetchPush();
		} catch (e) {
			failed(e, (m) => (status = m));
		} finally {
			busy = false;
		}
	}

	async function sendTest() {
		busy = true;
		try {
			const { results } = await testPush();
			const mine = results.find((r) => r.endpoint === sub?.endpoint);
			status = !mine
				? 'The hub does not know this device; turn push off and on.'
				: mine.outcome === 'sent'
					? 'Sent; it shows in a moment.'
					: `Failed: ${'error' in mine ? mine.error : mine.status}`;
			push = await fetchPush();
		} catch (e) {
			status = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	const device = (ua: string | null) =>
		(ua || '')
			.match(/(Firefox|Chrome|Safari)\/[\d.]+|Android|iPhone|Linux|Windows|Mac/g)
			?.join(' ') || 'unknown device';

	const labelOf = (tokenId: number | null) =>
		devices?.tokens.find((t) => t.id === tokenId)?.label ?? null;

	const pushOn = (t: Token) => push?.subscriptions.some((s) => s.token_id === t.id) ?? false;

	function saveQuick() {
		prefs.setQuick(quickText.split('\n'));
		quickText = prefs.quick.join('\n');
	}

	function saveTemplates() {
		prefs.setTemplates(templates);
		templates = prefs.templates.map((t) => ({ ...t }));
	}
</script>

<div class="mx-auto flex max-w-3xl flex-col gap-8 p-3 sm:p-4">
	<section class="flex flex-col gap-2">
		<h2 class="text-base font-semibold">Connect a phone</h2>
		<p class="text-sm text-muted-foreground">
			Every link pairs one device with a token of its own and works for an hour until it is opened.
			Scan it with the phone's camera while it is on the same network or VPN; the phone keeps the
			token and shows up under Devices.
		</p>
		<form
			class="flex flex-wrap gap-2"
			onsubmit={(e) => {
				e.preventDefault();
				newLink();
			}}
		>
			<Input
				bind:value={label}
				class="max-w-48"
				placeholder="Label"
				aria-label="Label of the new device"
				maxlength={60}
			/>
			<Button type="submit" size="sm" disabled={busy}>New link</Button>
		</form>
		{#if pairing}
			<img
				class="w-48 rounded-md bg-white p-2"
				alt="QR code of the link below"
				src="data:image/svg+xml;charset=utf-8,{encodeURIComponent(pairing.svg)}"
			/>
			<code class="rounded bg-muted px-2 py-1 font-mono text-xs break-all">{pairing.url}</code>
			<div class="flex flex-wrap items-center gap-2">
				<Button
					variant="outline"
					size="sm"
					onclick={async () => {
						await navigator.clipboard?.writeText(pairing!.url);
						copied = true;
					}}>{copied ? 'Copied' : 'Copy link'}</Button
				>
				<span class="text-xs text-muted-foreground">
					For "{pairing.token.label}"; unopened, it expires {pairing.token.expires_at
						? `in ${fmtDuration(pairing.token.expires_at - live.now)}`
						: 'never'}.
				</span>
			</div>
		{:else if pairError}
			<p class="text-sm text-critical">{pairError}</p>
		{/if}
	</section>

	<section class="flex flex-col gap-2">
		<h2 class="text-base font-semibold">This device</h2>
		{#if devices?.bootstrap}
			<p class="text-sm">
				<span class="font-medium">Shared token.</span> This device is on the hub's configured token, which
				cannot be revoked by itself. Re-pair to give it its own: open a new link here, or take one now.
			</p>
			<div>
				<Button size="sm" disabled={busy} onclick={ownToken}>Give this device its own token</Button>
			</div>
		{:else if current}
			<p class="text-sm text-muted-foreground">
				Paired as <span class="font-medium text-foreground">{current.label}</span>; revoke it under
				Devices to sign out.
			</p>
		{/if}
		{#if pwa.installPrompt}
			<div>
				<Button variant="outline" size="sm" onclick={() => pwa.install()}>Install as an app</Button>
			</div>
		{:else if !pwa.installed}
			<p class="text-sm text-muted-foreground">
				Installable from the browser menu (Add to Home screen).
			</p>
		{:else}
			<p class="text-sm text-muted-foreground">Installed.</p>
		{/if}
		{#if !pwa.pushSupported}
			<p class="text-sm text-muted-foreground">This browser has no Web Push.</p>
		{:else if !isSecureContext}
			<p class="text-sm text-muted-foreground">Push notifications need HTTPS.</p>
		{:else if push && !push.enabled}
			<p class="text-sm text-muted-foreground">Push is off in the hub's configuration.</p>
		{:else if Notification.permission === 'denied'}
			<p class="text-sm text-muted-foreground">
				Notifications are blocked for this site in the browser.
			</p>
		{:else if push}
			<div class="flex flex-wrap gap-2">
				<Button size="sm" disabled={busy} onclick={togglePush}
					>{sub ? 'Turn off push notifications' : 'Turn on push notifications'}</Button
				>
				{#if sub}<Button variant="outline" size="sm" disabled={busy} onclick={sendTest}
						>Send a test</Button
					>{/if}
			</div>
			{#if push.subscriptions.length}
				<ul class="text-sm">
					{#each push.subscriptions as d (d.id)}
						<li class={d.last_failure_at ? 'text-critical' : ''}>
							{labelOf(d.token_id) ?? device(d.user_agent)}{sub && d.endpoint === sub.endpoint
								? ' (this device)'
								: ''}
							<span class="text-muted-foreground">
								· {d.last_failure_at
									? `failed ${fmtDuration(live.now - d.last_failure_at)} ago: ${d.last_failure}`
									: d.last_success_at
										? `sent ${fmtDuration(live.now - d.last_success_at)} ago`
										: 'nothing sent yet'}
							</span>
						</li>
					{/each}
				</ul>
			{/if}
		{:else if pushError}
			<p class="text-sm text-critical">{pushError}</p>
		{/if}
		{#if status}<p class="text-sm text-muted-foreground" role="status">{status}</p>{/if}
	</section>

	<section class="flex flex-col gap-2">
		<h2 class="text-base font-semibold">Devices</h2>
		<p class="text-sm text-muted-foreground">
			Every device paired with a link of its own; click a label to rename it. Revoking signs the
			device out at its next request and ends its push notifications.
		</p>
		{#if devices}
			{#if devices.tokens.length}
				<ul class="flex flex-col gap-1.5">
					{#each devices.tokens as t (t.id)}
						<li class="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md border px-2.5 py-1.5">
							<div class="flex min-w-0 flex-1 flex-col gap-0.5">
								<div class="flex items-center gap-2 text-sm">
									{#if editId === t.id}
										<Input
											bind:ref={editRef}
											bind:value={editLabel}
											class="h-7 max-w-56"
											aria-label="Label"
											maxlength={60}
											onblur={saveLabel}
											onkeydown={(e) => {
												if (e.key === 'Enter') saveLabel();
												else if (e.key === 'Escape') editId = null;
											}}
										/>
									{:else}
										<button
											type="button"
											class="truncate font-medium hover:underline"
											title="Rename"
											onclick={() => edit(t)}>{t.label}</button
										>
									{/if}
									{#if t.current}<Badge variant="secondary">this device</Badge>{/if}
									{#if t.expires_at}<Badge variant="outline">link not opened</Badge>{/if}
								</div>
								<div class="text-xs text-muted-foreground">
									{device(t.user_agent)} · added {fmtStamp(t.created_at)} ·
									{#if t.expires_at}
										expires in {fmtDuration(t.expires_at - live.now)}
									{:else if t.last_seen_at}
										seen {fmtDuration(live.now - t.last_seen_at)} ago
									{:else}
										never seen
									{/if}
									{#if pushOn(t)}· push on{/if}
								</div>
							</div>
							<Button variant="outline" size="xs" disabled={busy} onclick={() => (revoking = t)}
								>Revoke</Button
							>
						</li>
					{/each}
				</ul>
			{:else}
				<p class="text-sm text-muted-foreground">
					No device has a token of its own yet; every one is on the shared token.
				</p>
			{/if}
		{:else if devicesError}
			<p class="text-sm text-critical">{devicesError}</p>
		{/if}
		{#if devices && devicesError}
			<p class="text-sm text-critical">{devicesError}</p>
		{/if}
	</section>

	<section class="flex flex-col gap-2">
		<h2 class="text-base font-semibold">Theme</h2>
		<div class="flex gap-2">
			{#each ['auto', 'light', 'dark'] as t (t)}
				<Button
					variant={prefs.theme === t ? 'default' : 'outline'}
					size="sm"
					onclick={() => prefs.setTheme(t as Theme)}>{t}</Button
				>
			{/each}
		</div>
	</section>
	<section class="flex flex-col gap-2">
		<h2 class="text-base font-semibold">Colours</h2>
		<div class="flex gap-2">
			{#each SCHEMES as s (s.id)}
				<Button
					variant={prefs.scheme === s.id ? 'default' : 'outline'}
					size="sm"
					onclick={() => prefs.setScheme(s.id)}>{s.label}</Button
				>
			{/each}
		</div>
		<p class="text-xs text-muted-foreground">
			Neutral is shadcn's palette as it was; ccdash is the warm one from the logo, with its blue as
			the accent and a lighter dark, a first draft to iterate on.
		</p>
	</section>

	<section class="flex flex-col gap-2">
		<h2 class="text-base font-semibold">Composer</h2>
		<label class="flex flex-col gap-1 text-sm">
			<span>Quick replies, one per line</span>
			<Textarea bind:value={quickText} rows={5} class="font-mono text-xs" />
		</label>
		<div class="flex gap-2">
			<Button size="sm" onclick={saveQuick}>Save</Button>
			<Button variant="outline" size="sm" onclick={() => (quickText = DEFAULT_QUICK.join('\n'))}
				>Defaults</Button
			>
		</div>
		<p class="text-sm">Templates</p>
		{#each templates as t, i (i)}
			<div class="flex flex-col gap-1 rounded-md border p-2">
				<Input bind:value={t.name} placeholder="Name" class="h-8" />
				<Textarea bind:value={t.text} rows={3} placeholder="Text" class="text-xs" />
				<div>
					<Button
						variant="ghost"
						size="xs"
						onclick={() => (templates = templates.filter((_, j) => j !== i))}>Remove</Button
					>
				</div>
			</div>
		{/each}
		<div class="flex gap-2">
			<Button
				variant="outline"
				size="sm"
				onclick={() => (templates = [...templates, { name: '', text: '' }])}>Add template</Button
			>
			<Button size="sm" onclick={saveTemplates}>Save templates</Button>
		</div>
	</section>

	<section class="flex flex-col gap-2">
		<h2 class="text-base font-semibold">Claude Code state</h2>
		<p class="text-sm text-muted-foreground">
			Each machine's copy, Resync, conflicts and sizes are on the
			<a href="/state" class="underline underline-offset-2">State</a> page.
		</p>
	</section>

	<section class="flex flex-col gap-2">
		<h2 class="text-base font-semibold">Hosts</h2>
		<ul class="text-sm">
			{#each live.hosts as h (h.name)}
				<li>
					<i
						class="mr-1.5 inline-block size-2 rounded-full {h.connected
							? 'bg-your-turn'
							: 'bg-critical'}"
						aria-hidden="true"
					></i>{h.name}
					<span class="text-muted-foreground">
						· {h.connected ? 'connected' : 'offline'} for {fmtDuration(live.now - h.since)}</span
					>
				</li>
			{/each}
		</ul>
		<p class="text-xs text-muted-foreground">Hub: {live.host}.</p>
	</section>
</div>

<Dialog.Root
	open={revoking !== null}
	onOpenChange={(open) => {
		if (!open) revoking = null;
	}}
>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>
				{revoking?.current ? 'Sign this device out?' : `Revoke ${revoking?.label ?? ''}?`}
			</Dialog.Title>
			<Dialog.Description>
				{#if revoking?.current}
					This signs this device out: its token stops working and its push notifications end. To
					come back, pair it again from another device or with
					<code class="font-mono">ccdash url --hub --qr</code>.
				{:else}
					The device loses access at its next request and its push notifications end.
				{/if}
			</Dialog.Description>
		</Dialog.Header>
		<Dialog.Footer>
			<Button variant="outline" onclick={() => (revoking = null)}>Cancel</Button>
			<Button variant="destructive" disabled={busy} onclick={() => revoking && revoke(revoking)}
				>Revoke</Button
			>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
