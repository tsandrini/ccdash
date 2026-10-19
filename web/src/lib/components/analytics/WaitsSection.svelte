<script lang="ts">
	// Waits: how long sessions waited on you and how fast you replied,
	// apart from parked time; how each wait ended; the ones you came back
	// to late.
	import type { Analytics } from '#lib/api/types.js';
	import { bucketEdges, bucketName, columns, hourly, OUTCOMES, sum } from '#lib/analytics.js';
	import { COLOR, hostTag } from '#lib/charts.js';
	import { fmtDuration, fmtStamp, place, plural } from '#lib/format.js';
	import { MINUTE } from '#lib/range.js';
	import { rangeState } from '#lib/range.svelte.js';
	import Columns from '../charts/Columns.svelte';
	import Figure from '../charts/Figure.svelte';
	import Note from '../charts/Note.svelte';
	import Pair from '../charts/Pair.svelte';
	import SortableTable from '../charts/SortableTable.svelte';
	import StatTile from '../charts/StatTile.svelte';
	import Tiles from '../charts/Tiles.svelte';
	import Histogram from './Histogram.svelte';
	import SessionRef from './SessionRef.svelte';

	let { data }: { data: Analytics } = $props();

	const brush = (from: number, to: number) =>
		rangeState.set({ kind: 'absolute', from, to: Math.min(to, rangeState.now()) });
	const w = $derived(data.waiting);
	const parked = $derived(['came_back', 'finished', 'left_open'].map((k) => w[k as 'came_back']));
	const hour = $derived(`${Math.round(w.parked_after_ms / MINUTE)} min`);
	const cols = $derived(columns(data));
	const edges = $derived(bucketEdges(data));
	const anyAnswers = $derived(data.buckets.some((d) => d.answers));
	const outcomes = $derived(
		OUTCOMES.map(([key, label, what, counts]) => ({
			label,
			what,
			counts,
			waits: w[key].waits,
			ms: w[key].ms
		}))
	);
</script>

<Tiles>
	<StatTile
		name="Waited on you"
		value={fmtDuration(sum(data, 'permission_ms') + sum(data, 'your_turn_ms'))}
		sub="{plural(w.answered.waits + w.waiting.waits, 'wait')} and permission prompts"
	/>
	<StatTile
		name="Answer time"
		value={w.answer_p50_ms === null ? '–' : fmtDuration(w.answer_p50_ms)}
		sub={w.answer_p90_ms === null ? 'median' : `median; 90% within ${fmtDuration(w.answer_p90_ms)}`}
	/>
	<StatTile
		name="Parked"
		value={fmtDuration(parked.reduce((n, t) => n + t.ms, 0))}
		sub="{plural(
			parked.reduce((n, t) => n + t.waits, 0),
			'wait'
		)}, not counted"
	/>
	<StatTile name="Came back later" value={w.came_back.waits} sub="replied after {hour}" />
</Tiles>
<Pair>
	<Figure
		title={hourly(data) ? 'Answer time per hour' : 'Answer time per day'}
		sub="Median time from Claude handing over to your reply, for replies within {hour}"
		empty={!anyAnswers && 'No replies in this range.'}
		table={{
			headers: [hourly(data) ? 'Hour' : 'Day', 'Replies', 'Median'],
			rows: data.buckets.map((d) => [
				bucketName(data, d.start),
				d.answers,
				d.answers ? fmtDuration(d.answer_p50_ms ?? 0) : '–'
			])
		}}
	>
		<Columns
			data={cols}
			series={[
				{
					key: 'p50',
					label: 'Median',
					color: COLOR.your_turn,
					value: (d) => (d.b.answer_p50_ms ?? 0) / MINUTE
				}
			]}
			fmtY={(v) => (v === 0 ? '0' : fmtDuration(v * MINUTE))}
			height={170}
			onbrush={brush}
			{edges}
			tip={(d) => [
				d.b.answers ? fmtDuration(d.b.answer_p50_ms ?? 0) : '–',
				`median of ${plural(d.b.answers, 'reply', 'replies')} · ${bucketName(data, d.b.start)}`
			]}
		/>
	</Figure>
	<Histogram title="How long you took to reply" hist={data.responses} role="your_turn" />
</Pair>
<Figure
	title="How waits ended"
	sub="A wait runs from Claude handing the turn back to whatever came next; time inside this range"
>
	<SortableTable
		columns={[
			{ key: 'label', label: 'Outcome', text: true },
			{ key: 'what', label: 'What happened', text: true },
			{ key: 'waits', label: 'Waits' },
			{ key: 'ms', label: 'Time', fmt: (v) => fmtDuration(Number(v)) },
			{ key: 'counts', label: 'Counts as', text: true }
		]}
		rows={outcomes}
		sortKey="ms"
	/>
</Figure>
<Figure
	title="Came back later"
	sub="Waits you replied to after {hour}, longest first: parked and picked up again, or forgotten"
	empty={!w.late.length && 'None in this range.'}
>
	<ol class="divide-y text-sm">
		{#each w.late as l, i (i)}
			<li class="flex gap-3 py-2">
				<span class="min-w-[6ch] font-medium tabular-nums">{fmtDuration(l.ms)}</span>
				<span class="min-w-0 flex-1">
					<SessionRef id={l.session} text={l.label} />
					<span class="block text-xs text-muted-foreground"
						>{place(l.cwd)}{hostTag(data, l.host)} · handed over {fmtStamp(l.start)}</span
					>
				</span>
			</li>
		{/each}
	</ol>
</Figure>
<Note
	text="Permission prompts always count as waiting on you. A session left on your turn for {hour} is parked; History draws parked time lighter."
/>
