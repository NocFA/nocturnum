<script lang="ts">
	import type { CollectionView } from '#lib/collection.svelte.ts';
	import { app } from '#lib/state.svelte.ts';
	import { bytes, bytesParts, percent } from '#lib/format.ts';

	let { view }: { view: CollectionView } = $props();

	const status = $derived(view.status);
	const graph = $derived(status?.segments.reduce((n, s) => n + s.graph_bytes, 0) ?? 0);
	const mapped = $derived(status?.segments.reduce((n, s) => n + s.vector_bytes, 0) ?? 0);
	const memtable = $derived(status?.memtable.bytes ?? 0);
	const heap = $derived(graph + memtable);
	const segDisk = $derived(status?.segments.reduce((n, s) => n + s.disk_bytes, 0) ?? 0);
	const wal = $derived(status?.wal.bytes ?? 0);
	const disk = $derived(app.node?.disk.collections ?? null);
	const share = (part: number, whole: number) => (whole > 0 ? (part / whole) * 100 : 0);
</script>

<section class="panel" aria-labelledby="h-res">
	<div class="panel-h"><h2 id="h-res">Resources</h2><div class="aside">this collection</div></div>
	{#if status}
		<div class="res num">
			<div>
				<div class="lbl">
					<span>Memory</span>
					<span><span class="big">{bytesParts(heap)[0]}</span> <span class="of">{bytesParts(heap)[1]} held</span></span>
				</div>
				<div class="stack-bar" role="img" aria-label="Memory: graphs {bytes(graph)}, memtable {bytes(memtable)}">
					<i style="width: {share(graph, heap)}%; background: #8fa3c7"></i>
					<i style="width: {share(memtable, heap)}%; background: #5d6e8c"></i>
				</div>
				<div class="legend">
					<span><i style="background: #8fa3c7"></i>Graphs {bytes(graph)}</span>
					<span><i style="background: #5d6e8c"></i>Memtable {bytes(memtable)}</span>
					<span class="mapped" title="Vectors are mapped from disk and cached by the system as memory allows">+ {bytes(mapped)} mapped</span>
				</div>
			</div>
			<div>
				<div class="lbl">
					<span>Disk</span>
					<span>
						<span class="big">{bytesParts(segDisk + wal)[0]}</span>
						<span class="of">{bytesParts(segDisk + wal)[1]}{#if disk}&nbsp;of {bytes(disk.total_bytes)}{/if}</span>
					</span>
				</div>
				{#if disk}
					<div class="bar lg" role="progressbar" aria-label="Share of the disk this collection takes" aria-valuenow={Math.round(share(segDisk + wal, disk.total_bytes))} aria-valuemin={0} aria-valuemax={100}>
						<i style="width: {Math.max(0.6, share(segDisk + wal, disk.total_bytes))}%"></i>
					</div>
					<div class="legend">
						<span>Segments {bytes(segDisk)}</span>
						<span>WAL {bytes(wal)}</span>
						<span class="free">{percent(disk.available_bytes / disk.total_bytes, 0)} of the disk free</span>
					</div>
				{:else}
					<div class="legend"><span>Segments {bytes(segDisk)}</span><span>WAL {bytes(wal)}</span></div>
				{/if}
			</div>
		</div>
	{:else}
		<div class="panel-body stack"><span class="skeleton" style="width: 80%"></span><span class="skeleton" style="width: 50%"></span></div>
	{/if}
</section>

<style>
	.res {
		padding: 14px 16px 16px;
		display: grid;
		gap: 16px;
	}
	.mapped {
		cursor: help;
		text-decoration: underline dotted var(--line-3);
		text-underline-offset: 3px;
	}
	.lbl {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		margin-bottom: 8px;
		gap: 8px;
	}
	.lbl > span:first-child {
		color: var(--text-2);
	}
	.big {
		font-size: 15px;
		font-weight: 500;
		letter-spacing: -0.01em;
	}
	.of {
		color: var(--text-3);
		font-size: 12px;
	}
	.stack-bar {
		display: flex;
		height: 6px;
		border-radius: 3px;
		overflow: hidden;
		background: var(--track);
		gap: 2px;
	}
	.stack-bar i {
		display: block;
		height: 100%;
	}
	.legend {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
		margin-top: 8px;
		font-size: 12px;
		color: var(--text-3);
	}
	.legend span {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.legend i {
		width: 8px;
		height: 8px;
		border-radius: 2px;
		display: inline-block;
	}
	.free {
		margin-left: auto;
	}
</style>
