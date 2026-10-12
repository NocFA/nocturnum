<script lang="ts">
	import type { CollectionView } from '#lib/collection.svelte.ts';
	import { bytes, int, percent } from '#lib/format.ts';
	import Icon from './Icon.svelte';

	let { view }: { view: CollectionView } = $props();

	const status = $derived(view.status);
	const ratio = $derived(status?.optimizer.vacuum_ratio ?? null);
	const running = $derived(new Set(status?.optimizer.running ?? []));
	const rows = $derived(
		(status?.segments ?? []).map((s) => {
			const dead = s.rows ? (s.rows - s.live) / s.rows : 0;
			return { ...s, dead, over: ratio !== null && dead > ratio, busy: running.has(s.id) };
		})
	);
	const over = $derived(rows.filter((r) => r.over && !r.busy).length);
	const mem = $derived(status?.memtable);
	const fill = $derived(mem ? Math.min(1, mem.bytes / mem.flush_bytes) : 0);
</script>

<section class="panel" aria-labelledby="h-seg">
	<div class="panel-h">
		<h2 id="h-seg">Segments</h2>
		<span class="dim num">{status ? status.segments.length : ''}</span>
		<div class="aside">
			{#if over}
				<span class="flag">
					<Icon name="warn" size={12} />
					{over === 1 ? '1 segment' : `${over} segments`} over the vacuum threshold
				</span>
			{:else if status?.optimizer.running}
				<span class="flag idle"><Icon name="restore" size={12} />rewriting {status.optimizer.running.join(', ')}</span>
			{/if}
		</div>
	</div>
	{#if status}
		<div class="scroll-x">
			<table class="num">
				<thead>
					<tr>
						<th scope="col">Segment</th>
						<th scope="col">Index</th>
						<th scope="col" class="r">Points</th>
						<th scope="col" class="r">Dead</th>
						<th scope="col" class="r">Vectors</th>
						<th scope="col" class="r">Disk</th>
						<th scope="col">State</th>
					</tr>
				</thead>
				<tbody>
					{#each rows as s (s.id)}
						<tr>
							<td><span class="seg">seg {s.id}</span></td>
							<td class="muted">{s.index === 'hnsw' ? 'HNSW' : 'Plain'}</td>
							<td class="r">{int(s.live)}</td>
							<td class="r" class:warn-txt={s.over} class:muted={!s.over}>{percent(s.dead)}</td>
							<td class="r muted">{bytes(s.vector_bytes)}</td>
							<td class="r muted">{bytes(s.disk_bytes)}</td>
							<td>
								{#if s.busy}
									<span class="state"><i class="dot idle"></i>Rewriting</span>
								{:else if s.over}
									<span class="state warn" title="The optimizer rewrites it without its dead rows"><i class="dot warn"></i>Vacuum due</span>
								{:else}
									<span class="state"><i class="dot"></i>Ready</span>
								{/if}
							</td>
						</tr>
					{/each}
					{#if mem}
						<tr class="memtable">
							<td><span class="seg">memtable</span></td>
							<td class="muted">In memory</td>
							<td class="r">{int(mem.entries)}</td>
							<td class="r dim">–</td>
							<td class="r dim">–</td>
							<td class="r muted" title="Kept safe by the WAL until it is written out">WAL {bytes(status.wal.bytes)}</td>
							<td>
								{#if mem.frozen_entries}
									<span class="state warn" title="A memtable of {int(mem.frozen_entries)} entries is being written out, or waits for a retry after a failed flush"><i class="dot warn"></i>Writing out {int(mem.frozen_entries)}</span>
								{:else}
									<div class="meter" title="Written out as a segment at {bytes(mem.flush_bytes)}">
										<div class="bar" role="progressbar" aria-label="Memtable fill before it is written out" aria-valuenow={Math.round(fill * 100)} aria-valuemin={0} aria-valuemax={100}><i style="width: {fill * 100}%"></i></div>
										<span class="muted small">{percent(fill, 0)} to flush</span>
									</div>
								{/if}
							</td>
						</tr>
					{/if}
				</tbody>
			</table>
		</div>
		{#if status.segments.length === 0}
			<p class="note">Writes wait in the memtable, kept safe by the WAL, until it fills and is written out as the first segment.</p>
		{/if}
	{:else}
		<div class="panel-body stack"><span class="skeleton" style="width: 90%"></span><span class="skeleton" style="width: 70%"></span></div>
	{/if}
</section>

<style>
	.scroll-x {
		overflow-x: auto;
		container-type: inline-size;
	}
	@container (max-width: 640px) {
		th:nth-child(5),
		td:nth-child(5) {
			display: none;
		}
	}
	@container (max-width: 520px) {
		th:nth-child(2),
		td:nth-child(2),
		th:nth-child(6),
		td:nth-child(6) {
			display: none;
		}
		.meter .bar {
			display: none;
		}
	}
	.seg {
		font-family: var(--mono);
		font-size: 12px;
		color: var(--text);
	}
	.memtable .seg {
		color: var(--text-2);
	}
	.memtable td {
		background: rgba(255, 255, 255, 0.012);
	}
	.state {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		color: var(--text-2);
	}
	.state.warn {
		color: var(--warn);
	}
	.warn-txt {
		color: var(--warn);
		font-weight: 500;
	}
	.meter {
		display: flex;
		align-items: center;
		gap: 10px;
		min-width: 150px;
	}
	.meter .bar {
		max-width: 76px;
	}
	.small {
		font-size: 12px;
	}
	.flag.idle {
		color: var(--text-2);
		background: var(--raised);
	}
	.note {
		margin: 0;
		padding: 10px 16px 12px;
		border-top: 1px solid var(--line);
		color: var(--text-3);
		font-size: 12px;
	}
</style>
