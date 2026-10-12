<script lang="ts">
	import { onMount } from 'svelte';
	import { api, ApiError, type CollectionStatus } from '#lib/api.ts';
	import { app, toasts } from '#lib/state.svelte.ts';
	import { bytes, duration, int, mergeStage, ms, percent, utc, own } from '#lib/format.ts';
	import { byCollection, parse, routes, type RouteStats } from '#lib/prom.ts';
	import Icon from '#lib/components/Icon.svelte';
	import { Streams } from '#lib/streams.ts';

	let stats = $state<RouteStats[]>([]);
	let fsyncSeconds = $state(new Map<string, number>());
	let statuses = $state<Record<string, CollectionStatus>>({});
	let metricsError = $state<string | null>(null);

	const streams = new Streams();

	async function refresh(routine = false) {
		const m = streams.start('metrics', routine);
		if (m !== null) {
			api.metrics().then(
				(text) => {
					if (!streams.finish('metrics', m)) return;
					const samples = parse(text);
					stats = routes(samples);
					fsyncSeconds = byCollection(samples, 'nocturnum_wal_fsync_seconds_total');
					metricsError = null;
				},
				(e) => {
					if (streams.finish('metrics', m)) metricsError = e instanceof ApiError ? e.message : String(e);
				}
			);
		}
		for (const c of app.collections) {
			// Collection keys carry a suffix, so a collection called metrics has its own stream.
			const key = `${c.name}\0status`;
			const n = streams.start(key, routine);
			if (n === null) continue;
			api.status(c.name).then(
				(s) => {
					if (streams.finish(key, n)) statuses[c.name] = s;
				},
				() => streams.finish(key, n)
			);
		}
	}

	onMount(() => {
		refresh();
		const t = setInterval(() => {
			if (document.visibilityState === 'visible') refresh(true);
		}, 5000);
		return () => clearInterval(t);
	});

	const node = $derived(app.node);
	const totalRequests = $derived(stats.reduce((n, r) => n + r.requests, 0));
	const totalErrors = $derived(stats.reduce((n, r) => n + r.serverErrors, 0));
	const roles = { admin: 'admin key', 'read-only': 'read-only key', anonymous: 'no keys set' };

	// The raw text is fetched with the key and opened from memory, since a plain link would
	// go without the Authorization header and be refused on a server with keys.
	async function raw() {
		try {
			const text = await api.metrics();
			const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
			window.open(url, '_blank', 'noopener');
			setTimeout(() => URL.revokeObjectURL(url), 60_000);
		} catch (e) {
			toasts.error(e);
		}
	}

	function job(s: CollectionStatus) {
		const j = s.optimizer.last;
		if (s.optimizer.last_error) return 'failed';
		if (s.optimizer.running) {
			const stage = mergeStage(s.optimizer.progress);
			return `rewriting ${s.optimizer.running.join(', ')}${stage ? `, ${stage}` : ''}`;
		}
		if (!j) return 'none yet';
		if (j.kind === 'flush') return 'flush';
		if (j.kind === 'vacuum') return `vacuum ${j.inputs?.[0]}`;
		return `merge ${j.inputs?.join(' + ')}`;
	}
</script>

<svelte:head><title>Telemetry · nocturnum</title></svelte:head>

<div class="page-head flat">
	<div class="head-row">
		<div>
			<div class="title"><h1>Telemetry</h1></div>
			<div class="meta num">
				{#if node}
					<span>nocturnum <code>{node.version}</code></span>
					<span>up {duration(node.uptime_seconds)}, since {utc(node.started_at)} UTC</span>
					<span>{int(totalRequests)} requests served</span>
				{/if}
			</div>
		</div>
		<div class="actions">
			<button class="btn" onclick={raw}><Icon name="external" />Raw metrics</button>
		</div>
	</div>
</div>

<main class="body">
	<div class="stack">
		<div class="cards">
			<section class="panel">
				<div class="panel-h"><h2>Node</h2></div>
				<dl class="kv panel-body num">
					<dt>Health</dt>
					<dd>
						{#if app.problem}<span class="bad" title={app.problem.message}>{app.problem.status === 0 ? 'unreachable, last state below' : app.problem.message}</span>
						{:else if app.collections.some((c) => c.failure)}<span class="bad">{app.collections.filter((c) => c.failure).length} collection needs a restart</span>
						{:else if app.notReady}<span class="bad" title={app.notReady}>not ready, {app.notReady}</span>
						{:else}<span class="ok">every collection healthy</span>{/if}
					</dd>
					<dt>Mode</dt><dd>single node</dd>
					<dt>This browser</dt><dd>{node ? roles[node.role] : '…'}</dd>
					<dt>Server errors</dt><dd class:bad={totalErrors > 0}>{int(totalErrors)}</dd>
				</dl>
			</section>
			{#each [['Collections disk', node?.disk.collections], ['Snapshots disk', node?.disk.snapshots]] as [label, d] (label)}
				<section class="panel">
					<div class="panel-h"><h2>{label}</h2></div>
					<div class="panel-body num">
						{#if d && typeof d === 'object'}
							{@const used = 1 - d.available_bytes / d.total_bytes}
							<div class="disk-row">
								<span class="big">{bytes(d.available_bytes)}</span>
								<span class="dim">free of {bytes(d.total_bytes)}</span>
							</div>
							<div class="bar lg" role="progressbar" aria-label="{label} used" aria-valuenow={Math.round(used * 100)} aria-valuemin={0} aria-valuemax={100}>
								<i class:warn={used > 0.9} style="width: {used * 100}%"></i>
							</div>
							<p class="dim small">{percent(used, 0)} used</p>
						{:else}
							<p class="dim">This platform does not report disk space.</p>
						{/if}
					</div>
				</section>
			{/each}
		</div>

		<section class="panel" aria-labelledby="h-eng">
			<div class="panel-h"><h2 id="h-eng">Collections</h2><div class="aside">engine state, refreshed every 5 s</div></div>
			<div class="scroll">
				<table class="num">
					<thead>
						<tr>
							<th scope="col">Collection</th>
							<th scope="col" class="r">Points</th>
							<th scope="col" class="r">Segments</th>
							<th scope="col" class="r">Memtable</th>
							<th scope="col" class="r">WAL</th>
							<th scope="col" class="r">Last seq</th>
							<th scope="col" class="r">fsyncs</th>
							<th scope="col" class="r">Mean fsync</th>
							<th scope="col">Optimizer</th>
						</tr>
					</thead>
					<tbody>
						{#each app.collections as c (c.name)}
							{@const s = own(statuses, c.name)}
							{@const fs = fsyncSeconds.get(c.name)}
							<tr>
								<td><a class="name" href="/collections/{encodeURIComponent(c.name)}">{c.name}</a>{#if c.failure}<span class="flag bad">{c.failure}</span>{/if}</td>
								<td class="r">{int(c.points)}</td>
								<td class="r muted">{c.segments}</td>
								<td class="r muted">{s ? int(s.memtable.entries) : '…'}</td>
								<td class="r muted">{s ? bytes(s.wal.bytes) : '…'}</td>
								<td class="r mono muted">{s ? int(s.wal.last_seq) : '…'}</td>
								<td class="r muted">{s ? int(s.wal.fsyncs) : '…'}</td>
								<td class="r muted">{s && fs !== undefined && s.wal.fsyncs ? `${ms((fs / s.wal.fsyncs) * 1000)} ms` : '–'}</td>
								<td class:bad={!!s?.optimizer.last_error} title={s?.optimizer.last_error ?? ''}>{s ? job(s) : '…'}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</section>

		<section class="panel" aria-labelledby="h-req">
			<div class="panel-h"><h2 id="h-req">Requests</h2><div class="aside">since the server started</div></div>
			{#if metricsError}
				<p class="panel-body error-line">{metricsError}</p>
			{:else if stats.length === 0}
				<p class="panel-body dim">No requests recorded yet.</p>
			{:else}
				<div class="scroll">
					<table class="num">
						<thead>
							<tr>
								<th scope="col">Route</th>
								<th scope="col" class="r">Requests</th>
								<th scope="col" class="r">4xx</th>
								<th scope="col" class="r">5xx</th>
								<th scope="col" class="r">Mean</th>
								<th scope="col" class="r">p95 under</th>
							</tr>
						</thead>
						<tbody>
							{#each stats as r (r.route)}
								<tr>
									<td class="mono route">{r.route}</td>
									<td class="r">{int(r.requests)}</td>
									<td class="r muted">{r.clientErrors ? int(r.clientErrors) : '–'}</td>
									<td class="r" class:bad={r.serverErrors > 0}>{r.serverErrors ? int(r.serverErrors) : '–'}</td>
									<td class="r muted">{ms(r.meanMs)} ms</td>
									<td class="r muted">{r.p95Ms === null ? '–' : `${ms(r.p95Ms)} ms`}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</section>
		<p class="help">nocturnum never sends telemetry anywhere, these numbers come from this server's own /metrics.</p>
	</div>
</main>

<style>
	.cards {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 16px;
	}
	.disk-row {
		display: flex;
		align-items: baseline;
		gap: 8px;
		margin-bottom: 10px;
	}
	.big {
		font-size: 18px;
		font-weight: 500;
		letter-spacing: -0.01em;
	}
	.small {
		margin: 8px 0 0;
		font-size: 12px;
	}
	.ok {
		color: var(--ok);
	}
	.bad {
		color: var(--bad);
	}
	.scroll {
		overflow-x: auto;
	}
	.name {
		color: var(--text);
		font-weight: 500;
		text-decoration: none;
	}
	.flag {
		margin-left: 8px;
	}
	.route {
		color: var(--text-2);
	}
	code {
		font-family: var(--mono);
		color: var(--text);
	}
	.help {
		margin: 0 2px;
		color: var(--text-3);
		font-size: 12px;
	}
	@media (max-width: 1000px) {
		.cards {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
