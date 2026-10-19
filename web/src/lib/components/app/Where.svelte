<script lang="ts">
	// Where a session runs, on one line: host (when there are several),
	// workspace, project directory, git branch, model, background id.
	import GitBranchIcon from '@lucide/svelte/icons/git-branch';
	import type { Session } from '#lib/api/types.js';
	import { place } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { isAgent } from '#lib/session.js';

	let {
		session: s,
		workspace = false,
		class: className = ''
	}: { session: Session; workspace?: boolean; class?: string } = $props();

	const before = $derived(
		[live.multiHost ? s.host : null, workspace ? s.workspace : null, place(s.cwd)]
			.filter(Boolean)
			.join(' · ')
	);
	const after = $derived(
		[s.model, isAgent(s) && s.short_id ? `bg ${s.short_id}` : null].filter(Boolean).join(' · ')
	);
</script>

<span
	class="flex min-w-0 items-center gap-1 text-xs text-muted-foreground {className}"
	title={s.cwd}
>
	<span class="truncate">{before}</span>
	{#if s.branch}
		<span class="inline-flex max-w-[45%] min-w-0 items-center gap-0.5" title="Branch {s.branch}">
			<GitBranchIcon class="size-3 shrink-0" aria-label="branch" />
			<span class="truncate">{s.branch}</span>
		</span>
	{/if}
	{#if after}<span class="truncate">· {after}</span>{/if}
</span>
