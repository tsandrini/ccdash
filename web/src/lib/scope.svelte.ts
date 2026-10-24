// Which workspaces and host the views show: a choice of this device, kept
// across pages and reloads like the theme. Not in the URL: every link
// without it (a card, a notification, the installed app's start page)
// would reset it.

import { browser } from '$app/env';
import type { Session } from './api/types';
import { live } from './live/store.svelte';

export type Scope = { ws: string[]; host: string };

const SCOPE_KEY = 'ccdash.scope';

/** The stored scope; missing or malformed is every workspace and host. */
function load(): Scope {
	const all: Scope = { ws: [], host: '' };
	if (!browser) return all;
	try {
		const raw = JSON.parse(localStorage.getItem(SCOPE_KEY) ?? 'null');
		return {
			ws: Array.isArray(raw?.ws)
				? raw.ws.filter((n: unknown): n is string => typeof n === 'string' && n !== '')
				: [],
			host: typeof raw?.host === 'string' ? raw.host : ''
		};
	} catch {
		return all;
	}
}

class ScopeState {
	#stored = $state<Scope>(load());

	constructor() {
		// NOTE: else another tab's next change would overwrite this one's
		if (browser)
			addEventListener('storage', (e) => {
				if (e.key === SCOPE_KEY) this.#stored = load();
			});
	}

	/** The stored scope without workspaces the hub no longer has (a renamed
	 * or removed directory; the API refuses unknown ones). */
	get current(): Scope {
		const { ws, host } = this.#stored;
		return { ws: live.ready ? ws.filter((n) => live.workspaces.includes(n)) : ws, host };
	}

	/** The session is within the picked workspaces and host. */
	covers(s: Session): boolean {
		const { ws, host } = this.current;
		return (!ws.length || ws.includes(s.workspace ?? '')) && (!host || s.host === host);
	}

	/** `workspace=` and `host=` for API calls. */
	params(): { workspace?: string; host?: string } {
		const { ws, host } = this.current;
		return { workspace: ws.length ? ws.join(',') : undefined, host: host || undefined };
	}

	set(changes: Partial<Scope>) {
		this.#stored = { ...this.current, ...changes };
		localStorage.setItem(SCOPE_KEY, JSON.stringify(this.#stored));
	}
}

export const scope = new ScopeState();
