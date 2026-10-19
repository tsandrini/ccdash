<script lang="ts">
	// A line of explanation under a section's charts.
	let { text }: { text: string } = $props();
</script>

<p class="text-xs text-muted-foreground">{text}</p>
