// The start sheet's state: opened from Now (the scope's workspace), a
// project page (its project) or an ended session's page (continuing it).

import type { Session } from './api/types';

export type StartPreset = {
	ws?: string | null;
	/** Null is the workspace's own directory. */
	project?: string | null;
	/** The ended session to continue: its host, workspace and project. */
	resume?: Session;
};

class Starter {
	open = $state(false);
	preset = $state<StartPreset>({});

	show(preset: StartPreset = {}) {
		this.preset = preset;
		this.open = true;
	}
}

export const starter = new Starter();
