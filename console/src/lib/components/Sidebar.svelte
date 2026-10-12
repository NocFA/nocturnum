<script lang="ts">
	import { page } from '$app/state';
	import { app } from '#lib/state.svelte.ts';
	import { compact, duration } from '#lib/format.ts';
	import Icon from './Icon.svelte';

	let { onnavigate }: { onnavigate?: () => void } = $props();

	const current = $derived(page.params.name ?? null);
	const path = $derived(page.url.pathname);
	const needle = $derived(app.search.trim().toLowerCase());
	// The top search narrows the list when it reads as a name rather than a point filter.
	const shown = $derived(
		needle && !/[:<>=]/.test(needle) && !/^\d+$/.test(needle)
			? app.collections.filter((c) => c.name.includes(needle))
			: app.collections
	);

	const engine = [
		{ href: '/snapshots', label: 'Snapshots', icon: 'archive' },
		{ href: '/telemetry', label: 'Telemetry', icon: 'pulse' },
		{ href: '/keys', label: 'API keys', icon: 'key' }
	];
</script>

<aside class="side" aria-label="Collections and engine">
	<a class="brand" href="/" onclick={onnavigate}>
		<svg width="18" height="18" viewBox="0 0 18 18" aria-hidden="true">
			<circle cx="9" cy="9" r="7.25" fill="none" stroke="#e5e7eb" stroke-width="1.5" />
			<path d="M9 1.75a7.25 7.25 0 0 1 0 14.5a4.6 7.25 0 0 0 0-14.5z" fill="#dc244c" />
		</svg>
		<b>nocturnum</b>
		{#if app.node}<span class="ver">{app.node.version}</span>{/if}
	</a>

	<nav class="scroll">
		<div class="side-h">
			<a href="/" class:here={path === '/'} onclick={onnavigate}>Collections</a>
			{#if app.canWrite}
				<a class="icon-btn" href="/?new" aria-label="New collection" title="New collection" onclick={onnavigate}>
					<Icon name="plus" />
				</a>
			{/if}
		</div>
		{#if app.loaded && !app.collections.length && !app.problem}
			<p class="none">No collections yet</p>
		{:else if shown.length === 0 && needle}
			<p class="none">None match <span class="mono">{needle}</span></p>
		{/if}
		<ul class="coll">
			{#each shown as c (c.name)}
				<li>
					<a
						href="/collections/{encodeURIComponent(c.name)}"
						aria-current={current === c.name ? 'page' : undefined}
						title={c.failure ? `${c.name}: ${c.failure}` : c.name}
						onclick={onnavigate}
					>
						<span class="nm">
							<i class="dot" class:bad={!!c.failure} aria-hidden="true"></i>
							<span>{c.name}</span>
						</span>
						<span class="ct num">{compact(c.points)}</span>
					</a>
				</li>
			{/each}
		</ul>

		<div class="side-h"><span>Engine</span></div>
		<ul class="nav">
			{#each engine as e (e.href)}
				<li>
					<a href={e.href} aria-current={path.startsWith(e.href) ? 'page' : undefined} onclick={onnavigate}>
						<Icon name={e.icon} />{e.label}
					</a>
				</li>
			{/each}
		</ul>
	</nav>

	<dl class="foot">
		<div><dt>Node</dt><dd>single node</dd></div>
		<div><dt>Uptime</dt><dd class="num">{app.node ? duration(app.node.uptime_seconds) : '…'}</dd></div>
		<div><dt>REST</dt><dd class="mono">{typeof location !== 'undefined' ? location.host : ''}</dd></div>
	</dl>
</aside>

<style>
	.side {
		border-right: 1px solid var(--line);
		background: var(--surface);
		display: flex;
		flex-direction: column;
		min-height: 0;
		height: 100%;
	}
	.brand {
		height: 48px;
		flex: none;
		display: flex;
		align-items: center;
		gap: 9px;
		padding: 0 16px;
		border-bottom: 1px solid var(--line);
		text-decoration: none;
		color: var(--text);
	}
	.brand b {
		font-weight: 600;
		font-size: 15px;
		letter-spacing: -0.01em;
	}
	.ver {
		margin-left: auto;
		font-family: var(--mono);
		font-size: 11px;
		color: var(--text-3);
	}
	.scroll {
		flex: 1;
		overflow: auto;
		padding: 14px 8px 12px;
	}
	.side-h {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0 8px 6px;
		color: var(--text-3);
		font-size: 12px;
		font-weight: 500;
		min-height: 30px;
	}
	.side-h a:not(.icon-btn) {
		text-decoration: none;
		color: var(--text-3);
	}
	.side-h a:not(.icon-btn):hover,
	.side-h a.here {
		color: var(--text);
	}
	.none {
		margin: 0 0 12px;
		padding: 0 10px;
		color: var(--text-3);
		font-size: 12px;
	}
	.coll,
	.nav {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.coll {
		margin-bottom: 18px;
	}
	.coll a {
		position: relative;
		display: grid;
		grid-template-columns: 1fr auto;
		align-items: center;
		gap: 8px;
		height: 30px;
		padding: 0 8px 0 10px;
		border-radius: var(--r-sm);
		color: var(--text-2);
		text-decoration: none;
	}
	.coll a:hover,
	.nav a:hover {
		background: var(--hover);
		color: var(--text);
	}
	.coll a[aria-current='page'],
	.nav a[aria-current='page'] {
		background: var(--raised);
		color: var(--text);
	}
	.coll a[aria-current='page']::before,
	.nav a[aria-current='page']::before {
		content: '';
		position: absolute;
		left: -8px;
		top: 7px;
		bottom: 7px;
		width: 2px;
		background: var(--accent);
		border-radius: 0 2px 2px 0;
	}
	.nm {
		display: flex;
		align-items: center;
		gap: 8px;
		min-width: 0;
	}
	.nm span {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.ct {
		font-size: 12px;
		color: var(--text-3);
	}
	.nav a {
		position: relative;
		display: flex;
		align-items: center;
		gap: 10px;
		height: 30px;
		padding: 0 10px;
		border-radius: var(--r-sm);
		color: var(--text-2);
		text-decoration: none;
	}
	.nav a :global(svg) {
		color: var(--text-3);
	}
	.nav a[aria-current='page'] :global(svg) {
		color: var(--text-2);
	}
	.foot {
		flex: none;
		margin: 0;
		border-top: 1px solid var(--line);
		padding: 12px 16px;
		font-size: 12px;
		color: var(--text-3);
		display: grid;
		gap: 4px;
	}
	.foot div {
		display: flex;
		justify-content: space-between;
		gap: 12px;
	}
	.foot dd {
		margin: 0;
		color: var(--text-2);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.foot dd.mono {
		font-size: 11px;
	}
</style>
