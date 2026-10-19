<script lang="ts">
	// Tokens: by type, cache hit rate, API latency per model, commits, edits
	// by language, and your active time against Claude's (needs telemetry).
	import type { Analytics } from '#lib/api/types.js';
	import { bucketEdges, bucketName, columns, hourly } from '#lib/analytics.js';
	import { COLOR, fmtCount, fmtTokens, pct, roleLegend } from '#lib/charts.js';
	import { fmtDuration, fmtStamp, fmtUsd, plural } from '#lib/format.js';
	import { rangeState } from '#lib/range.svelte.js';
	import BarRows from '../charts/BarRows.svelte';
	import Columns from '../charts/Columns.svelte';
	import Figure from '../charts/Figure.svelte';
	import Note from '../charts/Note.svelte';
	import Pair from '../charts/Pair.svelte';
	import StatTile from '../charts/StatTile.svelte';
	import Tiles from '../charts/Tiles.svelte';
	import TimeLines from '../charts/TimeLines.svelte';

	let { data }: { data: Analytics } = $props();

	const brush = (from: number, to: number) =>
		rangeState.set({ kind: 'absolute', from, to: Math.min(to, rangeState.now()) });
	const t = $derived(data.telemetry);
	const cols = $derived(columns(data));
	const edges = $derived(bucketEdges(data));
	const read = $derived(t?.tokens_cache_read ?? 0);
	const fresh = $derived((t?.tokens_input ?? 0) + (t?.tokens_cache_write ?? 0));
	const all = $derived(read + fresh + (t?.tokens_output ?? 0));
	const hit = $derived(read + fresh ? (read / (read + fresh)) * 100 : null);
	const hitOf = (d: (typeof data.buckets)[number]) => {
		const inbound = d.tokens_input + d.tokens_cache_write + d.tokens_cache_read;
		return inbound ? (d.tokens_cache_read / inbound) * 100 : 0;
	};
	const you = $derived(
		data.buckets.map((d) => [d.start, d.active_user_secs / 3600] as [number, number])
	);
	const cli = $derived(
		data.buckets.map((d) => [d.start, d.active_cli_secs / 3600] as [number, number])
	);
	const youAt = $derived(Object.fromEntries(you));
	const cliAt = $derived(Object.fromEntries(cli));
	const models = $derived(
		(t?.models ?? []).map((m) => {
			const all = m.tokens_input + m.tokens_output + m.tokens_cache_read + m.tokens_cache_write;
			return {
				...m,
				label: m.model,
				all,
				total: all,
				parts: [[all, COLOR.neutral]] as [number, string][]
			};
		})
	);
	const sources = $derived(
		(t?.sources ?? []).map((sr) => ({
			...sr,
			label: sr.source,
			total: sr.tokens,
			parts: [[sr.tokens, COLOR.neutral]] as [number, string][]
		}))
	);
	const edits = $derived(
		(t?.edits ?? []).slice(0, 8).map((e) => ({
			...e,
			label: e.language,
			total: e.accepted + e.rejected,
			parts: [[e.accepted + e.rejected, COLOR.neutral]] as [number, string][]
		}))
	);
</script>

{#if !t}
	<div class="flex flex-col gap-2 rounded-lg border bg-card p-4 text-sm">
		<strong>Turn on Claude Code's OpenTelemetry export</strong>
		<p>
			Tokens, cache hits, API latency, commits, pull requests and edit decisions come from it. With
			the Home Manager module:
		</p>
		<pre
			class="rounded bg-muted p-2 font-mono text-xs">services.ccdash.claudeCode.telemetry.enable = true;</pre>
		<p class="text-xs text-muted-foreground">
			Sessions started after the switch report; data goes only to ccdash on this machine.
		</p>
	</div>
{:else}
	<Tiles>
		<StatTile name="Tokens" value={fmtTokens(all)} sub={plural(t.requests, 'API request')} />
		<StatTile
			name="Cache hit rate"
			value={hit === null ? '–' : pct(hit)}
			sub="input read from the prompt cache"
		/>
		<StatTile
			name="Output tokens"
			value={fmtTokens(t.tokens_output)}
			sub="{fmtTokens(t.tokens_cache_write)} written to the cache"
		/>
		<StatTile name="Commits" value={t.commits} sub={plural(t.pull_requests, 'pull request')} />
		<StatTile
			name="Your active time"
			value={fmtDuration(t.active_user_secs * 1000)}
			sub="Claude: {fmtDuration(t.active_cli_secs * 1000)}"
		/>
		<StatTile
			name="API errors"
			value={t.errors}
			sub={t.errors ? t.errors_by_status.map(([s, n]) => `${s}: ${n}`).join(', ') : 'none'}
		/>
	</Tiles>
	<Figure
		title={hourly(data) ? 'Tokens per hour' : 'Tokens per day'}
		sub="Fresh input, cache writes and output; cache reads show as the hit rate"
		legend={roleLegend([
			['input', 'Input'],
			['cache_write', 'Cache writes'],
			['output', 'Output']
		])}
		table={{
			headers: ['Day', 'Input', 'Cache writes', 'Cache reads', 'Output', 'Requests'],
			rows: data.buckets.map((d) => [
				bucketName(data, d.start),
				d.tokens_input,
				d.tokens_cache_write,
				d.tokens_cache_read,
				d.tokens_output,
				d.api_requests
			])
		}}
	>
		<Columns
			data={cols}
			series={[
				{ key: 'input', label: 'Input', color: COLOR.input, value: (d) => d.b.tokens_input },
				{
					key: 'cache_write',
					label: 'Cache writes',
					color: COLOR.cache_write,
					value: (d) => d.b.tokens_cache_write
				},
				{ key: 'output', label: 'Output', color: COLOR.output, value: (d) => d.b.tokens_output }
			]}
			fmtY={fmtTokens}
			onbrush={brush}
			{edges}
			tip={(d) => [
				fmtTokens(d.b.tokens_input + d.b.tokens_cache_write + d.b.tokens_output),
				bucketName(data, d.b.start),
				`input ${fmtTokens(d.b.tokens_input)} · cache writes ${fmtTokens(d.b.tokens_cache_write)} · output ${fmtTokens(d.b.tokens_output)}`,
				`${fmtTokens(d.b.tokens_cache_read)} read from the cache`
			]}
		/>
	</Figure>
	<Pair>
		<Figure
			title={hourly(data) ? 'Cache hit rate per hour' : 'Cache hit rate per day'}
			sub="Share of input read from the prompt cache"
		>
			<Columns
				data={cols}
				series={[{ key: 'hit', label: 'Hit rate', color: COLOR.neutral, value: (d) => hitOf(d.b) }]}
				fmtY={(v) => `${v}%`}
				height={170}
				tip={(d) => [pct(hitOf(d.b)), bucketName(data, d.b.start)]}
			/>
		</Figure>
		<Figure
			title={hourly(data) ? 'Active time per hour' : 'Active time per day'}
			sub="You at the keyboard, and Claude working"
			legend={roleLegend(
				[
					['your_turn', 'You'],
					['working', 'Claude']
				],
				true
			)}
		>
			<TimeLines
				series={[
					{ key: 'you', label: 'You', color: COLOR.your_turn, points: you },
					{ key: 'claude', label: 'Claude', color: COLOR.working, points: cli }
				]}
				since={data.buckets[0]?.start ?? data.since}
				until={(data.buckets.at(-1)?.start ?? data.until) + data.bucket_ms}
				fmt={(v) => fmtDuration(v * 3_600_000)}
				tip={(ts) => [
					`You ${fmtDuration((youAt[ts] ?? 0) * 3_600_000)}`,
					`Claude ${fmtDuration((cliAt[ts] ?? 0) * 3_600_000)}`,
					bucketName(data, ts)
				]}
			/>
		</Figure>
	</Pair>
	<Pair>
		<Figure
			title="By model"
			sub="All tokens; latency is the whole request"
			empty={!models.length && 'No API requests in this range.'}
			table={{
				headers: [
					'Model',
					'Requests',
					'Input',
					'Cache writes',
					'Cache reads',
					'Output',
					'Cost',
					'Median',
					'95%',
					'First token'
				],
				rows: models.map((m) => [
					m.model,
					m.requests,
					fmtTokens(m.tokens_input),
					fmtTokens(m.tokens_cache_write),
					fmtTokens(m.tokens_cache_read),
					fmtTokens(m.tokens_output),
					fmtUsd(m.cost_usd),
					fmtDuration(m.p50_ms ?? 0),
					fmtDuration(m.p95_ms ?? 0),
					fmtDuration(m.ttft_p50_ms ?? 0)
				])
			}}
		>
			<BarRows
				rows={models}
				valueText={(row) => `${fmtTokens(row.all)} · ${fmtUsd(row.cost_usd)}`}
				tip={(row) => [
					fmtTokens(row.all),
					row.label,
					plural(row.requests, 'request'),
					`median ${fmtDuration(row.p50_ms ?? 0)}, first token ${fmtDuration(row.ttft_p50_ms ?? 0)}`
				]}
			/>
		</Figure>
		<Figure
			title="By source"
			sub="Main conversation, subagents, background work"
			empty={!sources.length && 'No API requests in this range.'}
		>
			<BarRows
				rows={sources}
				valueText={(row) => `${fmtTokens(row.tokens)} · ${plural(row.requests, 'request')}`}
				tip={(row) => [fmtTokens(row.tokens), row.label]}
			/>
		</Figure>
	</Pair>
	<Pair>
		<Figure title={hourly(data) ? 'Commits per hour' : 'Commits per day'} sub="Made by Claude Code">
			<Columns
				data={cols}
				series={[
					{ key: 'commits', label: 'Commits', color: COLOR.neutral, value: (d) => d.b.commits }
				]}
				fmtY={fmtCount}
				height={170}
				tip={(d) => [
					plural(d.b.commits, 'commit'),
					`${plural(d.b.pull_requests, 'pull request')} · ${bucketName(data, d.b.start)}`
				]}
			/>
		</Figure>
		<Figure
			title="Edits by language"
			sub="Accepted and rejected file edits"
			empty={!edits.length && 'No edits in this range.'}
		>
			<BarRows
				rows={edits}
				valueText={(row) =>
					`${row.accepted} accepted${row.rejected ? ` · ${row.rejected} rejected` : ''}`}
				tip={(row) => [plural(row.total, 'edit'), row.label]}
			/>
		</Figure>
	</Pair>
	<Note text="From Claude Code's OpenTelemetry export; last received {fmtStamp(t.last_at)}." />
{/if}
