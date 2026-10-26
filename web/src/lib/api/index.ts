// Typed fetchers over the hub's routes (`src/hub/http.rs`).

import { del, deleteJson, getJson, patchJson, postJson, putJson, query } from './client';
import type {
	Analytics,
	Answer,
	Conversation,
	Graph,
	History,
	Hit,
	Launch,
	LaunchBody,
	LaunchTarget,
	Message,
	Note,
	NoteOp,
	NoteView,
	Projects,
	Prompt,
	PushOutcome,
	PutNote,
	Session,
	Stats,
	Subscription,
	Keep,
	SyncStats,
	SyncView,
	Token,
	Written
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

/** Pins the session's project (its notes' default); null lets its directory decide again. */
export const setSessionProject = (id: string, project: string | null) =>
	postJson<{ session: Session }>(`sessions/${encodeURIComponent(id)}/project`, { project });

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

/** Starts a background session (or continues an ended one); the launch
 * moves on through `launch` updates on the stream. */
export const startLaunch = (body: LaunchBody) => postJson<{ launch: Launch }>('launches', body);

/** Connected hosts, their workspaces and the projects with notes and a directory there. */
export const fetchTargets = () => getJson<{ hosts: LaunchTarget[] }>('launch-targets');

export const fetchLaunch = (id: string) =>
	getJson<{ launch: Launch }>(`launches/${encodeURIComponent(id)}`);

/** `claude stop` (background) or SIGTERM (interactive) on the session's host. */
export const stopSession = (id: string) =>
	postJson<{ session: Session }>(`sessions/${encodeURIComponent(id)}/stop`, {});

/** Sends the wrap-up prompt; the hub closes the session when that turn ends. */
export const resolveSession = (id: string) =>
	postJson<{ session: Session }>(`sessions/${encodeURIComponent(id)}/resolved`, {});

/** Takes the resolved mark back: the session stays open after its wrap-up. */
export const keepSessionOpen = (id: string) =>
	del<{ session: Session }>(`sessions/${encodeURIComponent(id)}/resolved`);

/** Marks the session ended on the hub (a ghost, an offline host). */
export const forgetSession = (id: string) =>
	postJson<{ session: Session }>(`sessions/${encodeURIComponent(id)}/forget`, {});

/** Focuses its window or attaches it in a terminal on its host. */
export const openSession = (id: string) =>
	postJson<{ session: Session }>(`sessions/${encodeURIComponent(id)}/open`, {});

/** Every machine's copy of Claude Code's state, and the open conflicts. */
export const fetchSync = () => getJson<SyncView>('sync');

/** The machine compares every file with the hub again. */
export const resyncHost = (host: string) =>
	postJson<{ ok: boolean }>(`sync/${encodeURIComponent(host)}/resync`, {});

/** Hides a conflict; the copy stays on its machine. */
export const dismissConflict = (id: number) => del<{ ok: boolean }>(`sync/conflicts/${id}`);

/** Asks the machine holding the copy to keep the hub's version (the copy
 * goes) or the copy (it replaces the hub's; the replaced one is kept). */
export const resolveConflict = (id: number, keep: Keep) =>
	postJson<{ ok: boolean }>(`sync/conflicts/${id}/resolve`, { keep });

/** Sizes by kind, project and session, and growth per day. */
export const fetchSyncStats = () => getJson<SyncStats>('sync/stats');

export const fetchPrompts = (params: { session?: string; workspace?: string; limit?: number }) =>
	getJson<{ prompts: Prompt[] }>(`prompts${query(params)}`);

/** The devices with a token; `bootstrap` when this one is on the configured token. */
export type Devices = { tokens: Token[]; bootstrap: boolean };
export const fetchTokens = () => getJson<Devices>('tokens');

/** A fresh device token: the link to open (shown this once) and its QR code. */
export type Pairing = { token: Token; url: string; svg: string };
export const mintToken = (label: string) => postJson<Pairing>('tokens', { label });

export const relabelToken = (id: number, label: string) =>
	patchJson<{ token: Token }>(`tokens/${id}`, { label });

/** Revokes the token and drops the device's push subscriptions. */
export const revokeToken = (id: number) =>
	del<{ revoked: boolean; subscriptions: number }>(`tokens/${id}`);

export const fetchHistory = (params: {
	since?: number;
	until?: number;
	workspace?: string;
	host?: string;
}) => getJson<History>(`history${query(params)}`);

/** `tz`: the viewer's offset from UTC in minutes, for the day buckets. */
export const fetchAnalytics = (params: {
	since?: number;
	until?: number;
	tz: number;
	workspace?: string;
	host?: string;
}) => getJson<Analytics>(`analytics${query(params)}`);

export const fetchNotes = (params: {
	workspace?: string;
	q?: string;
	kind?: string;
	project?: string;
	status?: string;
	limit?: number;
}) => getJson<{ hits: Hit[] }>(`notes${query(params)}`);

export const fetchProjects = (params: { workspace?: string }) =>
	getJson<Projects>(`projects${query(params)}`);

/** One workspace's notes and links; `status` as for notes. */
export const fetchGraph = (workspace: string, status?: string) =>
	getJson<Graph>(`graph${query({ workspace, status })}`);

const notePath = (ws: string, slug: string) =>
	`notes/${encodeURIComponent(ws)}/${encodeURIComponent(slug)}`;

/** The note with its links and backlinks, plus the body rendered. */
export type NotePage = NoteView & { html: string };
export const fetchNote = (ws: string, slug: string) => getJson<NotePage>(notePath(ws, slug));

/** Creates or replaces a note; 409 when `base_version` is behind. */
export const putNote = (ws: string, slug: string, body: PutNote) =>
	putJson<{ note: Note }>(notePath(ws, slug), body);

/** A tool operation (verify, issue_update, …) on the note. */
export const noteOp = (ws: string, slug: string, op: NoteOp) =>
	postJson<{ written: Written }>(`${notePath(ws, slug)}/op`, op);

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
