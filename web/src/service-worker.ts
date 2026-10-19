// The panel's service worker: keeps the app shell available offline (the
// build's files cache-first, the page network-first with the cache as the
// fallback; the API, the stream, `/act` and the previous panel are never
// touched) and shows Web Push notifications with buttons that answer
// prompts through `/act/<key>/<action>`. Tapping the body opens the
// session's page; a `close` message takes an answered prompt down.

/// <reference types="@sveltejs/kit" />
/// <reference no-default-lib="true"/>
/// <reference lib="esnext" />
/// <reference lib="webworker" />

import { version } from '$app/env';
import { assets, immutable } from '$app/manifest';

declare const self: ServiceWorkerGlobalScope;

const CACHE = `ccdash-${version}`;
const SHELL = '/';
/** The build's chunks and the static files, as absolute paths. */
const ASSETS = new Set(
	[...immutable, ...assets]
		.map((entry) => (typeof entry === 'string' ? entry : (entry as { path: string }).path))
		.map((p) => (p.startsWith('/') ? p : `/${p}`))
);

self.addEventListener('install', (event) => {
	event.waitUntil(
		caches
			.open(CACHE)
			.then((cache) => cache.addAll([SHELL, ...ASSETS]))
			.then(() => self.skipWaiting())
	);
});

self.addEventListener('activate', (event) => {
	event.waitUntil(
		caches
			.keys()
			.then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
			.then(() => self.clients.claim())
	);
});

/** Same origin, GET, not the API, a button, the collector link or the previous panel. */
function ours(request: Request): URL | null {
	if (request.method !== 'GET') return null;
	const url = new URL(request.url);
	if (url.origin !== self.location.origin) return null;
	const p = url.pathname;
	if (p.startsWith('/api/') || p.startsWith('/act/') || p === '/collect' || p.startsWith('/v1'))
		return null;
	return url;
}

self.addEventListener('fetch', (event) => {
	const url = ours(event.request);
	if (!url) return;
	if (event.request.mode === 'navigate') {
		event.respondWith(networkFirst(event.request, SHELL));
	} else if (ASSETS.has(url.pathname)) {
		event.respondWith(cacheFirst(event.request));
	}
});

async function networkFirst(request: Request, key: string): Promise<Response> {
	const cache = await caches.open(CACHE);
	try {
		const response = await fetch(request);
		if (response.ok) cache.put(key, response.clone());
		return response;
	} catch {
		return (await cache.match(key)) || Response.error();
	}
}

async function cacheFirst(request: Request): Promise<Response> {
	const cache = await caches.open(CACHE);
	const hit = await cache.match(request);
	if (hit) return hit;
	const response = await fetch(request);
	if (response.ok) cache.put(request, response.clone());
	return response;
}

// --- push -------------------------------------------------------------------

type PushData = {
	type?: string;
	tag?: string;
	title?: string;
	body?: string;
	at?: number;
	urgent?: boolean;
	url?: string;
	session?: string;
	key?: string;
	buttons?: { action: string; label: string }[];
	more?: boolean;
};

function parse(event: PushEvent): PushData {
	try {
		return event.data?.json() ?? {};
	} catch {
		return { title: 'ccdash', body: event.data?.text() ?? '' };
	}
}

self.addEventListener('push', (event) => {
	const data = parse(event);
	if (data.type === 'close') {
		event.waitUntil(closeTag(data.tag));
		return;
	}
	event.waitUntil(show(data));
});

async function closeTag(tag?: string) {
	if (!tag) return;
	for (const n of await self.registration.getNotifications({ tag })) n.close();
}

/** Lays the hub's buttons out within what this platform shows. */
function actions(data: PushData): { action: string; title: string }[] {
	const max = (Notification as unknown as { maxActions?: number }).maxActions ?? 0;
	const buttons = (data.buttons ?? []).map((b) => ({ action: b.action, title: b.label }));
	if (!max) return [];
	if (data.more) return [...buttons.slice(0, max - 1), { action: 'open', title: 'Answer' }];
	return buttons.slice(0, max);
}

function show(data: PushData) {
	return self.registration.showNotification(data.title ?? 'ccdash', {
		body: data.body ?? '',
		tag: data.tag,
		icon: '/icon-192.png',
		badge: '/icon-192.png',
		timestamp: data.at,
		requireInteraction: Boolean(data.urgent),
		actions: actions(data),
		data: { url: data.url, session: data.session, key: data.key }
	} as NotificationOptions);
}

self.addEventListener('notificationclick', (event) => {
	const { url, session, key } = (event.notification.data ?? {}) as PushData;
	event.notification.close();
	if (event.action && event.action !== 'open' && key) {
		event.waitUntil(fetch(`/act/${key}/${event.action}`, { method: 'POST' }));
		return;
	}
	event.waitUntil(openSession(url, session));
});

/** Focuses an open panel and tells it which session, else opens one. */
async function openSession(url?: string, session?: string) {
	const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
	const client = windows.find((c) => new URL(c.url).origin === self.location.origin);
	if (client) {
		if (session) client.postMessage({ type: 'session', id: session });
		// NOTE: focus() rejects without a user gesture; the message got through anyway
		await client.focus().catch(() => {});
		return;
	}
	await self.clients.openWindow(url || (session ? `/sessions/${session}` : '/'));
}

// NOTE: the page re-sends the current subscription at every start, so the hub learns the new endpoint
self.addEventListener('pushsubscriptionchange', (event) => {
	const e = event as Event & { oldSubscription?: PushSubscription | null };
	const options = e.oldSubscription?.options;
	if (!options?.applicationServerKey) return;
	(e as ExtendableEvent).waitUntil(self.registration.pushManager.subscribe(options));
});
