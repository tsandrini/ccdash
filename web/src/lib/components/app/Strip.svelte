<script lang="ts">
	// Last-hour activity bar; the newest stretch comes from the live state.
	import type { Activity, Lane, Session } from '#lib/api/types.js';
	import { live } from '#lib/live/store.svelte.js';
	import { barActivity } from '#lib/session.js';

	const STRIP_MS = 60 * 60_000;

	let { session, lane }: { session: Session; lane?: Lane } = $props();

	const colour: Record<Activity, string> = {
		working: 'bg-working',
		permission: 'bg-permission',
		your_turn: 'bg-your-turn',
		parked: 'bg-parked',
		idle: 'bg-idle',
		gone: ''
	};

	const segments = $derived.by(() => {
		const t1 = live.now;
		const t0 = t1 - STRIP_MS;
		const segs: [number, number, Activity][] = (lane?.segments ?? []).map((s) => [...s]);
		if (session.activity !== 'gone') {
			const from = Math.max(session.activity_since, t0);
			while (segs.length && segs[segs.length - 1][0] >= from) segs.pop();
			const last = segs[segs.length - 1];
			if (last) last[1] = Math.min(last[1], from);
			if (last && last[2] === barActivity(session)) last[1] = t1;
			else segs.push([from, t1, barActivity(session)]);
		}
		return segs
			.filter(([, b, a]) => b > t0 && a !== 'gone')
			.map(([a, b, activity]) => ({
				left: ((Math.max(a, t0) - t0) / STRIP_MS) * 100,
				width: ((Math.min(b, t1) - Math.max(a, t0)) / STRIP_MS) * 100,
				activity
			}));
	});
</script>

<span class="relative block h-1 w-full overflow-hidden rounded bg-muted/60" aria-hidden="true">
	{#each segments as seg, i (i)}
		<i
			class="absolute top-0 h-full {colour[seg.activity]}"
			style:left="{seg.left}%"
			style:width="calc({seg.width}% - 1px)"
		></i>
	{/each}
</span>
