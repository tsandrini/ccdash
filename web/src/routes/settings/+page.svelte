<script lang="ts">
	// Pairing, this device (install, push), theme, the composer's quick
	// replies and templates, hosts.
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchPair, fetchPush, testPush, type Pair, type PushInfo } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Textarea } from '#lib/components/ui/textarea/index.js';
	import { fmtDuration } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { DEFAULT_QUICK, prefs, SCHEMES, type Theme } from '#lib/prefs.svelte.js';
	import { pwa } from '#lib/pwa.svelte.js';

	let pair = $state<Pair | null>(null);
	let pairError = $state('');
	let copied = $state(false);
	let push = $state<PushInfo | null>(null);
	let pushError = $state('');
	let sub = $state<PushSubscription | null>(null);
	let status = $state('');
	let busy = $state(false);

	let quickText = $state(prefs.quick.join('\n'));
	let templates = $state(prefs.templates.map((t) => ({ ...t })));

	async function load() {
		try {
			pair = await fetchPair();
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else pairError = (e as Error).message;
		}
		if (!pwa.pushSupported) return;
		try {
			push = await fetchPush();
			sub = await pwa.subscription();
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else pushError = (e as Error).message;
		}
	}

	$effect(() => {
		if (live.ready) load();
	});

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
			if (e instanceof Unauthorized) live.needToken = true;
			else status = (e as Error).message;
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
		{#if pair}
			<p class="text-sm text-muted-foreground">
				Scan this with the phone's camera while it is on the same network or VPN. The link carries
				the access token and the phone keeps it, so this is a one-time step per device.
			</p>
			<img
				class="w-48 rounded-md bg-white p-2"
				alt="QR code of the link below"
				src="data:image/svg+xml;charset=utf-8,{encodeURIComponent(pair.svg)}"
			/>
			<code class="rounded bg-muted px-2 py-1 font-mono text-xs break-all">{pair.url}</code>
			<div>
				<Button
					variant="outline"
					size="sm"
					onclick={async () => {
						await navigator.clipboard?.writeText(pair!.url);
						copied = true;
					}}>{copied ? 'Copied' : 'Copy link'}</Button
				>
			</div>
		{:else if pairError}
			<p class="text-sm text-critical">{pairError}</p>
		{/if}
	</section>

	<section class="flex flex-col gap-2">
		<h2 class="text-base font-semibold">This device</h2>
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
			{#if status}<p class="text-sm text-muted-foreground" role="status">{status}</p>{/if}
			{#if push.subscriptions.length}
				<ul class="text-sm">
					{#each push.subscriptions as d (d.id)}
						<li class={d.last_failure_at ? 'text-critical' : ''}>
							{device(d.user_agent)}{sub && d.endpoint === sub.endpoint ? ' (this device)' : ''}
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
