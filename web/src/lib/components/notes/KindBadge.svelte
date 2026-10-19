<script lang="ts">
	// A note's kind as a coloured dot with its name; the colour carries the
	// identity, the text stays in text ink.
	import type { NoteKind } from '#lib/api/types.js';
	import { KIND_COLOR, KIND_LABEL } from '#lib/notes.js';
	import { cn } from '#lib/utils.js';

	let { kind, class: className }: { kind: NoteKind; class?: string } = $props();
</script>

<span
	class={cn(
		'inline-flex items-center gap-1.5 text-xs whitespace-nowrap text-muted-foreground',
		className
	)}
>
	<i
		class="inline-block size-2 shrink-0 rounded-full"
		style="background: {KIND_COLOR[kind]}"
		aria-hidden="true"
	></i>
	{KIND_LABEL[kind]}
</span>
