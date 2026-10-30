<script lang="ts">
	// Workspace › Configuration: the unit globs, what never travels, what
	// travels although ignored (with the quiescent flag), the size limits,
	// the conflict shape and the per-bundle thresholds. Per-unit overrides
	// live on the unit page.
	import { putWorkspaceConfig, type WorkspaceConfig } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Textarea } from '#lib/components/ui/textarea/index.js';

	let {
		config,
		version,
		bundles,
		onchange
	}: { config: WorkspaceConfig; version: number; bundles: string[]; onchange: () => void } =
		$props();

	let open = $state(false);
	let include = $state('');
	let exclude = $state('');
	let always = $state('');
	let ignored = $state('');
	let maxFile = $state(64);
	let autoInclude = $state(500);
	let depth = $state(2);
	let conflicts = $state<'markers' | 'set_aside'>('markers');
	let perBundle = $state<Record<string, string>>({});
	let loaded = $state(-1);
	let busy = $state(false);
	let error = $state('');
	let saved = $state(false);

	const lines = (list: string[]) => list.join('\n');
	const list = (text: string) =>
		text
			.split('\n')
			.map((l) => l.trim())
			.filter(Boolean);

	// NOTE: the form follows the hub's version, not every poll, so typing is not overwritten
	$effect(() => {
		if (loaded === version) return;
		loaded = version;
		include = lines(config.include);
		exclude = lines(config.exclude);
		always = lines(config.always_exclude);
		ignored = config.sync_ignored
			.map((i) => (i.quiescent ? `${i.pattern} quiescent` : i.pattern))
			.join('\n');
		maxFile = config.max_file_mb;
		autoInclude = config.auto_include_mb;
		depth = config.depth;
		conflicts = config.conflicts;
		perBundle = Object.fromEntries(
			bundles.map((b) => [b, config.bundles[b]?.auto_include_mb?.toString() ?? ''])
		);
	});

	async function save() {
		busy = true;
		error = '';
		saved = false;
		try {
			const next: WorkspaceConfig = {
				...config,
				depth,
				include: list(include),
				exclude: list(exclude),
				always_exclude: list(always),
				sync_ignored: list(ignored).map((l) => {
					const [pattern, flag] = l.split(/\s+/);
					return { pattern, quiescent: flag === 'quiescent' };
				}),
				max_file_mb: Math.max(1, Math.round(maxFile)),
				auto_include_mb: Math.max(0, Math.round(autoInclude)),
				conflicts,
				bundles: Object.fromEntries(
					Object.entries({ ...config.bundles }).map(([b, o]) => [b, { ...o }])
				)
			};
			for (const [b, text] of Object.entries(perBundle)) {
				const value = text.trim() === '' ? null : Number(text);
				const current = next.bundles[b] ?? {
					auto_include_mb: null,
					sync_ignored: [],
					always_exclude: []
				};
				current.auto_include_mb = value === null || Number.isNaN(value) ? null : value;
				if (
					current.auto_include_mb === null &&
					!current.sync_ignored.length &&
					!current.always_exclude.length
				)
					delete next.bundles[b];
				else next.bundles[b] = current;
			}
			await putWorkspaceConfig(next);
			saved = true;
			onchange();
		} catch (e) {
			error = (e as Error).message;
		} finally {
			busy = false;
		}
	}
</script>

<section class="flex flex-col gap-2">
	<div class="flex items-center gap-2">
		<h2 class="text-base font-semibold">Configuration</h2>
		<span class="text-xs text-muted-foreground">v{version}</span>
		<Button variant="ghost" size="xs" class="ml-auto" onclick={() => (open = !open)}
			>{open ? 'Hide' : 'Edit'}</Button
		>
	</div>
	{#if open}
		<form
			class="grid gap-3 sm:grid-cols-2"
			onsubmit={(e) => {
				e.preventDefault();
				save();
			}}
		>
			<label class="flex flex-col gap-1 text-xs">
				<span class="font-medium">Include (globs on unit ids, one per line)</span>
				<Textarea bind:value={include} rows={5} class="font-mono text-xs" />
			</label>
			<label class="flex flex-col gap-1 text-xs">
				<span class="font-medium">Exclude (wins over include)</span>
				<Textarea bind:value={exclude} rows={5} class="font-mono text-xs" />
			</label>
			<label class="flex flex-col gap-1 text-xs">
				<span class="font-medium">Never travels (directory names, anywhere)</span>
				<Textarea bind:value={always} rows={5} class="font-mono text-xs" />
			</label>
			<label class="flex flex-col gap-1 text-xs">
				<span class="font-medium"
					>Travels although ignored (a path per line; add <code>quiescent</code> for a state directory
					a server may hold open)</span
				>
				<Textarea bind:value={ignored} rows={5} class="font-mono text-xs" />
			</label>
			<div class="grid grid-cols-2 gap-2 sm:col-span-2 sm:grid-cols-4">
				<label class="flex flex-col gap-1 text-xs">
					<span class="font-medium">Unit depth</span>
					<Input type="number" min={1} max={6} bind:value={depth} class="h-8" />
				</label>
				<label class="flex flex-col gap-1 text-xs">
					<span class="font-medium">Skip untracked files above (MB)</span>
					<Input type="number" min={1} bind:value={maxFile} class="h-8" />
				</label>
				<label class="flex flex-col gap-1 text-xs">
					<span class="font-medium">Include new directories under (MB)</span>
					<Input type="number" min={0} bind:value={autoInclude} class="h-8" />
				</label>
				<label class="flex flex-col gap-1 text-xs">
					<span class="font-medium">Text conflicts</span>
					<select bind:value={conflicts} class="h-8 rounded-md border bg-background px-2 text-xs">
						<option value="markers">markers in the file</option>
						<option value="set_aside">keep local, other side on the hub</option>
					</select>
				</label>
			</div>
			{#if bundles.length}
				<div class="flex flex-col gap-1 text-xs sm:col-span-2">
					<span class="font-medium">Auto-include threshold per bundle (MB, empty = default)</span>
					<div class="flex flex-wrap gap-2">
						{#each bundles as b (b)}
							<label class="flex items-center gap-1">
								<span class="font-mono">{b}</span>
								<Input
									bind:value={perBundle[b]}
									class="h-7 w-20 text-xs"
									placeholder={String(autoInclude)}
								/>
							</label>
						{/each}
					</div>
				</div>
			{/if}
			<div class="flex items-center gap-2 sm:col-span-2">
				<Button type="submit" size="sm" disabled={busy}>Save</Button>
				{#if saved}<span class="text-xs text-muted-foreground" role="status"
						>Saved; every collector has it.</span
					>{/if}
				{#if error}<span class="text-sm text-critical" role="alert">{error}</span>{/if}
			</div>
		</form>
	{:else}
		<p class="text-xs text-muted-foreground">
			{config.include.length ? config.include.join(', ') : 'every directory'} at depth {config.depth};
			{config.exclude.length} excluded; never: {config.always_exclude.join(', ')}; carried although
			ignored: {config.sync_ignored.map((i) => i.pattern).join(', ') || 'nothing'}; new directories
			under {config.auto_include_mb} MB are included by themselves.
		</p>
	{/if}
</section>
