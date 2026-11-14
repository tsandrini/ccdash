// What the panel derives from a session: its shown state, grouping,
// labels, and the signature that tells the page to reload the chat.

import type { Activity, Exchange, Session } from './api/types';

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
export const isBackground = (s: Session) => s.kind === 'background';
/** Started by Claude, so it answers to that session. One you started, from
 * the panel or a terminal, is yours like any other. */
export const isAgent = (s: Session) => isBackground(s) && Boolean(s.parent) && !s.launched;

/** Agents answer to their parent, so a done turn is "finished". */
export const shown = (s: Session): Shown =>
	isAgent(s) && (s.activity === 'your_turn' || s.activity === 'parked') ? 'finished' : s.activity;

/** State colour for bars: finished agents read as idle. */
export const barActivity = (s: Session): Activity =>
	shown(s) === 'finished' ? 'idle' : s.activity;

/**
 * An ended session whose conversation goes on in a copy that is listed
 * too: the lists leave it to that one, whose chat starts with its part.
 *
 * @param s - The session.
 * @param listed - The ids of the sessions the list shows.
 * @returns Whether the list leaves it out.
 */
export const folded = (s: Session, listed: Set<string>) =>
	s.activity === 'gone' && s.continued_as !== null && listed.has(s.continued_as);

/**
 * How the session `s` is linked to is named beside it: by its title when
 * that tells the two apart (a copy takes the title over), else by its id.
 *
 * @param s - The session the link is shown on.
 * @param other - The linked session, when the panel has it.
 * @param id - The linked session's id.
 * @returns The name to show.
 *
 * @example
 * linkName(copy, undefined, '94f90b09-83ae-…') // 'session 94f90b09'
 */
export const linkName = (s: Session, other: Session | undefined, id: string) =>
	other && label(other) !== label(s) ? label(other) : `session ${id.slice(0, 8)}`;

export const children = (all: Iterable<Session>, s: Session) =>
	[...all].filter((c) => c.parent === s.id && c.activity !== 'gone');

/** Short-lived sessions that never worked (the `--resume` picker). */
export const stub = (s: Session) =>
	s.activity === 'gone' && !s.hooked && s.last_event_at - s.started_at < 60_000;

/** How long a working session may say nothing: between tools, in a tool. */
const QUIET_MS = 10 * 60_000;
const QUIET_TOOL_MS = 30 * 60_000;

/** How long a working session has been silent, once that is unusual: a hung
 * command or a dead process. Only hooks move `last_event_at`. */
export function quietFor(s: Session, now: number): number | null {
	if (s.activity !== 'working' || !s.hooked) return null;
	const silent = now - s.last_event_at;
	return silent > (s.tool ? QUIET_TOOL_MS : QUIET_MS) ? silent : null;
}

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
	const listed = new Set(all.map((s) => s.id));
	const mine = all.filter((s) => !isAgent(s));
	const agents = all.filter(isAgent);
	return {
		all,
		needs: all.filter((s) => s.activity === 'permission' || shown(s) === 'your_turn'),
		working: mine.filter((s) => s.activity === 'working'),
		parked: mine.filter((s) => s.activity === 'parked'),
		idle: mine.filter((s) => s.activity === 'idle'),
		agents: agents.filter((s) => s.activity !== 'gone' && s.activity !== 'permission'),
		gone: all.filter((s) => s.activity === 'gone' && !folded(s, listed))
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

/** What started an exchange instead of a prompt (a task notification or a
 * subagent's message), as its bubble shows it. */
export type Notice = {
	/** `TaskNotification.source`; `agent` for a subagent's message. */
	source: string;
	label: string;
	name: string | null;
	/** After the name: the outcome, or the subagent's type. */
	outcome: string;
	status: string | null;
	tools: number | null;
	ms: number | null;
	event: string | null;
	html: string | null;
};

export function notice(ex: Exchange): Notice | null {
	const t = ex.task;
	if (t)
		return {
			...t,
			label: TASK_SOURCE[t.source] ?? 'Background task',
			outcome: t.summary
		};
	const a = ex.agent;
	if (a)
		return {
			source: 'agent',
			label: a.hand_back ? 'Subagent report' : 'Subagent message',
			name: a.name ?? `agent ${a.from.slice(0, 8)}`,
			outcome: a.agent_type ?? '',
			status: null,
			tools: null,
			ms: null,
			event: null,
			html: a.html
		};
	return null;
}

/** An exchange's notice in a few words: what it is and its name. */
export function noticeTitle(ex: Exchange): string | null {
	const n = notice(ex);
	return n && `${n.label}: ${n.name ?? n.outcome}`;
}

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
		s.outbox?.length,
		s.interrupting,
		s.continues
	].join('|');

/** The session's workspace name and project directory, for headers. */
export function where(s: Session, multiHost: boolean): string[] {
	return [multiHost ? s.host : null, s.workspace, s.model].filter((x): x is string => Boolean(x));
}
