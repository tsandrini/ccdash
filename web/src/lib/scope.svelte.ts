// What scopes a view lives in the URL query (`ws`, `host`, `range`), so
// links are shareable and the back button works.

import { goto } from '$app/navigation';
import { page } from '$app/state';
import type { Session } from './api/types';

export type Scope = { ws: string[]; host: string; range: string };

/** The page's URL as a plain one (`page.url` is read-only). */
const current = () => new URL(page.url.href);

function parse(url: URL): Scope {
	return {
		ws: (url.searchParams.get('ws') ?? '').split(',').filter(Boolean),
		host: url.searchParams.get('host') ?? '',
		range: url.searchParams.get('range') ?? ''
	};
}

export const scope = {
	get current(): Scope {
		return parse(current());
	},
	/** The session is within the picked workspaces and host. */
	covers(s: Session): boolean {
		const { ws, host } = parse(current());
		return (!ws.length || ws.includes(s.workspace ?? '')) && (!host || s.host === host);
	},
	/** `workspace=` and `host=` for API calls. */
	params(): { workspace?: string; host?: string } {
		const { ws, host } = parse(current());
		return { workspace: ws.length ? ws.join(',') : undefined, host: host || undefined };
	},
	set(changes: Partial<Scope>) {
		const url = current();
		const next = { ...parse(url), ...changes };
		const put = (key: string, value: string) => {
			if (value) url.searchParams.set(key, value);
			else url.searchParams.delete(key);
		};
		put('ws', next.ws.join(','));
		put('host', next.host);
		put('range', next.range);
		goto(`${url.pathname}${url.search}`, { replace: true });
	}
};
