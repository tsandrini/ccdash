<script lang="ts">
	// One number with its label and a line of context.
	let { name, value, sub }: { name: string; value: string | number; sub?: string | null } =
		$props();
</script>

<div class="flex min-w-0 flex-col gap-0.5 rounded-lg border bg-card p-3">
	<span class="text-xs text-muted-foreground">{name}</span>
	<span class="truncate text-lg font-semibold">{value}</span>
	{#if sub}<span class="text-xs text-muted-foreground">{sub}</span>{/if}
</div>
