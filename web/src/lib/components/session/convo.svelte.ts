// The loaded conversation of the open session page: pages of exchanges,
// the totals, process samples and messages; refreshed when the live
// session changes.

import { Unauthorized } from '#lib/api/client.js';
import {
	fetchSession,
	type Exchange,
	type Message,
	type Session,
	type Stats
} from '#lib/api/index.js';
import { live } from '#lib/live/store.svelte.js';

export const PAGE = 10;

export class Convo {
	id: string;
	exchanges = $state<Exchange[]>([]);
	more = $state(false);
	stats = $state<Stats | null>(null);
	resources = $state<[number, number, number][] | null>(null);
	messages = $state<Message[]>([]);
	loaded = $state(false);
	loading = $state(false);
	/** True when the session is unknown to the hub. */
	missing = $state(false);
	/** The session as the page fetched it: an ended one comes from history. */
	session = $state<Session | null>(null);
	/** Bumped when a load changed the exchanges, for scroll handling. */
	changed = $state<{ mode: Mode; n: number }>({ mode: 'initial', n: 0 });
	#timer: ReturnType<typeof setTimeout> | null = null;

	constructor(id: string) {
		this.id = id;
	}

	/** Refetches the newest exchanges at most once a second. */
	scheduleRefresh() {
		if (this.#timer) return;
		this.#timer = setTimeout(() => {
			this.#timer = null;
			this.load('refresh');
		}, 1000);
	}

	async load(mode: Mode) {
		const params =
			mode === 'older'
				? { before: this.exchanges[0]?.at, limit: PAGE }
				: { limit: mode === 'refresh' ? 4 : PAGE };
		let page;
		this.loading = true;
		try {
			page = await fetchSession(this.id, params);
		} catch (e) {
			if (e instanceof Unauthorized) live.needToken = true;
			else if ((e as Error).message === 'no such session') this.missing = true;
			return;
		} finally {
			this.loading = false;
		}
		if (page.session) {
			this.session = page.session;
			// NOTE: an ended session from history stays out of the live lists
			if (page.session.activity !== 'gone' || live.sessions.has(page.session.id))
				live.put(page.session);
		}
		const conv = page.conversation;
		if (mode === 'older') {
			this.exchanges = [...conv.exchanges, ...this.exchanges];
			this.more = conv.more;
		} else if (mode === 'initial' || !conv.more || !this.exchanges.length) {
			this.exchanges = conv.exchanges;
			this.more = conv.more;
		} else {
			const cut = conv.exchanges[0].at;
			const kept = this.exchanges.filter((e) => e.at < cut);
			if (kept.length === this.exchanges.length && kept.length) return void this.load('initial');
			this.exchanges = [...kept, ...conv.exchanges];
		}
		if (page.stats) this.stats = page.stats;
		if (page.resources) this.resources = page.resources;
		if (page.messages) this.messages = page.messages;
		this.loaded = true;
		this.changed = { mode, n: this.changed.n + 1 };
	}

	/** When Claude last gave a final reply, to leave only that one unfolded. */
	get lastReplyAt(): number | null {
		for (let i = this.exchanges.length - 1; i >= 0; i--) {
			const reply = this.exchanges[i].items.findLast((it) => it.kind === 'reply');
			if (reply) return reply.at;
		}
		return null;
	}
}

export type Mode = 'initial' | 'refresh' | 'older';
