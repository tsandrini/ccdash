// History and Analytics payloads for the scope and range in the URL: the
// previous one stays on screen (dimmed) while the next arrives, and a
// payload fetched moments ago is reused when its page is opened again.

import type { Analytics, History } from './api/types';
import { Unauthorized } from './api/client';
import { live } from './live/store.svelte';

const FRESH_MS = 30_000;

export class Ranged<T> {
	data = $state.raw<T | null>(null);
	loading = $state(false);
	error = $state('');
	#key = '';
	#at = 0;
	#seq = 0;

	/** Fetches unless the same `key` was fetched moments ago (`force` fetches anyway). */
	async load(key: string, fetcher: () => Promise<T>, force = false) {
		if (!force && key === this.#key && this.data && Date.now() - this.#at < FRESH_MS) return;
		if (key !== this.#key) this.#key = key;
		const seq = ++this.#seq;
		this.loading = true;
		try {
			const data = await fetcher();
			if (seq !== this.#seq) return;
			this.data = data;
			this.error = '';
			this.#at = Date.now();
		} catch (e) {
			if (seq !== this.#seq) return;
			if (e instanceof Unauthorized) live.needToken = true;
			else this.error = e instanceof Error ? e.message : String(e);
		} finally {
			if (seq === this.#seq) this.loading = false;
		}
	}
}

export const historyData = new Ranged<History>();
export const analyticsData = new Ranged<Analytics>();
