<script lang="ts">
	// Projects: a sortable table per directory, and time and cost charts.
	import type { Analytics } from '#lib/api/types.js';
	import {
		COLOR,
		compact,
		fmtCpu,
		fmtTokens,
		STATES,
		stateLegend,
		type SortColumn
	} from '#lib/charts.js';
	import { fmtDuration, fmtUsd, place, plural } from '#lib/format.js';
	import { ACTIVITY } from '#lib/session.js';
	import BarRows from '../charts/BarRows.svelte';
	import Figure from '../charts/Figure.svelte';
	import Note from '../charts/Note.svelte';
	import Pair from '../charts/Pair.svelte';
	import SortableTable from '../charts/SortableTable.svelte';

	let { data }: { data: Analytics } = $props();

	type Row = {
		project: string;
		sessions: number;
		working: number;
		waiting: number;
		permission_ms: number;
		your_turn_ms: number;
		parked: number;
		prompts: number;
		cost: number;
		added: number;
		removed: number;
		lines: number;
		tools: number;
		failures: number;
		compactions: number;
		tokens: number;
		cpu: number;
	};

	const rows = $derived.by(() => {
		const by: Record<string, Row> = {};
		for (const p of data.projects) {
			const key = place(p.cwd);
			const row = (by[key] ??= {
				project: key,
				sessions: 0,
				working: 0,
				waiting: 0,
				permission_ms: 0,
				your_turn_ms: 0,
				parked: 0,
				prompts: 0,
				cost: 0,
				added: 0,
				removed: 0,
				lines: 0,
				tools: 0,
				failures: 0,
				compactions: 0,
				tokens: 0,
				cpu: 0
			});
			row.sessions += p.sessions;
			row.working += p.working_ms;
			row.permission_ms += p.permission_ms;
			row.your_turn_ms += p.your_turn_ms;
			row.waiting += p.permission_ms + p.your_turn_ms;
			row.parked += p.parked_ms;
			row.prompts += p.prompts;
			row.cost += p.cost_usd;
			row.added += p.lines_added;
			row.removed += p.lines_removed;
			row.lines = row.added + row.removed;
			row.tools += p.tools;
			row.failures += p.tool_failures;
			row.compactions += p.compactions;
			row.tokens += p.tokens;
			row.cpu += p.cpu_secs;
		}
		return Object.values(by).filter(
			(r) => r.working || r.waiting || r.prompts || r.cost || r.tools
		);
	});
	const columns = $derived.by(() => {
		const cols: SortColumn<Record<string, unknown>>[] = [
			{ key: 'project', label: 'Project', text: true },
			{ key: 'working', label: 'Working', fmt: (v) => fmtDuration(Number(v)) },
			{ key: 'waiting', label: 'Waited', fmt: (v) => fmtDuration(Number(v)) },
			{ key: 'parked', label: 'Parked', fmt: (v) => fmtDuration(Number(v)) },
			{ key: 'prompts', label: 'Prompts' },
			{ key: 'cost', label: 'Cost', fmt: (v) => fmtUsd(Number(v)) },
			{
				key: 'lines',
				label: 'Lines',
				fmt: (_, r) => `+${compact.format(Number(r.added))} −${compact.format(Number(r.removed))}`
			},
			{
				key: 'tools',
				label: 'Tool calls',
				fmt: (v, r) => (r.failures ? `${v} (${r.failures} failed)` : String(v))
			},
			{ key: 'compactions', label: 'Compactions' },
			{ key: 'sessions', label: 'Sessions' }
		];
		if (data.telemetry)
			cols.push({ key: 'tokens', label: 'Tokens', fmt: (v) => fmtTokens(Number(v)) });
		cols.push({ key: 'cpu', label: 'CPU', fmt: (v) => fmtCpu(Number(v)) });
		return cols;
	});
	const top = (key: 'working' | 'cost') =>
		[...rows]
			.sort((a, b) => b[key] - a[key])
			.filter((r) => r[key] > 0)
			.slice(0, 8);
	const timeRows = $derived(
		top('working').map((r) => ({
			...r,
			label: r.project,
			total: r.working + r.waiting,
			parts: [
				[r.working, COLOR.working],
				[r.permission_ms, COLOR.permission],
				[r.your_turn_ms, COLOR.your_turn]
			] as [number, string][]
		}))
	);
	const costRows = $derived(
		top('cost').map((r) => ({
			...r,
			label: r.project,
			total: r.cost,
			parts: [[r.cost, COLOR.neutral]] as [number, string][]
		}))
	);
</script>

{#if !rows.length}
	<p class="text-sm text-muted-foreground">No project activity in this range.</p>
{:else}
	<Figure title="Projects" sub="Per directory; select a column to sort by it">
		<SortableTable
			{columns}
			rows={rows as unknown as Record<string, unknown>[]}
			sortKey="working"
		/>
	</Figure>
	<Pair>
		<Figure title="Time by project" sub="Working and waiting on you" legend={stateLegend(STATES)}>
			<BarRows
				rows={timeRows}
				valueText={(row) => fmtDuration(row.total)}
				tip={(row, value, part) => [
					fmtDuration(value),
					`${ACTIVITY[STATES[part]].label} · ${row.label}`
				]}
			/>
		</Figure>
		<Figure
			title="Cost by project"
			sub="API-equivalent, from statusline totals"
			empty={!costRows.length && 'No cost data in this range.'}
		>
			<BarRows
				rows={costRows}
				valueText={(row) => fmtUsd(row.cost)}
				tip={(row) => [fmtUsd(row.cost), row.label, plural(row.prompts, 'prompt')]}
			/>
		</Figure>
	</Pair>
	<Note
		text="Waited counts waits on you only; parked time (no reply within an hour, or closed without one) has its own column."
	/>
{/if}
