<script lang="ts">
	import { page } from '$app/state';
	import { app } from '#lib/state.svelte.ts';
	import Icon from './Icon.svelte';

	let { onmenu, menuButton = $bindable() }: { onmenu: () => void; menuButton?: HTMLButtonElement } = $props();

	let input: HTMLInputElement | undefined = $state();

	const tabs: Record<string, string> = { points: 'Points', query: 'Query', snapshots: 'Snapshots' };
	const crumbs = $derived.by(() => {
		const parts = page.url.pathname.split('/').filter(Boolean).map(decodeURIComponent);
		if (parts[0] === 'collections' && parts[1]) {
			const out = [
				{ label: 'Collections', href: '/' },
				{ label: parts[1], href: `/collections/${encodeURIComponent(parts[1])}` }
			];
			if (parts[2]) out.push({ label: tabs[parts[2]] ?? parts[2], href: '' });
			return out;
		}
		const names: Record<string, string> = { snapshots: 'Snapshots', telemetry: 'Telemetry', keys: 'API keys' };
		return [{ label: names[parts[0]] ?? 'Collections', href: '' }];
	});

	const failing = $derived(app.collections.filter((c) => c.failure).length);
	const health = $derived(
		app.problem || failing || app.notReady ? 'bad' : app.node ? 'ok' : 'idle'
	);
	const healthText = $derived(
		app.problem
			? app.problem.status === 0
				? 'Unreachable'
				: 'Refused'
			: failing
				? `${failing} failing`
				: app.notReady
					? 'Not ready'
					: app.node
						? 'Healthy'
						: 'Connecting'
	);
	const roleText = $derived(
		app.role === 'admin' ? 'admin key' : app.role === 'read-only' ? 'read-only key' : app.role === 'anonymous' ? 'no keys set' : ''
	);

	function onkeydown(e: KeyboardEvent) {
		const t = e.target as HTMLElement;
		const typing = t.closest('input, textarea, select, [contenteditable="true"]');
		if (e.key === '/' && !typing && !e.metaKey && !e.ctrlKey) {
			e.preventDefault();
			input?.focus();
			input?.select();
		}
	}
</script>

<svelte:window {onkeydown} />

<header class="top">
	<button class="icon-btn menu" aria-label="Open navigation" bind:this={menuButton} onclick={onmenu}><Icon name="menu" size={16} /></button>
	<nav class="crumbs" aria-label="Breadcrumb">
		{#each crumbs as c, i (i)}
			{#if i > 0}<Icon name="chevron" size={12} />{/if}
			{#if c.href && i < crumbs.length - 1}
				<a href={c.href}>{c.label}</a>
			{:else}
				<span class="cur" aria-current="page">{c.label}</span>
			{/if}
		{/each}
	</nav>

	<label class="search">
		<Icon name="search" />
		<span class="sr">Search collections and points</span>
		<input
			bind:this={input}
			bind:value={app.search}
			type="search"
			placeholder="Search collections, ids, field:value"
			spellcheck="false"
			autocomplete="off"
			onkeydown={(e) => {
				if (e.key === 'Escape') {
					if (app.search) app.search = '';
					else input?.blur();
				}
			}}
		/>
		<kbd aria-hidden="true">/</kbd>
	</label>

	<div class="right">
		<a class="health" href="/telemetry" title={app.problem?.message ?? app.notReady ?? 'Engine telemetry'}>
			<i class="dot {health}" aria-hidden="true"></i>
			<span>{healthText}</span>
			{#if roleText}
				<span class="sep" aria-hidden="true"></span>
				<span class="dim">{roleText}</span>
			{/if}
		</a>
	</div>
</header>

<style>
	.top {
		height: 48px;
		flex: none;
		display: flex;
		align-items: center;
		gap: 16px;
		padding: 0 20px 0 24px;
		border-bottom: 1px solid var(--line);
		min-width: 0;
	}
	.menu {
		display: none;
		margin-left: -6px;
		width: 30px;
		height: 30px;
	}
	.crumbs {
		display: flex;
		align-items: center;
		gap: 8px;
		color: var(--text-3);
		white-space: nowrap;
		min-width: 0;
	}
	.crumbs a {
		color: var(--text-3);
		text-decoration: none;
	}
	.crumbs a:hover {
		color: var(--text);
	}
	.cur {
		color: var(--text);
		font-weight: 500;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.search {
		margin-left: 24px;
		flex: 0 1 380px;
		position: relative;
		display: flex;
		align-items: center;
		min-width: 120px;
	}
	.search > :global(svg) {
		position: absolute;
		left: 10px;
		color: var(--text-3);
		pointer-events: none;
	}
	.search input {
		width: 100%;
		height: 30px;
		padding: 0 40px 0 32px;
		background: var(--surface);
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		color: var(--text);
	}
	.search input::placeholder {
		color: var(--text-3);
	}
	.search input::-webkit-search-cancel-button {
		display: none;
	}
	.search input:hover {
		border-color: var(--line-3);
	}
	.search input:focus-visible {
		outline: none;
		border-color: var(--accent-line);
		box-shadow: 0 0 0 3px var(--accent-soft);
	}
	.search kbd {
		position: absolute;
		right: 8px;
	}
	.right {
		margin-left: auto;
		display: flex;
		align-items: center;
		gap: 14px;
	}
	.health {
		display: flex;
		align-items: center;
		gap: 8px;
		height: 28px;
		padding: 0 10px;
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		color: var(--text-2);
		text-decoration: none;
		white-space: nowrap;
	}
	.health:hover {
		background: var(--hover);
		color: var(--text);
	}
	.sep {
		width: 1px;
		height: 12px;
		background: var(--line-2);
	}
	@media (max-width: 900px) {
		.menu {
			display: grid;
		}
		.top {
			padding: 0 12px 0 16px;
			gap: 10px;
		}
		.search {
			margin-left: auto;
		}
		.crumbs {
			display: none;
		}
	}
	@media (max-width: 560px) {
		.health .sep,
		.health .dim {
			display: none;
		}
		.search kbd {
			display: none;
		}
		.search input {
			padding-right: 10px;
		}
	}
</style>
