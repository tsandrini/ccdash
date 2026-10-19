// The Projects data, loaded per workspace scope and reloaded when notes
// change or a live session's claim moves (the board derives from both).

import { Unauthorized } from './api/client';
import { fetchProjects } from './api/index';
import type { Projects, WorkspaceProjects } from './api/types';
import { live } from './live/store.svelte';

class ProjectsStore {
	data = $state<Projects | null>(null);
	error = $state('');
	loading = $state(false);
	#key = '';
	#timer: ReturnType<typeof setTimeout> | null = null;

	/** What the board derives its live part from. */
	get claimsSignature(): string {
		return live.list
			.filter((s) => s.activity !== 'gone' && s.claim)
			.map((s) => `${s.id}:${s.claim?.issue ?? ''}`)
			.sort()
			.join('|');
	}

	workspace(name: string): WorkspaceProjects | undefined {
		return this.data?.workspaces.find((w) => w.name === name);
	}

	/** Loads (again) after a short pause; `workspace` as for the API. */
	schedule(workspace?: string) {
		if (this.#timer) clearTimeout(this.#timer);
		this.#timer = setTimeout(() => this.load(workspace), 300);
	}

	async load(workspace?: string) {
		const key = workspace ?? '';
		if (key !== this.#key) {
			this.#key = key;
			this.data = null;
		}
		this.loading = true;
		this.error = '';
		try {
			const data = await fetchProjects({ workspace });
			if (this.#key === key) this.data = data;
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else this.error = (e as Error).message;
		} finally {
			this.loading = false;
		}
	}
}

export const projects = new ProjectsStore();
