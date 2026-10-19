<script lang="ts">
	// A prompt: the textarea and its toolbar, shared by the session
	// composer and the start sheet. Enter submits on the desktop
	// (Shift+Enter breaks); on touch Enter is a newline and the buttons
	// submit. Up and down walk the prompt history; the clock button opens
	// it with search. Templates, `[[` for a note link, a code fence around
	// the selection, a markdown preview.
	import type { Snippet } from 'svelte';
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import CodeIcon from '@lucide/svelte/icons/code';
	import EyeIcon from '@lucide/svelte/icons/eye';
	import HistoryIcon from '@lucide/svelte/icons/history';
	import LinkIcon from '@lucide/svelte/icons/link';
	import { marked } from 'marked';
	import { Unauthorized } from '#lib/api/client.js';
	import type { Prompt } from '#lib/api/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js';
	import * as Sheet from '#lib/components/ui/sheet/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { fmtStamp } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { palette } from '#lib/palette.svelte.js';
	import { prefs } from '#lib/prefs.svelte.js';

	let {
		text = $bindable(''),
		prompts,
		historyNote,
		session,
		placeholder,
		label = 'Prompt',
		disabled = false,
		rows = 1,
		onsubmit,
		actions
	}: {
		text?: string;
		/** The history, loaded on first use (duplicates are dropped). */
		prompts: () => Promise<Prompt[]>;
		historyNote: string;
		/** The session the field writes to: other sessions' prompts are marked. */
		session?: string;
		placeholder: string;
		label?: string;
		disabled?: boolean;
		rows?: number;
		onsubmit: () => void;
		/** Buttons at the end of the toolbar. */
		actions: Snippet;
	} = $props();

	let area = $state<HTMLTextAreaElement | null>(null);
	let preview = $state(false);
	let historyOpen = $state(false);
	let historyQuery = $state('');
	let history = $state<Prompt[]>([]);
	let historyLoaded = false;
	/** Where up/down is in `history`; -1 is the field's own text. */
	let cursor = $state(-1);
	let stash = '';

	const touch = () => matchMedia('(pointer: coarse)').matches;
	const filtered = $derived(
		history.filter(
			(p) => !historyQuery.trim() || p.text.toLowerCase().includes(historyQuery.toLowerCase())
		)
	);
	const rendered = $derived(preview ? (marked.parse(text, { async: false }) as string) : '');

	/** After a send: the text heads the history, the field shrinks back. */
	export function remember(body: string) {
		cursor = -1;
		history = history.filter((p) => p.text !== body);
		history.unshift({ at: live.now, session: session ?? '', text: body });
		preview = false;
		requestAnimationFrame(grow);
	}

	export function focus() {
		area?.focus();
	}

	function grow() {
		if (!area) return;
		area.style.height = 'auto';
		area.style.height = `${Math.min(area.scrollHeight, 220)}px`;
	}

	async function loadHistory() {
		if (historyLoaded) return;
		historyLoaded = true;
		try {
			const seen: Record<string, true> = {};
			for (const p of history) seen[p.text] = true;
			for (const p of await prompts()) {
				if (seen[p.text]) continue;
				seen[p.text] = true;
				history.push(p);
			}
		} catch (e) {
			historyLoaded = false;
			if (e instanceof Unauthorized) live.needToken = true;
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
			onsubmit();
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
		onsubmit();
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
		{rows}
		maxlength={20000}
		{placeholder}
		aria-label={label}
		{disabled}
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
		<span class="ml-auto flex gap-1.5">{@render actions()}</span>
	</div>
</form>

<Sheet.Root bind:open={historyOpen}>
	<Sheet.Content
		side={touch() ? 'bottom' : 'right'}
		class="flex max-h-[85vh] flex-col gap-2 p-4 sm:max-w-md"
	>
		<Sheet.Header class="p-0">
			<Sheet.Title>Prompt history</Sheet.Title>
			<Sheet.Description>{historyNote}</Sheet.Description>
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
							>{fmtStamp(p.at)}{session && p.session !== session ? ' · another session' : ''}</span
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
