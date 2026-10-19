<script lang="ts" generics="T extends Record<string, unknown>">
	// A table whose column headers sort it; text columns sort by name,
	// ascending first.
	import type { SortColumn } from '#lib/charts.js';

	let { columns, rows, sortKey }: { columns: SortColumn<T>[]; rows: T[]; sortKey: string } =
		$props();

	// the initial sort only; the reader changes it from there
	// svelte-ignore state_referenced_locally
	let key = $state(sortKey);
	let desc = $state(true);

	const column = $derived(columns.find((c) => c.key === key) ?? columns[0]);
	const sorted = $derived(
		[...rows].sort((a, b) => {
			const c = column.text
				? String(a[key] ?? '').localeCompare(String(b[key] ?? ''))
				: (Number(a[key] ?? -Infinity) || -Infinity) - (Number(b[key] ?? -Infinity) || -Infinity);
			return desc ? -c : c;
		})
	);

	function sortBy(c: SortColumn<T>) {
		if (key === c.key) desc = !desc;
		else {
			key = c.key;
			desc = !c.text;
		}
	}

	const text = (c: SortColumn<T>, row: T) =>
		c.fmt ? c.fmt(row[c.key], row) : String(row[c.key] ?? '');
</script>

<div class="overflow-x-auto">
	<table class="w-full border-collapse text-xs tabular-nums">
		<thead>
			<tr>
				{#each columns as c (c.key)}
					<th
						scope="col"
						class="border-b px-2 py-1.5 font-medium whitespace-nowrap text-muted-foreground {c.text
							? 'text-left'
							: 'text-right'}"
						aria-sort={c.key === key ? (desc ? 'descending' : 'ascending') : 'none'}
					>
						<button type="button" class="hover:text-foreground" onclick={() => sortBy(c)}
							>{c.label}{c.key === key ? (desc ? ' ↓' : ' ↑') : ''}</button
						>
					</th>
				{/each}
			</tr>
		</thead>
		<tbody>
			{#each sorted as row, i (i)}
				<tr>
					{#each columns as c, j (c.key)}
						{#if j === 0}
							<th
								scope="row"
								class="sticky left-0 max-w-[28ch] truncate border-b bg-card px-2 py-1.5 text-left font-medium"
								>{text(c, row)}</th
							>
						{:else}
							<td
								class="border-b px-2 py-1.5 whitespace-nowrap {c.text ? 'text-left' : 'text-right'}"
								>{text(c, row)}</td
							>
						{/if}
					{/each}
				</tr>
			{/each}
		</tbody>
	</table>
</div>
