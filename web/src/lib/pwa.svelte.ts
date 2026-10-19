// The installed app: the service worker (shell cache, push), the
// deferred install prompt and this device's push subscription.

import { browser, dev } from '$app/env';
import { subscribePush, unsubscribePush } from './api';
import { token } from './api/client';

/** Chromium's deferred install prompt. */
type InstallPrompt = Event & { prompt: () => Promise<void>; userChoice: Promise<unknown> };

class Pwa {
	installPrompt = $state<InstallPrompt | null>(null);

	get installed(): boolean {
		return browser && matchMedia('(display-mode: standalone)').matches;
	}

	get pushSupported(): boolean {
		return (
			browser && 'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window
		);
	}

	/** Captures the install prompt and registers the worker (built app only). */
	async start(onSession: (id: string) => void) {
		addEventListener('beforeinstallprompt', (event) => {
			event.preventDefault();
			this.installPrompt = event as InstallPrompt;
		});
		if (!('serviceWorker' in navigator)) return;
		navigator.serviceWorker.addEventListener('message', (event) => {
			if (event.data?.type === 'session' && event.data.id) onSession(event.data.id);
		});
		if (dev) return;
		try {
			await navigator.serviceWorker.register('/service-worker.js');
		} catch (e) {
			console.warn('service worker:', e);
			return;
		}
		this.syncSubscription();
	}

	async install() {
		if (!this.installPrompt) return;
		this.installPrompt.prompt();
		await this.installPrompt.userChoice;
		this.installPrompt = null;
	}

	/** Re-sends this device's subscription, so a renewed endpoint reaches the hub. */
	async syncSubscription() {
		if (!this.pushSupported || !token()) return;
		try {
			const sub = await this.subscription();
			if (sub) await subscribePush(sub.toJSON());
		} catch {
			// best effort
		}
	}

	async subscription(): Promise<PushSubscription | null> {
		if (!this.pushSupported) return null;
		const registration = await navigator.serviceWorker.ready;
		return registration.pushManager.getSubscription();
	}

	async enablePush(key: string): Promise<PushSubscription> {
		if ((await Notification.requestPermission()) !== 'granted')
			throw new Error('Notification permission was not granted.');
		const registration = await navigator.serviceWorker.ready;
		const sub = await registration.pushManager.subscribe({
			userVisibleOnly: true,
			applicationServerKey: keyBytes(key)
		});
		await subscribePush(sub.toJSON());
		return sub;
	}

	async disablePush(sub: PushSubscription) {
		await unsubscribePush(sub.endpoint);
		await sub.unsubscribe();
	}
}

/** `applicationServerKey` from the hub's base64url VAPID public key. */
const keyBytes = (b64: string) =>
	Uint8Array.from(atob(b64.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0));

export const pwa = new Pwa();
