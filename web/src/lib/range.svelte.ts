// The range the page shows, from the URL (`range`, so links and the back
// button keep it), else the page's own default; resolved against the hub's
// clock.

import { goto } from '$app/navigation';
import { page } from '$app/state';
import { live } from './live/store.svelte';
import { decodeRange, encodeRange, rangeParams, rangeSetup, type Range } from './range';

/** The page's URL as a plain one (`page.url` is read-only). */
const pageUrl = () => new URL(page.url.href);

export const rangeState = {
	/** Whether the page has a range at all. */
	get applies(): boolean {
		return rangeSetup(page.url.pathname) !== null;
	},
	get current(): Range {
		const setup = rangeSetup(page.url.pathname);
		const text = page.url.searchParams.get('range') ?? '';
		return decodeRange(text) ?? setup?.fallback ?? { kind: 'days', days: 7 };
	},
	get quick(): Range[] {
		return rangeSetup(page.url.pathname)?.quick ?? [];
	},
	/** Hub time. */
	now(): number {
		return Date.now() + live.skew;
	},
	set(range: Range) {
		const url = pageUrl();
		url.searchParams.set('range', encodeRange(range));
		goto(`${url.pathname}${url.search}`, { replace: true });
	},
	/** `since` and `until` for the API, for the range as of now. */
	params() {
		return rangeParams(this.current, this.now());
	}
};
