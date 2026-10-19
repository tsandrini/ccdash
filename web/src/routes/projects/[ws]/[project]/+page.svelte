<script lang="ts">
	// One project of a workspace, with its tabs.
	import { page } from '$app/state';
	import ProjectPage from '#lib/components/projects/ProjectPage.svelte';
</script>

<ProjectPage ws={page.params.ws!} project={page.params.project!} />
