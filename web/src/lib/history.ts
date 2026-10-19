// What the History page derives from the payload.

import type { Activity, Lane } from './api/types';

export type Totals = Record<Activity, number>;

/** Time per state in a lane's stretches. */
export function totals(lane: Lane): Totals {
	const out: Totals = { working: 0, permission: 0, your_turn: 0, parked: 0, idle: 0, gone: 0 };
	for (const [from, to, activity] of lane.segments) out[activity] += to - from;
	return out;
}
