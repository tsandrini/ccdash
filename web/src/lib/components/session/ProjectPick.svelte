<script lang="ts">
	// Pins a running session's project (its notes' default, and whose other
	// sessions it hears about), or lets its directory decide again. The
	// choices are the workspace's projects on the session's host (as the
	// start sheet offers them) and the one it has now.
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchTargets, setSessionProject, type Session } from '#lib/api/index.js';
	import { live } from '#lib/live/store.svelte.js';

	let { session: s }: { session: Session } = $props();

	/** The select's value for "the directory decides". */
	const DIR = '';
	let names = $state<string[] | null>(null);
	let busy = $state(false);
	let error = $state('');

	const value = $derived(s.project_pinned ? (s.project ?? DIR) : DIR);
	const options = $derived(
		[...new Set([...(names ?? []), ...(s.project_pinned && s.project ? [s.project] : [])])].sort()
	);
	const fromDir = $derived(
		s.project && !s.project_pinned ? `${s.project} (from the directory)` : 'none (the workspace)'
	);

	$effect(() => {
		const { host, workspace } = s;
		if (!workspace) return;
		fetchTargets()
			.then(({ hosts }) => {
				names =
					hosts.find((t) => t.host === host)?.workspaces.find((w) => w.name === workspace)
						?.projects ?? [];
			})
			.catch((e) => {
				if (e instanceof Unauthorized) live.needToken = true;
				else error = (e as Error).message;
			});
	});

	async function pick(project: string) {
		busy = true;
		error = '';
		try {
			live.put((await setSessionProject(s.id, project || null)).session);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		} finally {
			busy = false;
		}
	}
</script>

<div class="flex flex-col gap-1">
	<select
		class="h-8 w-full max-w-xs min-w-0 rounded-lg border border-input bg-transparent px-2 text-sm text-foreground dark:bg-input/30"
		aria-label="Project"
		{value}
		disabled={busy || names === null}
		onchange={(e) => pick(e.currentTarget.value)}
	>
		<option value={DIR}>{fromDir}</option>
		{#each options as p (p)}<option value={p}>{p}</option>{/each}
	</select>
	<p class="text-xs text-muted-foreground">
		Notes default to it; the project's other sessions are the ones this one hears about. A session
		at the workspace root sets it with <code class="font-mono">claim</code>.
	</p>
	{#if error}<p class="text-xs text-critical" role="alert">{error}</p>{/if}
</div>
