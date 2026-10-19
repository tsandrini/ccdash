// The range the page shows, from the URL (`range`), else the page's own
// default; resolved against the hub's clock.

import { page } from '$app/state';
import { live } from './live/store.svelte';
import { decodeRange, encodeRange, rangeParams, rangeSetup, type Range } from './range';
import { scope } from './scope.svelte';

export const rangeState = {
	/** Whether the page has a range at all. */
	get applies(): boolean {
		return rangeSetup(page.url.pathname) !== null;
	},
	get current(): Range {
		const setup = rangeSetup(page.url.pathname);
		return decodeRange(scope.current.range) ?? setup?.fallback ?? { kind: 'days', days: 7 };
	},
	get quick(): Range[] {
		return rangeSetup(page.url.pathname)?.quick ?? [];
	},
	/** Hub time. */
	now(): number {
		return Date.now() + live.skew;
	},
	set(range: Range) {
		scope.set({ range: encodeRange(range) });
	},
	/** `since` and `until` for the API, for the range as of now. */
	params() {
		return rangeParams(this.current, this.now());
	}
};
