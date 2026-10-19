<script lang="ts">
	import type { Task } from '#lib/api/types.js';

	let { tasks }: { tasks: Task[] } = $props();
	const icon = { completed: '✓', in_progress: '◐', pending: '○' };
</script>

<ul class="mt-1 flex flex-col gap-0.5 text-xs">
	{#each tasks as t (t.id)}
		<li
			class="flex gap-2 {t.status === 'completed' ? 'text-muted-foreground line-through' : ''}"
			data-status={t.status}
		>
			<span aria-hidden="true">{icon[t.status]}</span>
			<span>{t.status === 'in_progress' && t.active_form ? t.active_form : t.subject}</span>
			<span class="sr-only">({t.status.replace('_', ' ')})</span>
		</li>
	{/each}
</ul>
