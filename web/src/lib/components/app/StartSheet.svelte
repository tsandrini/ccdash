<script lang="ts">
	// Starts a background session (`claude --bg` on a connected host, in a
	// project the hub knows) or continues an ended one: host, workspace and
	// project, the model, the permission mode, the prompt. Waits for the
	// session to report and opens its page.
	import { untrack } from 'svelte';
	import { goto } from '$app/navigation';
	import LoaderCircleIcon from '@lucide/svelte/icons/loader-circle';
	import { Unauthorized } from '#lib/api/client.js';
	import {
		fetchLaunch,
		fetchProjects,
		fetchPrompts,
		startLaunch,
		type Launch
	} from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Sheet from '#lib/components/ui/sheet/index.js';
	import PromptField from '#lib/components/session/PromptField.svelte';
	import { place } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { prefs } from '#lib/prefs.svelte.js';
	import { scope } from '#lib/scope.svelte.js';
	import { label } from '#lib/session.js';
	import { starter } from '#lib/starter.svelte.js';

	const MODELS: [string, string][] = [
		['', 'Default'],
		['fable', 'Fable'],
		['opus', 'Opus'],
		['sonnet', 'Sonnet'],
		['haiku', 'Haiku']
	];
	const MODES: [string, string][] = [
		['', 'Default'],
		['plan', 'Plan'],
		['acceptEdits', 'Accept edits']
	];
	/** The project select's value for the workspace's own directory. */
	const ROOT = '/';
	const POLL_MS = 2000;

	let host = $state('');
	let ws = $state('');
	let project = $state('');
	let model = $state('');
	let mode = $state('');
	let text = $state('');
	let field = $state<PromptField | null>(null);
	let projectsOf = $state<Record<string, string[]>>({});
	let pending = $state<string | null>(null);
	let busy = $state(false);
	let error = $state('');

	const resume = $derived(starter.preset.resume);
	const draftKey = $derived(resume ? `resume:${resume.id}` : 'start');
	const connected = $derived(live.hosts.filter((h) => h.connected).map((h) => h.name));
	const names = $derived(projectsOf[ws]);
	const launch = $derived<Launch | undefined>(pending ? live.launches.get(pending) : undefined);
	const ready = $derived(
		Boolean(host && ws && (resume || project) && text.trim() && !busy && !pending)
	);
	const touch = () => matchMedia('(pointer: coarse)').matches;

	// A fresh sheet: the preset, else the last choice and the scope.
	$effect(() => {
		if (!starter.open) return;
		const preset = starter.preset;
		untrack(() => {
			const r = preset.resume;
			const last = prefs.start;
			host = r?.host ?? (connected.includes(last.host) ? last.host : connected[0]) ?? '';
			ws = r?.workspace ?? preset.ws ?? scope.current.ws[0] ?? live.workspaces[0] ?? '';
			project = r ? (r.project ?? ROOT) : preset.project === null ? ROOT : (preset.project ?? '');
			model = last.model;
			mode = last.mode;
			pending = null;
			error = '';
		});
	});

	$effect(() => {
		if (starter.open) text = prefs.draft(draftKey);
	});

	$effect(() => {
		if (starter.open) prefs.setDraft(draftKey, text);
	});

	// The workspace's projects, as `/projects` lists them.
	$effect(() => {
		const name = ws;
		if (!starter.open || resume || !name || projectsOf[name]) return;
		fetchProjects({ workspace: name })
			.then((data) => {
				const list = (data.workspaces.find((w) => w.name === name)?.projects ?? [])
					.map((p) => p.project)
					.filter((p): p is string => Boolean(p));
				projectsOf = { ...projectsOf, [name]: list };
			})
			.catch((e) => {
				if (e instanceof Unauthorized) live.needToken = true;
				else error = (e as Error).message;
			});
	});

	// The launch moves on through the stream; polled too, for a missed update.
	$effect(() => {
		const id = pending;
		if (!id) return;
		const timer = setInterval(async () => {
			try {
				live.launches.set(id, (await fetchLaunch(id)).launch);
			} catch (e) {
				if (e instanceof Unauthorized) live.needToken = true;
			}
		}, POLL_MS);
		return () => clearInterval(timer);
	});

	$effect(() => {
		if (!launch) return;
		if (launch.state === 'started') {
			const session = launch.session;
			pending = null;
			text = '';
			starter.open = false;
			goto(`/sessions/${session}`);
		} else if (launch.state === 'failed') {
			pending = null;
			error = launch.error;
		}
	});

	async function prompts() {
		return ws ? (await fetchPrompts({ workspace: ws, limit: 50 })).prompts : [];
	}

	async function start() {
		const body = text.trim();
		if (!ready) return;
		busy = true;
		error = '';
		try {
			const { launch } = await startLaunch({
				host,
				workspace: ws,
				project: project === ROOT ? undefined : project,
				prompt: body,
				model: model || undefined,
				permission_mode: mode || undefined,
				resume: resume?.id
			});
			prefs.setStart({ host, model, mode });
			field?.remember(body);
			live.launches.set(launch.id, launch);
			pending = launch.id;
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	const selectClass =
		'h-8 w-full min-w-0 rounded-lg border border-input bg-transparent px-2 text-sm text-foreground dark:bg-input/30';
</script>

<Sheet.Root bind:open={starter.open}>
	<Sheet.Content
		side={touch() ? 'bottom' : 'right'}
		class="flex max-h-[92vh] flex-col gap-3 overflow-y-auto p-4 sm:max-w-lg"
	>
		<Sheet.Header class="p-0 pr-8">
			<Sheet.Title>{resume ? 'Continue the session' : 'Start a session'}</Sheet.Title>
			<Sheet.Description>
				{#if resume}
					Continues “{label(resume)}” in the background on {resume.host}, in {place(resume.cwd)}.
				{:else}
					Runs <code class="font-mono">claude --bg</code> on the host, in the project's directory. Its
					questions and approvals reach this device.
				{/if}
			</Sheet.Description>
		</Sheet.Header>

		{#if !connected.length}
			<p class="text-sm text-muted-foreground">
				No host is connected: sessions start on a workstation whose ccdash collector is up.
			</p>
		{:else}
			{#if !resume}
				<div class="grid grid-cols-1 gap-2 sm:grid-cols-3">
					<label class="flex min-w-0 flex-col gap-1 text-xs text-muted-foreground">
						Host
						<select class={selectClass} bind:value={host}>
							{#each connected as h (h)}<option value={h}>{h}</option>{/each}
						</select>
					</label>
					<label class="flex min-w-0 flex-col gap-1 text-xs text-muted-foreground">
						Workspace
						<select class={selectClass} bind:value={ws} onchange={() => (project = '')}>
							{#each live.workspaces as w (w)}<option value={w}>{w}</option>{/each}
						</select>
					</label>
					<label class="flex min-w-0 flex-col gap-1 text-xs text-muted-foreground">
						Project
						<select class={selectClass} bind:value={project}>
							<option value="" disabled>{names ? 'Pick a project' : 'Loading…'}</option>
							{#each names ?? [] as p (p)}<option value={p}>{p}</option>{/each}
							<option value={ROOT}>(the workspace itself)</option>
						</select>
					</label>
				</div>
			{:else if !connected.includes(resume.host)}
				<p class="text-sm text-critical">{resume.host} is not connected.</p>
			{/if}
			<div class="grid grid-cols-2 gap-2">
				<label class="flex min-w-0 flex-col gap-1 text-xs text-muted-foreground">
					Model
					<select class={selectClass} bind:value={model}>
						{#each MODELS as [value, name] (value)}<option {value}>{name}</option>{/each}
					</select>
				</label>
				<label class="flex min-w-0 flex-col gap-1 text-xs text-muted-foreground">
					Permission mode
					<select class={selectClass} bind:value={mode}>
						{#each MODES as [value, name] (value)}<option {value}>{name}</option>{/each}
					</select>
				</label>
			</div>
			<PromptField
				bind:this={field}
				bind:text
				{prompts}
				historyNote="The workspace's prompts."
				placeholder={resume ? 'What next?' : 'What should Claude do?'}
				label="Prompt"
				rows={3}
				disabled={busy || Boolean(pending)}
				onsubmit={start}
			>
				{#snippet actions()}
					<Button type="submit" size="sm" disabled={!ready}>
						{#if busy || pending}<LoaderCircleIcon class="animate-spin" />{/if}
						{resume ? 'Continue' : 'Start'}
					</Button>
				{/snippet}
			</PromptField>
			{#if pending}
				<p class="text-xs text-muted-foreground" role="status">
					Starting on {host}; the session's page opens when it reports.
				</p>
			{/if}
		{/if}
		{#if error}<p class="text-xs text-critical" role="alert">{error}</p>{/if}
	</Sheet.Content>
</Sheet.Root>
