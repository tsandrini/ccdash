<script lang="ts">
	// The History page: totals, the By host table, the lanes, time by
	// project, tools and the limit usage. Brushing any time chart narrows
	// the range.
	import type { History } from '#lib/api/types.js';
	import { COLOR, spansHosts, STATES, stateLegend } from '#lib/charts.js';
	import { fmtDuration, fmtUsd, plural } from '#lib/format.js';
	import { totals, type Totals } from '#lib/history.js';
	import { rangeState } from '#lib/range.svelte.js';
	import Figure from '../charts/Figure.svelte';
	import HostsTable from '../charts/HostsTable.svelte';
	import Lanes from '../charts/Lanes.svelte';
	import StatTile from '../charts/StatTile.svelte';
	import Tiles from '../charts/Tiles.svelte';
	import ProjectsChart from './ProjectsChart.svelte';
	import ToolsChart from './ToolsChart.svelte';
	import UsageChart from './UsageChart.svelte';

	let { data }: { data: History } = $props();

	const sum = $derived.by(() => {
		const out: Totals = { working: 0, permission: 0, your_turn: 0, parked: 0, idle: 0, gone: 0 };
		for (const lane of data.lanes)
			for (const [k, v] of Object.entries(totals(lane))) out[k as keyof Totals] += v;
		return out;
	});
	const brush = (from: number, to: number) => rangeState.set({ kind: 'absolute', from, to });
	const multi = $derived(spansHosts(data));
	void COLOR;
</script>

<Tiles>
	<StatTile
		name="Waited on you"
		value={fmtDuration(sum.permission + sum.your_turn)}
		sub={sum.parked
			? `total across sessions; ${fmtDuration(sum.parked)} parked apart`
			: 'total across sessions'}
	/>
	<StatTile name="Working time" value={fmtDuration(sum.working)} sub="total across sessions" />
	<StatTile name="Prompts" value={data.prompts} sub={plural(data.lanes.length, 'session')} />
	<StatTile name="API-equivalent cost" value={fmtUsd(data.cost_usd)} sub="from statusline totals" />
</Tiles>
<HostsTable
	hosts={data.hosts}
	title="By host"
	sub="Totals across the sessions on each machine"
	columns={[
		{ key: 'host', label: 'Host', text: true },
		{ key: 'sessions', label: 'Sessions' },
		{ key: 'working_ms', label: 'Working', fmt: (v) => fmtDuration(Number(v)) },
		{ key: 'waited', label: 'Waited', fmt: (v) => fmtDuration(Number(v)) },
		{ key: 'parked_ms', label: 'Parked', fmt: (v) => fmtDuration(Number(v)) },
		{ key: 'prompts', label: 'Prompts' },
		{ key: 'cost_usd', label: 'Cost', fmt: (v) => fmtUsd(Number(v)) }
	]}
/>
<Figure
	title="Session activity"
	sub="When each session was working or waiting on you. Parked: no reply within an hour, or the session was closed without one."
	legend={stateLegend([...STATES, 'parked', 'idle'])}
	empty={!data.lanes.length && 'No session activity in this range.'}
	table={{
		headers: [
			'Session',
			...(multi ? ['Host'] : []),
			'Working',
			'Needs permission',
			'Your turn',
			'Idle'
		],
		rows: data.lanes.map((lane) => {
			const t = totals(lane);
			return [
				lane.label,
				...(multi ? [lane.host] : []),
				fmtDuration(t.working),
				fmtDuration(t.permission),
				fmtDuration(t.your_turn),
				fmtDuration(t.idle)
			];
		})
	}}
>
	<Lanes {data} onbrush={brush} />
</Figure>
<ProjectsChart {data} />
<ToolsChart {data} />
<UsageChart {data} onbrush={brush} />
