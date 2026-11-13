<script lang="ts">
	// Starts a background session (`claude --bg` on a connected host, at
	// the workspace's root, under a project with notes and a directory on
	// that host) or continues an ended one, on the host it ran on or
	// another that syncs it: host, workspace and project, the model, the
	// permission mode, the prompt. A dialog over most of the page, the
	// prompt's editor filling it: the longest prompts are written here. For
	// an issue the prompt is written already, to edit. The draft lives on
	// the hub, so closing the dialog loses nothing. Waits for the session to
	// report and opens its page.
	import { untrack } from 'svelte';
	import { goto } from '$app/navigation';
	import LoaderCircleIcon from '@lucide/svelte/icons/loader-circle';
	import { Unauthorized } from '#lib/api/client.js';
	import {
		cancelEdit,
		editDraft,
		fetchLaunch,
		fetchPrompts,
		fetchTargets,
		startLaunch,
		type Launch,
		type LaunchTarget
	} from '#lib/api/index.js';
	import type { StartPreset } from '#lib/starter.svelte.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { SmartSelect, type Choice } from '#lib/components/ui/smart-select/index.js';
	import PromptField from '#lib/components/session/PromptField.svelte';
	import {
		clearDraft,
		editHostFor,
		flushDraft,
		initialDraft,
		markSynced,
		noteDraft,
		remoteDraft
	} from '#lib/drafts.js';
	import { fmtBytes, place, plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { fmtPct } from '#lib/machines.js';
	import { customModel, EFFORTS, MODELS } from '#lib/models.js';
	import { issuePrompt } from '#lib/notes.js';
	import { prefs } from '#lib/prefs.svelte.js';
	import { scope } from '#lib/scope.svelte.js';
	import { starter } from '#lib/starter.svelte.js';

	const MODES: Choice[] = [
		{ value: '', label: 'Default', hint: "The host's own setting" },
		{ value: 'plan', label: 'Plan', hint: 'Reads and plans, changes nothing' },
		{ value: 'acceptEdits', label: 'Accept edits', hint: 'Edits files without asking' }
	];
	/** The project select's value for no project (workspace-wide). */
	const ROOT = '/';
	const POLL_MS = 2000;

	let host = $state('');
	let ws = $state('');
	let project = $state('');
	let model = $state('');
	let mode = $state('');
	let effort = $state('');
	let text = $state('');
	let field = $state<PromptField | null>(null);
	/** Where sessions can start; null until loaded. */
	let targets = $state<LaunchTarget[] | null>(null);
	let pending = $state<string | null>(null);
	let busy = $state(false);
	let error = $state('');

	const resume = $derived(starter.preset.resume);
	const issue = $derived(starter.preset.issue);
	const draftKey = $derived(
		resume ? `resume:${resume.id}` : issue ? `issue:${issue.slug}` : 'start'
	);
	const draft = $derived(starter.open ? live.drafts.get(draftKey) : undefined);
	const editHost = $derived(editHostFor(host));
	const connected = $derived((targets ?? []).map((t) => t.host));
	/** Where a session to resume can run: connected machines with its workspace. */
	const resumeHosts = $derived(
		resume
			? (targets ?? [])
					.filter((t) => t.workspaces.some((w) => w.name === resume.workspace))
					.map((t) => t.host)
			: []
	);
	const workspacesHere = $derived(targets?.find((t) => t.host === host)?.workspaces ?? []);
	const names = $derived(workspacesHere.find((w) => w.name === ws)?.projects);
	const launch = $derived<Launch | undefined>(pending ? live.launches.get(pending) : undefined);
	const ready = $derived(
		Boolean(
			host && ws && (resume || project) && text.trim() && !busy && !pending && !draft?.editing
		)
	);
	const vimEsc = $derived(prefs.vimOn);

	/** The lists of a session to resume: no pick keeps what it ran on. */
	const kept = (list: Choice[]): Choice[] =>
		resume ? list.map((c) => (c.value ? c : { ...c, label: 'As it was', hint: undefined })) : list;
	const modelChoices = $derived(kept(MODELS));
	const effortChoices = $derived(kept(EFFORTS));
	const modeChoices = $derived(kept(MODES));

	/** "3 sessions", or nothing for none: a row's figure. */
	const open = (n: number) => (n ? plural(n, 'session') : undefined);
	const running = $derived(live.list.filter((s) => s.activity !== 'gone'));
	const hostChoices = $derived<Choice[]>(
		connected.map((h) => {
			const m = live.hosts.find((x) => x.name === h)?.machine;
			const parts = [
				h === live.here ? 'this device' : null,
				m && m.cpu.usage_pct !== null ? `CPU ${fmtPct(m.cpu.usage_pct)}` : null,
				m?.memory.total ? `mem ${fmtBytes(m.memory.used)} of ${fmtBytes(m.memory.total)}` : null
			];
			return {
				value: h,
				label: h,
				hint: parts.filter(Boolean).join(' · ') || undefined,
				meta: open(running.filter((s) => s.host === h).length)
			};
		})
	);
	const wsChoices = $derived<Choice[]>(
		workspacesHere.map((w) => ({
			value: w.name,
			label: w.name,
			hint: plural(w.projects.length, 'project'),
			meta: open(running.filter((s) => s.workspace === w.name).length)
		}))
	);
	const projectChoices = $derived<Choice[]>([
		...(names ?? []).map((p) => ({
			value: p,
			label: p,
			meta: open(running.filter((s) => s.workspace === ws && s.project === p).length)
		})),
		{
			value: ROOT,
			label: '(no project)',
			hint: 'The workspace itself',
			meta: open(running.filter((s) => s.workspace === ws && !s.project).length)
		}
	]);
	let fieldFocused = $state(false);

	// A fresh sheet: where sessions can start, then the preset, else the
	// last choice and the scope.
	$effect(() => {
		if (!starter.open) return;
		const preset = starter.preset;
		untrack(() => {
			// a session to resume keeps its own options unless one is picked
			model = preset.resume ? '' : prefs.start.model;
			mode = preset.resume ? '' : prefs.start.mode;
			effort = preset.resume ? '' : prefs.start.effort;
			pending = null;
			error = '';
			targets = null;
			fetchTargets()
				.then(({ hosts }) => {
					if (!starter.open) return;
					const starting = hosts.filter((t) => live.features(t.host).launch);
					targets = starting;
					pick(preset, starting);
				})
				.catch((e) => {
					if (e instanceof Unauthorized) live.needToken = true;
					else error = (e as Error).message;
				});
		});
	});

	function pick(preset: StartPreset, hosts: LaunchTarget[]) {
		const r = preset.resume;
		const last = prefs.start.host;
		const has = (h: string | undefined) =>
			hosts.some((t) => t.host === h && (!r || t.workspaces.some((w) => w.name === r.workspace)));
		host =
			(r && has(r.host) ? r.host : has(last) ? last : hosts.find((t) => has(t.host))?.host) ?? '';
		const here = hosts.find((t) => t.host === host)?.workspaces ?? [];
		const want = r?.workspace ?? preset.ws ?? scope.current.ws[0] ?? '';
		ws = r?.workspace ?? (here.some((w) => w.name === want) ? want : (here[0]?.name ?? ''));
		const known = here.find((w) => w.name === ws)?.projects ?? [];
		project = r
			? (r.project ?? ROOT)
			: preset.project === null
				? ROOT
				: preset.project && known.includes(preset.project)
					? preset.project
					: '';
	}

	/** Another host: keep the workspace and project where that host has them. */
	function pickHost() {
		const here = workspacesHere;
		if (!here.some((w) => w.name === ws)) ws = here[0]?.name ?? '';
		const known = here.find((w) => w.name === ws)?.projects ?? [];
		if (project !== ROOT && !known.includes(project)) project = '';
	}

	// the draft of this key, else the issue's prompt
	$effect(() => {
		if (!starter.open) return;
		const key = draftKey;
		const forIssue = issue;
		untrack(() => {
			text = initialDraft(key) || (forIssue ? issuePrompt(forIssue.slug, forIssue.title) : '');
		});
	});

	$effect(() => {
		if (starter.open) noteDraft(draftKey, text);
	});

	// what the hub says (another device, an editor) lands unless typed over
	$effect(() => {
		if (!starter.open) return;
		const adopted = remoteDraft(draftKey, draft);
		if (adopted !== null) text = adopted;
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
		// NOTE: `launch` derives from `pending`: read it before clearing that
		const current = launch;
		if (!current) return;
		if (current.state === 'started') {
			pending = null;
			text = '';
			clearDraft(draftKey);
			starter.open = false;
			goto(`/sessions/${current.session}`);
		} else if (current.state === 'failed') {
			pending = null;
			error = current.error;
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
				effort: effort || undefined,
				resume: resume?.id,
				issue: issue?.slug
			});
			if (!resume) prefs.setStart({ host, model, mode, effort });
			field?.remember(body);
			// NOTE: the stream may have moved the launch on before this answer came
			if (!live.launches.has(launch.id)) live.launches.set(launch.id, launch);
			pending = launch.id;
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	async function edit() {
		if (!editHost) return;
		error = '';
		try {
			const sent = text;
			const { draft } = await editDraft(draftKey, editHost, sent);
			markSynced(draftKey, sent);
			live.putDraft(draft);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		}
	}

	async function cancel() {
		error = '';
		try {
			live.putDraft((await cancelEdit(draftKey)).draft);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		}
	}

	const choiceClass = 'flex min-w-0 flex-col gap-1 text-xs text-muted-foreground';
</script>

<Dialog.Root bind:open={starter.open}>
	<Dialog.Content
		class="flex h-[min(92dvh,70rem)] w-[min(96vw,80rem)] max-w-none flex-col gap-3 p-4 max-sm:h-dvh max-sm:w-screen max-sm:max-w-none max-sm:rounded-none sm:max-w-none"
		escapeKeydownBehavior={vimEsc && fieldFocused ? 'ignore' : 'close'}
	>
		<Dialog.Header class="gap-1 pr-8">
			<Dialog.Title
				>{resume
					? 'Resume the session'
					: issue
						? 'Work on this issue'
						: 'Start a session'}</Dialog.Title
			>
			<Dialog.Description>
				{#if issue}
					Starts a session on “{issue.title}”. It claims the issue when it reports, so the board
					shows it in progress.
				{:else if resume}
					Resumes “{resume.title}” in the background on {host || resume.host}, in
					{place(resume.cwd)}: its conversation goes on from where it stopped.
				{:else}
					Runs <code class="font-mono">claude --bg</code> on the host at the workspace's root (its harness
					and direnv), placed under the project. Its questions and approvals reach this device; the draft
					stays on the hub until the session starts.
				{/if}
			</Dialog.Description>
		</Dialog.Header>

		{#if !targets}
			<p class="text-sm text-muted-foreground">Loading…</p>
		{:else if !connected.length}
			<p class="text-sm text-muted-foreground">
				No connected machine starts sessions: they start on a workstation whose ccdash collector is
				up, with <code class="font-mono">[launch] command</code> set.
			</p>
		{:else}
			<div class="flex flex-wrap items-end gap-2">
				{#if !resume}
					<div class="{choiceClass} w-36">
						Host
						<SmartSelect label="Host" bind:value={host} options={hostChoices} onchange={pickHost} />
					</div>
					<div class="{choiceClass} w-40">
						Workspace
						<SmartSelect
							label="Workspace"
							bind:value={ws}
							options={wsChoices}
							onchange={() => (project = '')}
						/>
					</div>
					<div class="{choiceClass} min-w-56 flex-1">
						Project
						<SmartSelect
							label="Project"
							bind:value={project}
							options={projectChoices}
							placeholder={names?.length ? 'Pick a project' : 'No project here'}
							searchPlaceholder="Filter projects"
						/>
					</div>
				{:else}
					<div class="{choiceClass} flex-1" role="group" aria-label="Machine">
						Machine
						<div class="flex flex-wrap gap-1.5">
							{#each resumeHosts as h (h)}
								<Button
									variant={host === h ? 'default' : 'outline'}
									size="sm"
									aria-pressed={host === h}
									onclick={() => (host = h)}
									>{h}{#if h === resume.host}<span class="opacity-70">· last ran here</span
										>{/if}</Button
								>
							{/each}
							{#if !connected.includes(resume.host)}
								<Button variant="outline" size="sm" disabled title="Its collector is not connected"
									>{resume.host} <span class="opacity-70">· offline</span></Button
								>
							{/if}
						</div>
					</div>
				{/if}
				<div class="{choiceClass} w-36">
					Model
					<SmartSelect
						label="Model"
						bind:value={model}
						options={modelChoices}
						custom={customModel}
						searchPlaceholder="Opus, 4.8, a full name…"
					/>
				</div>
				<div class="{choiceClass} w-32">
					Effort
					<SmartSelect label="Effort" bind:value={effort} options={effortChoices} />
				</div>
				<div class="{choiceClass} w-36">
					Permission mode
					<SmartSelect label="Permission mode" bind:value={mode} options={modeChoices} />
				</div>
			</div>
			{#if resume}
				{#if !resumeHosts.length}
					<p class="text-xs text-critical">
						No connected machine has the workspace {resume.workspace}.
					</p>
				{:else if host && host !== resume.host}
					<p class="text-xs text-muted-foreground">
						It last ran on {resume.host}; {host} fetches its transcript from the hub first, then resumes
						it as a copy under a new id (the old one stays as it was).
					</p>
				{:else if model || effort || mode}
					<p class="text-xs text-muted-foreground">
						With another model, effort or permission mode Claude Code continues it as a copy under a
						new id; the old one stays as it was.
					</p>
				{/if}
			{/if}
			<div
				class="flex min-h-0 flex-1 flex-col"
				onfocusin={() => (fieldFocused = true)}
				onfocusout={() => (fieldFocused = false)}
			>
				<PromptField
					bind:this={field}
					bind:text
					{prompts}
					historyNote="The workspace's prompts."
					{host}
					workspace={ws || null}
					placeholder={resume ? 'What next?' : 'What should Claude do?'}
					label="Prompt"
					rows={6}
					fill
					disabled={busy || Boolean(pending)}
					editing={draft?.editing ?? null}
					{editHost}
					notice={draft?.notice ?? null}
					onsubmit={start}
					onedit={edit}
					oncanceledit={cancel}
					onsave={() => flushDraft(draftKey)}
				>
					{#snippet actions()}
						<Button type="submit" size="sm" disabled={!ready} title="Ctrl Enter">
							{#if busy || pending}<LoaderCircleIcon class="animate-spin" />{/if}
							{resume ? 'Resume' : 'Start'}
						</Button>
					{/snippet}
				</PromptField>
			</div>
			{#if pending}
				<p class="text-xs text-muted-foreground" role="status">
					Starting on {host}; the session's page opens when it reports.
				</p>
			{/if}
		{/if}
		{#if error}<p class="text-xs text-critical" role="alert">{error}</p>{/if}
	</Dialog.Content>
</Dialog.Root>
