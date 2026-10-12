<script lang="ts">
	import { untrack } from 'svelte';
	import { Streams } from '#lib/streams.ts';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { api, ApiError, type CollectionStatus, type Distance, type ScheduleState } from '#lib/api.ts';
	import { app, toasts } from '#lib/state.svelte.ts';
	import { bytes, int, plural, relative, own } from '#lib/format.ts';
	import Icon from '#lib/components/Icon.svelte';

	type Detail = { status?: CollectionStatus; schedule?: ScheduleState | null };
	let details = $state<Record<string, Detail>>({});

	// Status and schedules are fetched per collection whenever the set of names changes or
	// something was changed from the console.
	// Schedules run and fail on their own, so the details are fetched again every 15 s too.
	let tick = $state(0);
	let lastTick = 0;
	$effect(() => {
		const t = setInterval(() => {
			if (document.visibilityState === 'visible') tick++;
		}, 15_000);
		return () => clearInterval(t);
	});
	const names = $derived(app.collections.map((c) => c.name).join('\n'));
	$effect(() => {
		void app.changes;
		const routine = tick !== lastTick;
		lastTick = tick;
		const list = names ? names.split('\n') : [];
		// Rows of deleted collections go.
		untrack(() => {
			for (const old of Object.keys(details)) if (!list.includes(old)) delete details[old];
		});
		untrack(() => fetchAll(list, routine));
	});

	const known = (name: string) => app.collections.some((c) => c.name === name);

	const streams = new Streams();

	function fetchAll(list: string[], routine: boolean) {
		for (const name of list) {
			const st = streams.start(`${name}\0status`, routine);
			if (st !== null) {
				api.status(name).then(
					(status) => {
						if (streams.finish(`${name}\0status`, st) && known(name)) details[name] = { ...own(details, name), status };
					},
					() => streams.finish(`${name}\0status`, st)
				);
			}
			const sc = streams.start(`${name}\0schedule`, routine);
			if (sc !== null) {
				api.schedule(name).then(
					(schedule) => {
						if (streams.finish(`${name}\0schedule`, sc) && known(name)) details[name] = { ...own(details, name), schedule };
					},
					(e) => {
						const fresh = streams.finish(`${name}\0schedule`, sc);
						if (fresh && e instanceof ApiError && e.status === 404 && known(name)) details[name] = { ...own(details, name), schedule: null };
					}
				);
			}
		}
	}


	const total = $derived(app.collections.reduce((n, c) => n + c.points, 0));
	const disk = $derived(app.node?.disk.collections ?? null);

	// The form opens from the sidebar's plus, as /?new, or from the button here.
	let creating = $state(false);
	$effect(() => {
		if (page.url.searchParams.has('new')) creating = true;
	});
	let name = $state('');
	let dim = $state(768);
	let distance = $state<Distance>('cosine');
	let busy = $state(false);
	let error = $state<ApiError | null>(null);

	async function create(e: SubmitEvent) {
		e.preventDefault();
		if (busy) return;
		// What was submitted, so edits made while the request runs cannot change its outcome.
		const target = name;
		busy = true;
		error = null;
		try {
			const r = await api.create(target, { dim, distance });
			toasts.show(r.created ? `Created ${target}` : `${target} already exists with that config`);
			await app.changed();
			goto(`/collections/${encodeURIComponent(target)}`);
		} catch (err) {
			error = err instanceof ApiError ? err : new ApiError(0, 'unknown', String(err));
		} finally {
			busy = false;
		}
	}

	function close() {
		creating = false;
		error = null;
		if (page.url.searchParams.has('new')) goto('/', { replaceState: true });
	}

	const validName = $derived(/^[a-z0-9][a-z0-9_-]{0,62}$/.test(name));
</script>

<svelte:head><title>Collections · nocturnum</title></svelte:head>

<div class="page-head flat">
	<div class="head-row">
		<div>
			<div class="title"><h1>Collections</h1></div>
			<div class="meta num">
				<span>{plural(app.collections.length, 'collection')}</span>
				<span>{plural(total, 'point')}</span>
				{#if disk}
					<span>{bytes(disk.available_bytes)} free of {bytes(disk.total_bytes)}</span>
				{/if}
			</div>
		</div>
		{#if app.canWrite && !creating}
			<div class="actions">
				<button class="btn primary" onclick={() => (creating = true)}><Icon name="plus" />New collection</button>
			</div>
		{/if}
	</div>
</div>

<main class="body">
	<div class="stack">
		{#if creating}
			<form class="panel create" onsubmit={create}>
				<div class="panel-h">
					<h2>New collection</h2>
					<div class="aside">
						<button type="button" class="icon-btn" aria-label="Close" onclick={close}><Icon name="x" /></button>
					</div>
				</div>
				<fieldset class="panel-body" disabled={busy}>
					<div class="form-row">
						<label class="field name">
							<span>Name</span>
							<!-- svelte-ignore a11y_autofocus -->
							<input
								class="input mono"
								bind:value={name}
								placeholder="notes"
								autocomplete="off"
								spellcheck="false"
								autofocus
								aria-invalid={name !== '' && !validName}
							/>
							<small>Lowercase letters, digits, <code>_</code> and <code>-</code>, up to 63</small>
						</label>
						<label class="field">
							<span>Dimensions</span>
							<input class="input num" type="number" min="1" max="65536" bind:value={dim} />
							<small>Your embedding model's output size</small>
						</label>
						<label class="field">
							<span>Distance</span>
							<select class="select-native" bind:value={distance}>
								<option value="cosine">Cosine</option>
								<option value="dot">Dot product</option>
								<option value="euclid">Euclidean</option>
								<option value="manhattan">Manhattan</option>
							</select>
							<small>Cosine suits most text models</small>
						</label>
						<div class="submit">
							<button class="btn primary" type="submit" disabled={busy || !validName || !(dim >= 1)}>
								{busy ? 'Creating' : 'Create collection'}
							</button>
						</div>
					</div>
					{#if error}
						<p class="error-line" role="alert">
							<Icon name="warn" />{error.message}
						</p>
					{/if}
				</fieldset>
			</form>
		{/if}

		{#if !app.loaded}
			<section class="panel">
				<div class="panel-body stack">
					<span class="skeleton" style="width: 40%"></span>
					<span class="skeleton" style="width: 70%"></span>
					<span class="skeleton" style="width: 55%"></span>
				</div>
			</section>
		{:else if app.collections.length === 0}
			<section class="panel">
				<div class="empty">
					<strong>No collections yet</strong>
					<p>
						A collection holds points of one vector size and distance, each with a JSON payload. Create one
						here, or from a script:
					</p>
					<pre class="snippet">curl -X PUT {location.origin}/v1/collections/notes \
  -H 'Content-Type: application/json' \
  -d '{`{"vectors": {"dim": 768, "distance": "cosine"}}`}'</pre>
					{#if app.canWrite && !creating}
						<button class="btn primary" onclick={() => (creating = true)}><Icon name="plus" />New collection</button>
					{/if}
				</div>
			</section>
		{:else}
			<section class="panel" aria-labelledby="h-all">
				<div class="panel-h">
					<h2 id="h-all">All collections</h2>
					<span class="dim num">{app.collections.length}</span>
				</div>
				<div class="scroll-x">
					<table class="num">
						<thead>
							<tr>
								<th scope="col">Name</th>
								<th scope="col">Status</th>
								<th scope="col" class="r">Points</th>
								<th scope="col">Vectors</th>
								<th scope="col" class="r">Segments</th>
								<th scope="col" class="r">Disk</th>
								<th scope="col">Snapshots</th>
							</tr>
						</thead>
						<tbody>
							{#each app.collections as c (c.name)}
								{@const d = own(details, c.name)}
								<tr class="link-row" onclick={() => goto(`/collections/${encodeURIComponent(c.name)}`)}>
									<td>
										<a class="name" href="/collections/{encodeURIComponent(c.name)}">{c.name}</a>
									</td>
									<td>
										{#if c.failure}
											<span class="pill bad" title={c.failure}><i class="dot"></i>needs restart</span>
										{:else if d?.status?.optimizer.running}
											<span class="pill idle"><i class="dot"></i>optimizing</span>
										{:else}
											<span class="pill ok"><i class="dot"></i>ok</span>
										{/if}
									</td>
									<td class="r">{int(c.points)}</td>
									<td class="muted">
										<span class="mono">{c.vectors.dim}</span> dims, {c.vectors.distance}
									</td>
									<td class="r muted">{c.segments}</td>
									<td class="r muted">{d?.status ? bytes(d.status.disk_bytes) : '…'}</td>
									<td class="muted">
										{#if d?.schedule === undefined}
											…
										{:else if d.schedule}
											<span class="sched">
												<code class="mono">{d.schedule.cron}</code>
												{#if d.schedule.last_error}
													<span class="warn-txt">last run failed</span>
												{:else}
													<span class="dim">next {relative(d.schedule.next_run)}</span>
												{/if}
											</span>
										{:else}
											<span class="dim">no schedule</span>
										{/if}
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</section>
		{/if}
	</div>
</main>

<style>
	fieldset {
		margin: 0;
		border: 0;
		min-width: 0;
	}
	.create .form-row {
		display: grid;
		grid-template-columns: minmax(180px, 1.4fr) minmax(120px, 0.8fr) minmax(140px, 1fr) auto;
		gap: 16px;
		align-items: start;
	}
	.submit {
		padding-top: 22px;
	}
	.create .error-line {
		margin: 12px 0 0;
	}
	small code {
		font-family: var(--mono);
		color: var(--text-2);
	}
	.scroll-x {
		overflow-x: auto;
	}
	.name {
		font-weight: 500;
		color: var(--text);
		text-decoration: none;
	}
	.name:hover {
		text-decoration: underline;
	}
	.sched {
		display: inline-flex;
		gap: 10px;
		align-items: baseline;
	}
	.sched code {
		color: var(--text-2);
	}
	.warn-txt {
		color: var(--warn);
	}
	.snippet {
		margin: 4px 0 8px;
		padding: 12px 14px;
		background: var(--ground);
		border: 1px solid var(--line);
		border-radius: var(--r);
		font-family: var(--mono);
		font-size: 12px;
		color: var(--text-2);
		text-align: left;
		white-space: pre-wrap;
		max-width: 100%;
	}
	@media (max-width: 760px) {
		fieldset {
		margin: 0;
		border: 0;
		min-width: 0;
	}
	.create .form-row {
			grid-template-columns: 1fr;
		}
		.submit {
			padding-top: 0;
		}
	}
</style>
