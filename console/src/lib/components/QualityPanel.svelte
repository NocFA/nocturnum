<script lang="ts">
	// Recall against exact search at several beam widths, measured by the engine on sampled
	// points, so the ef worth asking for can be read off the curve.
	import { api, ApiError, type RecallReport } from '#lib/api.ts';
	import type { CollectionView } from '#lib/collection.svelte.ts';
	import { int, ms, relative } from '#lib/format.ts';
	import Icon from './Icon.svelte';

	let { view }: { view: CollectionView } = $props();

	type Saved = { at: string; report: RecallReport };
	// svelte-ignore state_referenced_locally
	const storeKey = `nocturnum.recall.${view.name}`;

	function loadSaved(): Saved | null {
		try {
			const raw = localStorage.getItem(storeKey);
			return raw ? (JSON.parse(raw) as Saved) : null;
		} catch {
			return null;
		}
	}

	let saved = $state<Saved | null>(loadSaved());
	let busy = $state(false);
	let error = $state<string | null>(null);
	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 30_000);
		return () => clearInterval(t);
	});

	async function run() {
		busy = true;
		error = null;
		try {
			const report = await api.recall(view.name, { samples: 100, limit: 10 });
			saved = { at: new Date().toISOString(), report };
			now = Date.now();
			try {
				localStorage.setItem(storeKey, JSON.stringify(saved));
			} catch {
				// Kept for this visit only.
			}
		} catch (e) {
			error = e instanceof ApiError ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	const report = $derived(saved?.report ?? null);
	const defaultEf = $derived(view.status?.index.default_ef ?? 128);
	const runs = $derived(report?.runs ?? []);
	const atDefault = $derived(
		runs.find((r) => r.hnsw_ef === Math.max(defaultEf, report?.limit ?? 0)) ?? runs[runs.length - 1] ?? null
	);
	const enough = $derived(runs.find((r) => r.recall !== null && r.recall >= 0.99) ?? null);
	const exactOnly = $derived(report !== null && report.indexed_points === 0);

	// One column per ef, evenly spaced, with recall up the side: the climb from left to right
	// is what a wider beam buys, and the latency under each column is what it costs.
	const W = 300;
	const H = 104;
	const top = 18;
	const base = 66;
	const chart = $derived.by(() => {
		if (runs.length < 2 || runs.every((r) => r.recall === null)) return null;
		const ys = runs.map((r) => r.recall ?? 0);
		const floor = Math.min(0.9, Math.floor(Math.min(...ys) * 20) / 20);
		const step = W / runs.length;
		const y = (v: number) => top + (1 - (v - floor) / (1 - floor)) * (base - top);
		const cols = runs.map((r, i) => ({ r, cx: step * (i + 0.5), cy: y(r.recall ?? floor) }));
		return { cols, floor, line: cols.map((c) => `${c.cx.toFixed(1)},${c.cy.toFixed(1)}`).join(' ') };
	});
</script>

<section class="panel" aria-labelledby="h-q">
	<div class="panel-h">
		<h2 id="h-q">Search quality</h2>
		<div class="aside">
			<button class="btn sm" class:busy disabled={busy || !view.info?.points} onclick={run}>
				<Icon name={busy ? 'clock' : 'play'} size={12} />{busy ? 'Checking' : report ? 'Re-run' : 'Run check'}
			</button>
		</div>
	</div>
	<div class="quality num">
		{#if error}
			<p class="error-line" role="alert"><Icon name="warn" />{error}</p>
		{/if}
		{#if busy}
			<p class="running" role="status">Searching 100 sampled points exactly and at each ef, which takes longest on big collections</p>
		{/if}
		{#if !report}
			<p class="intro">
				Samples 100 points, searches with each one exactly and at ef 16 to 256, and shows how many of the true top 10
				graph search finds and how fast.
			</p>
			{#if !view.info?.points}<p class="dim small">The collection needs points first.</p>{/if}
		{:else if atDefault}
			<div class="recall">
				<span class="v">{atDefault.recall === null ? '–' : atDefault.recall.toFixed(3)}</span>
				<span class="k">recall@{report.limit} at <span class="mono">ef</span> {atDefault.hnsw_ef}</span>
			</div>
			{#if exactOnly}
				<p class="small muted">No segment has a graph yet, so every search is exact and recall is 1.</p>
			{:else if chart}
				<svg class="curve" viewBox="0 0 {W} {H}" role="img" aria-label="Recall and median latency for each ef">
					<line x1="0" x2={W} y1={top} y2={top} class="ref" />
					<line x1="0" x2={W} y1={base} y2={base} class="ref" />
					<polyline points={chart.line} class="path" />
					{#each chart.cols as c (c.r.hnsw_ef)}
						<a
							href="/collections/{encodeURIComponent(view.name)}/query?ef={c.r.hnsw_ef}"
							class:on={c.r.hnsw_ef === atDefault.hnsw_ef}
							aria-label="Search at ef {c.r.hnsw_ef}"
						>
							<rect x={c.cx - W / runs.length / 2} y="0" width={W / runs.length} height={H} class="hit" />
							<circle cx={c.cx} cy={c.cy} r="3.2" />
							<text x={c.cx} y={c.cy - 7} text-anchor="middle" class="val">{c.r.recall === null ? '–' : c.r.recall.toFixed(3)}</text>
							<text x={c.cx} y={base + 15} text-anchor="middle" class="ef">ef {c.r.hnsw_ef}</text>
							<text x={c.cx} y={base + 29} text-anchor="middle" class="ms">{ms(c.r.p50_ms)} ms</text>
							<title>ef {c.r.hnsw_ef}: recall {c.r.recall?.toFixed(3)}, p50 {ms(c.r.p50_ms)} ms, p99 {ms(c.r.p99_ms)} ms, open in Query</title>
						</a>
					{/each}
				</svg>
				<table class="sr">
					<caption>Recall and latency per ef</caption>
					<thead><tr><th>ef</th><th>recall</th><th>p50 ms</th><th>p99 ms</th></tr></thead>
					<tbody>
						{#each runs as r (r.hnsw_ef)}<tr><td>{r.hnsw_ef}</td><td>{r.recall}</td><td>{r.p50_ms}</td><td>{r.p99_ms}</td></tr>{/each}
					</tbody>
				</table>
			{/if}
			<dl class="lat">
				<div><dt>p50 at ef {atDefault.hnsw_ef}</dt><dd>{ms(atDefault.p50_ms)}<small>ms</small></dd></div>
				<div><dt>p99</dt><dd>{ms(atDefault.p99_ms)}<small>ms</small></dd></div>
				<div><dt>exact p50</dt><dd>{ms(report.exact.p50_ms)}<small>ms</small></dd></div>
			</dl>
			<p class="note" title="Each sampled point is among its own nearest, which lifts recall a little at small limits">
				{#if enough && !exactOnly && enough.hnsw_ef < atDefault.hnsw_ef}
					<span class="muted">ef {enough.hnsw_ef} reaches {enough.recall?.toFixed(3)}</span>,
				{/if}
				{int(report.samples)} samples, {relative(saved?.at, now)}
			</p>
		{/if}
	</div>
</section>

<style>
	.quality {
		padding: 14px 16px 16px;
	}
	.running {
		margin: 0 0 10px;
		font-size: 12px;
		color: var(--text-2);
	}
	.intro {
		margin: 0 0 8px;
		color: var(--text-2);
		text-wrap: pretty;
		line-height: 1.55;
	}
	.small {
		font-size: 12px;
		margin: 6px 0 0;
	}
	.recall {
		display: flex;
		align-items: baseline;
		gap: 10px;
	}
	.recall .v {
		font-size: 30px;
		line-height: 34px;
		font-weight: 500;
		letter-spacing: -0.03em;
	}
	.recall .k {
		color: var(--text-2);
	}
	.curve {
		display: block;
		width: 100%;
		height: auto;
		margin-top: 8px;
		overflow: visible;
	}
	.ref {
		stroke: var(--line-2);
		stroke-dasharray: 2 3;
	}
	.path {
		fill: none;
		stroke: #4d5a73;
		stroke-width: 1.2;
		stroke-linejoin: round;
	}
	.curve circle {
		fill: var(--surface);
		stroke: #8fa3c7;
		stroke-width: 1.4;
	}
	.curve text {
		fill: var(--text-3);
		font: 11px var(--mono);
	}
	.curve .val {
		fill: var(--text-2);
	}
	.curve .ef {
		fill: var(--text-2);
		font-family: var(--sans);
		font-size: 11px;
	}
	.curve .hit {
		fill: transparent;
	}
	.curve a:hover .hit,
	.curve a:focus-visible .hit {
		fill: rgba(255, 255, 255, 0.03);
	}
	.curve a:focus-visible {
		outline: none;
	}
	.curve a {
		cursor: pointer;
	}
	.curve a:hover circle {
		stroke: var(--text);
	}
	.curve a:hover .ef {
		fill: var(--text);
		text-decoration: underline;
	}
	.curve a.on circle {
		fill: var(--accent);
		stroke: var(--accent);
	}
	.curve a.on .val,
	.curve a.on .ef {
		fill: var(--text);
	}
	.lat {
		margin: 12px 0 0;
		display: grid;
		grid-template-columns: repeat(3, 1fr);
		border-top: 1px solid var(--line);
	}
	.lat div {
		padding-top: 10px;
		min-width: 0;
	}
	.lat div + div {
		border-left: 1px solid var(--line);
		padding-left: 12px;
	}
	.lat dt {
		color: var(--text-3);
		font-size: 12px;
		white-space: nowrap;
	}
	.lat dd {
		margin: 2px 0 0;
		font-size: 15px;
		font-weight: 500;
	}
	.lat dd small {
		font-size: 12px;
		color: var(--text-3);
		font-weight: 400;
		margin-left: 2px;
	}
	.note {
		margin: 10px 0 0;
		font-size: 12px;
		color: var(--text-3);
		text-wrap: pretty;
		line-height: 1.5;
	}
</style>
