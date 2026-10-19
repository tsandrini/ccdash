<script lang="ts">
	// Parallel: how many sessions worked or waited on you at once.
	import type { Analytics } from '#lib/api/types.js';
	import { COLOR, pct, roleLegend } from '#lib/charts.js';
	import { fmtDuration, fmtStamp, plural } from '#lib/format.js';
	import BarRows from '../charts/BarRows.svelte';
	import Figure from '../charts/Figure.svelte';
	import Note from '../charts/Note.svelte';
	import StatTile from '../charts/StatTile.svelte';
	import Tiles from '../charts/Tiles.svelte';
	import TimeLines from '../charts/TimeLines.svelte';

	let { data }: { data: Analytics } = $props();

	const p = $derived(data.parallel);
	const avg = $derived(p.busy_ms ? p.working_ms / p.busy_ms : 0);
	const queue = $derived(p.waiting_ms ? p.queued_ms / p.waiting_ms : 0);
	const bySlot = $derived(Object.fromEntries(p.series.map((row) => [row[0], row])));
	const spread = $derived(
		p.working_time
			.map((ms, i) => ({
				label: i === 5 ? '6 or more sessions' : plural(i + 1, 'session'),
				total: ms,
				parts: [[ms, COLOR.working]] as [number, string][]
			}))
			.filter((r) => r.total > 0)
	);
	const fmtCount = (v: number) => (Number.isInteger(v) ? String(v) : v.toFixed(1));
</script>

<Tiles>
	<StatTile
		name="Most at once"
		value={p.peak_working ? plural(p.peak_working, 'session') : '–'}
		sub="working at the same time"
	/>
	<StatTile
		name="Average at once"
		value={p.busy_ms ? avg.toFixed(1) : '–'}
		sub="sessions working, while any did"
	/>
	<StatTile name="Claude busy" value={fmtDuration(p.busy_ms)} sub="at least one session working" />
	<StatTile
		name="All waiting on you"
		value={fmtDuration(p.blocked_ms)}
		sub="sessions waiting, none working"
	/>
	<StatTile
		name="Waiting at once"
		value={p.waiting_ms ? queue.toFixed(1) : '–'}
		sub={p.peak_waiting ? `on average; ${p.peak_waiting} at most` : 'no waits'}
	/>
</Tiles>
<Figure
	title="Sessions at once"
	sub="Average working and waiting on you, per point"
	legend={roleLegend(
		[
			['working', 'Working'],
			['your_turn', 'Waiting on you']
		],
		true
	)}
	table={{
		headers: ['Time', 'Working', 'Most working', 'Waiting', 'Most waiting'],
		rows: p.series
			.filter(([, w, , q]) => w || q)
			.map(([t, w, wMax, q, qMax]) => [fmtStamp(t), w.toFixed(2), wMax, q.toFixed(2), qMax])
	}}
>
	<TimeLines
		series={[
			{
				key: 'working',
				label: 'Working',
				color: COLOR.working,
				points: p.series.map(([t, w]) => [t, w]),
				area: true
			},
			{
				key: 'waiting',
				label: 'Waiting on you',
				color: COLOR.your_turn,
				points: p.series.map(([t, , , q]) => [t, q])
			}
		]}
		since={data.since}
		until={data.until}
		fmt={fmtCount}
		tip={(t) => {
			const row = bySlot[t];
			if (!row) return [];
			const [, w, wMax, q, qMax] = row;
			return [
				`${w.toFixed(1)} working`,
				`${q.toFixed(1)} waiting on you`,
				`at most ${wMax} working, ${qMax} waiting`,
				fmtStamp(t)
			];
		}}
	/>
</Figure>
<Figure
	title="How many worked at once"
	sub="Time with exactly this many sessions working"
	empty={!spread.length && 'No work in this range.'}
	table={{
		headers: ['Sessions working', 'Time', 'Share'],
		rows: spread.map((r) => [r.label, fmtDuration(r.total), pct((r.total / p.busy_ms) * 100)])
	}}
>
	<BarRows
		rows={spread}
		valueText={(row) => `${fmtDuration(row.total)} · ${pct((row.total / p.busy_ms) * 100)}`}
		tip={(row) => [fmtDuration(row.total), row.label]}
	/>
</Figure>
<Note
	text="Parked sessions (no reply within an hour, or closed without one) don't count as waiting on you; see Waits."
/>
