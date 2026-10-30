// The Workspace page's vocabulary: unit states with their colour and
// label, unit ids as links.

import type { UnitState, UnitStatus, UnitView } from './api/types';

export const STATE: Record<UnitState, { label: string; dot: string; busy?: boolean }> = {
	in_step: { label: 'in step', dot: 'bg-your-turn' },
	ahead: { label: 'sending', dot: 'bg-working', busy: true },
	behind: { label: 'receiving', dot: 'bg-working', busy: true },
	merging: { label: 'merging', dot: 'bg-working', busy: true },
	conflict: { label: 'conflict', dot: 'bg-critical' },
	held: { label: 'held', dot: 'bg-permission' },
	paused: { label: 'paused', dot: 'bg-idle' },
	skipped: { label: 'waiting', dot: 'bg-parked' },
	waiting: { label: 'waiting', dot: 'bg-parked' },
	failed: { label: 'failing', dot: 'bg-critical' },
	seeding: { label: 'seeding', dot: 'bg-working', busy: true },
	unadopted: { label: 'to adopt', dot: 'bg-permission' },
	creating: { label: 'creating', dot: 'bg-working', busy: true },
	missing: { label: 'missing', dot: 'bg-idle' }
};

/** States that call for the owner. */
export const ATTENTION: UnitState[] = ['conflict', 'held', 'failed', 'unadopted'];

/** The unit page. */
export const unitHref = (unit: string) =>
	`/workspace/${unit.split('/').map(encodeURIComponent).join('/')}`;

/** The first path component of a unit id. */
export const bundleOf = (unit: string) => unit.split('/')[0] ?? unit;

/** The unit's name within its bundle (the bundle root is `.`). */
export const unitName = (unit: string) => {
	const i = unit.indexOf('/');
	return i < 0 ? '.' : unit.slice(i + 1);
};

/** The worst state across a unit's hosts, for the row's chip and the filters. */
export function worst(u: UnitView): UnitState | null {
	const states = Object.values(u.hosts).map((s: UnitStatus) => s.state);
	if (!states.length) return null;
	for (const s of ATTENTION) if (states.includes(s)) return s;
	for (const s of ['seeding', 'creating', 'merging', 'ahead', 'behind'] as UnitState[])
		if (states.includes(s)) return s;
	for (const s of ['skipped', 'waiting', 'missing', 'paused'] as UnitState[])
		if (states.includes(s)) return s;
	return 'in_step';
}

/** A short commit id. */
export const short = (sha: string | null | undefined) => (sha ? sha.slice(0, 8) : '—');
