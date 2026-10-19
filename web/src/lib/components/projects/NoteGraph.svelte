<script lang="ts">
	// The note graph: d3 force layout drawn in SVG. Nodes are coloured by
	// kind and sized by recalls (progress entries are small grey diamonds),
	// edges styled by link kind. Wheel zooms, dragging the background pans,
	// dragging a node moves it, a click opens the note.
	import { untrack } from 'svelte';
	import {
		forceCenter,
		forceCollide,
		forceLink,
		forceManyBody,
		forceSimulation,
		type Simulation,
		type SimulationLinkDatum,
		type SimulationNodeDatum
	} from 'd3-force';
	import MinusIcon from '@lucide/svelte/icons/minus';
	import PlusIcon from '@lucide/svelte/icons/plus';
	import ScanIcon from '@lucide/svelte/icons/scan';
	import type { GraphEdge, GraphNode, LinkKind } from '#lib/api/types.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { fmtDate, plural } from '#lib/format.js';
	import { KIND_COLOR, KIND_LABEL } from '#lib/notes.js';

	type N = SimulationNodeDatum & { id: number; node: GraphNode; r: number };
	type L = SimulationLinkDatum<N> & { kind: LinkKind };
	type Dot = { id: number; x: number; y: number; r: number; node: GraphNode };
	type Line = { key: string; x1: number; y1: number; x2: number; y2: number; kind: LinkKind };

	let {
		nodes,
		edges,
		onpick
	}: { nodes: GraphNode[]; edges: GraphEdge[]; onpick: (node: GraphNode) => void } = $props();

	let width = $state(600);
	let height = $state(400);
	let view = $state({ k: 1, x: 0, y: 0 });
	let dots = $state.raw<Dot[]>([]);
	let lines = $state.raw<Line[]>([]);
	let hovered = $state<Dot | null>(null);
	let svg = $state<SVGSVGElement | null>(null);

	let sim: Simulation<N, L> | null = null;
	/** Positions survive filter changes. */
	const placed: Record<number, N> = {};
	let fitted = false;

	const radius = (n: GraphNode) =>
		n.kind === 'progress' ? 3.5 : Math.min(14, 5 + Math.sqrt(n.recalls) * 2);

	function rebuild() {
		sim?.stop();
		// a changed node set fits itself once more, unless the user pans or zooms first
		fitted = false;
		const ns: N[] = nodes.map((node) => {
			const old = placed[node.id];
			const n: N = old
				? { ...old, node, r: radius(node), fx: null, fy: null }
				: { id: node.id, node, r: radius(node) };
			placed[node.id] = n;
			return n;
		});
		const ids = new Set(ns.map((n) => n.id));
		const ls: L[] = edges
			.filter((e) => ids.has(e.src) && ids.has(e.dst))
			.map((e) => ({ source: e.src, target: e.dst, kind: e.kind }));
		sim = forceSimulation<N, L>(ns)
			.force(
				'link',
				forceLink<N, L>(ls)
					.id((d) => d.id)
					.distance((l) => 30 + (l.source as N).r + (l.target as N).r)
					.strength(0.6)
			)
			.force('charge', forceManyBody<N>().strength(-40).distanceMax(250))
			.force('center', forceCenter(width / 2, height / 2).strength(0.05))
			.force(
				'collide',
				forceCollide<N>((d) => d.r + 3)
			)
			.alpha(ns.some((n) => n.x === undefined) ? 1 : 0.4)
			.on('end', () => {
				if (!fitted) {
					fitted = true;
					fit();
				}
			})
			.on('tick', () => {
				dots = ns.map((n) => ({ id: n.id, x: n.x ?? 0, y: n.y ?? 0, r: n.r, node: n.node }));
				lines = ls.map((l) => {
					const s = l.source as N;
					const t = l.target as N;
					return {
						key: `${s.id}-${t.id}-${l.kind}`,
						x1: s.x ?? 0,
						y1: s.y ?? 0,
						x2: t.x ?? 0,
						y2: t.y ?? 0,
						kind: l.kind
					};
				});
			});
	}

	$effect(() => {
		void nodes;
		void edges;
		untrack(rebuild);
		return () => sim?.stop();
	});

	// --- pan, zoom, drag -----------------------------------------------------

	let pointer = $state.raw<
		{ kind: 'pan'; x: number; y: number } | { kind: 'node'; n: N; moved: boolean } | null
	>(null);

	/** Graph coordinates of a client point. */
	function toGraph(event: PointerEvent | WheelEvent): [number, number] {
		const box = svg!.getBoundingClientRect();
		return [
			(event.clientX - box.left - view.x) / view.k,
			(event.clientY - box.top - view.y) / view.k
		];
	}

	function zoomBy(factor: number, at?: [number, number]) {
		fitted = true;
		const k = Math.min(6, Math.max(0.2, view.k * factor));
		const [cx, cy] = at ?? [width / 2, height / 2];
		// keep the point under the cursor fixed
		view = { k, x: cx - ((cx - view.x) * k) / view.k, y: cy - ((cy - view.y) * k) / view.k };
	}

	function fit() {
		if (!dots.length) return void (view = { k: 1, x: 0, y: 0 });
		const xs = dots.map((d) => d.x);
		const ys = dots.map((d) => d.y);
		const [x0, x1, y0, y1] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
		const k = Math.min(
			3,
			0.9 * Math.min(width / Math.max(40, x1 - x0), height / Math.max(40, y1 - y0))
		);
		view = { k, x: width / 2 - ((x0 + x1) / 2) * k, y: height / 2 - ((y0 + y1) / 2) * k };
	}

	$effect(() => {
		const el = svg;
		if (!el) return;
		const onwheel = (event: WheelEvent) => {
			event.preventDefault();
			const box = el.getBoundingClientRect();
			zoomBy(event.deltaY < 0 ? 1.15 : 1 / 1.15, [
				event.clientX - box.left,
				event.clientY - box.top
			]);
		};
		// NOTE: wheel listeners are passive by default, which forbids preventDefault
		el.addEventListener('wheel', onwheel, { passive: false });
		return () => el.removeEventListener('wheel', onwheel);
	});

	function down(event: PointerEvent, dot?: Dot) {
		if (event.button !== 0) return;
		(event.currentTarget as Element).setPointerCapture(event.pointerId);
		if (dot) {
			const n = placed[dot.id];
			pointer = { kind: 'node', n, moved: false };
			n.fx = n.x;
			n.fy = n.y;
			sim?.alphaTarget(0.3).restart();
		} else {
			fitted = true;
			pointer = { kind: 'pan', x: event.clientX - view.x, y: event.clientY - view.y };
		}
	}

	function move(event: PointerEvent) {
		if (!pointer) return;
		if (pointer.kind === 'pan') {
			view = { ...view, x: event.clientX - pointer.x, y: event.clientY - pointer.y };
		} else {
			const [x, y] = toGraph(event);
			pointer.n.fx = x;
			pointer.n.fy = y;
			pointer.moved = true;
		}
	}

	function up(event: PointerEvent, dot?: Dot) {
		if (!pointer) return;
		if (pointer.kind === 'node') {
			pointer.n.fx = null;
			pointer.n.fy = null;
			sim?.alphaTarget(0);
			if (!pointer.moved && dot) onpick(dot.node);
		}
		pointer = null;
		void event;
	}

	const dash: Record<LinkKind, string | undefined> = {
		mentions: undefined,
		relates: '3 3',
		supersedes: '6 3',
		blocks: undefined
	};
	/** Titles on every note while the graph is sparse or near 1:1 or closer
	 * (zoomed out they would collide); else on the most recalled and the
	 * hovered one. */
	const named = $derived(dots.filter((d) => d.node.kind !== 'progress'));
	const labelled = $derived(
		named.length <= 25 || view.k >= 0.8
			? dots.filter((d) => d.node.kind !== 'progress' || d.id === hovered?.id)
			: dots.filter((d) => d.r >= 9 || d.id === hovered?.id)
	);
	const screen = (d: Dot) => [d.x * view.k + view.x, d.y * view.k + view.y] as const;
</script>

<div class="relative h-[60vh] min-h-72 w-full overflow-hidden rounded-lg border bg-card">
	<div class="absolute inset-0" bind:clientWidth={width} bind:clientHeight={height}>
		<svg
			bind:this={svg}
			{width}
			{height}
			class="touch-none select-none {pointer?.kind === 'pan' ? 'cursor-grabbing' : 'cursor-grab'}"
			role="img"
			aria-label="Note graph"
			onpointerdown={(e) => down(e)}
			onpointermove={move}
			onpointerup={(e) => up(e)}
			onpointercancel={(e) => up(e)}
		>
			<g transform="translate({view.x} {view.y}) scale({view.k})">
				{#each lines as l (l.key)}
					<line
						x1={l.x1}
						y1={l.y1}
						x2={l.x2}
						y2={l.y2}
						stroke={l.kind === 'blocks' ? 'var(--color-critical)' : 'var(--color-muted-foreground)'}
						stroke-opacity={l.kind === 'mentions' ? 0.3 : 0.6}
						stroke-width={(l.kind === 'supersedes' ? 2 : 1.2) / view.k}
						stroke-dasharray={dash[l.kind]}
					/>
				{/each}
				{#each dots as d (d.id)}
					{@const retired = d.node.status === 'archived' || d.node.status === 'superseded'}
					<!-- marks keep their screen size at every zoom level -->
					{@const r = d.r / view.k}
					<g
						transform="translate({d.x} {d.y})"
						class="cursor-pointer"
						opacity={retired ? 0.45 : 1}
						role="button"
						tabindex="-1"
						aria-label={d.node.title}
						onpointerdown={(e) => {
							e.stopPropagation();
							down(e, d);
						}}
						onpointerup={(e) => {
							e.stopPropagation();
							up(e, d);
						}}
						onpointerenter={() => (hovered = d)}
						onpointerleave={() => (hovered = hovered?.id === d.id ? null : hovered)}
					>
						{#if d.node.kind === 'progress'}
							<rect
								x={-r}
								y={-r}
								width={r * 2}
								height={r * 2}
								transform="rotate(45)"
								fill={KIND_COLOR.progress}
								stroke="var(--color-card)"
								stroke-width={1.5 / view.k}
							/>
						{:else}
							<circle
								{r}
								fill={KIND_COLOR[d.node.kind]}
								stroke="var(--color-card)"
								stroke-width={2 / view.k}
								stroke-dasharray={d.node.status === 'stale' ? '2 2' : undefined}
							/>
						{/if}
						<circle r={Math.max(r, 10 / view.k)} fill="transparent" />
					</g>
				{/each}
				{#each labelled as d (d.id)}
					<text
						x={d.x}
						y={d.y - (d.r + 3) / view.k}
						text-anchor="middle"
						font-size={10 / view.k}
						class="pointer-events-none fill-foreground"
						opacity={d.id === hovered?.id ? 1 : 0.85}
						paint-order="stroke"
						stroke="var(--color-card)"
						stroke-width={3 / view.k}
						>{d.node.title.length > 32 && d.id !== hovered?.id
							? `${d.node.title.slice(0, 31)}…`
							: d.node.title}</text
					>
				{/each}
			</g>
		</svg>
	</div>
	{#if hovered}
		{@const [sx, sy] = screen(hovered)}
		<div
			class="pointer-events-none absolute z-10 max-w-64 rounded-md border bg-popover px-2 py-1.5 text-xs shadow"
			style="left: {Math.min(sx + 12, width - 260)}px; top: {Math.max(sy + 12, 0)}px"
		>
			<span class="font-medium">{hovered.node.title}</span>
			<span class="block text-muted-foreground"
				>{KIND_LABEL[hovered.node.kind]}{hovered.node.project
					? ` · ${hovered.node.project}`
					: ''}{hovered.node.status !== 'active' ? ` · ${hovered.node.status}` : ''}</span
			>
			<span class="block text-muted-foreground"
				>{plural(hovered.node.recalls, 'recall')} · created {fmtDate(hovered.node.created_at)}</span
			>
		</div>
	{/if}
	<div class="absolute top-2 right-2 flex flex-col gap-1">
		<Button variant="outline" size="icon-sm" aria-label="Zoom in" onclick={() => zoomBy(1.3)}>
			<PlusIcon />
		</Button>
		<Button variant="outline" size="icon-sm" aria-label="Zoom out" onclick={() => zoomBy(1 / 1.3)}>
			<MinusIcon />
		</Button>
		<Button variant="outline" size="icon-sm" aria-label="Fit" title="Fit" onclick={fit}>
			<ScanIcon />
		</Button>
	</div>
	{#if !nodes.length}
		<p class="absolute inset-0 grid place-items-center text-sm text-muted-foreground">
			Nothing to show with these filters.
		</p>
	{/if}
</div>
