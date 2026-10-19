<script lang="ts">
	// Turns: what fills a turn (Claude, tools, permission prompts), tools and
	// shell commands by total time, the slowest calls, the commands that fail most.
	import type { Analytics } from '#lib/api/types.js';
	import { bucketEdges, bucketName, columns, hourly } from '#lib/analytics.js';
	import { COLOR, fmtHoursAxis, hostTag, pct, roleLegend } from '#lib/charts.js';
	import { fmtDuration, fmtStamp, plural, toolName } from '#lib/format.js';
	import { rangeState } from '#lib/range.svelte.js';
	import Columns from '../charts/Columns.svelte';
	import Figure from '../charts/Figure.svelte';
	import Note from '../charts/Note.svelte';
	import Pair from '../charts/Pair.svelte';
	import SortableTable from '../charts/SortableTable.svelte';
	import StatTile from '../charts/StatTile.svelte';
	import Tiles from '../charts/Tiles.svelte';
	import SessionRef from './SessionRef.svelte';
	import ToolStatChart from './ToolStatChart.svelte';

	let { data }: { data: Analytics } = $props();

	const brush = (from: number, to: number) =>
		rangeState.set({ kind: 'absolute', from, to: Math.min(to, rangeState.now()) });
	const a = $derived(data.anatomy);
	const total = $derived(a.claude_ms + a.tools_ms + a.permission_ms);
	const share = (ms: number) => (total ? pct((ms / total) * 100) : '–');
	const cols = $derived(columns(data));
	const edges = $derived(bucketEdges(data));
	const failing = $derived(a.failing.map((c) => ({ ...c, rate: c.failed / c.calls })));
</script>

<Tiles>
	<StatTile name="Turns" value={a.turns} sub="finished in this range" />
	<StatTile
		name="Median turn"
		value={data.turns.median_ms === null ? '–' : fmtDuration(data.turns.median_ms)}
		sub="prompt to reply"
	/>
	<StatTile
		name="Claude"
		value={share(a.claude_ms)}
		sub="{fmtDuration(a.claude_ms)} thinking and writing"
	/>
	<StatTile name="Tools" value={share(a.tools_ms)} sub="{fmtDuration(a.tools_ms)} running" />
	<StatTile
		name="Permission prompts"
		value={share(a.permission_ms)}
		sub="{fmtDuration(a.permission_ms)} waiting on you"
	/>
</Tiles>
<Figure
	title="Where turn time goes"
	sub="Finished turns, by the {hourly(data) ? 'hour' : 'day'} they started"
	legend={roleLegend([
		['tools', 'Tools'],
		['working', 'Claude'],
		['permission', 'Permission prompt']
	])}
	table={{
		headers: ['Day', 'Turns', 'Claude', 'Tools', 'Permission'],
		rows: data.buckets.map((d) => [
			bucketName(data, d.start),
			d.turns,
			fmtDuration(d.turn_claude_ms),
			fmtDuration(d.turn_tools_ms),
			fmtDuration(d.turn_permission_ms)
		])
	}}
>
	<Columns
		data={cols}
		series={[
			{
				key: 'tools',
				label: 'Tools',
				color: COLOR.tools,
				value: (d) => d.b.turn_tools_ms / 3_600_000
			},
			{
				key: 'claude',
				label: 'Claude',
				color: COLOR.working,
				value: (d) => d.b.turn_claude_ms / 3_600_000
			},
			{
				key: 'permission',
				label: 'Permission prompt',
				color: COLOR.permission,
				value: (d) => d.b.turn_permission_ms / 3_600_000
			}
		]}
		fmtY={fmtHoursAxis}
		onbrush={brush}
		{edges}
		tip={(d) => [
			fmtDuration(d.b.turn_tools_ms + d.b.turn_claude_ms + d.b.turn_permission_ms),
			`${plural(d.b.turns, 'turn')} · ${bucketName(data, d.b.start)}`,
			`tools ${fmtDuration(d.b.turn_tools_ms)} · Claude ${fmtDuration(d.b.turn_claude_ms)} · permission ${fmtDuration(d.b.turn_permission_ms)}`
		]}
	/>
</Figure>
<Pair>
	<ToolStatChart
		title="Tools by total time"
		sub="Main conversation"
		stats={a.tools}
		label={toolName}
	/>
	<ToolStatChart
		title="Commands by total time"
		sub="Shell commands, wrappers like direnv exec left out"
		stats={a.commands}
		label={(n) => n}
	/>
</Pair>
<Figure
	title="Slowest calls"
	sub="Single tool calls that took longest"
	empty={!a.slowest.length && 'No tool calls in this range.'}
>
	<ol class="divide-y text-sm">
		{#each a.slowest as c, i (i)}
			<li class="flex gap-3 py-2">
				<span class="min-w-[6ch] font-medium tabular-nums">{fmtDuration(c.ms)}</span>
				<span class="min-w-0 flex-1">
					<code class="rounded bg-muted px-1 py-0.5 font-mono text-xs break-all"
						>{c.summary || toolName(c.tool)}</code
					>
					<span class="block text-xs text-muted-foreground"
						>{toolName(c.tool)}{c.ok ? '' : ' · failed'} · <SessionRef
							id={c.session}
							text={c.label}
						/>{hostTag(data, c.host)} · {fmtStamp(c.ts)}</span
					>
				</span>
			</li>
		{/each}
	</ol>
</Figure>
<Figure
	title="Commands that fail most"
	sub="Shell commands by failed runs"
	empty={!a.failing.length && 'No failed commands in this range.'}
>
	<SortableTable
		columns={[
			{ key: 'name', label: 'Command', text: true },
			{ key: 'failed', label: 'Failed' },
			{ key: 'calls', label: 'Runs' },
			{ key: 'rate', label: 'Rate', fmt: (v) => pct(Number(v) * 100) },
			{ key: 'last_failed_at', label: 'Last failed', fmt: (v) => (v ? fmtStamp(Number(v)) : '–') }
		]}
		rows={failing}
		sortKey="failed"
	/>
</Figure>
<Note
	text="Claude's share is a turn's time outside tools and permission prompts: thinking, writing, and waiting on the API."
/>
