// Typed fetchers over the hub's routes (`src/hub/http.rs`).

import { deleteJson, getJson, postJson, query } from './client';
import type {
	Answer,
	Conversation,
	History,
	Hit,
	Message,
	Prompt,
	PushOutcome,
	Session,
	Stats,
	Subscription
} from './types';

export type { Unauthorized } from './client';
export * from './types';

/** `GET /sessions/{id}`: the first page carries the totals. */
export type SessionPage = {
	session: Session;
	conversation: Conversation;
	stats: Stats | null;
	/** `[ts, cpu %, memory bytes]` of the last hour. */
	resources: [number, number, number][] | null;
	messages: Message[] | null;
};

export const fetchSession = (id: string, params: { before?: number; limit?: number } = {}) =>
	getJson<SessionPage>(`sessions/${encodeURIComponent(id)}${query(params)}`);

export const renameSession = (id: string, title: string) =>
	postJson<{ session: Session }>(`sessions/${encodeURIComponent(id)}/title`, { title });

/** A turn through the session's channel. */
export const replyTo = (id: string, text: string) =>
	postJson<{ session: Session }>(`sessions/${encodeURIComponent(id)}/reply`, { text });

/** Context for the session's next prompt. */
export const messageTo = (id: string, text: string) =>
	postJson<{ message: Message }>(`sessions/${encodeURIComponent(id)}/messages`, { text });

export const answerAsk = (id: string, ask: string, answer: Answer) =>
	postJson<{ session: Session }>(
		`sessions/${encodeURIComponent(id)}/asks/${encodeURIComponent(ask)}`,
		answer
	);

export const fetchPrompts = (params: { session?: string; workspace?: string; limit?: number }) =>
	getJson<{ prompts: Prompt[] }>(`prompts${query(params)}`);

export type Pair = { url: string; svg: string; lan: boolean };
export const fetchPair = () => getJson<Pair>('pair');

export const fetchHistory = (params: {
	since?: number;
	until?: number;
	workspace?: string;
	host?: string;
}) => getJson<History>(`history${query(params)}`);

export const fetchNotes = (params: {
	workspace?: string;
	q?: string;
	kind?: string;
	project?: string;
	status?: string;
	limit?: number;
}) => getJson<{ hits: Hit[] }>(`notes${query(params)}`);

export type PushInfo = { enabled: boolean; key: string | null; subscriptions: Subscription[] };
export const fetchPush = () => getJson<PushInfo>('push');

export const subscribePush = (subscription: PushSubscriptionJSON) =>
	postJson<{ subscription: Subscription }>('push/subscriptions', subscription);

export const unsubscribePush = (endpoint: string) =>
	deleteJson<{ removed: number }>('push/subscriptions', { endpoint });

export type PushTestResult = PushOutcome & {
	id: number;
	endpoint: string;
	user_agent: string | null;
};
export const testPush = () => postJson<{ results: PushTestResult[] }>('push/test', {});
