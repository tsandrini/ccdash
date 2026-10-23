<script lang="ts">
	// State › sizes and growth: where the space goes (by kind of file, by
	// project, the largest sessions) and what grew, per day and per project.
	import type { StatGroup, SyncStats } from '#lib/api/index.js';
	import BarRows from '#lib/components/charts/BarRows.svelte';
	import Columns from '#lib/components/charts/Columns.svelte';
	import Figure from '#lib/components/charts/Figure.svelte';
	import SortableTable from '#lib/components/charts/SortableTable.svelte';
	import StatTile from '#lib/components/charts/StatTile.svelte';
	import Tiles from '#lib/components/charts/Tiles.svelte';
	import { COLOR } from '#lib/charts.js';
	import { fmtBytes, fmtStamp, plural } from '#lib/format.js';

	let { stats }: { stats: SyncStats } = $props();

	/** What each top-level directory holds. */
	const KINDS: Record<string, string> = {
		projects: 'Sessions (transcripts, subagents, tool results)',
		'file-history': 'Rewind checkpoints',
		tasks: 'Task lists',
		plans: 'Plans',
		'paste-cache': 'Pasted content'
	};

	const grew = (bytes: number) =>
		bytes > 0 ? `+${fmtBytes(bytes)}` : bytes < 0 ? `−${fmtBytes(-bytes)}` : '—';
	const sum = (pick: (g: StatGroup) => number) =>
		stats.kinds.reduce((total, g) => total + pick(g), 0);

	const kindRows = $derived(
		stats.kinds.map((g) => ({
			label: KINDS[g.key] ?? g.key,
			total: g.size.bytes,
			parts: [[g.size.bytes, COLOR.neutral]] as [number, string][],
			g
		}))
	);
	const days = $derived(
		stats.days.map(([day, bytes]) => ({ key: day, label: day.slice(5), bytes }))
	);
	const fastest = $derived(
		[...stats.projects]
			.filter((p) => p.week > 0)
			.sort((a, b) => b.week - a.week)
			.slice(0, 5)
	);
	const projectRows = $derived(
		stats.projects.map((p) => ({
			project: p.label,
			bytes: p.size.bytes,
			files: p.size.files,
			sessions: p.sessions,
			day: p.day,
			week: p.week,
			month: p.month
		}))
	);
</script>

<Tiles>
	<StatTile
		name="On the hub"
		value={fmtBytes(stats.total.bytes)}
		sub={plural(stats.total.files, 'file')}
	/>
	<StatTile name="Grew today" value={grew(sum((g) => g.day))} sub="all machines" />
	<StatTile name="Grew in 7 days" value={grew(sum((g) => g.week))} sub="all machines" />
	<StatTile
		name="Prompt history"
		value={plural(stats.history.files, 'prompt')}
		sub={fmtBytes(stats.history.bytes)}
	/>
</Tiles>
{#if stats.since}
	<p class="text-xs text-muted-foreground">
		Growth counts from {stats.since}, when the hub started recording it; sizes cover everything.
	</p>
{:else}
	<p class="text-xs text-muted-foreground">
		Growth shows here from the first write the hub records; sizes cover everything.
	</p>
{/if}

<Figure
	title="Where the space goes"
	sub="By kind of file"
	table={{
		headers: ['Kind', 'Size', 'Files', 'Today', '7 days'],
		rows: stats.kinds.map((g) => [
			KINDS[g.key] ?? g.key,
			fmtBytes(g.size.bytes),
			g.size.files,
			grew(g.day),
			grew(g.week)
		])
	}}
>
	<BarRows
		rows={kindRows}
		valueText={(row) => fmtBytes(row.total)}
		tip={(row) => [fmtBytes(row.total), row.label, plural(row.g.size.files, 'file')]}
	/>
</Figure>

<Figure
	title="Growth per day"
	sub="Bytes added on all machines, the last 30 days"
	empty={!days.length && 'Nothing recorded yet.'}
	table={{ headers: ['Day', 'Added'], rows: days.map((d) => [d.key, grew(d.bytes)]) }}
>
	<Columns
		data={days}
		series={[{ key: 'bytes', label: 'Added', color: COLOR.neutral, value: (d) => d.bytes }]}
		fmtY={(v) => fmtBytes(Math.abs(v))}
		height={160}
		tip={(d) => [grew(d.bytes), d.key]}
	/>
</Figure>

{#if fastest.length}
	<section class="flex flex-col gap-1.5">
		<h2 class="text-base font-semibold">Growing fastest</h2>
		<ol class="flex flex-col gap-1 text-sm">
			{#each fastest as p (p.key)}
				<li class="flex flex-wrap items-baseline gap-x-2">
					<span class="font-medium">{p.label}</span>
					<span class="text-muted-foreground"
						>{grew(p.week)} in 7 days · {grew(p.day)} today · {fmtBytes(p.size.bytes)} in all</span
					>
				</li>
			{/each}
		</ol>
	</section>
{/if}

<section class="flex flex-col gap-1.5">
	<h2 class="text-base font-semibold">Projects</h2>
	<div class="overflow-x-auto">
		<SortableTable
			sortKey="bytes"
			rows={projectRows}
			columns={[
				{ key: 'project', label: 'Project', text: true },
				{ key: 'bytes', label: 'Size', fmt: (v) => fmtBytes(Number(v)) },
				{ key: 'sessions', label: 'Sessions' },
				{ key: 'files', label: 'Files' },
				{ key: 'day', label: 'Today', fmt: (v) => grew(Number(v)) },
				{ key: 'week', label: '7 days', fmt: (v) => grew(Number(v)) },
				{ key: 'month', label: '30 days', fmt: (v) => grew(Number(v)) }
			]}
		/>
	</div>
</section>

<section class="flex flex-col gap-1.5">
	<h2 class="text-base font-semibold">Largest sessions</h2>
	<p class="text-xs text-muted-foreground">
		Transcript with its subagents and tool results, rewind checkpoints and task lists.
	</p>
	<ul class="flex flex-col gap-1">
		{#each stats.sessions as s (s.session)}
			<li class="flex flex-wrap items-baseline gap-x-2 text-sm">
				<span class="w-16 shrink-0 text-right tabular-nums">{fmtBytes(s.size.bytes)}</span>
				<a href="/sessions/{s.session}" class="min-w-0 font-medium hover:underline">
					{s.title ?? `Session ${s.session.slice(0, 8)}`}
				</a>
				<span class="text-xs text-muted-foreground">
					{[s.label, s.host, `written ${fmtStamp(s.last_write)}`].filter(Boolean).join(' · ')}
				</span>
			</li>
		{/each}
	</ul>
</section>
