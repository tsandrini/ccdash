// The hub's API: bearer token from `#token=` (pairing) kept in
// localStorage, JSON helpers, and `Unauthorized` for a rejected token.

import { browser } from '$app/env';

const TOKEN_KEY = 'ccdash.token';

export class Unauthorized extends Error {}

/** A rejected request, with the hub's status (409 for a version conflict). */
export class ApiError extends Error {
	constructor(
		message: string,
		public status: number
	) {
		super(message);
	}
}

export const token = () => (browser ? (localStorage.getItem(TOKEN_KEY) ?? '') : '');

export const setToken = (value: string) => localStorage.setItem(TOKEN_KEY, value);

/** A bare token, or the token from a pasted `ccdash url` link. */
export function tokenFrom(text: string): string {
	const match = text.match(/token=([^&\s#]+)/);
	return (match ? decodeURIComponent(match[1]) : text).trim();
}

export async function api(path: string, init: RequestInit = {}): Promise<Response> {
	const res = await fetch(`/api/v1/${path}`, {
		...init,
		headers: { ...init.headers, Authorization: `Bearer ${token()}` }
	});
	if (res.status === 401) throw new Unauthorized();
	if (!res.ok) {
		const text = await res.text();
		let message = text;
		try {
			message = JSON.parse(text).error || text;
		} catch {
			// not JSON
		}
		throw new ApiError(message || `${res.status} ${res.statusText}`, res.status);
	}
	return res;
}

export const getJson = async <T>(path: string): Promise<T> => (await api(path)).json();

const withBody = (method: string, body: unknown): RequestInit => ({
	method,
	headers: { 'Content-Type': 'application/json' },
	body: JSON.stringify(body)
});

export const postJson = async <T>(path: string, body: unknown): Promise<T> =>
	(await api(path, withBody('POST', body))).json();

export const putJson = async <T>(path: string, body: unknown): Promise<T> =>
	(await api(path, withBody('PUT', body))).json();

export const deleteJson = async <T>(path: string, body: unknown): Promise<T> =>
	(await api(path, withBody('DELETE', body))).json();

/** `a=1&b=2` from defined values only. */
export function query(params: Record<string, string | number | undefined | null>): string {
	const q = new URLSearchParams();
	for (const [k, v] of Object.entries(params)) {
		if (v !== undefined && v !== null && v !== '') q.set(k, String(v));
	}
	const s = q.toString();
	return s ? `?${s}` : '';
}
