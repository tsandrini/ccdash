<script lang="ts">
	// The chart's values as a table, folded under "Table".
	let { headers, rows }: { headers: string[]; rows: (string | number)[][] } = $props();
</script>

<details class="text-xs">
	<summary class="cursor-pointer text-muted-foreground select-none">Table</summary>
	<div class="mt-2 overflow-x-auto">
		<table class="w-full border-collapse tabular-nums">
			<thead>
				<tr>
					{#each headers as h, i (i)}
						<th class="border-b px-2 py-1 text-right font-medium first:text-left">{h}</th>
					{/each}
				</tr>
			</thead>
			<tbody>
				{#each rows as row, i (i)}
					<tr>
						{#each row as cell, j (j)}
							<td class="border-b px-2 py-1 text-right whitespace-nowrap first:text-left">{cell}</td
							>
						{/each}
					</tr>
				{/each}
			</tbody>
		</table>
	</div>
</details>
