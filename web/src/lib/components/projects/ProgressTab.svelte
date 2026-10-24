<script lang="ts">
	// The progress feed as a timeline: days newest first, each entry on a
	// rail with its time, title, the rest of its first lines (the whole
	// entry unfolds in place), the issue it is on with that issue's state,
	// the session that logged it and the project.
	import BotIcon from '@lucide/svelte/icons/bot';
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import ChevronUpIcon from '@lucide/svelte/icons/chevron-up';
	import CircleCheckIcon from '@lucide/svelte/icons/circle-check';
	import CircleDotIcon from '@lucide/svelte/icons/circle-dot';
	import FolderIcon from '@lucide/svelte/icons/folder';
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchNote } from '#lib/api/index.js';
	import type { Entry } from '#lib/api/types.js';
	import Prose from '#lib/components/session/Prose.svelte';
	import { fmtDate, fmtDuration, fmtTime, plural } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { dayKey } from '#lib/notes.js';
	import { notePanel } from '#lib/notepanel.svelte.js';

	let { ws, progress, showProject }: { ws: string; progress: Entry[]; showProject: boolean } =
		$props();

	const DAY_MS = 86_400_000;

	type Day = { key: string; day: string; items: Entry[]; issues: number; sessions: number };

	const days = $derived.by(() => {
		const out: Day[] = [];
		for (const e of progress) {
			const key = dayKey(e.logged_at);
			const last = out.at(-1);
			if (last && last.key === key) last.items.push(e);
			else out.push({ key, day: dayName(e.logged_at), items: [e], issues: 0, sessions: 0 });
		}
		for (const d of out) {
			d.issues = new Set(d.items.flatMap((e) => (e.issue ? [e.issue] : []))).size;
			d.sessions = new Set(d.items.flatMap((e) => (e.by ? [e.by.session] : []))).size;
		}
		return out;
	});
	const issues = $derived(new Set(progress.flatMap((e) => (e.issue ? [e.issue] : []))).size);
	const closedIssues = $derived(
		new Set(progress.filter((e) => e.on?.status === 'archived').map((e) => e.issue)).size
	);

	// Whole entries, fetched when unfolded.
	let open = $state<Record<string, boolean>>({});
	let bodies = $state<Record<string, { html: string; heading: string | null }>>({});
	let failed = $state<Record<string, string>>({});

	function dayName(ts: number): string {
		const today = dayKey(live.now);
		if (dayKey(ts) === today) return 'Today';
		if (dayKey(ts + DAY_MS) === today) return 'Yesterday';
		return fmtDate(ts);
	}

	/** The excerpt without the title it starts with. */
	function more(e: Entry): string {
		const head = e.title.replace(/…$/, '');
		const text = e.excerpt.replace(/^[-*\s]+/, '');
		if (!text.startsWith(head)) return text;
		const rest = text.slice(head.length).trim();
		return rest && e.title.endsWith('…') ? `…${rest}` : rest;
	}

	/** The rendered entry without its first block when that is the title
	 * (whole, where `title` is cut short), which comes back as `heading`. */
	function split(html: string, title: string): { html: string; heading: string | null } {
		const doc = new DOMParser().parseFromString(html, 'text/html');
		const first = doc.body.firstElementChild;
		const text = first?.textContent?.trim().replace(/^[-*\s]+/, '') ?? '';
		if (!first || !text.startsWith(title.replace(/…$/, '')) || text.length > 300) {
			return { html, heading: null };
		}
		first.remove();
		return { html: doc.body.innerHTML, heading: text };
	}

	async function toggle(e: Entry) {
		open[e.slug] = !open[e.slug];
		if (!open[e.slug] || bodies[e.slug]) return;
		try {
			const note = await fetchNote(ws, e.slug);
			bodies[e.slug] = split(note.html, e.title);
			delete failed[e.slug];
		} catch (err) {
			if (err instanceof Unauthorized) live.needToken = true;
			else failed[e.slug] = (err as Error).message;
		}
	}

	const ago = (ts: number) =>
		live.now - ts < DAY_MS ? `${fmtDuration(live.now - ts).split(' ')[0]} ago` : null;
</script>

{#if !progress.length}
	<p class="text-sm text-muted-foreground">No progress logged yet.</p>
{:else}
	<p class="mb-3 text-xs text-muted-foreground">
		{plural(progress.length, 'entry', 'entries')} since {fmtDate(progress.at(-1)!.logged_at)}{issues
			? ` · on ${plural(issues, 'issue')}, ${closedIssues} closed now`
			: ''}
	</p>
{/if}
<div class="flex flex-col gap-5">
	{#each days as group (group.key)}
		<section>
			<h3
				class="sticky top-0 z-[1] mb-2 flex items-baseline gap-2 bg-background/90 py-1 text-xs font-semibold text-muted-foreground uppercase backdrop-blur"
			>
				{group.day}
				<span class="font-normal normal-case"
					>{[
						plural(group.items.length, 'entry', 'entries'),
						group.issues ? plural(group.issues, 'issue') : null,
						group.sessions ? plural(group.sessions, 'session') : null
					]
						.filter(Boolean)
						.join(' · ')}</span
				>
			</h3>
			<ol class="relative ml-2 border-l border-border">
				{#each group.items as e (e.slug)}
					{@const closed = e.on?.status === 'archived'}
					{@const rest = more(e)}
					<li class="relative pb-4 pl-5 last:pb-1">
						<span
							class="absolute top-0.5 -left-[9px] flex size-[17px] items-center justify-center rounded-full border bg-background {closed
								? 'text-your-turn'
								: 'text-kind-progress'}"
							aria-hidden="true"
						>
							{#if closed}<CircleCheckIcon class="size-3" />{:else}<span
									class="size-2 rounded-full bg-current"
								></span>{/if}
						</span>
						<div class="flex min-w-0 flex-col gap-1 text-sm">
							<span class="flex flex-wrap items-baseline gap-x-2 text-xs text-muted-foreground">
								<span class="font-mono tabular-nums">{fmtTime(e.logged_at)}</span>
								{#if ago(e.logged_at)}<span>{ago(e.logged_at)}</span>{/if}
								{#if e.updated_at - e.logged_at > 60_000}<span>· edited</span>{/if}
							</span>
							<button
								type="button"
								class="text-left font-medium break-words hover:underline"
								onclick={() => notePanel.show(ws, e.slug)}
								>{(open[e.slug] && bodies[e.slug]?.heading) || e.title}</button
							>
							{#if open[e.slug]}
								{#if bodies[e.slug]?.html.trim()}
									<div class="rounded-md border bg-card px-3 py-2">
										<Prose html={bodies[e.slug].html} onnote={(slug) => notePanel.show(ws, slug)} />
									</div>
								{:else if bodies[e.slug]}
									<p class="text-xs text-muted-foreground">The title is all of it.</p>
								{:else if failed[e.slug]}
									<p class="text-xs text-critical" role="alert">{failed[e.slug]}</p>
								{:else}
									<p class="text-xs text-muted-foreground">Loading…</p>
								{/if}
							{:else if rest}
								<p class="line-clamp-2 text-xs text-muted-foreground">{rest}</p>
							{/if}
							<span class="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
								{#if e.on}
									{@const on = e.on}
									<button
										type="button"
										class="inline-flex max-w-full min-w-0 items-center gap-1 hover:underline"
										title={on.title ? `Issue ${on.slug}` : `${on.slug} is gone`}
										onclick={() => notePanel.show(ws, on.slug)}
									>
										{#if closed}<CircleCheckIcon class="size-3.5 shrink-0 text-your-turn" />
										{:else}<CircleDotIcon class="size-3.5 shrink-0 text-kind-issue" />{/if}
										<span class="truncate">{on.title ?? on.slug}</span>
										<span class="text-muted-foreground">{closed ? 'closed' : 'open'}</span>
									</button>
								{/if}
								{#if e.by}
									{@const by = e.by}
									<a
										href="/sessions/{by.session}"
										class="inline-flex min-w-0 items-center gap-1 text-muted-foreground hover:underline"
										title="Logged by session {by.session.slice(0, 8)}"
									>
										<BotIcon class="size-3.5 shrink-0" />
										<span class="truncate">{by.label}</span>
										{#if by.host}<span>on {by.host}</span>{/if}
										{#if by.live}<i
												class="inline-block size-1.5 rounded-full bg-working"
												aria-label="still running"
											></i>{/if}
									</a>
								{/if}
								{#if showProject && e.project}
									<span class="inline-flex items-center gap-1 text-muted-foreground"
										><FolderIcon class="size-3.5" />{e.project}</span
									>
								{/if}
								{#if rest || e.excerpt.endsWith('…')}<button
										type="button"
										class="ml-auto inline-flex items-center gap-0.5 text-muted-foreground hover:text-foreground"
										aria-expanded={Boolean(open[e.slug])}
										onclick={() => toggle(e)}
									>
										{#if open[e.slug]}Less <ChevronUpIcon class="size-3.5" />{:else}All of it
											<ChevronDownIcon class="size-3.5" />{/if}
									</button>{/if}
							</span>
						</div>
					</li>
				{/each}
			</ol>
		</section>
	{/each}
</div>
