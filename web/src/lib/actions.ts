// What can be done to a session, in one list: the cards' Actions menus,
// the buttons Now and the session page pin, and their confirmations all
// come from here.

import type { Component } from 'svelte';
import CircleCheckIcon from '@lucide/svelte/icons/circle-check';
import CircleOffIcon from '@lucide/svelte/icons/circle-off';
import EraserIcon from '@lucide/svelte/icons/eraser';
import LockOpenIcon from '@lucide/svelte/icons/lock-open';
import MonitorUpIcon from '@lucide/svelte/icons/monitor-up';
import OctagonXIcon from '@lucide/svelte/icons/octagon-x';
import PlayIcon from '@lucide/svelte/icons/play';
import RepeatIcon from '@lucide/svelte/icons/repeat';
import {
	forgetSession,
	interruptSession,
	keepSessionOpen,
	openSession,
	resolveSession,
	stopSession,
	type Session
} from './api/index';
import { live } from './live/store.svelte';
import { busy, isBackground, label } from './session';
import { resumableSession, starter } from './starter.svelte';

export type ActionId =
	'open' | 'interrupt' | 'resolve' | 'keep' | 'continue' | 'switch' | 'kill' | 'forget';

export type Confirm = { title: string; body: string; button: string };

export type SessionAction = {
	id: ActionId;
	label: (s: Session) => string;
	icon: Component;
	/** Ends the session (red in menus, a red confirm button). */
	destructive?: boolean;
	/** Offered for the session as it is now. */
	offered: (s: Session) => boolean;
	/** Why an offered action cannot run; it shows disabled with this. */
	blocked?: (s: Session) => string | null;
	/** Asks first. */
	confirm?: (s: Session) => Confirm;
	/** Does it; what to tell the owner afterwards, if anything. */
	run: (s: Session) => Promise<string | void>;
};

const running = (s: Session) => s.activity !== 'gone';
const online = (s: Session) => live.connected(s.host);
const shortId = (s: Session) => s.short_id ?? s.id.slice(0, 8);

async function put(call: Promise<{ session: Session }>) {
	live.put((await call).session);
}

export const ACTIONS: SessionAction[] = [
	{
		id: 'open',
		label: (s) => `Open on ${s.host}`,
		icon: MonitorUpIcon,
		offered: (s) => running(s) && online(s) && isBackground(s) && live.features(s.host).attach,
		run: async (s) => {
			await put(openSession(s.id));
			return `Opening on ${s.host}.`;
		}
	},
	{
		id: 'interrupt',
		label: () => 'Interrupt',
		icon: OctagonXIcon,
		offered: (s) => running(s) && online(s) && s.hooked && busy(s),
		blocked: (s) => (s.interrupting ? 'Asked already: Claude stops at its next step.' : null),
		run: async (s) => {
			await put(interruptSession(s.id));
			return 'Claude stops at its next step.';
		}
	},
	{
		id: 'resolve',
		label: () => 'Resolved',
		icon: CircleCheckIcon,
		offered: (s) =>
			running(s) && online(s) && s.hooked && !s.resolved && live.features(s.host).channel,
		blocked: (s) =>
			s.channel
				? null
				: s.asleep
					? 'Asleep: send it a message first, or kill it.'
					: 'Runs without the ccdash channel: wrap it up in its terminal.',
		confirm: (s) => ({
			title: `Resolve “${label(s)}”?`,
			body:
				'Claude gets one last prompt to wrap up with the ccdash wrap-up skill: issues, ' +
				'progress and memories recorded, its claim released. The session closes when that ' +
				'turn ends; its conversation is kept.' +
				(s.activity === 'working' || s.activity === 'permission'
					? ' It is working now, so the wrap-up comes after this turn.'
					: ''),
			button: 'Resolve'
		}),
		run: async (s) => {
			await put(resolveSession(s.id));
			return 'Wrapping up; it closes when that turn ends.';
		}
	},
	{
		id: 'keep',
		label: () => 'Keep open',
		icon: LockOpenIcon,
		offered: (s) => running(s) && Boolean(s.resolved),
		run: async (s) => {
			await put(keepSessionOpen(s.id));
			return 'It stays open after its wrap-up.';
		}
	},
	{
		id: 'continue',
		label: () => 'Resume…',
		icon: PlayIcon,
		offered: (s) => !running(s) && Boolean(s.workspace),
		run: async (s) => starter.show({ resume: resumableSession(s) })
	},
	{
		id: 'switch',
		label: () => 'Switch model & effort…',
		icon: RepeatIcon,
		// NOTE: a running session takes neither from outside: its sheet stops it and continues a copy
		offered: (s) =>
			running(s) &&
			online(s) &&
			isBackground(s) &&
			Boolean(s.workspace) &&
			live.features(s.host).launch,
		run: async (s) => starter.show({ resume: resumableSession(s), switching: true })
	},
	{
		id: 'kill',
		label: () => 'Kill',
		icon: CircleOffIcon,
		destructive: true,
		offered: (s) => running(s) && online(s),
		confirm: (s) => ({
			title: `Kill “${label(s)}” on ${s.host}?`,
			body: isBackground(s)
				? `Runs claude stop ${shortId(s)} on ${s.host}. The conversation is kept: continue it later from its page.`
				: `Sends SIGTERM to its Claude process${s.pid ? ` (pid ${s.pid})` : ''} on ${s.host}: ` +
					'the session in that terminal ends. The conversation is kept: continue it later from its page.',
			button: 'Kill'
		}),
		run: (s) => put(stopSession(s.id))
	},
	{
		id: 'forget',
		label: () => 'Forget',
		icon: EraserIcon,
		offered: (s) => running(s) && !online(s),
		confirm: (s) => ({
			title: `Forget “${label(s)}” on ${s.host}?`,
			body:
				`${s.host} is offline, so nothing can stop the session from here. Forgetting marks it ` +
				`ended on the hub; it comes back if ${s.host} reports it running again.`,
			button: 'Forget'
		}),
		run: (s) => put(forgetSession(s.id))
	}
];

/** Where a resolved session stands, for a line on its card and page. */
export function resolvedNote(s: Session): string | null {
	if (!s.resolved || s.activity === 'gone') return null;
	if (!s.resolved.wrapping_up) return 'Resolved: the wrap-up prompt waits for Claude';
	return s.activity === 'idle' ? 'Resolved: closing' : 'Resolved: wrapping up, then it closes';
}
