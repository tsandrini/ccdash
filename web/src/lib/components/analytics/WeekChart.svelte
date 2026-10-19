<script lang="ts">
	// A weekly limit since its window opened, with the recent pace carried
	// on to the reset when there is one.
	import { Line, Text } from 'layerchart';
	import type { Analytics, Forecast } from '#lib/api/types.js';
	import { outlook } from '#lib/analytics.js';
	import { COLOR, fmtWhen, pct } from '#lib/charts.js';
	import { fmtStamp } from '#lib/format.js';
	import { rangeState } from '#lib/range.svelte.js';
	import Figure from '../charts/Figure.svelte';
	import TimeLines from '../charts/TimeLines.svelte';

	let {
		title,
		name,
		forecast,
		points,
		data
	}: {
		title: string;
		name: string;
		forecast: Forecast | null;
		points: [number, number][];
		data: Analytics;
	} = $props();

	const since = $derived(
		forecast ? forecast.resets_at - forecast.window_ms : data.now - 7 * 86_400_000
	);
	const until = $derived(forecast ? forecast.resets_at : data.now);
	const at = $derived(Object.fromEntries(points.map(([t, v]) => [t, v])));
	const o = $derived(forecast ? outlook(forecast, rangeState.now()) : null);
	const projection = $derived.by(() => {
		if (!forecast || !o || o.at_reset === null) return null;
		const end = o.full_at ?? forecast.resets_at;
		const value = o.full_at ? 100 : o.at_reset;
		return {
			end,
			value: Math.min(value, 100),
			text: o.full_at ? `full ${fmtWhen(end, rangeState.now())}` : `${pct(value)} at reset`
		};
	});
</script>

<Figure
	{title}
	sub={forecast
		? `${name} limit used, with its pace carried on to the reset`
		: `${name} limit used over the last 7 days`}
	empty={!points.length && 'No readings yet.'}
	table={{
		headers: ['Hour', `${name} limit`],
		rows: points.map(([t, v]) => [fmtStamp(t), pct(v)])
	}}
>
	<TimeLines
		series={[{ key: 'limit', label: name, color: COLOR.working, points, area: true }]}
		{since}
		{until}
		max={100}
		fmt={(v) => `${v}%`}
		tip={(t) => [pct(at[t] ?? 0), fmtStamp(t)]}
	>
		{#snippet extra({ x, y })}
			<Line x1={x(since)} x2={x(until)} y1={y(100)} y2={y(100)} class="stroke-muted-foreground" />
			{#if forecast && projection}
				<Line
					x1={x(data.now)}
					y1={y(Math.min(forecast.used_pct, 100))}
					x2={x(projection.end)}
					y2={y(projection.value)}
					stroke={COLOR.working}
					strokeWidth={1.5}
					dashArray="6 5"
				/>
				<Text
					x={x(projection.end) - 4}
					y={y(projection.value) - 6}
					value={projection.text}
					textAnchor="end"
					class="fill-foreground"
				/>
			{/if}
		{/snippet}
	</TimeLines>
</Figure>
