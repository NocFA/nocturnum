<script lang="ts">
	import { untrack } from 'svelte';
	import { Streams } from '#lib/streams.ts';
	import { goto } from '$app/navigation';
	import Icon from '#lib/components/Icon.svelte';
	import { api, ApiError, type ScheduleState, type Snapshot } from '#lib/api.ts';
	import { app } from '#lib/state.svelte.ts';
	import { bytes, plural, relative, utc, own } from '#lib/format.ts';

	type Row = { snapshots?: Snapshot[]; schedule?: ScheduleState | null };
	let rows = $state<Record<string, Row>>({});
	let now = $state(Date.now());

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
		now = Date.now();
		const list = names ? names.split('\n') : [];
		untrack(() => {
			for (const old of Object.keys(rows)) if (!list.includes(old)) delete rows[old];
		});
		untrack(() => fetchAll(list, routine));
	});

	const known = (name: string) => app.collections.some((c) => c.name === name);

	const streams = new Streams();

	function fetchAll(list: string[], routine: boolean) {
		for (const name of list) {
			const sn = streams.start(`${name}\0snapshots`, routine);
			if (sn !== null) {
				api.snapshots(name).then(
					(r) => {
						if (streams.finish(`${name}\0snapshots`, sn) && known(name)) rows[name] = { ...own(rows, name), snapshots: r.snapshots };
					},
					() => streams.finish(`${name}\0snapshots`, sn)
				);
			}
			const sc = streams.start(`${name}\0schedule`, routine);
			if (sc !== null) {
				api.schedule(name).then(
					(sched) => {
						if (streams.finish(`${name}\0schedule`, sc) && known(name)) rows[name] = { ...own(rows, name), schedule: sched };
					},
					(e) => {
						const fresh = streams.finish(`${name}\0schedule`, sc);
						if (fresh && e instanceof ApiError && e.status === 404 && known(name)) rows[name] = { ...own(rows, name), schedule: null };
					}
				);
			}
		}
	}


	const all = $derived(Object.values(rows).flatMap((r) => r.snapshots ?? []));
	const failing = $derived(Object.values(rows).filter((r) => r.schedule?.last_error).length);
	const unscheduled = $derived(app.collections.filter((c) => own(rows, c.name)?.schedule === null).length);
	const storage = $derived(app.node?.disk.snapshots ?? null);
	let lookup = $state('');
</script>

<svelte:head><title>Snapshots · nocturnum</title></svelte:head>

<div class="page-head flat">
	<div class="head-row">
		<div>
			<div class="title"><h1>Snapshots</h1></div>
			<div class="meta num">
				<span>{plural(all.length, 'snapshot')}</span>
				<span>{bytes(all.reduce((n, s) => n + s.size, 0))} kept</span>
				{#if storage}<span>{bytes(storage.available_bytes)} free where they are kept</span>{/if}
			</div>
		</div>
	</div>
</div>

<main class="body">
	<div class="stack">
		{#if failing || unscheduled}
			<div class="notes">
				{#if failing}<span class="flag bad">{failing === 1 ? '1 schedule' : `${failing} schedules`} failed on the last run</span>{/if}
				{#if unscheduled}<span class="flag">{unscheduled === 1 ? '1 collection has' : `${unscheduled} collections have`} no schedule</span>{/if}
			</div>
		{/if}
		<section class="panel" aria-labelledby="h-sn">
			<div class="panel-h"><h2 id="h-sn">By collection</h2></div>
			{#if app.collections.length === 0}
				<div class="empty"><strong>No collections yet</strong><p>Snapshots appear here once collections have them.</p></div>
			{:else}
				<div class="scroll">
					<table class="num">
						<thead>
							<tr>
								<th scope="col">Collection</th>
								<th scope="col">Schedule</th>
								<th scope="col">Next run</th>
								<th scope="col">Last success</th>
								<th scope="col" class="r">Snapshots</th>
								<th scope="col" class="r">Size</th>
								<th scope="col">Newest (UTC)</th>
							</tr>
						</thead>
						<tbody>
							{#each app.collections as c (c.name)}
								{@const r = own(rows, c.name)}
								{@const newest = r?.snapshots?.at(-1)}
								<tr class="link-row" onclick={() => goto(`/collections/${encodeURIComponent(c.name)}/snapshots`)}>
									<td><a class="name" href="/collections/{encodeURIComponent(c.name)}/snapshots">{c.name}</a></td>
									<td>
										{#if r?.schedule === undefined}<span class="dim">…</span>
										{:else if r.schedule}<code class="mono">{r.schedule.cron}</code> <span class="dim">keep {r.schedule.keep}</span>
										{:else}<span class="dim">none</span>{/if}
									</td>
									<td class="muted">{r?.schedule ? relative(r.schedule.next_run, now) : ''}</td>
									<td>
										{#if r?.schedule?.last_error}<span class="bad" title={r.schedule.last_error}>failed, {relative(r.schedule.last_run, now)}</span>
										{:else if r?.schedule}<span class="muted">{relative(r.schedule.last_success, now)}</span>{/if}
									</td>
									<td class="r">{r?.snapshots ? r.snapshots.length : '…'}</td>
									<td class="r muted">{r?.snapshots ? bytes(r.snapshots.reduce((n, s) => n + s.size, 0)) : ''}</td>
									<td class="mono muted">{newest ? utc(newest.created_at) : ''}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</section>
		<form
			class="panel lookup"
			onsubmit={(e) => {
				e.preventDefault();
				if (lookup.trim()) goto(`/collections/${encodeURIComponent(lookup.trim())}/snapshots`);
			}}
		>
			<label class="field">
				<span>A deleted collection's snapshots</span>
				<div class="row">
					<input class="input mono" bind:value={lookup} placeholder="collection name" spellcheck="false" autocomplete="off" />
					<button class="btn" disabled={!lookup.trim()}><Icon name="search" />Open</button>
				</div>
				<small>Snapshots stay on disk after their collection is deleted, and restoring one brings the collection back</small>
			</label>
		</form>
	</div>
</main>

<style>
	.notes {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
	}
	.scroll {
		overflow-x: auto;
	}
	.name {
		color: var(--text);
		font-weight: 500;
		text-decoration: none;
	}
	code {
		color: var(--text-2);
	}
	.bad {
		color: var(--bad);
	}
	.lookup {
		padding: 14px 16px 16px;
		max-width: 560px;
	}
	.row {
		display: flex;
		gap: 8px;
	}
</style>
