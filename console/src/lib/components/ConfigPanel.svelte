<script lang="ts">
	import { api, type IndexSpec } from '#lib/api.ts';
	import type { CollectionView } from '#lib/collection.svelte.ts';
	import { app, toasts } from '#lib/state.svelte.ts';
	import { bytes, mergeStage, percent } from '#lib/format.ts';
	import Icon from './Icon.svelte';

	let { view }: { view: CollectionView } = $props();

	const info = $derived(view.info);
	const status = $derived(view.status);
	const distances = { cosine: 'Cosine', dot: 'Dot product', euclid: 'Euclidean', manhattan: 'Manhattan' };

	function job(j: NonNullable<NonNullable<typeof status>['optimizer']['last']>) {
		if (j.kind === 'flush') return 'flush';
		if (j.kind === 'vacuum') return `vacuum ${j.inputs?.[0]}`;
		return `merge ${j.inputs?.join(' + ')}`;
	}

	let adding = $state(false);
	let path = $state('');
	let kind = $state<IndexSpec['kind']>('keyword');
	let busy = $state(false);

	async function add(e: SubmitEvent) {
		e.preventDefault();
		if (busy) return;
		const spec = { path: path.trim(), kind };
		busy = true;
		try {
			const r = await api.createIndex(view.name, spec);
			toasts.show(r.created ? `Indexed ${spec.path} as ${spec.kind}` : `${spec.path} was already indexed as ${spec.kind}`);
			path = '';
			adding = false;
			await view.load();
		} catch (err) {
			toasts.error(err);
		} finally {
			busy = false;
		}
	}

	async function drop(spec: IndexSpec) {
		try {
			await api.dropIndex(view.name, spec);
			toasts.show(`Dropped the ${spec.kind} index on ${spec.path}`);
			await view.load();
		} catch (err) {
			toasts.error(err);
		}
	}
</script>

<section class="panel" aria-labelledby="h-config">
	<div class="panel-h">
		<h2 id="h-config">Configuration</h2>
	</div>
	{#if info && status}
		<div class="wrap"><div class="config">
			<section>
				<h3>Vector</h3>
				<dl class="kv num">
					<dt>Dimensions</dt><dd>{info.vectors.dim}</dd>
					<dt>Distance</dt><dd>{distances[info.vectors.distance]}</dd>
					<dt>Datatype</dt><dd class="mono">float32</dd>
					<dt>Storage</dt><dd>mapped</dd>
				</dl>
			</section>
			<section>
				<h3>HNSW index</h3>
				<dl class="kv num">
					<dt class="mono">m</dt><dd>{status.index.m}</dd>
					<dt class="mono">ef_construct</dt><dd>{status.index.ef_construct}</dd>
					<dt class="mono">ef</dt><dd>{status.index.default_ef}</dd>
					<dt title="A segment gets a graph once its vectors take this much">Graph at</dt><dd>{bytes(status.index.min_bytes)}</dd>
				</dl>
			</section>
			<section>
				<h3>Optimizer</h3>
				<dl class="kv num">
					<dt>State</dt>
					<dd>
						{#if !status.optimizer.enabled}<span class="dim">off</span>
						{:else if status.optimizer.running}<span class="busy">rewriting{#if status.optimizer.progress}, {mergeStage(status.optimizer.progress)}{/if}</span>
						{:else}<span class="on">idle</span>{/if}
					</dd>
					<dt title="A segment is rewritten once this share of its rows is dead">Vacuum at</dt><dd>{status.optimizer.vacuum_ratio === null ? '–' : percent(status.optimizer.vacuum_ratio, 0)}</dd>
					<dt title="Above this many segments the smallest neighbours are merged">Max segments</dt><dd>{status.optimizer.max_segments ?? '–'}</dd>
					<dt>Last job</dt>
					<dd class:bad={!!status.optimizer.last_error} title={status.optimizer.last_error ?? ''}>
						{status.optimizer.last_error ? 'failed' : status.optimizer.last ? job(status.optimizer.last) : 'none'}
					</dd>
				</dl>
			</section>
			<section>
				<h3 class="idx-h">
					Payload indexes
					{#if app.canWrite && !adding}
						<button class="icon-btn" aria-label="Add a payload index" title="Add a payload index" onclick={() => (adding = true)}><Icon name="plus" size={12} /></button>
					{/if}
				</h3>
				{#if adding}
					<form class="add" onsubmit={add}>
						<fieldset disabled={busy}>
						<!-- svelte-ignore a11y_autofocus -->
						<input class="input mono" placeholder="tags" bind:value={path} aria-label="Field path" autofocus spellcheck="false" />
						<select class="select-native" bind:value={kind} aria-label="Index kind">
							<option value="keyword">keyword</option>
							<option value="integer">integer</option>
							<option value="float">float</option>
							<option value="bool">bool</option>
						</select>
						<div class="add-row">
							<button type="button" class="btn sm ghost" onclick={() => (adding = false)}>Cancel</button>
							<button class="btn sm primary" disabled={busy || !path.trim()}>{busy ? 'Building' : 'Add index'}</button>
						</div>
						</fieldset>
					</form>
				{:else if info.indexes.length === 0}
					<p class="none">None, filters read payloads directly</p>
				{:else}
					<dl class="kv idx-list">
						{#each info.indexes as spec (spec.path + spec.kind)}
							<dt class="mono">{spec.path}</dt>
							<dd class="idx">
								<span class="tag">{spec.kind}</span>
								{#if app.canWrite}
									<button class="icon-btn x" aria-label="Drop the {spec.kind} index on {spec.path}" onclick={() => drop(spec)}><Icon name="x" size={10} /></button>
								{/if}
							</dd>
						{/each}
					</dl>
				{/if}
			</section>
		</div></div>
	{:else}
		<div class="panel-body stack"><span class="skeleton" style="width: 60%"></span><span class="skeleton" style="width: 80%"></span></div>
	{/if}
</section>

<style>
	.wrap {
		container-type: inline-size;
	}
	.config {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1.08fr) minmax(0, 1.02fr) minmax(0, 0.94fr);
	}
	.config section {
		padding: 14px 14px 16px;
		min-width: 0;
	}
	.config section:first-child {
		padding-left: 16px;
	}
	.config section + section {
		border-left: 1px solid var(--line);
	}
	h3 {
		margin: 0 0 10px;
		font-size: 12px;
		font-weight: 500;
		color: var(--text-3);
		min-height: 18px;
	}
	.idx-h {
		display: flex;
		align-items: center;
		justify-content: space-between;
		margin-top: -3px;
		margin-bottom: 7px;
	}
	.on {
		color: var(--ok);
	}
	.busy {
		color: var(--warn);
	}
	.bad {
		color: var(--bad);
	}
	.none {
		margin: 0;
		color: var(--text-3);
		font-size: 12px;
		line-height: 1.5;
	}
	.kv {
		grid-template-columns: minmax(0, 1fr) max-content;
		column-gap: 8px;
	}
	.kv :global(dt) {
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.kv.idx-list {
		grid-template-columns: minmax(0, max-content) minmax(0, 1fr);
	}
	.kv dd.idx {
		display: flex;
		justify-content: flex-end;
		align-items: center;
		gap: 4px;
	}
	.x {
		width: 18px;
		height: 18px;
		margin-right: -4px;
	}
	.add fieldset {
		margin: 0;
		padding: 0;
		border: 0;
		min-width: 0;
		display: grid;
		gap: 6px;
	}
	.add .input,
	.add .select-native {
		height: 26px;
	}
	.add-row {
		display: flex;
		justify-content: flex-end;
		gap: 4px;
	}
	@container (max-width: 639px) {
		.config {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.config section:nth-child(3) {
			border-left: 0;
		}
		.config section:nth-child(n + 3) {
			border-top: 1px solid var(--line);
		}
	}
	@container (max-width: 380px) {
		.config {
			grid-template-columns: minmax(0, 1fr);
		}
		.config section + section {
			border-left: 0;
			border-top: 1px solid var(--line);
		}
	}
</style>
