<script lang="ts">
	// Context: compactions and how full sessions got.
	import type { Analytics } from '#lib/api/types.js';
	import { bucketEdges, bucketName, columns, hourly, PEAK_LABELS } from '#lib/analytics.js';
	import { COLOR, fmtCount, pct, spansHosts, type SortColumn } from '#lib/charts.js';
	import { place, plural } from '#lib/format.js';
	import { rangeState } from '#lib/range.svelte.js';
	import Columns from '../charts/Columns.svelte';
	import Figure from '../charts/Figure.svelte';
	import Pair from '../charts/Pair.svelte';
	import SortableTable from '../charts/SortableTable.svelte';
	import StatTile from '../charts/StatTile.svelte';
	import Tiles from '../charts/Tiles.svelte';

	let { data }: { data: Analytics } = $props();

	const brush = (from: number, to: number) =>
		rangeState.set({ kind: 'absolute', from, to: Math.min(to, rangeState.now()) });
	const c = $derived(data.context);
	const cols = $derived(columns(data));
	const edges = $derived(bucketEdges(data));
	const peaks = $derived(
		c.peaks.map((n, i) => ({ key: PEAK_LABELS[i], label: PEAK_LABELS[i], n }))
	);
	const sessions = $derived(
		c.sessions.map((x) => ({ ...x, where: place(x.cwd), peak: x.peak_pct }))
	);
	const sessionColumns = $derived.by(() => {
		const cols: SortColumn<Record<string, unknown>>[] = [
			{ key: 'label', label: 'Session', text: true }
		];
		if (spansHosts(data)) cols.push({ key: 'host', label: 'Host', text: true });
		cols.push(
			{ key: 'where', label: 'Project', text: true },
			{ key: 'compactions', label: 'Compactions' },
			{ key: 'peak', label: 'Peak', fmt: (v) => (v === null ? '–' : pct(Number(v))) }
		);
		return cols;
	});
</script>

<Tiles>
	<StatTile name="Compactions" value={c.compactions} sub="{c.compactions_auto} automatic" />
	<StatTile name="Sessions compacted" value={c.compacted_sessions} sub="compacted at least once" />
	<StatTile
		name="Median peak context"
		value={c.median_peak === null ? '–' : pct(c.median_peak)}
		sub="highest use a session reached"
	/>
	<StatTile name="Past 90%" value={c.peaks[4]} sub="sessions that nearly filled the window" />
</Tiles>
<Pair>
	<Figure
		title={hourly(data) ? 'Compactions per hour' : 'Compactions per day'}
		sub="Automatic and manual (/compact)"
		table={{
			headers: ['Day', 'Compactions', 'Automatic'],
			rows: data.buckets.map((d) => [bucketName(data, d.start), d.compactions, d.compactions_auto])
		}}
	>
		<Columns
			data={cols}
			series={[
				{
					key: 'compactions',
					label: 'Compactions',
					color: COLOR.neutral,
					value: (d) => d.b.compactions
				}
			]}
			fmtY={fmtCount}
			height={170}
			onbrush={brush}
			{edges}
			tip={(d) => [
				plural(d.b.compactions, 'compaction'),
				`${d.b.compactions_auto} automatic · ${bucketName(data, d.b.start)}`
			]}
		/>
	</Figure>
	<Figure
		title="Peak context per session"
		sub="Sessions by the highest context use they reached"
		table={{ headers: ['Peak context', 'Sessions'], rows: peaks.map((p) => [p.label, p.n]) }}
	>
		<Columns
			data={peaks}
			series={[{ key: 'n', label: 'Sessions', color: COLOR.working, value: (d) => d.n }]}
			fmtY={fmtCount}
			height={170}
			tip={(d) => [plural(d.n, 'session'), d.label]}
		/>
	</Figure>
</Pair>
<Figure
	title="Sessions that compacted most"
	sub="Then the fullest"
	empty={!c.sessions.length && 'No sessions in this range.'}
>
	<SortableTable columns={sessionColumns} rows={sessions} sortKey="compactions" />
</Figure>
