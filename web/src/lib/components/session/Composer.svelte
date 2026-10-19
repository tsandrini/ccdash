<script lang="ts">
	// Text for the session: Send starts a turn through the channel, Next
	// prompt leaves it as context for the next prompt (also for sessions
	// without the channel). Enter sends on the desktop (Shift+Enter breaks);
	// on touch Enter is a newline and the button sends. Up and down walk
	// the prompt history (this session's, then the workspace's); the clock
	// button opens it with search. Quick replies, templates, `[[` for a note
	// link, a code fence around the selection, a markdown preview; the
	// draft survives reloads.
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import CodeIcon from '@lucide/svelte/icons/code';
	import EyeIcon from '@lucide/svelte/icons/eye';
	import HistoryIcon from '@lucide/svelte/icons/history';
	import LinkIcon from '@lucide/svelte/icons/link';
	import { marked } from 'marked';
	import { Unauthorized } from '#lib/api/client.js';
	import { fetchPrompts, messageTo, replyTo, type Prompt, type Session } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js';
	import * as Sheet from '#lib/components/ui/sheet/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { fmtStamp } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { palette } from '#lib/palette.svelte.js';
	import { prefs } from '#lib/prefs.svelte.js';

	let { session: s, onsent }: { session: Session; onsent?: (kind: 'reply' | 'message') => void } =
		$props();

	// svelte-ignore state_referenced_locally
	let text = $state(prefs.draft(s.id));
	let area = $state<HTMLTextAreaElement | null>(null);
	let busy = $state(false);
	let error = $state('');
	let preview = $state(false);
	let historyOpen = $state(false);
	let historyQuery = $state('');
	let history = $state<Prompt[]>([]);
	let historyLoaded = false;
	/** Where up/down is in `history`; -1 is the field's own text. */
	let cursor = $state(-1);
	let stash = '';

	const touch = () => matchMedia('(pointer: coarse)').matches;
	const canReply = $derived(s.channel && s.activity !== 'gone');
	const filtered = $derived(
		history.filter(
			(p) => !historyQuery.trim() || p.text.toLowerCase().includes(historyQuery.toLowerCase())
		)
	);
	const rendered = $derived(preview ? (marked.parse(text, { async: false }) as string) : '');

	$effect(() => {
		prefs.setDraft(s.id, text);
	});

	function grow() {
		if (!area) return;
		area.style.height = 'auto';
		area.style.height = `${Math.min(area.scrollHeight, 220)}px`;
	}

	async function loadHistory() {
		if (historyLoaded) return;
		historyLoaded = true;
		try {
			const own = (await fetchPrompts({ session: s.id, limit: 50 })).prompts;
			const ws = s.workspace
				? (await fetchPrompts({ workspace: s.workspace, limit: 50 })).prompts
				: [];
			const seen: Record<string, true> = {};
			for (const p of history) seen[p.text] = true;
			for (const p of [...own, ...ws]) {
				if (seen[p.text]) continue;
				seen[p.text] = true;
				history.push(p);
			}
		} catch (e) {
			historyLoaded = false;
			if (e instanceof Unauthorized) live.needToken = true;
		}
	}

	async function submit(kind: 'reply' | 'message') {
		const body = text.trim();
		if (!body || busy) return;
		busy = true;
		error = '';
		try {
			if (kind === 'reply') live.put((await replyTo(s.id, body)).session);
			else await messageTo(s.id, body);
			text = '';
			cursor = -1;
			history = history.filter((p) => p.text !== body);
			history.unshift({ at: live.now, session: s.id, text: body });
			preview = false;
			requestAnimationFrame(grow);
			onsent?.(kind);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else error = (e as Error).message;
		} finally {
			busy = false;
		}
	}

	function insert(snippet: string, select?: [number, number]) {
		if (!area) return;
		const start = area.selectionStart;
		const end = area.selectionEnd;
		text = text.slice(0, start) + snippet + text.slice(end);
		requestAnimationFrame(() => {
			area?.focus();
			const [a, b] = select ?? [start + snippet.length, start + snippet.length];
			area?.setSelectionRange(a, b);
			grow();
		});
	}

	function fence() {
		if (!area) return;
		const start = area.selectionStart;
		const end = area.selectionEnd;
		const selected = text.slice(start, end);
		const block = `\n\`\`\`\n${selected}\n\`\`\`\n`;
		text = text.slice(0, start) + block + text.slice(end);
		const at = start + 5;
		requestAnimationFrame(() => {
			area?.focus();
			area?.setSelectionRange(at, at + selected.length);
			grow();
		});
	}

	function noteLink() {
		palette.show({
			notes: true,
			onNote: (hit) => insert(`[[${hit.slug}]] `)
		});
	}

	function walk(direction: 1 | -1) {
		if (!history.length) return;
		if (cursor === -1) stash = text;
		const next = Math.min(history.length - 1, Math.max(-1, cursor + direction));
		cursor = next;
		text = next === -1 ? stash : history[next].text;
		requestAnimationFrame(() => {
			grow();
			area?.setSelectionRange(text.length, text.length);
		});
	}

	function onkeydown(event: KeyboardEvent) {
		if (event.isComposing) return;
		if (event.key === 'Enter' && !event.shiftKey) {
			if (touch() && !event.ctrlKey && !event.metaKey) return;
			event.preventDefault();
			submit(canReply ? 'reply' : 'message');
			return;
		}
		if ((event.key === 'ArrowUp' || event.key === 'ArrowDown') && area) {
			const before = text.slice(0, area.selectionStart);
			const after = text.slice(area.selectionEnd);
			const edge = event.key === 'ArrowUp' ? !before.includes('\n') : !after.includes('\n');
			if (!edge || (cursor === -1 && event.key === 'ArrowDown')) return;
			event.preventDefault();
			loadHistory().then(() => walk(event.key === 'ArrowUp' ? 1 : -1));
			return;
		}
		if (event.key === '[' && area) {
			const at = area.selectionStart;
			if (text[at - 1] === '[' && text[at - 2] !== '[') {
				event.preventDefault();
				text = text.slice(0, at - 1) + text.slice(area.selectionEnd);
				noteLink();
			}
		}
	}

	function pickHistory(p: Prompt) {
		text = p.text;
		cursor = -1;
		historyOpen = false;
		requestAnimationFrame(() => {
			grow();
			area?.focus();
		});
	}
</script>

<div class="flex flex-col gap-1.5">
	{#if prefs.quick.length}
		<div class="flex gap-1.5 overflow-x-auto pb-0.5">
			{#each prefs.quick as q (q)}
				<Button
					variant="outline"
					size="xs"
					class="shrink-0"
					disabled={busy}
					onclick={() => {
						text = q;
						submit(canReply ? 'reply' : 'message');
					}}>{q}</Button
				>
			{/each}
		</div>
	{/if}
	{#if preview}
		<div class="max-h-56 overflow-auto rounded-md border bg-card p-3 text-sm">
			<div class="prose-chat">
				<!-- eslint-disable-next-line svelte/no-at-html-tags -->
				{@html rendered}
			</div>
		</div>
	{/if}
	<form
		class="flex flex-col gap-1.5"
		onsubmit={(e) => {
			e.preventDefault();
			submit(canReply ? 'reply' : 'message');
		}}
	>
		<textarea
			bind:this={area}
			bind:value={text}
			oninput={() => {
				cursor = -1;
				grow();
			}}
			{onkeydown}
			rows={1}
			maxlength={20000}
			placeholder={canReply ? 'Message Claude…' : 'Note for the next prompt…'}
			aria-label="Message to Claude"
			disabled={busy}
			class="min-h-9 w-full resize-none rounded-md border bg-background px-3 py-2 text-sm outline-none focus-visible:ring-2 focus-visible:ring-ring/50 {preview
				? 'hidden'
				: ''}"></textarea>
		<div class="flex flex-wrap items-center gap-1">
			<Button
				variant="ghost"
				size="icon-sm"
				type="button"
				title="Insert a note link ([[)"
				aria-label="Insert a note link"
				onclick={noteLink}
			>
				<LinkIcon />
			</Button>
			<Button
				variant="ghost"
				size="icon-sm"
				type="button"
				title="Code fence around the selection"
				aria-label="Code fence"
				onclick={fence}
			>
				<CodeIcon />
			</Button>
			<Button
				variant={preview ? 'secondary' : 'ghost'}
				size="icon-sm"
				type="button"
				title="Preview"
				aria-label="Preview"
				aria-pressed={preview}
				onclick={() => (preview = !preview)}
			>
				<EyeIcon />
			</Button>
			<Button
				variant="ghost"
				size="icon-sm"
				type="button"
				title="Prompt history"
				aria-label="Prompt history"
				onclick={() => {
					historyOpen = true;
					loadHistory();
				}}
			>
				<HistoryIcon />
			</Button>
			{#if prefs.templates.length}
				<DropdownMenu.Root>
					<DropdownMenu.Trigger>
						{#snippet child({ props })}
							<Button variant="ghost" size="sm" type="button" {...props}>
								Templates <ChevronDownIcon class="size-3" />
							</Button>
						{/snippet}
					</DropdownMenu.Trigger>
					<DropdownMenu.Content align="start">
						{#each prefs.templates as t (t.name)}
							<DropdownMenu.Item onSelect={() => insert(t.text)}>{t.name}</DropdownMenu.Item>
						{/each}
					</DropdownMenu.Content>
				</DropdownMenu.Root>
			{/if}
			<span class="ml-auto flex gap-1.5">
				{#if canReply}
					<Button type="submit" size="sm" disabled={busy || !text.trim()}>Send</Button>
				{/if}
				<Button
					type="button"
					size="sm"
					variant={canReply ? 'outline' : 'default'}
					disabled={busy || !text.trim()}
					title="Hand the text to Claude as context with its next prompt, without starting a turn"
					onclick={() => submit('message')}>Next prompt</Button
				>
			</span>
		</div>
	</form>
	{#if error}<p class="text-xs text-critical" role="alert">{error}</p>{/if}
	{#if !s.channel && s.activity !== 'gone'}
		<p class="text-xs text-muted-foreground">
			To start a turn from here, the session needs the ccdash channel: exit it and run
			<code class="font-mono">claude --resume {s.id}</code>. A note still reaches it with its next
			prompt.
		</p>
	{/if}
</div>

<Sheet.Root bind:open={historyOpen}>
	<Sheet.Content
		side={touch() ? 'bottom' : 'right'}
		class="flex max-h-[85vh] flex-col gap-2 p-4 sm:max-w-md"
	>
		<Sheet.Header class="p-0">
			<Sheet.Title>Prompt history</Sheet.Title>
			<Sheet.Description>This session's prompts, then the workspace's.</Sheet.Description>
		</Sheet.Header>
		<Input bind:value={historyQuery} placeholder="Search…" aria-label="Search prompts" />
		<ul class="min-h-0 flex-1 divide-y overflow-auto text-sm">
			{#each filtered as p (p.at + p.session)}
				<li>
					<button
						type="button"
						class="flex w-full flex-col items-start gap-0.5 px-1 py-2 text-left hover:bg-accent"
						onclick={() => pickHistory(p)}
					>
						<span class="line-clamp-3 whitespace-pre-wrap">{p.text}</span>
						<span class="text-xs text-muted-foreground"
							>{fmtStamp(p.at)}{p.session === s.id ? '' : ' · another session'}</span
						>
					</button>
				</li>
			{:else}
				<li class="py-2 text-muted-foreground">
					{history.length ? 'No match.' : 'No prompts yet.'}
				</li>
			{/each}
		</ul>
	</Sheet.Content>
</Sheet.Root>
