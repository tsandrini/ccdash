// What the panel derives from a session: its shown state, grouping,
// labels, and the signature that tells the page to reload the chat.

import type { Activity, Session, TaskNotification } from './api/types';

export type Shown = Activity | 'finished';

export const ACTIVITY: Record<Shown, { label: string; rank: number }> = {
	permission: { label: 'Needs permission', rank: 0 },
	your_turn: { label: 'Your turn', rank: 1 },
	working: { label: 'Working', rank: 2 },
	parked: { label: 'Parked', rank: 3 },
	idle: { label: 'Idle', rank: 4 },
	gone: { label: 'Ended', rank: 5 },
	finished: { label: 'Finished', rank: 6 }
};

export const label = (s: Session) => s.title || s.name || s.id.slice(0, 8);
export const isAgent = (s: Session) => s.kind === 'background';

/** Background sessions answer to their parent, so a done turn is "finished". */
export const shown = (s: Session): Shown =>
	isAgent(s) && (s.activity === 'your_turn' || s.activity === 'parked') ? 'finished' : s.activity;

/** State colour for bars: finished agents read as idle. */
export const barActivity = (s: Session): Activity =>
	shown(s) === 'finished' ? 'idle' : s.activity;

export const children = (all: Iterable<Session>, s: Session) =>
	[...all].filter((c) => c.parent === s.id && c.activity !== 'gone');

/** Short-lived sessions that never worked (the `--resume` picker). */
export const stub = (s: Session) =>
	s.activity === 'gone' && !s.hooked && s.last_event_at - s.started_at < 60_000;

export const live = (s: Session) => s.activity !== 'gone';
export const busy = (s: Session) => s.activity === 'working' || s.activity === 'permission';

export function byAttention(a: Session, b: Session): number {
	return (
		ACTIVITY[a.activity].rank - ACTIVITY[b.activity].rank || a.activity_since - b.activity_since
	);
}

export type Groups = {
	all: Session[];
	needs: Session[];
	working: Session[];
	parked: Session[];
	idle: Session[];
	agents: Session[];
	gone: Session[];
};

/** The sessions in the groups the pages show, each sorted by attention. */
export function group(sessions: Session[]): Groups {
	const all = sessions.filter((s) => !stub(s)).sort(byAttention);
	const mine = all.filter((s) => !isAgent(s));
	const agents = all.filter(isAgent);
	return {
		all,
		needs: all.filter((s) => s.activity === 'permission' || shown(s) === 'your_turn'),
		working: mine.filter((s) => s.activity === 'working'),
		parked: mine.filter((s) => s.activity === 'parked'),
		idle: mine.filter((s) => s.activity === 'idle'),
		agents: agents.filter((s) => s.activity !== 'gone' && s.activity !== 'permission'),
		gone: all.filter((s) => s.activity === 'gone')
	};
}

/** The claim's paths relative to the session's directory. */
export function claimPaths(s: Session): string[] {
	const base = s.cwd.replace(/\/+$/, '');
	return (s.claim?.paths ?? []).map((p) =>
		p.startsWith(`${base}/`) ? p.slice(base.length + 1) : p
	);
}

/** What reported, by `TaskNotification.source`. */
export const TASK_SOURCE: Record<string, string> = {
	agent: 'Subagent',
	shell: 'Background command',
	monitor: 'Monitor',
	mcp: 'MCP task',
	workflow: 'Workflow'
};

/** A task notification in a few words: what reported and its name. */
export const taskTitle = (t: TaskNotification) =>
	`${TASK_SOURCE[t.source] ?? 'Background task'}: ${t.name ?? t.summary}`;

/** Changes that add or alter conversation items. */
export const signature = (s: Session) =>
	[
		s.activity,
		s.turn?.started_at,
		s.prompt?.at,
		s.reply?.at,
		s.turn?.tools,
		s.turn?.ended_at,
		s.subagents.length,
		s.tasks.map((t) => t.status).join(),
		s.inbox?.length,
		s.claim?.since,
		s.outbox?.length
	].join('|');

/** The session's workspace name and project directory, for headers. */
export function where(s: Session, multiHost: boolean): string[] {
	return [multiHost ? s.host : null, s.workspace, s.model].filter((x): x is string => Boolean(x));
}
