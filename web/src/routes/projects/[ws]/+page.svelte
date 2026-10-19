<script lang="ts">
	// The workspace as a whole, with the project tabs.
	import { page } from '$app/state';
	import ProjectPage from '#lib/components/projects/ProjectPage.svelte';
</script>

<ProjectPage ws={page.params.ws!} project={null} />
