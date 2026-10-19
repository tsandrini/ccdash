<script lang="ts">
	// Overview: the totals, the By host table, activity per bucket, when
	// you prompt, turn and wait histograms, cost, lines, memory and CPU.
	import type { Analytics } from '#lib/api/types.js';
	import { bucketEdges, bucketName, columns, hourly, sum, type Col } from '#lib/analytics.js';
	import {
		byteSeries,
		COLOR,
		compact,
		fmtCpu,
		fmtHoursAxis,
		fmtSigned,
		fmtTokens,
		STATES,
		stateLegend,
		type SortColumn
	} from '#lib/charts.js';
	import { fmtBytes, fmtDuration, fmtStamp, fmtUsd, place, plural } from '#lib/format.js';
	import { rangeState } from '#lib/range.svelte.js';
	import { ACTIVITY } from '#lib/session.js';
	import BarRows from '../charts/BarRows.svelte';
	import Columns from '../charts/Columns.svelte';
	import Figure from '../charts/Figure.svelte';
	import Heatmap from '../charts/Heatmap.svelte';
	import HostsTable from '../charts/HostsTable.svelte';
	import Pair from '../charts/Pair.svelte';
	import StatTile from '../charts/StatTile.svelte';
	import Tiles from '../charts/Tiles.svelte';
	import TimeLines from '../charts/TimeLines.svelte';
	import Histogram from './Histogram.svelte';

	let { data }: { data: Analytics } = $props();

	const brush = (from: number, to: number) =>
		rangeState.set({ kind: 'absolute', from, to: Math.min(to, rangeState.now()) });
	const cols = $derived(columns(data));
	const edges = $derived(bucketEdges(data));
	const peak = $derived(Math.max(0, ...data.buckets.map((d) => d.peak_mem_bytes)));
	const unit = $derived(hourly(data) ? 'hour' : 'day');
	const hostColumns = $derived.by(() => {
		const cols: SortColumn<Record<string, unknown>>[] = [
			{ key: 'host', label: 'Host', text: true },
			{ key: 'sessions', label: 'Sessions' },
			{ key: 'working_ms', label: 'Working', fmt: (v) => fmtDuration(Number(v)) },
			{ key: 'waited', label: 'Waited', fmt: (v) => fmtDuration(Number(v)) },
			{ key: 'parked_ms', label: 'Parked', fmt: (v) => fmtDuration(Number(v)) },
			{ key: 'prompts', label: 'Prompts' },
			{
				key: 'answer_p50_ms',
				label: 'Answer time',
				fmt: (v, r) => (r.answers ? fmtDuration(Number(v)) : '–')
			},
			{ key: 'cost_usd', label: 'Cost', fmt: (v) => fmtUsd(Number(v)) },
			{
				key: 'lines',
				label: 'Lines',
				fmt: (_, r) =>
					`+${compact.format(Number(r.lines_added))} −${compact.format(Number(r.lines_removed))}`
			},
			{
				key: 'tools',
				label: 'Tool calls',
				fmt: (v, r) => (r.tool_failures ? `${v} (${r.tool_failures} failed)` : String(v))
			}
		];
		if (data.telemetry) {
			cols.push(
				{ key: 'tokens', label: 'Tokens', fmt: (v) => fmtTokens(Number(v)) },
				{ key: 'api_requests', label: 'API requests' }
			);
		}
		cols.push({ key: 'cpu_secs', label: 'CPU', fmt: (v) => fmtCpu(Number(v)) });
		return cols;
	});
	const models = $derived(
		data.models
			.filter((m) => m.cost_usd > 0)
			.map((m) => ({
				...m,
				label: m.model,
				total: m.cost_usd,
				parts: [[m.cost_usd, COLOR.neutral]] as [number, string][]
			}))
	);
	const cpu = $derived(
		data.projects
			.filter((p) => p.cpu_secs > 0)
			.sort((a, b) => b.cpu_secs - a.cpu_secs)
			.slice(0, 8)
			.map((p) => ({
				...p,
				label: place(p.cwd),
				total: p.cpu_secs,
				parts: [[p.cpu_secs, COLOR.neutral]] as [number, string][]
			}))
	);
	const memory = $derived(byteSeries(data.memory));
	const memoryAt = $derived(Object.fromEntries(memory.points.map(([t, v]) => [t, v])));
</script>

<Tiles>
	<StatTile name="Prompts" value={sum(data, 'prompts')} sub={plural(data.sessions, 'session')} />
	<StatTile name="Working time" value={fmtDuration(sum(data, 'working_ms'))} sub="Claude busy" />
	<StatTile
		name="Waited on you"
		value={fmtDuration(sum(data, 'permission_ms') + sum(data, 'your_turn_ms'))}
		sub="{fmtDuration(sum(data, 'parked_ms'))} parked, not counted"
	/>
	<StatTile
		name="API-equivalent cost"
		value={fmtUsd(sum(data, 'cost_usd'))}
		sub="from statusline"
	/>
	<StatTile
		name="Lines changed"
		value="+{compact.format(sum(data, 'lines_added'))} −{compact.format(
			sum(data, 'lines_removed')
		)}"
		sub="added / removed"
	/>
	<StatTile name="Peak memory" value={peak ? fmtBytes(peak) : '–'} sub="all sessions at once" />
</Tiles>
<HostsTable
	hosts={data.hosts}
	title="By host"
	sub="Each machine's share; the Host filter above narrows every section to one"
	columns={hostColumns}
/>
<Figure
	title={hourly(data) ? 'Hourly activity' : 'Daily activity'}
	sub="Time each {unit} that sessions were working or waiting on you"
	legend={stateLegend(STATES)}
	table={{
		headers: ['Day', 'Working', 'Needs permission', 'Your turn', 'Prompts'],
		rows: data.buckets.map((d) => [
			bucketName(data, d.start),
			fmtDuration(d.working_ms),
			fmtDuration(d.permission_ms),
			fmtDuration(d.your_turn_ms),
			d.prompts
		])
	}}
>
	<Columns
		data={cols}
		series={STATES.map((a) => ({
			key: a,
			label: ACTIVITY[a].label,
			color: COLOR[a],
			value: (d: Col) =>
				d.b[`${a}_ms` as 'working_ms' | 'permission_ms' | 'your_turn_ms'] / 3_600_000
		}))}
		fmtY={fmtHoursAxis}
		onbrush={brush}
		{edges}
		tip={(d) => [
			fmtDuration(d.b.working_ms + d.b.permission_ms + d.b.your_turn_ms),
			bucketName(data, d.b.start),
			`working ${fmtDuration(d.b.working_ms)} · permission ${fmtDuration(d.b.permission_ms)} · your turn ${fmtDuration(d.b.your_turn_ms)}`
		]}
	/>
</Figure>
<Figure
	title="When you prompt"
	sub="Prompts by weekday and hour"
	empty={!data.heatmap.flat().some(Boolean) && 'No prompts in this range.'}
	table={{
		headers: ['Day', ...Array.from({ length: 24 }, (_, h) => String(h))],
		rows: data.heatmap.map((row, day) => [
			['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'][day],
			...row
		])
	}}
>
	<Heatmap matrix={data.heatmap} />
</Figure>
<Pair>
	<Histogram title="How long Claude works per prompt" hist={data.turns} role="working" />
	<Histogram title="How long sessions wait for you" hist={data.responses} role="your_turn" />
</Pair>
<Pair>
	<Figure
		title={hourly(data) ? 'Cost per hour' : 'Cost per day'}
		sub="API-equivalent, from statusline totals"
		table={{
			headers: ['Day', 'Cost'],
			rows: data.buckets.map((d) => [bucketName(data, d.start), fmtUsd(d.cost_usd)])
		}}
	>
		<Columns
			data={cols}
			series={[{ key: 'cost', label: 'Cost', color: COLOR.neutral, value: (d) => d.b.cost_usd }]}
			fmtY={(v) => `$${v}`}
			height={170}
			onbrush={brush}
			{edges}
			tip={(d) => [fmtUsd(d.b.cost_usd), bucketName(data, d.b.start)]}
		/>
	</Figure>
	<Figure
		title="Cost by model"
		sub="Model shown in the statusline"
		empty={!models.length && 'No cost data yet.'}
		table={{
			headers: ['Model', 'Cost', 'Sessions'],
			rows: models.map((r) => [r.label, fmtUsd(r.cost_usd), r.sessions])
		}}
	>
		<BarRows
			rows={models}
			valueText={(row) => fmtUsd(row.cost_usd)}
			tip={(row) => [fmtUsd(row.cost_usd), row.label, plural(row.sessions, 'session')]}
		/>
	</Figure>
</Pair>
<Figure
	title={hourly(data) ? 'Lines changed per hour' : 'Lines changed per day'}
	sub="Added above the line, removed below"
	table={{
		headers: ['Day', 'Added', 'Removed'],
		rows: data.buckets.map((d) => [bucketName(data, d.start), d.lines_added, d.lines_removed])
	}}
>
	<Columns
		data={cols}
		series={[
			{ key: 'added', label: 'Added', color: COLOR.neutral, value: (d) => d.b.lines_added },
			{ key: 'removed', label: 'Removed', color: COLOR.neutral, value: (d) => -d.b.lines_removed }
		]}
		fmtY={fmtSigned}
		height={170}
		onbrush={brush}
		{edges}
		tip={(d) => [
			`+${d.b.lines_added} added`,
			`−${d.b.lines_removed} removed`,
			bucketName(data, d.b.start)
		]}
	/>
</Figure>
<Pair>
	<Figure
		title="Memory"
		sub="All session process trees together (PSS), average per bucket"
		empty={memory.points.length < 2 && 'Not enough data yet.'}
		table={{
			headers: ['Time', 'Memory'],
			rows: memory.points.map(([t, v]) => [fmtStamp(t), memory.fmt(v)])
		}}
	>
		<TimeLines
			series={[
				{ key: 'mem', label: 'Memory', color: COLOR.working, points: memory.points, area: true }
			]}
			since={data.since}
			until={data.until}
			fmt={memory.fmt}
			floorMax={memory.floorMax}
			height={150}
			endLabel
			tip={(ts) => [memory.fmt(memoryAt[ts] ?? 0), fmtStamp(ts)]}
			onbrush={brush}
		/>
	</Figure>
	<Figure
		title="CPU by project"
		sub="Core-hours used by session process trees"
		empty={!cpu.length && 'No process data yet.'}
		table={{
			headers: ['Project', 'CPU', 'Peak memory'],
			rows: cpu.map((r) => [r.label, fmtCpu(r.cpu_secs), fmtBytes(r.peak_mem_bytes)])
		}}
	>
		<BarRows
			rows={cpu}
			valueText={(row) => fmtCpu(row.cpu_secs)}
			tip={(row) => [fmtCpu(row.cpu_secs), row.label, `peak ${fmtBytes(row.peak_mem_bytes)}`]}
		/>
	</Figure>
</Pair>
