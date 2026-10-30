<script lang="ts">
	// One unit: its state on every machine, the snapshot history, its
	// conflicts, the ignored directories present with a switch each, and the
	// unit's overrides (paused, limits, conflict shape, exclude it).
	import { page } from '$app/state';
	import { Unauthorized } from '#lib/api/client.js';
	import {
		fetchUnitHistory,
		fetchWorkspace,
		workspaceResync,
		workspaceUnitOp,
		type HistoryEntry,
		type UnitOverride,
		type WorkspaceView
	} from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import StateChip from '#lib/components/workspace/StateChip.svelte';
	import WsConflicts from '#lib/components/workspace/WsConflicts.svelte';
	import { fmtBytes, fmtStamp, plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { short } from '#lib/workspace.js';

	const POLL_MS = 3000;
	const unit = $derived(page.params.unit!);

	let view = $state<WorkspaceView | null>(null);
	let history = $state<HistoryEntry[] | null>(null);
	let error = $state('');
	let busy = $state(false);
	let notice = $state('');
	let maxFile = $state('');
	let conflicts = $state<'' | 'markers' | 'set_aside'>('');
	let loadedVersion = $state(-1);

	const row = $derived(view?.units.find((u) => u.unit === unit) ?? null);
	const over = $derived<UnitOverride>(
		view?.config.units[unit] ?? {
			include: null,
			paused: false,
			always_exclude: [],
			sync_ignored: [],
			max_file_mb: null,
			conflicts: null
		}
	);
	const unitConflicts = $derived(view?.conflicts.filter((c) => c.unit === unit) ?? []);
	const hosts = $derived(view?.hosts.filter((h) => h.status) ?? []);

	async function load() {
		try {
			view = await fetchWorkspace();
			error = '';
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		}
	}

	async function loadHistory() {
		try {
			history = (await fetchUnitHistory(unit)).history;
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
		}
	}

	$effect(() => {
		if (!live.ready) return;
		void unit;
		load();
		loadHistory();
		const timers = [setInterval(load, POLL_MS), setInterval(loadHistory, 30_000)];
		return () => timers.forEach(clearInterval);
	});

	$effect(() => {
		if (!view || loadedVersion === view.config_version) return;
		loadedVersion = view.config_version;
		maxFile = over.max_file_mb?.toString() ?? '';
		conflicts = over.conflicts ?? '';
	});

	async function send(op: string, value?: UnitOverride, host?: string) {
		busy = true;
		error = '';
		notice = '';
		try {
			await workspaceUnitOp(unit, { op, host: host ?? null, value: value ?? null });
			await load();
		} catch (e) {
			error = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	/** The overrides with `sync_ignored` toggled for `path`. */
	function toggled(path: string, on: boolean, quiescent: boolean): UnitOverride {
		const rest = over.sync_ignored.filter((i) => i.pattern !== path);
		return {
			...over,
			sync_ignored: on ? [...rest, { pattern: path, quiescent }] : rest
		};
	}

	function saveOverrides() {
		const mb = maxFile.trim() === '' ? null : Number(maxFile);
		send('override', {
			...over,
			max_file_mb: mb === null || Number.isNaN(mb) ? null : Math.max(1, Math.round(mb)),
			conflicts: conflicts === '' ? null : conflicts
		});
	}

	async function resync(host: string) {
		busy = true;
		error = '';
		try {
			await workspaceResync(host, unit);
			notice = `${host} fetches and snapshots ${unit} again.`;
		} catch (e) {
			error = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	/** Ignored directories present on any host, by path, with the hosts' numbers. */
	const ignoredDirs = $derived.by(() => {
		if (!row) return [];
		const byPath: Record<string, { files: number; bytes: number; hosts: string[] }> = {};
		for (const [host, s] of Object.entries(row.hosts)) {
			for (const d of s.ignored) {
				const e = byPath[d.path] ?? { files: 0, bytes: 0, hosts: [] };
				e.files = Math.max(e.files, d.files);
				e.bytes = Math.max(e.bytes, d.bytes);
				e.hosts.push(host);
				byPath[d.path] = e;
			}
		}
		return Object.entries(byPath).sort((a, b) => b[1].bytes - a[1].bytes);
	});
	const carried = (path: string) => over.sync_ignored.find((i) => i.pattern === path);
	const quiescentDefault = (path: string) => path.startsWith('.devenv/state');
</script>

<div class="mx-auto flex max-w-5xl flex-col gap-5 p-3 sm:p-4">
	<div class="flex flex-wrap items-baseline gap-x-3 gap-y-1">
		<a href="/workspace" class="text-sm text-muted-foreground hover:underline">Workspace</a>
		<h1 class="font-mono text-lg font-semibold">{unit}</h1>
		{#if row}
			<span class="text-xs text-muted-foreground">
				{row.kind} · {fmtBytes(row.size)} · {row.version} pushes · last {fmtStamp(row.at)}
			</span>
			{#if row.paused}<span class="rounded bg-muted px-1.5 text-xs">paused</span>{/if}
		{/if}
	</div>
	{#if error}<p class="text-sm text-critical" role="alert">{error}</p>{/if}
	{#if notice}<p class="text-sm text-muted-foreground" role="status">{notice}</p>{/if}
	{#if view && !row}
		<p class="text-sm text-muted-foreground">
			This unit does not travel. It may be new, excluded or ignored: see the Workspace page.
		</p>
		<div>
			<Button size="sm" disabled={busy} onclick={() => send('include')}>Include it</Button>
		</div>
	{:else if row}
		<section class="flex flex-col gap-2">
			<h2 class="text-base font-semibold">Machines</h2>
			<ul class="flex flex-col gap-1.5">
				{#each hosts as h (h.host)}
					{@const s = row.hosts[h.host]}
					<li class="flex flex-col gap-1 rounded-md border px-2.5 py-1.5">
						<div class="flex flex-wrap items-center gap-2">
							<span class="text-sm font-medium">{h.host}</span>
							<StateChip state={s?.state ?? null} />
							{#if s?.snapshot}
								<span class="font-mono text-xs text-muted-foreground">{short(s.snapshot)}</span>
							{/if}
							{#if s}
								<span class="text-xs text-muted-foreground"
									>{plural(s.files, 'file')}{s.last_pass
										? ` · pass ${fmtStamp(s.last_pass)}`
										: ''}</span
								>
							{/if}
							<span class="ml-auto flex gap-1">
								{#if s?.state === 'unadopted'}
									<Button
										size="xs"
										disabled={busy || !h.connected}
										onclick={() => send('adopt', undefined, h.host)}>Adopt here</Button
									>
								{/if}
								<Button
									variant="outline"
									size="xs"
									disabled={busy || !h.connected}
									onclick={() => resync(h.host)}>Resync</Button
								>
							</span>
						</div>
						{#if s?.detail}<div class="text-xs text-muted-foreground">{s.detail}</div>{/if}
						{#each s?.diverged ?? [] as d (`${d.host}/${d.branch}`)}
							<div class="text-xs">
								<span class="font-mono">{d.branch.replace('refs/heads/', '')}</span> diverged:
								<span class="font-mono">{short(d.remote)}</span> on {d.host},
								<span class="font-mono">{short(d.local)}</span> here; the other tip is at
								<code class="font-mono"
									>refs/sync/diverged/{d.host}/{d.branch.replace('refs/heads/', '')}</code
								>
								for you to merge or rebase.
							</div>
						{/each}
					</li>
				{/each}
			</ul>
		</section>

		{#if unitConflicts.length}
			<section class="flex flex-col gap-2">
				<h2 class="text-base font-semibold">Conflicts</h2>
				<WsConflicts conflicts={unitConflicts} onchange={load} compact />
			</section>
		{/if}

		<section class="flex flex-col gap-2">
			<h2 class="text-base font-semibold">Ignored directories</h2>
			{#if !ignoredDirs.length}
				<p class="text-sm text-muted-foreground">None present on any machine.</p>
			{:else}
				<p class="text-xs text-muted-foreground">
					Gitignored directories the machines hold. A switch carries one (every file of it travels
					and is watched; the first transfer costs what it weighs). State directories under
					<code class="font-mono">.devenv/state</code> are carried only while no local process has them
					open.
				</p>
				<ul class="flex flex-col gap-1">
					{#each ignoredDirs as [path, d] (path)}
						{@const c = carried(path)}
						<li
							class="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md border px-2.5 py-1.5 text-sm"
						>
							<span class="font-mono text-xs">{path}</span>
							<span class="text-xs text-muted-foreground"
								>{fmtBytes(d.bytes)}, {plural(d.files, 'file')} · on {d.hosts.join(', ')}</span
							>
							{#if c?.quiescent || (!c && quiescentDefault(path))}
								<span class="rounded bg-muted px-1 text-[10px] text-muted-foreground"
									>quiescent</span
								>
							{/if}
							<span class="ml-auto flex items-center gap-2 text-xs">
								{c ? 'carried' : 'stays home'}
								<Switch
									checked={!!c}
									disabled={busy}
									aria-label="Carry {path}"
									onCheckedChange={(on) =>
										send('override', toggled(path, on, quiescentDefault(path)))}
								/>
							</span>
						</li>
					{/each}
				</ul>
			{/if}
		</section>

		<section class="flex flex-col gap-2">
			<h2 class="text-base font-semibold">Overrides</h2>
			<form
				class="flex flex-wrap items-end gap-3"
				onsubmit={(e) => {
					e.preventDefault();
					saveOverrides();
				}}
			>
				<label class="flex flex-col gap-1 text-xs">
					<span class="font-medium">Skip untracked files above (MB)</span>
					<Input
						bind:value={maxFile}
						class="h-8 w-28"
						placeholder={String(view?.config.max_file_mb ?? 64)}
					/>
				</label>
				<label class="flex flex-col gap-1 text-xs">
					<span class="font-medium">Text conflicts</span>
					<select bind:value={conflicts} class="h-8 rounded-md border bg-background px-2 text-xs">
						<option value="">as configured</option>
						<option value="markers">markers in the file</option>
						<option value="set_aside">keep local, other side on the hub</option>
					</select>
				</label>
				<Button type="submit" size="sm" disabled={busy}>Save</Button>
				<Button
					type="button"
					variant="outline"
					size="sm"
					disabled={busy}
					onclick={() => send(over.paused ? 'resume' : 'pause')}
					>{over.paused ? 'Resume' : 'Pause'}</Button
				>
				<Button
					type="button"
					variant="ghost"
					size="sm"
					disabled={busy}
					onclick={() => send('exclude')}>Exclude this unit</Button
				>
			</form>
		</section>

		<section class="flex flex-col gap-2">
			<h2 class="text-base font-semibold">Snapshots</h2>
			{#if !history}
				<p class="text-sm text-muted-foreground">Loading…</p>
			{:else if !history.length}
				<p class="text-sm text-muted-foreground">No snapshot on the hub yet.</p>
			{:else}
				<ul class="flex flex-col divide-y rounded-md border text-sm">
					{#each history as h (h.commit)}
						<li class="flex flex-wrap items-center gap-x-3 gap-y-0.5 px-2.5 py-1">
							<span class="font-mono text-xs text-muted-foreground">{short(h.commit)}</span>
							<span class="font-medium">{h.host || '?'}</span>
							<span class="text-xs text-muted-foreground">{fmtStamp(h.at)}</span>
							{#if h.merge}<span class="rounded bg-muted px-1 text-[10px]">merge</span>{/if}
							{#if h.branch}<span class="font-mono text-xs">{h.branch}</span>{/if}
							<span class="ml-auto text-xs text-muted-foreground tabular-nums"
								>{plural(h.files, 'file')} changed</span
							>
						</li>
					{/each}
				</ul>
			{/if}
		</section>
	{:else if !error}
		<p class="text-sm text-muted-foreground">Loading…</p>
	{/if}
</div>
