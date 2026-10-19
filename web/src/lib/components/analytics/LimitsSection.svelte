<script lang="ts">
	// Limits: the 5-hour and 7-day limits and the per-model weekly ones,
	// with their pace and where they end up at the reset; the peak of each
	// 5-hour window; each workspace's share of the week.
	import type { Analytics, Forecast } from '#lib/api/types.js';
	import { hourly, outlook, paceText } from '#lib/analytics.js';
	import { COLOR, fmtWhen, pct, shortDay } from '#lib/charts.js';
	import { fmtStamp, fmtTime, fmtUsd } from '#lib/format.js';
	import { rangeState } from '#lib/range.svelte.js';
	import BarRows from '../charts/BarRows.svelte';
	import Columns from '../charts/Columns.svelte';
	import Figure from '../charts/Figure.svelte';
	import Note from '../charts/Note.svelte';
	import Pair from '../charts/Pair.svelte';
	import StatTile from '../charts/StatTile.svelte';
	import Tiles from '../charts/Tiles.svelte';
	import WeekChart from './WeekChart.svelte';

	let { data }: { data: Analytics } = $props();

	const l = $derived(data.limits);
	const week = $derived(l.seven_day);
	const days = $derived(week ? (week.resets_at - data.now) / 86_400_000 : 0);
	const hot = $derived(l.windows.filter((w) => w.peak >= 90).length);
	const windows = $derived(
		l.windows.map((w) => ({
			key: String(w.start),
			label: hourly(data) ? fmtTime(w.start) : shortDay.format(w.start),
			w
		}))
	);
	const total = $derived(l.shares.reduce((n, sh) => n + sh.cost_usd, 0));
	const shares = $derived(
		l.shares.map((sh) => ({
			label: sh.other ? 'Other workspaces' : (sh.workspace ?? 'Outside the workspaces'),
			cost: sh.cost_usd,
			total: sh.cost_usd,
			parts: [[sh.cost_usd, COLOR.neutral]] as [number, string][]
		}))
	);

	/** A limit's tiles: its reading, and where it is heading. */
	function forecast(name: string, f: Forecast | null) {
		if (!f) return [{ name: `${name} limit`, value: '–', sub: 'no live reading yet' }];
		const o = outlook(f, rangeState.now());
		const ahead =
			o.full_at !== null
				? {
						name: 'By the reset',
						value: 'full',
						sub: `at ${fmtWhen(o.full_at, rangeState.now())}; ${paceText(f)}`
					}
				: {
						name: 'By the reset',
						value: o.at_reset === null ? '–' : pct(o.at_reset),
						sub: o.at_reset === null ? paceText(f) : `at ${paceText(f)}`
					};
		return [
			{
				name: `${name} limit`,
				value: pct(f.used_pct),
				sub: `resets ${fmtWhen(f.resets_at, rangeState.now())}`
			},
			ahead
		];
	}
	const tiles = $derived.by(() => {
		const out = [...forecast('7-day', week)];
		if (week)
			out.push({
				name: 'Daily budget',
				value: days > 0 ? `${((100 - week.used_pct) / Math.max(days, 1)).toFixed(1)}%` : '–',
				sub: 'a day, to last until the reset'
			});
		for (const sc of l.scoped) out.push(...forecast(sc.label, sc.forecast));
		out.push(...forecast('5-hour', l.five_hour));
		out.push({
			name: 'Windows near the limit',
			value: l.windows.length ? `${hot} of ${l.windows.length}` : '–',
			sub: '5-hour windows that passed 90% in this range'
		});
		return out;
	});
</script>

<Tiles>
	{#each tiles as t, i (i)}
		<StatTile name={t.name} value={t.value} sub={t.sub} />
	{/each}
</Tiles>
<WeekChart title="This week's limit" name="7-day" forecast={week} points={l.week} {data} />
{#each l.scoped as sc (sc.label)}
	<WeekChart
		title="{sc.label} this week"
		name={sc.label}
		forecast={sc.forecast}
		points={sc.week}
		{data}
	/>
{/each}
<Pair>
	<Figure
		title="5-hour windows"
		sub="Highest reading in each window, in this range"
		empty={!l.windows.length && 'No readings in this range.'}
		table={{
			headers: ['Window', 'Highest'],
			rows: l.windows.map((w) => [`${fmtStamp(w.start)} – ${fmtTime(w.end)}`, pct(w.peak)])
		}}
	>
		<Columns
			data={windows}
			series={[{ key: 'peak', label: 'Highest', color: COLOR.neutral, value: (d) => d.w.peak }]}
			fmtY={(v) => `${v}%`}
			height={170}
			tip={(d) => [pct(d.w.peak), `${fmtStamp(d.w.start)} – ${fmtTime(d.w.end)}`]}
		/>
	</Figure>
	<Figure
		title="This week by workspace"
		sub="API-equivalent cost since the 7-day window opened, a stand-in for the share of the limit"
		empty={!(total > 0) && 'No cost data this week.'}
		table={{
			headers: ['Workspace', 'Cost', 'Share'],
			rows: shares.map((r) => [r.label, fmtUsd(r.cost), pct((r.cost / total) * 100)])
		}}
	>
		<BarRows
			rows={shares}
			valueText={(row) => `${pct((row.cost / total) * 100)} · ${fmtUsd(row.cost)}`}
			tip={(row) => [fmtUsd(row.cost), row.label]}
		/>
	</Figure>
</Pair>
<Note
	text="Limits are account-wide: the workspace filter doesn't apply (the workspaces it leaves out share one row in &quot;This week by workspace&quot;), and only the 5-hour windows follow the time range.{l
		.scoped.length
		? ` ${l.scoped.map((sc) => sc.label).join(' and ')} readings come from /usage every 10 minutes.`
		: ''}"
/>
