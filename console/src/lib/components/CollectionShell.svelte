<script lang="ts">
	import type { Snippet } from 'svelte';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { api } from '#lib/api.ts';
	import { CollectionView, provide } from '#lib/collection.svelte.ts';
	import { app, toasts } from '#lib/state.svelte.ts';
	import { bytes, int, plural } from '#lib/format.ts';
	import Icon from './Icon.svelte';
	import Confirm from './Confirm.svelte';

	let { name, children }: { name: string; children: Snippet } = $props();

	// svelte-ignore state_referenced_locally
	const view = new CollectionView(name);
	provide(view);

	onMount(() => {
		view.load();
		view.loadSnapshots();
		// Segments, the memtable and the optimizer change by themselves, so the status is
		// polled while the tab is visible.
		const timer = setInterval(() => {
			if (document.visibilityState === 'visible') view.poll();
		}, 3000);
		return () => clearInterval(timer);
	});

	const base = $derived(`/collections/${encodeURIComponent(name)}`);
	const tabs = $derived([
		{ href: base, label: 'Overview', count: null },
		{ href: `${base}/points`, label: 'Points', count: view.info ? int(view.info.points) : null },
		{ href: `${base}/query`, label: 'Query', count: null },
		{ href: `${base}/snapshots`, label: 'Snapshots', count: view.snapshots ? String(view.snapshots.length) : null }
	]);
	const here = $derived(page.url.pathname.replace(/\/$/, ''));

	let snapping = $state(false);
	async function snapshot() {
		snapping = true;
		try {
			const s = await api.takeSnapshot(name);
			toasts.show(`Snapshot ${s.name} taken, ${bytes(s.size)}`);
			await view.loadSnapshots();
		} catch (e) {
			toasts.error(e);
		} finally {
			snapping = false;
		}
	}

	let menu = $state(false);
	let deleting = $state(false);
	async function drop() {
		try {
			await api.drop(name);
			toasts.show(`Deleted ${name}, its snapshots are kept`);
			await app.changed();
			goto('/');
		} catch (e) {
			toasts.error(e);
		}
	}

	const failure = $derived(view.status?.failure ?? null);
	const optimizing = $derived(!!view.status?.optimizer.running);
	const missing = $derived(view.error?.status === 404);
</script>

<svelte:head><title>{name} · nocturnum</title></svelte:head>

<svelte:window
	onclick={(e) => {
		if (menu && !(e.target as HTMLElement).closest('.more')) menu = false;
	}}
	onkeydown={(e) => {
		if (e.key === 'Escape') menu = false;
	}}
/>

{#if missing && !here.endsWith('/snapshots')}
	<div class="body">
		<div class="empty">
			<strong>No collection named {name}</strong>
			<p>If it was deleted its snapshots stay on the server, and restoring one brings the collection back.</p>
			<a class="btn" href="{base}/snapshots">Its snapshots</a>
		</div>
	</div>
{:else if missing}
	<div class="page-head flat">
		<div class="title"><h1>{name}</h1><span class="pill idle"><i class="dot"></i>deleted</span></div>
		<div class="meta"><span>No collection by this name, its snapshots are below and restoring one brings it back</span></div>
	</div>
	{@render children()}
{:else}
	<div class="page-head">
		<div class="head-row">
			<div class="who">
				<div class="title">
					<h1>{name}</h1>
					{#if failure}
						<span class="pill bad" title={failure}><i class="dot"></i>needs restart</span>
					{:else if optimizing}
						<span class="pill idle"><i class="dot"></i>optimizing</span>
					{:else if view.status}
						<span class="pill ok"><i class="dot"></i>ok</span>
					{/if}
				</div>
				<div class="meta num">
					{#if view.info && view.status}
						<span>{plural(view.info.points, 'point')}</span>
						<span><code>{view.info.vectors.dim}</code> dims, {view.info.vectors.distance}, float32</span>
						<span>{plural(view.status.segments.length, 'segment')}</span>
						<span>{bytes(view.status.disk_bytes)} on disk</span>
					{:else}
						<span class="skeleton" style="width: 280px; margin-top: 5px"></span>
					{/if}
				</div>
			</div>
			{#if app.canWrite}
				<div class="actions">
					<button class="btn primary" class:busy={snapping} disabled={snapping || !!failure} onclick={snapshot}>
						<Icon name="camera" />{snapping ? 'Taking snapshot' : 'Create snapshot'}
					</button>
					<div class="more">
						<button class="btn sq" aria-label="More actions" aria-haspopup="menu" aria-expanded={menu} onclick={() => (menu = !menu)}>
							<Icon name="more" />
						</button>
						{#if menu}
							<div class="menu" role="menu">
								<button role="menuitem" class="danger" onclick={() => ((menu = false), (deleting = true))}>
									<Icon name="trash" />Delete collection
								</button>
							</div>
						{/if}
					</div>
				</div>
			{/if}
		</div>
		{#if failure}
			<p class="failure" role="alert">
				<Icon name="warn" />This collection stopped taking changes: {failure}. Reads still work, restart the server to recover.
			</p>
		{/if}
		<nav class="tabs" aria-label="Collection views">
			{#each tabs as t (t.href)}
				<a class="tab" href={t.href} aria-current={here === t.href ? 'page' : undefined}>
					{t.label}
					{#if t.count !== null}<span class="count num">{t.count}</span>{/if}
				</a>
			{/each}
		</nav>
	</div>

	{@render children()}
{/if}

<Confirm bind:open={deleting} title="Delete {name}?" action="Delete collection" require={name} onconfirm={drop}>
	<p>
		Every point in it goes, and this cannot be undone. Its snapshots stay on the server, so restoring one brings
		the collection back.
	</p>
</Confirm>

<style>
	.who {
		min-width: 0;
	}
	.failure {
		margin: 12px 0 0;
		display: flex;
		gap: 8px;
		align-items: center;
		color: var(--bad);
		font-size: 12px;
	}
	.tabs {
		display: flex;
		gap: 4px;
		margin-top: 14px;
		overflow-x: auto;
		scrollbar-width: none;
	}
	.tab {
		position: relative;
		height: 38px;
		padding: 0 10px;
		color: var(--text-3);
		font-weight: 500;
		display: inline-flex;
		align-items: center;
		gap: 7px;
		text-decoration: none;
		white-space: nowrap;
	}
	.tab:hover {
		color: var(--text);
	}
	.tab[aria-current='page'] {
		color: var(--text);
	}
	.tab[aria-current='page']::after {
		content: '';
		position: absolute;
		left: 10px;
		right: 10px;
		bottom: 0;
		height: 2px;
		background: var(--accent);
		border-radius: 2px 2px 0 0;
	}
	.tab:focus-visible {
		outline-offset: -2px;
	}
	.count {
		font-size: 11px;
		color: var(--text-3);
		background: var(--raised);
		border-radius: 4px;
		padding: 0 5px;
		line-height: 17px;
	}
	.more {
		position: relative;
	}
	.menu {
		position: absolute;
		right: 0;
		top: 36px;
		z-index: 10;
		min-width: 190px;
		padding: 4px;
		background: var(--raised);
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		box-shadow: 0 12px 32px rgba(0, 0, 0, 0.45);
	}
	.menu button {
		width: 100%;
		display: flex;
		align-items: center;
		gap: 9px;
		height: 30px;
		padding: 0 10px;
		background: none;
		border: 0;
		border-radius: var(--r-sm);
		color: var(--text-2);
		text-align: left;
	}
	.menu button:hover,
	.menu button:focus-visible {
		background: var(--hover);
		color: var(--text);
	}
	.menu button.danger {
		color: var(--bad);
	}
	.menu button.danger:hover {
		background: var(--bad-soft);
	}
</style>
