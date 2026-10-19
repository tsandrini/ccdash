// The command palette's state: opened from the topbar, Ctrl/⌘ K, or the
// composer's `[[` (scoped to notes, handing the picked note back).

import type { Hit } from './api/types';

class Palette {
	open = $state(false);
	/** Notes only, with a callback for the picked one. */
	notesOnly = $state(false);
	query = $state('');
	onNote: ((hit: Hit) => void) | null = null;

	show(options: { notes?: boolean; onNote?: (hit: Hit) => void; query?: string } = {}) {
		this.notesOnly = Boolean(options.notes);
		this.onNote = options.onNote ?? null;
		this.query = options.query ?? '';
		this.open = true;
	}

	close() {
		this.open = false;
		this.notesOnly = false;
		this.onNote = null;
	}
}

export const palette = new Palette();
