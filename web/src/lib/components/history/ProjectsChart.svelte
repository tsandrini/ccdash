<script lang="ts">
	// Working and waiting time per directory, the long tail folded into "Other".
	import type { Activity, History } from '#lib/api/types.js';
	import { COLOR, STATES, stateLegend } from '#lib/charts.js';
	import { fmtDuration, place, plural } from '#lib/format.js';
	import { totals } from '#lib/history.js';
	import { ACTIVITY } from '#lib/session.js';
	import BarRows from '../charts/BarRows.svelte';
	import Figure from '../charts/Figure.svelte';

	let { data }: { data: History } = $props();

	type Row = {
		label: string;
		sessions: number;
		total: number;
		working: number;
		permission: number;
		your_turn: number;
		parts: [number, string][];
	};

	const rows = $derived.by(() => {
		const by: Record<string, Row> = {};
		for (const lane of data.lanes) {
			const key = place(lane.cwd);
			const t = totals(lane);
			const p = (by[key] ??= {
				label: key,
				sessions: 0,
				total: 0,
				working: 0,
				permission: 0,
				your_turn: 0,
				parts: []
			});
			for (const a of STATES) p[a as 'working' | 'permission' | 'your_turn'] += t[a];
			p.sessions += 1;
		}
		let rows = Object.values(by)
			.map((p) => ({ ...p, total: p.working + p.permission + p.your_turn }))
			.filter((p) => p.total > 0)
			.sort((a, b) => b.total - a.total);
		if (rows.length > 8) {
			const rest = rows.slice(7);
			const sum = (k: 'working' | 'permission' | 'your_turn' | 'sessions' | 'total') =>
				rest.reduce((n, p) => n + p[k], 0);
			rows = [
				...rows.slice(0, 7),
				{
					label: `Other (${rest.length})`,
					sessions: sum('sessions'),
					total: sum('total'),
					working: sum('working'),
					permission: sum('permission'),
					your_turn: sum('your_turn'),
					parts: []
				}
			];
		}
		for (const row of rows) row.parts = STATES.map((a) => [row[a as 'working'], COLOR[a]]);
		return rows;
	});
</script>

<Figure
	title="Time by project"
	sub="Working and waiting time per directory"
	legend={stateLegend(STATES)}
	empty={!rows.length && 'No session activity in this range.'}
	table={{
		headers: ['Project', 'Working', 'Needs permission', 'Your turn', 'Sessions'],
		rows: rows.map((r) => [
			r.label,
			fmtDuration(r.working),
			fmtDuration(r.permission),
			fmtDuration(r.your_turn),
			r.sessions
		])
	}}
>
	<BarRows
		{rows}
		valueText={(row) => fmtDuration(row.total)}
		tip={(row, value, part) => [
			fmtDuration(value),
			`${ACTIVITY[STATES[part] as Activity].label} · ${row.label}`,
			plural(row.sessions, 'session')
		]}
	/>
</Figure>
