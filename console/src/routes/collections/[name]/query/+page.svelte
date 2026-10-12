<script lang="ts">
	import { tick } from 'svelte';
	import { page } from '$app/state';
	import { api, ApiError, type Candidates, type Json, type Scored } from '#lib/api.ts';
	import { current } from '#lib/collection.svelte.ts';
	import { toasts } from '#lib/state.svelte.ts';
	import { int, ms } from '#lib/format.ts';
	import { hasUnsafeIntegers, lines } from '#lib/json.ts';
	import Icon from '#lib/components/Icon.svelte';
	import { copyText } from '#lib/clipboard.ts';
	import JsonView from '#lib/components/JsonView.svelte';

	const view = current();

	/** What the search ranks by: a vector, words over a text index, or both fused. */
	let mode = $state<'vector' | 'text' | 'hybrid'>('vector');
	let source = $state<'point' | 'vector'>('point');
	let pointId = $state('');
	let vectorText = $state('');
	let filterText = $state('');
	let limit = $state(10);
	// The quality panel's sweep links here with the ef it measured.
	let ef = $state<number | null>(Number(page.url.searchParams.get('ef')) || null);
	let threshold = $state<number | null>(null);
	let exact = $state(false);

	// Text search, over a field with a text index.
	let field = $state('');
	let words = $state('');
	let k1 = $state<number | null>(null);
	let b = $state<number | null>(null);

	// Hybrid: each search's weight and how many candidates it hands to fusion.
	let fusion = $state<'rrf' | 'dbsf'>('rrf');
	let rrfK = $state<number | null>(null);
	let vectorWeight = $state(1);
	let textWeight = $state(1);
	let vectorCandidates = $state(50);
	let textCandidates = $state(50);

	const textFields = $derived((view.info?.indexes ?? []).filter((i) => i.kind === 'text').map((i) => i.path));
	$effect(() => {
		if (!textFields.includes(field)) field = textFields[0] ?? '';
	});

	let results = $state<Scored[] | null>(null);
	let overlap = $state<{ same: number; of: number } | null>(null);
	let candidates = $state<Candidates | null>(null);
	/** The mode and fusion the shown results came from, so the table keeps its labels while the form changes. */
	let ran = $state<{ mode: typeof mode; fusion: typeof fusion; limit: number } | null>(null);
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

	async function queryVector(): Promise<Json> {
		if (source === 'point') {
			if (!pointId.trim()) throw 'Enter the id of a point to search from';
			const p = await api.point(view.name, pointId.trim(), true);
			return p.vector ?? [];
		}
		const v = parseJson(vectorText, 'The vector');
		if (!Array.isArray(v)) throw 'The vector is a JSON array of numbers';
		return v;
	}

	function textQuery(): Record<string, Json> {
		if (!field) throw 'No field has a text index to search, add one with kind text on the Overview tab';
		if (!words.trim()) throw 'Enter some words to search for';
		const t: Record<string, Json> = { field, query: words };
		if (k1 !== null && !Number.isNaN(k1)) t.k1 = k1;
		if (b !== null && !Number.isNaN(b)) t.b = b;
		return t;
	}

	/** A whole number from 1 to 10,000 typed into a field, or a message saying what is wrong. */
	function count(value: number | null | undefined, what: string): number {
		if (typeof value !== 'number' || !Number.isInteger(value) || value < 1 || value > 10000) {
			throw `${what} is a whole number from 1 to 10,000`;
		}
		return value;
	}

	/**
	 * The request for the current form, the endpoint it goes to and what it was run as. Every
	 * field is read before the first await, so editing the form while a point's vector loads
	 * cannot mix two requests.
	 */
	async function body(): Promise<{
		path: 'search' | 'query';
		body: Record<string, Json>;
		ran: { mode: typeof mode; fusion: typeof fusion; limit: number };
	}> {
		const m = mode;
		const lim = count(limit, 'Limit');
		const req: Record<string, Json> = { limit: lim };
		const filter = parseJson(filterText, 'The filter');
		if (hasUnsafeIntegers(filter)) {
			throw 'The filter has a number beyond 2^53, which a browser rounds, so write large ids and values as strings, such as "9007199254740993"';
		}
		if (filter !== undefined) req.filter = filter;
		const ranAs = { mode: m, fusion, limit: lim };
		const vector = m === 'text' ? null : queryVector();
		// Settled later, or never when a field below throws first, so a failure is not left unhandled.
		vector?.catch(() => {});
		if (m === 'hybrid') {
			const v: Record<string, Json> = { limit: count(vectorCandidates, 'Vector candidates'), weight: vectorWeight };
			if (ef) v.hnsw_ef = ef;
			const t = { text: textQuery(), limit: count(textCandidates, 'Text candidates'), weight: textWeight };
			req.fusion = fusion;
			if (fusion === 'rrf' && rrfK !== null && rrfK !== undefined && !Number.isNaN(rrfK)) req.k = rrfK;
			v.vector = await vector;
			req.prefetch = [v, t];
			return { path: 'query', body: req, ran: ranAs };
		}
		if (m === 'text') {
			req.text = textQuery();
		} else {
			if (ef) req.hnsw_ef = ef;
			if (exact) req.exact = true;
			if (threshold !== null && threshold !== undefined && !Number.isNaN(threshold)) req.score_threshold = threshold;
			req.vector = await vector;
		}
		return { path: 'search', body: req, ran: ranAs };
	}

	async function run(compare = false) {
		if (busy) return;
		busy = true;
		error = null;
		overlap = null;
		try {
			const { path, body: b, ran: ranAs } = await body();
			const start = performance.now();
			const r: { points: Scored[]; candidates?: Candidates } =
				path === 'query' ? await api.query(view.name, b) : await api.search(view.name, b);
			took = performance.now() - start;
			results = r.points;
			candidates = r.candidates ?? null;
			ran = ranAs;
			selected = 0;
			if (compare && ranAs.mode === 'vector' && !exact) {
				const truth = await api.search(view.name, { ...b, exact: true, with_payload: false });
				const ids = new Set(truth.points.map((p) => String(p.id)));
				overlap = { same: r.points.filter((p) => ids.has(String(p.id))).length, of: truth.points.length };
			}
		} catch (e) {
			error = e instanceof ApiError ? e : String(e);
			results = null;
			candidates = null;
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
			const { path, body: b } = await body();
			const text = `curl -X POST ${location.origin}/v1/collections/${view.name}/points/${path} \\\n  -H 'Content-Type: application/json' \\\n  -H "Authorization: Bearer $NOCTURNUM_KEY" \\\n  -d ${shellQuote(JSON.stringify(b))}`;
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
	const scoreLabel = $derived(
		!ran || ran.mode === 'vector' ? (higher ? 'similarity' : 'distance') : ran.mode === 'text' ? 'BM25' : `${ran.fusion} score`
	);
	/** A page shorter than asked for while some search filled its candidate limit, so more may lie past it. */
	const cutShort = $derived(
		!!candidates && !!results && !!ran && results.length < ran.limit && candidates.sources.some((s) => s.found >= s.limit)
	);
	const needsText = $derived(mode !== 'vector' && textFields.length === 0);
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
					<span>Rank by</span>
					<div class="seg-ctl" role="group" aria-label="What the search ranks by">
						<button type="button" aria-pressed={mode === 'vector'} onclick={() => (mode = 'vector')}>Vector</button>
						<button type="button" aria-pressed={mode === 'text'} onclick={() => (mode = 'text')}>Text</button>
						<button type="button" aria-pressed={mode === 'hybrid'} onclick={() => (mode = 'hybrid')}>Hybrid</button>
					</div>
					<small>
						{#if mode === 'vector'}Nearness to a vector, by the collection's {view.info?.vectors.distance ?? 'distance'}
						{:else if mode === 'text'}BM25 relevance to some words, over a field with a text index
						{:else}A vector search and a text search run together, their rankings fused into one{/if}
					</small>
				</div>

				{#if mode !== 'text'}
					<div class="group" class:sub={mode === 'hybrid'}>
						{#if mode === 'hybrid'}<h3>Vector search</h3>{/if}
						<div class="field">
							<span>Query vector</span>
							<div class="seg-ctl" role="group" aria-label="Where the query vector comes from">
								<button type="button" aria-pressed={source === 'point'} onclick={() => (source = 'point')}>From a point</button>
								<button type="button" aria-pressed={source === 'vector'} onclick={() => (source = 'vector')}>Paste a vector</button>
							</div>
							{#if source === 'point'}
								<input class="input mono" bind:value={pointId} placeholder="point id" aria-label="Point id" spellcheck="false" />
								{#if mode === 'vector'}<small>Searches with that point's stored vector, so the point itself usually ranks near the top</small>{/if}
							{:else}
								<textarea class="textarea" rows="4" bind:value={vectorText} placeholder="[0.021, -0.084, 0.117, …]" aria-label="Vector as JSON" spellcheck="false"></textarea>
								<small>{view.info ? `${view.info.vectors.dim} numbers` : 'A JSON array of numbers'}</small>
							{/if}
						</div>
						{#if mode === 'hybrid'}
							<div class="nums">
								<label class="field"><span>Weight</span><input class="input num" type="number" min="0" step="any" aria-label="Vector search weight" bind:value={vectorWeight} /></label>
								<label class="field"><span>Candidates</span><input class="input num" type="number" min="1" max="10000" aria-label="Vector search candidates" bind:value={vectorCandidates} /></label>
								<label class="field"><span class="mono">hnsw_ef</span><input class="input num" type="number" min="1" placeholder={String(view.status?.index.default_ef ?? 128)} bind:value={ef} /></label>
							</div>
						{/if}
					</div>
				{/if}

				{#if mode !== 'vector'}
					<div class="group" class:sub={mode === 'hybrid'}>
						{#if mode === 'hybrid'}<h3>Text search</h3>{/if}
						{#if needsText}
							<p class="notice">
								No field has a text index yet, so there is nothing to search by words.
								Add one with kind <span class="mono">text</span> under Indexes on the
								<a class="link" href="/collections/{encodeURIComponent(view.name)}">Overview tab</a>.
							</p>
						{:else}
							<div class="pair">
								<label class="field">
									<span>Field</span>
									<select class="select-native mono" bind:value={field}>
										{#each textFields as f (f)}<option value={f}>{f}</option>{/each}
									</select>
								</label>
								<label class="field">
									<span>Words</span>
									<input class="input" bind:value={words} placeholder="caddy certificate renewal" spellcheck="false" />
								</label>
							</div>
							{#if mode === 'text'}<small class="note">Matched as lowercase runs of letters and digits, a repeated word counting once</small>{/if}
						{/if}
						{#if mode === 'hybrid'}
							<div class="nums">
								<label class="field"><span>Weight</span><input class="input num" type="number" min="0" step="any" aria-label="Text search weight" bind:value={textWeight} /></label>
								<label class="field"><span>Candidates</span><input class="input num" type="number" min="1" max="10000" aria-label="Text search candidates" bind:value={textCandidates} /></label>
								<span></span>
							</div>
						{/if}
					</div>
				{/if}

				{#if mode === 'hybrid'}
					<div class="field">
						<span>Fusion</span>
						<div class="fuse">
							<div class="seg-ctl" role="group" aria-label="How the two rankings are fused">
								<button type="button" aria-pressed={fusion === 'rrf'} onclick={() => (fusion = 'rrf')}>Rank (RRF)</button>
								<button type="button" aria-pressed={fusion === 'dbsf'} onclick={() => (fusion = 'dbsf')}>Score (DBSF)</button>
							</div>
							{#if fusion === 'rrf'}
								<label class="k"><span class="mono">k</span><input class="input num" type="number" min="0" step="any" placeholder="60" aria-label="RRF k" bind:value={rrfK} /></label>
							{/if}
						</div>
						<small>
							{fusion === 'rrf'
								? 'Each search adds weight / (k + rank) for every point it found, so only positions count'
								: "Each search's scores are rescaled by their own spread, then added up by weight"}
						</small>
					</div>
				{/if}

				<label class="field">
					<span>Filter</span>
					<textarea
						class="textarea"
						rows={mode === 'hybrid' ? 3 : 4}
						bind:value={filterText}
						placeholder={'{"must": [{"key": "source", "match": "notes"}]}'}
						spellcheck="false"
					></textarea>
					<small>{mode === 'hybrid' ? 'Optional, applies to both searches' : 'Optional, the filter object from docs/api/v1.md'}</small>
				</label>
				<div class="nums">
					<label class="field"><span>Limit</span><input class="input num" type="number" min="1" max="10000" bind:value={limit} /></label>
					{#if mode === 'vector'}
						<label class="field"><span class="mono">hnsw_ef</span><input class="input num" type="number" min="1" placeholder={String(view.status?.index.default_ef ?? 128)} bind:value={ef} /></label>
						<label class="field"><span>Score cutoff</span><input class="input num" type="number" step="any" placeholder="none" bind:value={threshold} /></label>
					{:else if mode === 'text'}
						<label class="field"><span class="mono">k1</span><input class="input num" type="number" min="0" max="10" step="any" placeholder="1.2" bind:value={k1} /></label>
						<label class="field"><span class="mono">b</span><input class="input num" type="number" min="0" max="1" step="any" placeholder="0.75" bind:value={b} /></label>
					{/if}
				</div>
				{#if mode === 'vector'}
					<label class="check"><input type="checkbox" bind:checked={exact} />Exact search, scanning every point</label>
				{/if}
				{#if errText}<p class="error-line" role="alert"><Icon name="warn" /><span>{errText}</span></p>{/if}
				{#if curlText}
					<div class="curl">
						<span class="dim">The browser refused the clipboard, so select the command here</span>
						<pre>{curlText}</pre>
					</div>
				{/if}
				<div class="row">
					<button class="btn primary" type="submit" disabled={busy || needsText}><Icon name="search" />{busy ? 'Searching' : 'Search'}</button>
					{#if mode === 'vector'}
						<button class="btn" type="button" disabled={busy || exact} title="Runs the same search exactly and counts the shared results" onclick={() => run(true)}>Compare with exact</button>
					{/if}
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
				{#if candidates && results}
					<div class="budget num">
						<span><strong>{int(candidates.pool)}</strong> candidates</span>
						{#each candidates.sources as src, i (i)}
							<span class:full={src.found >= src.limit}>
								{i === 0 ? 'vector' : 'text'} found <span class="mono">{int(src.found)}</span> of <span class="mono">{int(src.limit)}</span>
							</span>
						{/each}
						{#if cutShort}
							<span class="flag" title="A search found as many points as its candidate limit, so raising that limit may fill the page">
								<Icon name="warn" />Cut short by candidate limits
							</span>
						{/if}
					</div>
				{/if}
				{#if !results}
					<p class="placeholder">Results show here, best first, with their payloads beside them.</p>
				{:else if results.length === 0}
					<div class="empty">
						<strong>No matches</strong>
						<p>
							{ran?.mode === 'vector'
								? 'Nothing passed the filter and the score cutoff.'
								: ran?.mode === 'text'
									? 'No point that passes the filter holds any of these words.'
									: 'Neither search found a point that passes the filter.'}
						</p>
					</div>
				{:else}
					<div class="scroll">
						<table class="res num">
							<thead>
								<tr>
									<th scope="col" class="rank">#</th>
									<th scope="col">id</th>
									<th scope="col" class="r">{scoreLabel}</th>
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
	.group {
		display: grid;
		gap: 12px;
		min-width: 0;
	}
	.group.sub {
		padding-top: 12px;
		border-top: 1px solid var(--line);
	}
	.group h3 {
		margin: 0;
		font-size: 13px;
		font-weight: 600;
		letter-spacing: -0.005em;
	}
	.pair {
		display: grid;
		grid-template-columns: minmax(0, 2fr) minmax(0, 3fr);
		gap: 10px;
	}
	.note {
		margin-top: -6px;
		color: var(--text-3);
		font-size: 12px;
	}
	.notice {
		margin: 0;
		padding: 10px 12px;
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		background: var(--ground);
		color: var(--text-2);
		font-size: 12px;
		line-height: 1.5;
	}
	.fuse {
		display: flex;
		align-items: center;
		gap: 10px;
	}
	.k {
		display: inline-flex;
		align-items: center;
		gap: 8px;
		color: var(--text-2);
		font-size: 12px;
	}
	.k .input {
		width: 72px;
	}
	.budget {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 16px;
		padding: 8px 16px;
		border-bottom: 1px solid var(--line);
		color: var(--text-3);
		font-size: 12px;
	}
	.budget strong {
		color: var(--text);
		font-weight: 500;
	}
	.budget .mono {
		color: var(--text-2);
	}
	.budget .full .mono {
		color: var(--text);
	}
	.budget .flag {
		margin-left: auto;
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
