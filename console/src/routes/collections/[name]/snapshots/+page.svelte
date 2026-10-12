<script lang="ts">
	import { onMount } from 'svelte';
	import { api, ApiError, downloadSnapshot, type ScheduleState, type Snapshot } from '#lib/api.ts';
	import { current } from '#lib/collection.svelte.ts';
	import { app, toasts } from '#lib/state.svelte.ts';
	import { bytes, relative, utc } from '#lib/format.ts';
	import Icon from '#lib/components/Icon.svelte';
	import { copyText } from '#lib/clipboard.ts';
	import Confirm from '#lib/components/Confirm.svelte';

	const view = current();

	let schedule = $state<ScheduleState | null | undefined>(undefined);
	let cron = $state('0 3 * * *');
	let keep = $state(7);
	let saving = $state(false);
	/** Whether the form holds edits the server has not seen, which polling must not undo. */
	let dirty = $state(false);
	let scheduleError = $state<string | null>(null);
	let now = $state(Date.now());

	// Every read and change takes a number, and a read that answers after a newer one began,
	// or after a save, is dropped.
	let revision = 0;
	let reading = 0;

	async function loadSchedule(routine = false) {
		// Reads wait while a save or remove is in flight, and the change bumps the number again
		// when it lands, so a read started meanwhile cannot undo it. A routine poll also waits
		// for the last read to answer, so a slow server is not asked again first.
		if (saving || (routine && reading > 0)) return;
		const mine = ++revision;
		reading++;
		try {
			const s = await api.schedule(view.name);
			if (mine !== revision) return;
			schedule = s;
			if (!dirty) {
				cron = schedule.cron;
				keep = schedule.keep;
			}
		} catch (e) {
			if (mine === revision && e instanceof ApiError && e.status === 404) schedule = null;
		} finally {
			reading--;
		}
	}

	onMount(() => {
		loadSchedule();
		view.loadSnapshots();
		const t = setInterval(() => {
			now = Date.now();
			if (document.visibilityState === 'visible') {
				loadSchedule(true);
				view.pollSnapshots();
			}
		}, 15_000);
		return () => clearInterval(t);
	});

	async function save(e: SubmitEvent) {
		e.preventDefault();
		if (saving) return;
		saving = true;
		scheduleError = null;
		revision++;
		try {
			schedule = await api.setSchedule(view.name, cron.trim(), keep);
			revision++;
			dirty = false;
			toasts.show(`Schedule saved, next run ${relative(schedule.next_run)}`);
		} catch (err) {
			scheduleError = err instanceof Error ? err.message : String(err);
		} finally {
			saving = false;
		}
	}

	async function unschedule() {
		if (saving) return;
		saving = true;
		revision++;
		try {
			await api.deleteSchedule(view.name);
			revision++;
			schedule = null;
			dirty = false;
			toasts.show('Schedule removed, the snapshots are kept');
		} catch (e) {
			toasts.error(e);
		} finally {
			saving = false;
		}
	}

	const presets = [
		{ label: 'Hourly', cron: '0 * * * *' },
		{ label: 'Daily 03:00', cron: '0 3 * * *' },
		{ label: 'Sundays 03:00', cron: '0 3 * * 0' }
	];

	let restoring = $state<Snapshot | null>(null);
	let removing = $state<Snapshot | null>(null);
	let restoreOpen = $state(false);
	let removeOpen = $state(false);

	async function restore() {
		if (!restoring) return;
		const s = restoring;
		try {
			await api.restore(view.name, s.name);
			toasts.show(`Restored ${view.name} from ${s.name}`);
			await Promise.all([view.load(), app.changed()]);
		} catch (e) {
			toasts.error(e);
		}
	}

	async function remove() {
		if (!removing) return;
		const s = removing;
		try {
			await api.deleteSnapshot(view.name, s.name);
			toasts.show(`Deleted ${s.name}`);
			await view.loadSnapshots();
		} catch (e) {
			toasts.error(e);
		}
	}

	async function download(s: Snapshot) {
		try {
			toasts.show(`Downloading ${s.name}, ${bytes(s.size)}`);
			await downloadSnapshot(view.name, s.name);
		} catch (e) {
			toasts.error(e);
		}
	}

	function copy(text: string, what: string) {
		copyText(text).then((ok) =>
			ok ? toasts.show(`${what} copied`) : toasts.show(`The browser refused the clipboard, ${what.toLowerCase()} ${text}`, 'bad')
		);
	}

	const list = $derived([...(view.snapshots ?? [])].reverse());
	const damaged = $derived(new Set(schedule?.damaged ?? []));
	const total = $derived(list.reduce((n, s) => n + s.size, 0));
</script>

<svelte:head><title>Snapshots of {view.name} · nocturnum</title></svelte:head>

<main class="body">
	<div class="grid">
		<section class="panel" aria-labelledby="h-snaps">
			<div class="panel-h">
				<h2 id="h-snaps">Snapshots</h2>
				<span class="dim num">{view.snapshots ? list.length : ''}</span>
				<div class="aside num">{#if list.length}{bytes(total)} kept{/if}</div>
			</div>
			{#if view.snapshots === null}
				<div class="panel-body stack"><span class="skeleton" style="width: 70%"></span></div>
			{:else if list.length === 0}
				<div class="empty">
					<strong>No snapshots yet</strong>
					<p>A snapshot is one tar file of the collection with its checksum, taken while writes carry on.</p>
				</div>
			{:else}
				<div class="scroll">
					<table class="num">
						<thead>
							<tr>
								<th scope="col">Snapshot</th>
								<th scope="col">Taken (UTC)</th>
								<th scope="col" class="r">Size</th>
								<th scope="col">SHA-256</th>
								<th scope="col" class="r"><span class="sr">Actions</span></th>
							</tr>
						</thead>
						<tbody>
							{#each list as s (s.name)}
								<tr>
									<td>
										<span class="snap mono">{s.name}</span>
										{#if damaged.has(s.name)}<span class="flag bad">fails its checksum</span>{/if}
									</td>
									<td title="SHA-256 {s.sha256}"><span class="mono">{utc(s.created_at)}</span> <span class="dim ago">{relative(s.created_at, now)}</span></td>
									<td class="r muted">{bytes(s.size)}</td>
									<td>
										<button class="sha mono" title="Copy the full checksum" onclick={() => copy(s.sha256, 'Checksum')}>{s.sha256.slice(0, 12)}</button>
									</td>
									<td class="r acts">
										<button class="btn sm ghost sq" aria-label="Download {s.name}" title="Download" onclick={() => download(s)}><Icon name="download" size={12} /></button>
										{#if app.canWrite}
											<button class="btn sm ghost sq" aria-label="Restore {s.name}" title="Restore" onclick={() => ((restoring = s), (restoreOpen = true))}><Icon name="restore" size={12} /></button>
											<button class="btn sm ghost sq del" aria-label="Delete {s.name}" title="Delete" onclick={() => ((removing = s), (removeOpen = true))}><Icon name="trash" size={12} /></button>
										{/if}
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
				<div class="foot-note">
					<p>Downloads pass through this browser's memory, so for big snapshots use curl</p>
					<pre>curl -O {location.origin}/v1/collections/{view.name}/snapshots/&lt;name&gt;</pre>
				</div>
			{/if}
		</section>

		<section class="panel" aria-labelledby="h-sched">
			<div class="panel-h">
				<h2 id="h-sched">Schedule</h2>
				<div class="aside">
					{#if schedule}
						{#if schedule.running}<span class="pill idle"><i class="dot"></i>running</span>
						{:else if schedule.last_error}<span class="pill bad"><i class="dot"></i>last run failed</span>
						{:else}<span class="pill ok"><i class="dot"></i>on</span>{/if}
					{:else if schedule === null}
						<span class="pill idle"><i class="dot"></i>off</span>
					{/if}
				</div>
			</div>
			{#if schedule}
				<dl class="kv state num">
					<dt>Next run</dt><dd><span class="mono">{utc(schedule.next_run)}</span> <span class="dim">{relative(schedule.next_run, now)}</span></dd>
					<dt>Last success</dt><dd>{#if schedule.last_success}<span class="mono">{utc(schedule.last_success)}</span> <span class="dim">{relative(schedule.last_success, now)}</span>{:else}<span class="dim">none yet</span>{/if}</dd>
					<dt>Last snapshot</dt><dd class="mono">{schedule.last_snapshot ?? '–'}</dd>
					{#if schedule.last_error}<dt>Last error</dt><dd class="bad" title={schedule.last_error}>{schedule.last_error}</dd>{/if}
					{#if schedule.damaged.length}<dt>Damaged</dt><dd class="bad">{schedule.damaged.length} kept aside, not counted</dd>{/if}
				</dl>
			{/if}
			{#if app.canWrite}
				<form class="panel-body sched-form" onsubmit={save}>
					<fieldset class="stack" disabled={saving || schedule === undefined}>
					<label class="field">
						<span>When, as cron in UTC</span>
						<input class="input mono" bind:value={cron} oninput={() => (dirty = true)} spellcheck="false" aria-invalid={!!scheduleError} />
						<div class="presets">
							{#each presets as p (p.cron)}
								<button type="button" class="btn sm" aria-pressed={cron.trim() === p.cron} onclick={() => ((cron = p.cron), (dirty = true))}>{p.label}</button>
							{/each}
						</div>
					</label>
					<label class="field">
						<span>Keep the newest</span>
						<input class="input num" type="number" min="1" max="1000" bind:value={keep} oninput={() => (dirty = true)} />
						<small>Snapshots that pass their checksum, manual ones included, older ones are deleted after each run</small>
					</label>
					{#if scheduleError}<p class="error-line" role="alert"><Icon name="warn" />{scheduleError}</p>{/if}
					<div class="row">
						<button class="btn primary" disabled={saving || !cron.trim() || !(keep >= 1)}>{saving ? 'Saving' : schedule ? 'Update schedule' : 'Start schedule'}</button>
						{#if schedule}<button type="button" class="btn ghost" onclick={unschedule}>Remove</button>{/if}
					</div>
					</fieldset>
				</form>
			{:else if schedule === null}
				<p class="panel-body dim">No schedule, and this key cannot set one.</p>
			{/if}
		</section>
	</div>
</main>

<Confirm bind:open={restoreOpen} title="Restore {view.name}?" action="Restore snapshot" require={view.name} onconfirm={restore}>
	<p>Every point in {view.name} is replaced by what <code>{restoring?.name}</code> holds, taken {relative(restoring?.created_at)}.</p>
	<p>The snapshot is checked first, and requests wait while the copy is swapped in.</p>
</Confirm>
<Confirm bind:open={removeOpen} title="Delete this snapshot?" action="Delete snapshot" onconfirm={remove}>
	<p><code>{removing?.name}</code> is removed from the server's disk, and this cannot be undone.</p>
</Confirm>

<style>
	.grid {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(320px, 400px);
		gap: 20px;
		align-items: start;
	}
	.scroll {
		overflow-x: auto;
		container-type: inline-size;
	}
	/* The checksum copies from the row's title on narrow panels, so the actions stay in view. */
	@container (max-width: 860px) {
		th:nth-child(4),
		td:nth-child(4) {
			display: none;
		}
	}
	@container (max-width: 640px) {
		.ago {
			display: none;
		}
	}
	.snap {
		color: var(--text);
	}
	.flag.bad {
		margin-left: 8px;
	}
	.sha {
		background: none;
		border: 0;
		padding: 0;
		color: var(--text-3);
		font-size: 12px;
	}
	.sha:hover {
		color: var(--text);
	}
	.acts {
		white-space: nowrap;
	}
	.acts .btn {
		margin-left: 2px;
	}
	.del:hover {
		color: var(--bad) !important;
	}
	.foot-note {
		margin: 0;
		padding: 10px 16px 12px;
		border-top: 1px solid var(--line);
		color: var(--text-3);
		font-size: 12px;
		line-height: 1.6;
	}
	code {
		font-family: var(--mono);
		color: var(--text-2);
	}
	.foot-note p {
		margin: 0 0 6px;
	}
	.foot-note pre {
		margin: 0;
		font-family: var(--mono);
		font-size: 12px;
		color: var(--text-2);
		overflow-x: auto;
		white-space: pre;
	}
	.state {
		padding: 14px 16px 2px;
	}
	.state dd {
		min-width: 0;
	}
	.bad {
		color: var(--bad);
	}
	fieldset {
		margin: 0;
		padding: 0;
		border: 0;
		min-width: 0;
	}
	.sched-form {
		border-top: 1px solid var(--line);
		margin-top: 12px;
	}
	.presets {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
	}
	.presets .btn[aria-pressed='true'] {
		border-color: var(--accent-line);
		color: var(--accent-ink);
	}
	.row {
		display: flex;
		gap: 8px;
	}
	@media (max-width: 1100px) {
		.grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
