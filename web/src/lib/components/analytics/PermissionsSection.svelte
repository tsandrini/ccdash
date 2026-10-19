<script lang="ts">
	// Permissions: what asks you most by the allow rule that would cover it,
	// how you answered and how fast, and what auto mode blocked.
	import type { Analytics } from '#lib/api/types.js';
	import { DECISION_SOURCES } from '#lib/analytics.js';
	import { COLOR } from '#lib/charts.js';
	import { fmtDuration, fmtStamp, plural, toolName } from '#lib/format.js';
	import BarRows from '../charts/BarRows.svelte';
	import Figure from '../charts/Figure.svelte';
	import Note from '../charts/Note.svelte';
	import SortableTable from '../charts/SortableTable.svelte';
	import StatTile from '../charts/StatTile.svelte';
	import Tiles from '../charts/Tiles.svelte';

	let { data }: { data: Analytics } = $props();

	const p = $derived(data.permissions);
	const blocked = $derived(p.blocked.reduce((n, [, c]) => n + c, 0));
	const rules = $derived(
		p.rules.map((r) => ({ ...r, candidate: r.asks >= 3 && !r.denied ? 1 : 0 }))
	);
	const decisions = $derived(
		(data.telemetry?.decisions ?? []).map((d) => ({
			...d,
			label: `${DECISION_SOURCES[d.source] ?? d.source}${d.decision === 'reject' ? ' (refused)' : ''}`,
			total: d.count,
			parts: [[d.count, COLOR.neutral]] as [number, string][]
		}))
	);
</script>

<Tiles>
	<StatTile name="Permission prompts" value={p.asks} sub="{p.allowed} allowed, {p.denied} not" />
	<StatTile
		name="Answer time"
		value={p.median_answer_ms === null ? '–' : fmtDuration(p.median_answer_ms)}
		sub="median, prompt to answer"
	/>
	<StatTile
		name="Questions"
		value={p.questions}
		sub={p.median_question_ms === null
			? 'Claude asked you'
			: `answered in ${fmtDuration(p.median_question_ms)} (median)`}
	/>
	<StatTile
		name="Blocked by auto mode"
		value={blocked}
		sub={blocked
			? p.blocked
					.slice(0, 3)
					.map(([tool, n]) => `${toolName(tool)} ${n}`)
					.join(', ')
			: 'nothing'}
	/>
	{#if p.plans}
		<StatTile name="Plans" value={p.plans} sub="{p.plans_approved} approved" />
	{/if}
</Tiles>
<Figure
	title="What asks you most"
	sub="Grouped by the allow rule that would cover it"
	empty={!p.rules.length && 'No permission prompts in this range.'}
>
	<SortableTable
		columns={[
			{ key: 'rule', label: 'Rule', text: true },
			{ key: 'asks', label: 'Asked' },
			{ key: 'allowed', label: 'Allowed' },
			{ key: 'denied', label: 'Not' },
			{
				key: 'median_answer_ms',
				label: 'Answer',
				fmt: (v) => (v === null ? '–' : fmtDuration(Number(v)))
			},
			{ key: 'last_at', label: 'Last', fmt: (v) => fmtStamp(Number(v)) },
			{ key: 'candidate', label: 'Allow?', fmt: (v) => (v ? 'candidate' : '') }
		]}
		rows={rules}
		sortKey="asks"
	/>
</Figure>
{#if decisions.length}
	<Figure
		title="How tool calls were let through"
		sub="Every tool call, from Claude Code's OpenTelemetry export"
	>
		<BarRows
			rows={decisions}
			valueText={(row) => String(row.count)}
			tip={(row) => [plural(row.count, 'call'), row.label]}
		/>
	</Figure>
{/if}
<Note
	text="Answers are inferred: a prompt counts as allowed when the tool ran right after it. Check a rule before adding it to permissions.allow in settings.json; candidates were asked 3 or more times and never refused."
/>
