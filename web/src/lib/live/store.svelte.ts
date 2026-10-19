// One live store for the whole panel: the snapshot and the SSE stream
// (sessions, hosts, limits, note changes), reconnecting with backoff and
// restarting a stream that went quiet. Pages derive their views from it.

import { browser } from '$app/env';
import { SvelteMap } from 'svelte/reactivity';
import { Unauthorized, api, getJson, token } from '../api/client';
import type { HostStatus, RateLimits, Session, Snapshot, Update } from '../api/types';

const STALE_STREAM_MS = 45_000;

export type ConnKind = 'connecting' | 'live' | 'reconnecting' | 'offline';

class Live {
	host = $state('');
	/** Hub clock minus ours. */
	skew = $state(0);
	sessions = new SvelteMap<string, Session>();
	rateLimits = $state<RateLimits | null>(null);
	workspaces = $state<string[]>([]);
	hosts = $state<HostStatus[]>([]);
	conn = $state<{ kind: ConnKind; text: string }>({ kind: 'connecting', text: 'connecting' });
	/** The token was rejected or is missing; the shell asks for one. */
	needToken = $state(false);
	/** Bumped when a workspace's notes change. */
	notes = $state<{ workspace: string; n: number }>({ workspace: '', n: 0 });
	/** Ticks every second, hub time. */
	now = $state(Date.now());
	/** The snapshot arrived at least once. */
	ready = $state(false);

	#abort: AbortController | null = null;
	#streaming = false;
	#last = 0;
	#timers: ReturnType<typeof setInterval>[] = [];

	get list(): Session[] {
		return [...this.sessions.values()];
	}

	get multiHost(): boolean {
		return this.hosts.length > 1;
	}

	hostLabel(host: string): string {
		const status = this.hosts.find((x) => x.name === host);
		return status && !status.connected ? `${host} (offline)` : host;
	}

	start() {
		if (!browser || this.#timers.length) return;
		this.#timers.push(setInterval(() => (this.now = Date.now() + this.skew), 1000));
		this.#timers.push(setInterval(() => this.reconnectIfStale(), 10_000));
		document.addEventListener('visibilitychange', () => {
			if (document.visibilityState === 'visible') this.reconnectIfStale();
		});
		if (!token()) {
			this.needToken = true;
			this.setConn('offline', 'token needed');
			return;
		}
		this.run();
	}

	/** Starts streaming (again) once a token is there. */
	run() {
		if (this.#streaming) return;
		this.needToken = false;
		this.#streaming = true;
		this.loop().finally(() => (this.#streaming = false));
	}

	setConn(kind: ConnKind, text: string = kind) {
		this.conn = { kind, text };
	}

	reconnectIfStale() {
		if (this.#abort && Date.now() - this.#last > STALE_STREAM_MS) this.#abort.abort();
	}

	private async loop() {
		let delay = 1000;
		for (;;) {
			this.#abort = new AbortController();
			try {
				this.setConn('connecting');
				const res = await api('stream', { signal: this.#abort.signal });
				this.setConn('live');
				delay = 1000;
				this.#last = Date.now();
				const reader = res.body!.pipeThrough(new TextDecoderStream()).getReader();
				let buffer = '';
				for (;;) {
					const { value, done } = await reader.read();
					if (done) break;
					this.#last = Date.now();
					buffer += value;
					let end;
					while ((end = buffer.indexOf('\n\n')) >= 0) {
						this.handle(buffer.slice(0, end));
						buffer = buffer.slice(end + 2);
					}
				}
			} catch (e) {
				if (e instanceof Unauthorized) {
					this.needToken = true;
					this.setConn('offline', 'token needed');
					return;
				}
			}
			this.setConn('reconnecting');
			await new Promise((resolve) => setTimeout(resolve, delay));
			delay = Math.min(delay * 2, 30_000);
		}
	}

	private handle(raw: string) {
		let event = 'message';
		let data = '';
		for (const line of raw.split('\n')) {
			if (line.startsWith('event:')) event = line.slice(6).trim();
			else if (line.startsWith('data:')) data += line.slice(5).trimStart();
		}
		if (event === 'lagged') return void this.refreshSnapshot();
		if (!data) return;
		const payload = JSON.parse(data);
		if (event === 'snapshot') this.applySnapshot(payload);
		else if (event === 'update') this.applyUpdate(payload);
	}

	async refreshSnapshot() {
		try {
			this.applySnapshot(await getJson<Snapshot>('snapshot'));
		} catch (e) {
			if (e instanceof Unauthorized) this.needToken = true;
		}
	}

	applySnapshot(snapshot: Snapshot) {
		this.host = snapshot.host;
		this.skew = snapshot.now - Date.now();
		this.now = Date.now() + this.skew;
		const ids = new Set(snapshot.sessions.map((s) => s.id));
		for (const id of [...this.sessions.keys()]) if (!ids.has(id)) this.sessions.delete(id);
		for (const s of snapshot.sessions) this.sessions.set(s.id, s);
		this.rateLimits = snapshot.rate_limits;
		this.workspaces = snapshot.workspaces ?? [];
		this.hosts = snapshot.hosts ?? [];
		this.ready = true;
	}

	applyUpdate(update: Update) {
		switch (update.type) {
			case 'session':
				this.sessions.set(update.session.id, update.session);
				break;
			case 'removed':
				this.sessions.delete(update.id);
				break;
			case 'rate_limits':
				this.rateLimits = update.rate_limits;
				break;
			case 'hosts':
				this.hosts = update.hosts;
				break;
			case 'notes':
				this.notes = { workspace: update.workspace, n: this.notes.n + 1 };
				break;
		}
	}

	/** Puts a session the API returned into the store. */
	put(session: Session) {
		this.sessions.set(session.id, session);
	}
}

export const live = new Live();
