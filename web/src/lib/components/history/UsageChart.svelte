<script lang="ts">
	// The 5-hour limit over the range: the highest reading per bucket.
	import type { History } from '#lib/api/types.js';
	import { COLOR, dayFmt, pct, timeTicks } from '#lib/charts.js';
	import { fmtDuration, fmtTime } from '#lib/format.js';
	import Figure from '../charts/Figure.svelte';
	import TimeLines from '../charts/TimeLines.svelte';

	let { data, onbrush }: { data: History; onbrush: (from: number, to: number) => void } = $props();

	const points = $derived(
		data.usage.filter((p): p is typeof p & { five_hour: number } => p.five_hour !== null)
	);
	const byTs = $derived(Object.fromEntries(points.map((p) => [p.ts, p])));
	const daily = $derived(timeTicks(data.since, data.until, 600).daily);
	const when = (ts: number) => `${daily ? `${dayFmt.format(ts)} ` : ''}${fmtTime(ts)}`;
</script>

<Figure
	title="5-hour limit used"
	sub="Highest reading per {fmtDuration(data.bucket_ms)}"
	empty={!points.length && 'No statusline data in this range yet.'}
	table={{
		headers: ['Time', '5-hour', '7-day'],
		rows: points.map((p) => [
			when(p.ts),
			pct(p.five_hour),
			p.seven_day !== null ? pct(p.seven_day) : '–'
		])
	}}
>
	<TimeLines
		series={[
			{
				key: 'five_hour',
				label: '5-hour limit',
				color: COLOR.working,
				points: points.map((p) => [p.ts, p.five_hour]),
				area: true
			}
		]}
		since={data.since}
		until={data.until}
		max={100}
		fmt={pct}
		height={160}
		endLabel
		tip={(ts) => {
			const p = byTs[ts];
			return p
				? [
						pct(p.five_hour),
						`5-hour limit · ${when(p.ts)}`,
						p.seven_day !== null ? `7-day limit ${pct(p.seven_day)}` : null
					]
				: [];
		}}
		{onbrush}
	/>
</Figure>
