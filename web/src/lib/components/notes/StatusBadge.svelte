<script lang="ts">
	// A non-active status (stale, closed, superseded, archived) as a chip.
	import type { NoteKind, Status } from '#lib/api/types.js';
	import { statusLabel } from '#lib/notes.js';
	import { cn } from '#lib/utils.js';

	let { note, class: className }: { note: { kind: NoteKind; status: Status }; class?: string } =
		$props();
</script>

{#if note.status !== 'active'}
	<span
		class={cn(
			'rounded-full border px-1.5 text-[10px] whitespace-nowrap uppercase',
			note.status === 'stale' ? 'border-warning/60 text-warning' : 'text-muted-foreground',
			className
		)}>{statusLabel(note)}</span
	>
{/if}
