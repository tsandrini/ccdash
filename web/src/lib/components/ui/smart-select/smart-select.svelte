<script lang="ts">
	// A select whose rows say more than a name (a hint, a figure, a group)
	// and that narrows as you type: a button that opens a list under it.
	// Short lists show no search row until a key is typed; `custom` lets a
	// typed value that is not listed be picked.
	import ChevronsUpDownIcon from '@lucide/svelte/icons/chevrons-up-down';
	import * as Command from '#lib/components/ui/command/index.js';
	import * as Popover from '#lib/components/ui/popover/index.js';
	import { cn } from '#lib/utils.js';
	import { matches, type Choice } from './choice.js';

	/** Lists longer than this show the search row at once. */
	const SEARCH_FROM = 6;

	let {
		value = $bindable(''),
		options,
		label,
		placeholder = 'Pick one',
		searchable = 'auto',
		searchPlaceholder = 'Type to filter',
		empty = 'Nothing matches.',
		custom,
		disabled = false,
		class: className,
		onchange
	}: {
		/** The picked choice's value. */
		value?: string;
		options: Choice[];
		/** The accessible name, when no `<label>` wraps the select. */
		label?: string;
		/** The trigger's text while nothing is picked. */
		placeholder?: string;
		/** The search row: always, never (typing still narrows), or by length. */
		searchable?: boolean | 'auto';
		searchPlaceholder?: string;
		/** The list's text when the search leaves no row. */
		empty?: string;
		/** A row for a typed value that is not listed; null for none. */
		custom?: (query: string) => Choice | null;
		disabled?: boolean;
		/** Classes of the trigger. */
		class?: string;
		/** Called after a pick changed the value. */
		onchange?: (value: string) => void;
	} = $props();

	let open = $state(false);
	let query = $state('');
	/** The highlighted row. */
	let active = $state('');

	const picked = $derived(options.find((o) => o.value === value));
	const shown = $derived(options.filter((o) => matches(o, query)));
	const typed = $derived.by(() => {
		const q = query.trim();
		const known = (text: string) => text.toLowerCase() === q.toLowerCase();
		if (!custom || !q || options.some((o) => known(o.value) || known(o.label))) return null;
		return custom(q);
	});
	const groups = $derived.by(() => {
		const out: { heading: string | undefined; rows: Choice[] }[] = [];
		for (const o of shown) {
			const last = out.at(-1);
			if (last && last.heading === o.group) last.rows.push(o);
			else out.push({ heading: o.group, rows: [o] });
		}
		return out;
	});
	const searchRow = $derived(
		searchable === true ||
			query !== '' ||
			(searchable === 'auto' && (options.length > SEARCH_FROM || Boolean(custom)))
	);

	function opened(now: boolean) {
		if (!now) return;
		query = '';
		active = value;
	}

	// the first row that is left, as the list narrows
	$effect(() => {
		if (!open || !query) return;
		active = shown.find((o) => !o.disabled)?.value ?? typed?.value ?? '';
	});

	// NOTE: focusing the search row on a touch screen raises the keyboard over the list
	function keepKeyboardDown(e: Event) {
		if (matchMedia('(pointer: coarse)').matches) e.preventDefault();
	}

	function pick(next: string) {
		open = false;
		if (next === value) return;
		value = next;
		onchange?.(next);
	}
</script>

{#snippet row(o: Choice)}
	<Command.Item
		value={o.value}
		disabled={o.disabled}
		data-checked={o.value === value}
		title={o.hint}
		onSelect={() => pick(o.value)}
		class="gap-3"
	>
		<span class="shrink-0 font-medium">{o.label}</span>
		{#if o.hint}
			<span class="min-w-0 flex-1 truncate text-xs text-muted-foreground">{o.hint}</span>
		{:else}
			<span class="flex-1"></span>
		{/if}
		{#if o.meta}
			<span class="shrink-0 text-xs text-muted-foreground tabular-nums">{o.meta}</span>
		{/if}
	</Command.Item>
{/snippet}

<Popover.Root bind:open onOpenChange={opened}>
	<Popover.Trigger
		{disabled}
		role="combobox"
		aria-expanded={open}
		aria-label={label}
		data-slot="smart-select-trigger"
		class={cn(
			'flex h-8 w-full min-w-0 items-center justify-between gap-1 rounded-lg border border-input bg-transparent px-2 text-left text-sm text-foreground outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-input/30',
			className
		)}
	>
		<span class={cn('truncate', !picked && !value && 'text-muted-foreground')}>
			{picked?.label ?? (value || placeholder)}
		</span>
		<ChevronsUpDownIcon class="size-3.5 shrink-0 opacity-50" />
	</Popover.Trigger>
	<Popover.Content
		align="start"
		collisionPadding={8}
		onOpenAutoFocus={keepKeyboardDown}
		class="w-max max-w-[min(34rem,calc(100vw-1rem))] min-w-(--bits-popover-anchor-width) gap-0 p-0"
	>
		<Command.Root shouldFilter={false} bind:value={active} class="p-0">
			<!-- NOTE: kept in the tree when hidden: it holds the focus the arrow keys need -->
			<div class={searchRow ? '' : 'sr-only'}>
				<Command.Input bind:value={query} placeholder={searchPlaceholder} aria-label={label} />
			</div>
			<Command.List class="max-h-[min(22rem,50dvh)] p-1">
				{#each groups as g (g.heading ?? '')}
					{#if g.heading}
						<Command.Group heading={g.heading} class="p-0">
							{#each g.rows as o (o.value)}{@render row(o)}{/each}
						</Command.Group>
					{:else}
						{#each g.rows as o (o.value)}{@render row(o)}{/each}
					{/if}
				{/each}
				{#if typed}{@render row(typed)}{/if}
				{#if !shown.length && !typed}
					<p class="px-2 py-4 text-center text-xs text-muted-foreground">{empty}</p>
				{/if}
			</Command.List>
		</Command.Root>
	</Popover.Content>
</Popover.Root>
