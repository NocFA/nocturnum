<script lang="ts">
	import { tick } from 'svelte';
	import { page } from '$app/state';
	import { api, ApiError, type Json, type Scored } from '#lib/api.ts';
	import { current } from '#lib/collection.svelte.ts';
	import { toasts } from '#lib/state.svelte.ts';
	import { int, ms } from '#lib/format.ts';
	import { hasUnsafeIntegers, lines } from '#lib/json.ts';
	import Icon from '#lib/components/Icon.svelte';
	import { copyText } from '#lib/clipboard.ts';
	import JsonView from '#lib/components/JsonView.svelte';

	const view = current();

	let source = $state<'point' | 'vector'>('point');
	let pointId = $state('');
	let vectorText = $state('');
	let filterText = $state('');
	let limit = $state(10);
	// The quality panel's sweep links here with the ef it measured.
	let ef = $state<number | null>(Number(page.url.searchParams.get('ef')) || null);
	let threshold = $state<number | null>(null);
	let exact = $state(false);

	let results = $state<Scored[] | null>(null);
	let overlap = $state<{ same: number; of: number } | null>(null);
	let took = $state<number | null>(null);
	let error = $state<ApiError | string | null>(null);
	let busy = $state(false);
	let selected = $state(0);
	let curlText = $state<string | null>(null);
	let tbody: HTMLTableSectionElement | undefined = $state();

	// A first point to search from, so the form runs as soon as the page opens.
	$effect(() => {
		if (pointId) return;
		api.scroll(view.name, { limit: 1 }).then(
			(p) => {
				if (!pointId && p.points[0]) pointId = String(p.points[0].id);
			},
			() => {}
		);
	});

	function parseJson(text: string, what: string): Json | undefined {
		if (!text.trim()) return undefined;
		try {
			return JSON.parse(text);
		} catch (e) {
			throw `${what} is not valid JSON: ${(e as Error).message}`;
		}
	}

	async function body(): Promise<Record<string, Json>> {
		let vector: Json;
		if (source === 'point') {
			if (!pointId.trim()) throw 'Enter the id of a point to search from';
			const p = await api.point(view.name, pointId.trim(), true);
			vector = p.vector ?? [];
		} else {
			const v = parseJson(vectorText, 'The vector');
			if (!Array.isArray(v)) throw 'The vector is a JSON array of numbers';
			vector = v;
		}
		const b: Record<string, Json> = { vector, limit };
		const filter = parseJson(filterText, 'The filter');
		if (hasUnsafeIntegers(filter)) {
			throw 'The filter has a number beyond 2^53, which a browser rounds, so write large ids and values as strings, such as "9007199254740993"';
		}
		if (filter !== undefined) b.filter = filter;
		if (ef) b.hnsw_ef = ef;
		if (threshold !== null && !Number.isNaN(threshold)) b.score_threshold = threshold;
		if (exact) b.exact = true;
		return b;
	}

	async function run(compare = false) {
		if (busy) return;
		busy = true;
		error = null;
		overlap = null;
		try {
			const b = await body();
			const start = performance.now();
			const r = await api.search(view.name, b);
			took = performance.now() - start;
			results = r.points;
			selected = 0;
			if (compare && !exact) {
				const truth = await api.search(view.name, { ...b, exact: true, with_payload: false });
				const ids = new Set(truth.points.map((p) => String(p.id)));
				overlap = { same: r.points.filter((p) => ids.has(String(p.id))).length, of: truth.points.length };
			}
		} catch (e) {
			error = e instanceof ApiError ? e : String(e);
			results = null;
		} finally {
			busy = false;
		}
	}

	/** One shell word in single quotes, with each apostrophe closed, escaped and reopened. */
	function shellQuote(text: string): string {
		return `'${text.replaceAll("'", `'\\''`)}'`;
	}

	async function curl() {
		try {
			const b = await body();
			const text = `curl -X POST ${location.origin}/v1/collections/${view.name}/points/search \\\n  -H 'Content-Type: application/json' \\\n  -H "Authorization: Bearer $NOCTURNUM_KEY" \\\n  -d ${shellQuote(JSON.stringify(b))}`;
			if (await copyText(text)) {
				curlText = null;
				toasts.show('Copied as curl, with the key left as $NOCTURNUM_KEY');
			} else {
				// Shown under the form to select by hand when the browser refuses the clipboard.
				curlText = text;
			}
		} catch (e) {
			toasts.error(e instanceof ApiError ? e : new Error(String(e)));
		}
	}

	async function move(i: number) {
		if (!results?.length) return;
		selected = Math.min(results.length - 1, Math.max(0, i));
		await tick();
		(tbody?.querySelectorAll('tr')[selected] as HTMLElement | undefined)?.focus();
	}

	function onkeydown(e: KeyboardEvent) {
		const moves: Record<string, number> = { ArrowDown: 1, ArrowUp: -1, j: 1, k: -1 };
		if (e.key in moves) {
			e.preventDefault();
			move(selected + moves[e.key]);
		}
	}

	const chosen = $derived(results?.[selected] ?? null);

	/** The payload's scalar fields as "key value" pairs, text first, for one line of a table. */
	function summary(payload: Record<string, Json> | undefined): string {
		const entries = Object.entries(payload ?? {}).filter(([, v]) => v !== null && typeof v !== 'object');
		entries.sort(([, a], [, b]) => Number(typeof b === 'string') - Number(typeof a === 'string'));
		return entries.map(([k, v]) => `${k} ${v}`).join('  ·  ') || '{}';
	}
	const higher = $derived(view.info ? view.info.vectors.distance === 'cosine' || view.info.vectors.distance === 'dot' : true);
	const errText = $derived(
		error === null ? null : typeof error === 'string' ? error : `${error.message}${error.at ? `, at ${error.at}` : ''}`
	);
</script>

<svelte:head><title>Query {view.name} · nocturnum</title></svelte:head>

<main class="body">
	<div class="grid">
		<!-- ⌘Enter anywhere in the form runs it, as Enter would in a single field. -->
		<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
		<form
			class="panel form"
			onsubmit={(e) => (e.preventDefault(), run())}
			onkeydown={(e) => {
				if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
					e.preventDefault();
					run();
				}
			}}
		>
			<div class="panel-h">
				<h2>Search</h2>
				<div class="aside"><span class="hint"><kbd>⌘</kbd><kbd>Enter</kbd> runs</span></div>
			</div>
			<div class="panel-body stack">
				<div class="field">
					<span>Query vector</span>
					<div class="seg-ctl" role="group" aria-label="Where the query vector comes from">
						<button type="button" aria-pressed={source === 'point'} onclick={() => (source = 'point')}>From a point</button>
						<button type="button" aria-pressed={source === 'vector'} onclick={() => (source = 'vector')}>Paste a vector</button>
					</div>
					{#if source === 'point'}
						<input class="input mono" bind:value={pointId} placeholder="point id" aria-label="Point id" spellcheck="false" />
						<small>Searches with that point's stored vector, so the point itself usually ranks near the top</small>
					{:else}
						<textarea class="textarea" rows="4" bind:value={vectorText} placeholder="[0.021, -0.084, 0.117, …]" aria-label="Vector as JSON" spellcheck="false"></textarea>
						<small>{view.info ? `${view.info.vectors.dim} numbers` : 'A JSON array of numbers'}</small>
					{/if}
				</div>
				<label class="field">
					<span>Filter</span>
					<textarea
						class="textarea"
						rows="4"
						bind:value={filterText}
						placeholder={'{"must": [{"key": "source", "match": "notes"}]}'}
						spellcheck="false"
					></textarea>
					<small>Optional, the filter object from docs/api/v1.md</small>
				</label>
				<div class="nums">
					<label class="field"><span>Limit</span><input class="input num" type="number" min="1" max="10000" bind:value={limit} /></label>
					<label class="field"><span class="mono">hnsw_ef</span><input class="input num" type="number" min="1" placeholder={String(view.status?.index.default_ef ?? 128)} bind:value={ef} /></label>
					<label class="field"><span>Score cutoff</span><input class="input num" type="number" step="any" placeholder="none" bind:value={threshold} /></label>
				</div>
				<label class="check"><input type="checkbox" bind:checked={exact} />Exact search, scanning every point</label>
				{#if errText}<p class="error-line" role="alert"><Icon name="warn" /><span>{errText}</span></p>{/if}
				{#if curlText}
					<div class="curl">
						<span class="dim">The browser refused the clipboard, so select the command here</span>
						<pre>{curlText}</pre>
					</div>
				{/if}
				<div class="row">
					<button class="btn primary" type="submit" disabled={busy}><Icon name="search" />{busy ? 'Searching' : 'Search'}</button>
					<button class="btn" type="button" disabled={busy || exact} title="Runs the same search exactly and counts the shared results" onclick={() => run(true)}>Compare with exact</button>
					<button class="btn ghost sq" type="button" aria-label="Copy as curl" title="Copy as curl" onclick={curl}><Icon name="copy" /></button>
				</div>
			</div>
		</form>

		<div class="stack">
			<section class="panel" aria-labelledby="h-res">
				<div class="panel-h">
					<h2 id="h-res">Results</h2>
					{#if results}<span class="dim num">{results.length}</span>{/if}
					<div class="aside num">
						{#if overlap}
							<span class:warn={overlap.same < overlap.of}>{overlap.same} of {overlap.of} match exact search</span>
						{/if}
						{#if took !== null && results}<span>{ms(took)} ms round trip</span>{/if}
					</div>
				</div>
				{#if !results}
					<p class="placeholder">Results show here, best first, with their payloads beside them.</p>
				{:else if results.length === 0}
					<div class="empty"><strong>No matches</strong><p>Nothing passed the filter and the score cutoff.</p></div>
				{:else}
					<div class="scroll">
						<table class="res num">
							<thead>
								<tr>
									<th scope="col" class="rank">#</th>
									<th scope="col">id</th>
									<th scope="col" class="r">{higher ? 'similarity' : 'distance'}</th>
									<th scope="col">payload</th>
								</tr>
							</thead>
							<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
							<tbody bind:this={tbody} {onkeydown}>
								{#each results as p, i (String(p.id))}
									<tr tabindex={i === selected ? 0 : -1} aria-selected={i === selected} onclick={() => move(i)}>
										<td class="rank dim">{i + 1}</td>
										<td class="mono id">{p.id}</td>
										<td class="r mono">{p.score.toFixed(4)}</td>
										<td class="pl" title={JSON.stringify(p.payload ?? {})}>{summary(p.payload)}</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{/if}
			</section>
			{#if chosen}
				<section class="panel" aria-labelledby="h-chosen">
					<div class="panel-h">
						<h2 id="h-chosen">Point <span class="mono">{chosen.id}</span></h2>
						<div class="aside num">rank {int(selected + 1)}, score {chosen.score.toFixed(4)}</div>
					</div>
					<JsonView lines={lines(chosen.payload ?? {})} label="Payload JSON" />
				</section>
			{/if}
		</div>
	</div>
</main>

<style>
	.grid {
		display: grid;
		grid-template-columns: minmax(320px, 400px) minmax(0, 1fr);
		gap: 20px;
		align-items: start;
	}
	.seg-ctl {
		display: inline-flex;
		justify-self: start;
		border: 1px solid var(--line-2);
		border-radius: var(--r-sm);
		padding: 1px;
		background: var(--ground);
	}
	.seg-ctl button {
		height: 24px;
		padding: 0 10px;
		border: 0;
		border-radius: 3px;
		background: none;
		color: var(--text-3);
		font-size: 12px;
	}
	.seg-ctl button[aria-pressed='true'] {
		background: var(--raised);
		color: var(--text);
	}
	.nums {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 10px;
	}
	.row {
		display: flex;
		gap: 8px;
	}
	.curl {
		display: grid;
		gap: 6px;
		font-size: 12px;
	}
	.curl pre {
		margin: 0;
		padding: 10px 12px;
		background: var(--ground);
		border: 1px solid var(--line);
		border-radius: var(--r);
		font-family: var(--mono);
		font-size: 12px;
		color: var(--text-2);
		white-space: pre-wrap;
		word-break: break-word;
		user-select: all;
	}
	.hint {
		display: inline-flex;
		gap: 4px;
		align-items: center;
	}
	.placeholder {
		margin: 0;
		padding: 36px 16px;
		text-align: center;
		color: var(--text-3);
	}
	.scroll {
		overflow-x: auto;
	}
	.res {
		table-layout: fixed;
	}
	.res td {
		height: 28px;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.res .rank {
		width: 44px;
	}
	.res th:nth-child(2) {
		width: 120px;
	}
	.res th:nth-child(3) {
		width: 110px;
	}
	.res tbody tr {
		outline: none;
		cursor: default;
	}
	.res tbody tr:hover td {
		background: var(--hover);
	}
	.res tbody tr[aria-selected='true'] td {
		background: var(--accent-soft);
	}
	.res tbody tr[aria-selected='true'] td:first-child {
		box-shadow: inset 2px 0 0 var(--accent);
	}
	.id {
		color: var(--text-2);
	}
	.pl {
		font-family: var(--mono);
		font-size: 12px;
		color: var(--text-3);
		max-width: 0;
	}
	.warn {
		color: var(--warn);
	}
	@media (max-width: 1000px) {
		.grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
