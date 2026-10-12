<script lang="ts">
	// Points in id order beside the selected point's JSON, kept in step from the keyboard:
	// j and k or the arrows move, Home and End jump, Enter opens the point, and what is typed
	// in the top search filters the table as you type.
	import { goto } from '$app/navigation';
	import { tick, untrack } from 'svelte';
	import { api, ApiError, type Json, type Payload, type Point, type PointId } from '#lib/api.ts';
	import { app, toasts } from '#lib/state.svelte.ts';
	import { int } from '#lib/format.ts';
	import { hasUnsafeIntegers, lines, vectorLines } from '#lib/json.ts';
	import { matchesWords, parse } from '#lib/search.ts';
	import { copyText } from '#lib/clipboard.ts';
	import Icon from './Icon.svelte';
	import JsonView from './JsonView.svelte';
	import Confirm from './Confirm.svelte';

	let {
		name,
		compact = false,
		pageSize = compact ? 12 : 50
	}: { name: string; compact?: boolean; pageSize?: number } = $props();

	let rows = $state<Point[]>([]);
	let cursors = $state<(PointId | null)[]>([null]);
	let pageIndex = $state(0);
	let next = $state<PointId | null>(null);
	let total = $state<number | null>(null);
	let loading = $state(true);
	let error = $state<ApiError | null>(null);
	let selected = $state(0);
	let mode = $state<'payload' | 'vector'>('payload');
	let vector = $state<number[] | null>(null);
	let editing = $state(false);
	/** The point the editor opened on, which every save writes to. */
	let editId = $state<PointId | null>(null);
	let draft = $state('');
	let draftError = $state<string | null>(null);
	let saving = $state(false);
	let deleting = $state(false);
	/** The point the delete dialog was opened for, which is the one it deletes. */
	let deleteId = $state<PointId | null>(null);
	let tbody: HTMLTableSectionElement | undefined = $state();

	// The search is read 200 ms after the last keystroke, so typing does not send a request
	// per letter.
	let term = $state(app.search);
	$effect(() => {
		const s = app.search;
		const t = setTimeout(() => (term = s), 200);
		return () => clearTimeout(t);
	});
	const query = $derived(parse(term));
	const filterKey = $derived(JSON.stringify(query.filter));
	const visible = $derived(rows.filter((p) => matchesWords(p.payload, query.words)));
	const point = $derived(visible[selected] ?? null);

	// Each load gets a number, so an answer that arrives after a newer request was sent is
	// dropped instead of replacing the newer one's rows.
	let latest = 0;

	async function load(index: number) {
		const mine = ++latest;
		loading = true;
		const filter = query.filter ?? undefined;
		try {
			const [page, count] = await Promise.all([
				api.scroll(name, { filter, limit: pageSize, after: cursors[index] }),
				index === 0 ? api.count(name, filter) : Promise.resolve(null)
			]);
			if (mine !== latest) return;
			rows = page.points;
			next = page.next;
			if (count) total = count.count;
			pageIndex = index;
			cursors[index + 1] = page.next;
			error = null;
			if (selected >= rows.length) selected = Math.max(0, rows.length - 1);
		} catch (e) {
			if (mine !== latest) return;
			error = e instanceof ApiError ? e : new ApiError(0, 'unknown', String(e));
			rows = [];
			total = null;
		} finally {
			if (mine === latest) loading = false;
		}
	}

	// A new filter, or a change made from the console, starts again from the first page.
	$effect(() => {
		void filterKey;
		void app.changes;
		untrack(() => {
			cursors = [null];
			pageIndex = 0;
			selected = 0;
			load(0);
		});
	});

	// The vector is fetched for the selected point only, and only when it is being looked at.
	$effect(() => {
		const id = point?.id;
		untrack(() => (vector = null));
		if (id === undefined || mode !== 'vector') return;
		// The effect's cleanup runs when the selection or the view changes, after which this
		// read's answer is no longer wanted.
		let current = true;
		const t = setTimeout(() => {
			api.point(name, id, true).then(
				(p) => {
					if (current) vector = p.vector ?? null;
				},
				() => {}
			);
		}, 120);
		return () => {
			current = false;
			clearTimeout(t);
		};
	});

	// Columns are the payload keys most of the page has, names and text first, then dates,
	// numbers and lists, with booleans last since they say the least in a narrow cell.
	const columns = $derived.by(() => {
		const seen = new Map<string, { n: number; simple: number; rank: number }>();
		for (const p of visible) {
			for (const [k, v] of Object.entries(p.payload ?? {})) {
				const e = seen.get(k) ?? { n: 0, simple: 0, rank: 9 };
				e.n++;
				if (v === null || typeof v !== 'object' || (Array.isArray(v) && v.length <= 4)) e.simple++;
				e.rank = Math.min(e.rank, rank(k, v));
				seen.set(k, e);
			}
		}
		const picked = [...seen.entries()]
			.filter(([, e]) => e.simple * 2 >= e.n && e.n * 3 >= visible.length)
			.sort((a, b) => a[1].rank - b[1].rank || b[1].n - a[1].n || a[0].localeCompare(b[0]))
			// The compact table sits beside a JSON pane that shows the rest, so it keeps two.
			.slice(0, compact ? 2 : 6)
			.map(([k, e]) => ({ key: k, rank: e.rank }));
		// Narrow kinds get a fixed width, and when there is a title the other text columns
		// do too, so the title takes what is left.
		const titled = picked.some((c) => c.rank === 0);
		return picked.map((c) => ({ key: c.key, width: (titled ? widths : narrow)[c.rank] ?? null }));
	});

	const narrow: Record<number, string> = { 3: '104px', 4: '96px', 6: '76px' };
	const widths: Record<number, string> = $derived(compact
		? { ...narrow, 1: '104px', 2: '128px' }
		: { ...narrow, 1: '120px', 2: '160px' });

	function rank(key: string, v: Json): number {
		if (/^(title|name|label|text|content|summary)$/i.test(key) && typeof v === 'string') return 0;
		if (typeof v === 'string') return /^\d{4}-\d\d-\d\dT/.test(v) ? 3 : 1;
		if (typeof v === 'number') return 4;
		if (Array.isArray(v)) return 2;
		if (typeof v === 'boolean') return 6;
		return 7;
	}

	function cell(v: Json | undefined): { text: string; kind: string } {
		if (v === undefined) return { text: '', kind: 'none' };
		if (v === null) return { text: 'null', kind: 'null' };
		if (typeof v === 'number') return { text: String(v), kind: 'num' };
		if (typeof v === 'boolean') return { text: String(v), kind: 'bool' };
		if (typeof v === 'string') {
			if (/^\d{4}-\d\d-\d\dT\d\d:\d\d/.test(v)) return { text: v.slice(0, 10), kind: 'date' };
			return { text: v, kind: 'str' };
		}
		if (Array.isArray(v)) return { text: v.map((x) => (typeof x === 'object' ? '{…}' : String(x))).join(', '), kind: 'list' };
		return { text: '{…}', kind: 'obj' };
	}

	async function select(i: number, focus: boolean) {
		if (!visible.length) return;
		selected = Math.min(visible.length - 1, Math.max(0, i));
		editing = false;
		if (focus) {
			await tick();
			(tbody?.querySelectorAll('tr')[selected] as HTMLElement | undefined)?.focus();
		}
	}

	function open() {
		if (!point) return;
		if (compact) {
			app.search = String(point.id);
			goto(`/collections/${encodeURIComponent(name)}/points`);
		} else if (app.canWrite) {
			startEdit();
		}
	}

	function onkeydown(e: KeyboardEvent) {
		if (e.metaKey || e.ctrlKey || e.altKey) return;
		const moves: Record<string, number> = { ArrowDown: 1, ArrowUp: -1, j: 1, k: -1 };
		if (e.key in moves) {
			e.preventDefault();
			select(selected + moves[e.key], true);
		} else if (e.key === 'Home') {
			e.preventDefault();
			select(0, true);
		} else if (e.key === 'End') {
			e.preventDefault();
			select(visible.length - 1, true);
		} else if (e.key === 'Enter') {
			e.preventDefault();
			open();
		} else if (e.key === 'v') {
			mode = mode === 'payload' ? 'vector' : 'payload';
		}
	}

	function copy() {
		if (!point) return;
		const text = JSON.stringify(mode === 'vector' && vector ? vector : (point.payload ?? {}), null, 2);
		copyText(text).then((ok) =>
			ok
				? toasts.show(mode === 'vector' ? 'Vector copied' : 'Payload copied')
				: toasts.show('The browser refused the clipboard, select the JSON in the pane instead', 'bad')
		);
	}

	// The editor closes when the selection moves to another point, by key, click or filter.
	$effect(() => {
		const id = point?.id;
		untrack(() => {
			if (editing && id !== editId) editing = false;
		});
	});

	// Payload numbers beyond 2^53 were rounded when the browser read them, so such a payload is
	// shown with a warning and cannot be saved from here.
	const unsafe = $derived(point ? hasUnsafeIntegers(point.payload) : false);

	function startEdit() {
		if (!point) return;
		if (unsafe) {
			toasts.show('This payload holds integers beyond 2^53, which a browser cannot keep exactly, so edit it through the API', 'bad');
			return;
		}
		editId = point.id;
		mode = 'payload';
		draft = JSON.stringify(point.payload ?? {}, null, 2);
		draftError = null;
		editing = true;
	}

	async function save() {
		if (saving) return;
		const id = editId;
		if (id === null || point?.id !== id) {
			editing = false;
			return;
		}
		let payload: Payload;
		try {
			const parsed = JSON.parse(draft);
			if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
				draftError = 'A payload is a JSON object, such as {"title": "…"}';
				return;
			}
			if (hasUnsafeIntegers(parsed)) {
				draftError = 'Numbers beyond 2^53 lose digits in a browser, so set them through the API or as strings';
				return;
			}
			payload = parsed;
		} catch (e) {
			draftError = `Not valid JSON: ${(e as Error).message}`;
			return;
		}
		saving = true;
		// Reads already on their way may carry the old payload, so they are dropped and the
		// page is read again once the write is durable.
		latest++;
		try {
			const r = await api.overwritePayload(name, id, payload);
			rows = rows.map((p) => (p.id === id ? { ...p, payload } : p));
			load(pageIndex);
			if (editId === id) editing = false;
			toasts.show(`Payload of ${id} saved, WAL seq ${int(r.seq)}`);
		} catch (e) {
			draftError = e instanceof Error ? e.message : String(e);
			load(pageIndex);
		} finally {
			saving = false;
		}
	}

	async function remove() {
		const id = deleteId;
		if (id === null) return;
		latest++;
		try {
			await api.deletePoints(name, [id]);
			toasts.show(`Point ${id} deleted`);
			rows = rows.filter((p) => p.id !== id);
			if (total !== null) total--;
			if (selected >= visible.length) selected = Math.max(0, visible.length - 1);
			// The first page also brings a fresh count.
			load(pageIndex);
			app.refresh();
		} catch (e) {
			toasts.error(e);
			load(pageIndex);
		}
	}

	const first = $derived(pageIndex * pageSize + 1);
	const vecPreview = $derived(
		vector ? `[${vector.slice(0, 4).map((x) => x.toFixed(4)).join(', ')}, …] ${vector.length - 4} more` : null
	);
</script>

<div class="browser" class:compact>
	<section class="panel table-panel" aria-labelledby="h-pts-{name}">
		<div class="panel-h">
			<h2 id="h-pts-{name}">Points</h2>
			<span class="dim num">{total === null ? '' : int(total)}</span>
			<div class="aside">
				{#if query.filter || query.words.length}
					<button class="chip" title="Clear the top search" onclick={() => (app.search = '')}>
						<Icon name="filter" size={12} /><span class="mono">{term.trim()}</span><Icon name="x" size={10} />
					</button>
				{:else}
					<span>id order</span>
				{/if}
				{#if compact}
					<a class="link" href="/collections/{encodeURIComponent(name)}/points">All points <Icon name="chevron" size={11} /></a>
				{/if}
			</div>
		</div>

		{#if error}
			<div class="state-msg">
				<p class="error-line" role="alert"><Icon name="warn" /><span>{error.message}{#if error.at}, at <code>{error.at}</code>{/if}</span></p>
			</div>
		{:else if !loading && visible.length === 0}
			<div class="empty">
				{#if query.filter || query.words.length}
					<strong>No points match</strong>
					<p>Nothing on this page matches <span class="mono">{term.trim()}</span>.</p>
					<button class="btn sm" onclick={() => (app.search = '')}>Clear the search</button>
				{:else}
					<strong>No points yet</strong>
					<p>Points arrive through the API, for example <code class="mono">PUT /v1/collections/{name}/points</code>.</p>
				{/if}
			</div>
		{:else}
			<div class="scroll">
				<table class="points num" aria-label="Points, in id order" aria-busy={loading}>
					<thead>
						<tr>
							<th scope="col" class="id-col">id</th>
							{#each columns as col (col.key)}
								<th scope="col" style:width={col.width}>{col.key}</th>
							{/each}
						</tr>
					</thead>
					<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
					<tbody bind:this={tbody} {onkeydown}>
						{#each visible as p, i (p.id)}
							<tr
								tabindex={i === selected ? 0 : -1}
								aria-selected={i === selected}
								onclick={() => select(i, true)}
								ondblclick={open}
							>
								<td class="id">{p.id}</td>
								{#each columns as col (col.key)}
									{@const c = cell(p.payload?.[col.key])}
									<td class="v {c.kind}" title={c.text}>{c.text}</td>
								{/each}
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}

		<div class="tbl-foot">
			<span class="num">
				{#if visible.length}
					{int(first)} to {int(first + rows.length - 1)}{#if total !== null}&nbsp;of {int(total)}{/if}
					{#if visible.length < rows.length}<span class="dim">, {visible.length} shown</span>{/if}
				{:else if loading}
					Loading
				{:else}
					&nbsp;
				{/if}
			</span>
			<span class="foot-r">
				<span class="hint"><kbd>j</kbd><kbd>k</kbd> move <kbd>/</kbd> search</span>
				{#if !compact}
					<span class="pager">
						<button class="btn sm sq" aria-label="Previous page" disabled={pageIndex === 0 || loading} onclick={() => load(pageIndex - 1)}><Icon name="left" size={12} /></button>
						<button class="btn sm sq" aria-label="Next page" disabled={next === null || loading} onclick={() => load(pageIndex + 1)}><Icon name="chevron" size={12} /></button>
					</span>
				{/if}
			</span>
		</div>
	</section>

	<section class="panel pane" aria-labelledby="h-pl-{name}">
		<div class="panel-h">
			<h2 id="h-pl-{name}">
				{#if point}Point <span class="mono pid">{point.id}</span>{:else}Point{/if}
			</h2>
			<div class="aside">
				{#if point && !editing}
					<div class="seg-ctl" role="group" aria-label="Pane view">
						<button aria-pressed={mode === 'payload'} onclick={() => (mode = 'payload')}>Payload</button>
						<button aria-pressed={mode === 'vector'} onclick={() => (mode = 'vector')}>Vector</button>
					</div>
					<button class="icon-btn" aria-label="Copy JSON" title="Copy JSON" onclick={copy}><Icon name="copy" /></button>
					{#if app.canWrite}
						<button class="icon-btn" aria-label="Edit payload" title="Edit payload" onclick={startEdit}><Icon name="edit" /></button>
						<button class="icon-btn del" aria-label="Delete point" title="Delete point" onclick={() => ((deleteId = point?.id ?? null), (deleting = true))}><Icon name="trash" /></button>
					{/if}
				{/if}
			</div>
		</div>
		{#if !point}
			<p class="pane-empty">{loading ? 'Loading' : 'Select a point to see its payload.'}</p>
		{:else if editing}
			<form class="edit" onsubmit={(e) => (e.preventDefault(), save())}>
				<textarea
					class="textarea"
					bind:value={draft}
					rows={Math.min(18, Math.max(6, draft.split('\n').length + 1))}
					spellcheck="false"
					aria-label="Payload JSON"
					aria-invalid={!!draftError}
					readonly={saving}
					onkeydown={(e) => {
						if (e.key === 'Escape') editing = false;
						if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) save();
					}}
				></textarea>
				{#if draftError}<p class="error-line" role="alert"><Icon name="warn" />{draftError}</p>{/if}
				<div class="edit-row">
					<span class="dim">Replaces the whole payload, <kbd>⌘</kbd><kbd>Enter</kbd> saves</span>
					<button type="button" class="btn sm ghost" onclick={() => (editing = false)}>Cancel</button>
					<button type="submit" class="btn sm primary" disabled={saving}>{saving ? 'Saving' : 'Save payload'}</button>
				</div>
			</form>
		{:else if mode === 'payload'}
			{#if unsafe}
				<p class="unsafe"><Icon name="warn" size={12} />Integers beyond 2^53 show rounded here, the API returns them exactly</p>
			{/if}
			<JsonView lines={lines(point.payload ?? {})} label="Payload JSON" />
		{:else if vector}
			<JsonView lines={vectorLines(vector)} label="Vector JSON" />
		{:else}
			<p class="pane-empty">Loading the vector</p>
		{/if}
		{#if point && !editing}
			<dl class="pane-foot">
				<div>
					<dt>Vector</dt>
					{#if vecPreview}<dd class="mono">{vecPreview}</dd>
					{:else}<dd class="dim">{mode === 'vector' ? 'Loading' : 'Press v or choose Vector to load it'}</dd>{/if}
				</div>
			</dl>
		{/if}
	</section>
</div>

{#if deleteId !== null}
	<Confirm bind:open={deleting} title="Delete point {deleteId}?" action="Delete point" onconfirm={remove}>
		<p>Its vector and payload go once the write is durable, and this cannot be undone.</p>
	</Confirm>
{/if}

<style>
	.browser {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(320px, 380px);
		gap: 16px;
		align-items: start;
		min-width: 0;
	}
	.browser.compact {
		grid-template-columns: minmax(0, 1fr);
	}
	.pane {
		position: sticky;
		top: 0;
	}
	.compact .pane {
		position: static;
	}
	.scroll {
		overflow-x: auto;
	}
	.points {
		table-layout: fixed;
	}
	.points th.id-col {
		width: 92px;
	}
	.compact .points th.id-col {
		width: 72px;
	}
	.points td {
		height: 28px;
		cursor: default;
		overflow: hidden;
		text-overflow: ellipsis;
		max-width: 0;
	}
	.points tbody tr {
		outline: none;
	}
	.points tbody tr:hover td {
		background: var(--hover);
	}
	.points tbody tr[aria-selected='true'] td {
		background: var(--accent-soft);
	}
	.points tbody tr[aria-selected='true'] td:first-child {
		box-shadow: inset 2px 0 0 var(--accent);
	}
	.points tbody tr:focus-visible td {
		box-shadow:
			inset 0 1px 0 var(--accent-line),
			inset 0 -1px 0 var(--accent-line);
	}
	.points tbody tr:focus-visible td:first-child {
		box-shadow:
			inset 2px 0 0 var(--accent),
			inset 0 1px 0 var(--accent-line),
			inset 0 -1px 0 var(--accent-line);
	}
	td.id {
		font-family: var(--mono);
		font-size: 12px;
		color: var(--text-2);
	}
	tr[aria-selected='true'] td.id {
		color: var(--text);
	}
	td.v {
		color: var(--text);
	}
	td.num,
	td.date,
	td.bool,
	td.null {
		font-family: var(--mono);
		font-size: 12px;
	}
	td.date,
	td.list,
	td.obj {
		color: var(--text-2);
	}
	td.null,
	td.none {
		color: var(--text-3);
	}
	td.bool {
		color: #e58aa6;
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		height: 22px;
		max-width: 220px;
		padding: 0 7px;
		border-radius: var(--r-sm);
		background: var(--accent-soft);
		border: 1px solid var(--accent-line);
		color: var(--accent-ink);
	}
	.chip .mono {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-size: 11px;
	}
	.link {
		color: var(--text-2);
		text-decoration: none;
		font-size: 12px;
		display: inline-flex;
		align-items: center;
		gap: 4px;
	}
	.link:hover {
		color: var(--text);
	}
	.tbl-foot {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		min-height: 32px;
		padding: 0 10px 0 16px;
		border-top: 1px solid var(--line);
		color: var(--text-3);
		font-size: 12px;
	}
	.foot-r {
		display: inline-flex;
		align-items: center;
		gap: 14px;
	}
	.hint {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.pager {
		display: inline-flex;
		gap: 4px;
	}
	.state-msg {
		padding: 16px;
	}
	.state-msg code {
		font-family: var(--mono);
	}
	.empty code {
		color: var(--text-2);
	}
	.pid {
		font-size: 13px;
		font-weight: 500;
	}
	.seg-ctl {
		display: inline-flex;
		border: 1px solid var(--line-2);
		border-radius: var(--r-sm);
		padding: 1px;
		background: var(--ground);
	}
	.seg-ctl button {
		height: 20px;
		padding: 0 8px;
		border: 0;
		border-radius: 3px;
		background: none;
		color: var(--text-3);
		font-size: 12px;
	}
	.seg-ctl button:hover {
		color: var(--text);
	}
	.seg-ctl button[aria-pressed='true'] {
		background: var(--raised);
		color: var(--text);
	}
	.icon-btn.del:hover {
		color: var(--bad);
		background: var(--bad-soft);
	}
	.unsafe {
		margin: 0;
		padding: 8px 16px;
		display: flex;
		gap: 8px;
		align-items: center;
		color: var(--warn);
		font-size: 12px;
		border-bottom: 1px solid var(--line);
	}
	.pane-empty {
		margin: 0;
		padding: 28px 16px;
		color: var(--text-3);
		text-align: center;
	}
	.pane :global(.code) {
		max-height: min(56vh, 520px);
	}
	.compact .pane :global(.code) {
		max-height: 340px;
	}
	.pane-foot {
		margin: 0;
		border-top: 1px solid var(--line);
		padding: 9px 16px 10px;
		font-size: 12px;
	}
	.pane-foot div {
		display: flex;
		gap: 12px;
		align-items: baseline;
	}
	.pane-foot dt {
		color: var(--text-3);
		width: 52px;
		flex: none;
	}
	.pane-foot dd {
		margin: 0;
		color: var(--text-2);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.edit {
		padding: 12px 14px 14px;
		display: grid;
		gap: 10px;
	}
	.edit-row {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.edit-row .dim {
		margin-right: auto;
		font-size: 12px;
		display: inline-flex;
		gap: 4px;
		align-items: center;
	}
	@media (max-width: 1100px) {
		.browser {
			grid-template-columns: minmax(0, 1fr);
		}
		.pane {
			position: static;
		}
	}
	@media (max-width: 560px) {
		.hint {
			display: none;
		}
		.points th:nth-child(n + 4),
		.points td:nth-child(n + 4) {
			display: none;
		}
	}
</style>
