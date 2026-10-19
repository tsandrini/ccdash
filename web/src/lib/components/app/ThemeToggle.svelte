<script lang="ts">
	import MonitorIcon from '@lucide/svelte/icons/monitor';
	import MoonIcon from '@lucide/svelte/icons/moon';
	import SunIcon from '@lucide/svelte/icons/sun';
	import { Button } from '#lib/components/ui/button/index.js';
	import { prefs } from '#lib/prefs.svelte.js';
</script>

<Button
	variant="ghost"
	size="icon"
	aria-label="Theme: {prefs.theme}"
	title="Theme: {prefs.theme}"
	onclick={() => prefs.cycleTheme()}
>
	{#if prefs.theme === 'light'}
		<SunIcon />
	{:else if prefs.theme === 'dark'}
		<MoonIcon />
	{:else}
		<MonitorIcon />
	{/if}
</Button>
