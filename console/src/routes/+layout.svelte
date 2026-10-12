<script lang="ts">
	import '../app.css';
	import { onMount } from 'svelte';
	import { afterNavigate } from '$app/navigation';
	import { app } from '#lib/state.svelte.ts';
	import Sidebar from '#lib/components/Sidebar.svelte';
	import Topbar from '#lib/components/Topbar.svelte';
	import Toasts from '#lib/components/Toasts.svelte';
	import KeyGate from '#lib/components/KeyGate.svelte';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();
	let drawer = $state(false);
	let menuButton: HTMLButtonElement | undefined = $state();
	// Below 900 px the sidebar is a drawer, and while it is closed it is inert, so Tab never
	// lands on links that are off screen.
	let narrow = $state(false);
	onMount(() => {
		const q = window.matchMedia('(max-width: 900px)');
		narrow = q.matches;
		const change = () => (narrow = q.matches);
		q.addEventListener('change', change);
		return () => q.removeEventListener('change', change);
	});

	function closeDrawer() {
		if (!drawer) return;
		drawer = false;
		menuButton?.focus();
	}

	onMount(() => {
		app.refresh();
		// The sidebar counts and health stay current while the tab is open.
		const timer = setInterval(() => {
			if (document.visibilityState === 'visible') app.poll();
		}, 5000);
		return () => clearInterval(timer);
	});
	afterNavigate(() => (drawer = false));

	const locked = $derived(app.problem && (app.problem.status === 401 || app.problem.status === 403));
</script>

<div class="app" class:drawer>
	<div class="side-wrap" inert={narrow && !drawer}>
		<Sidebar onnavigate={() => (drawer = false)} />
	</div>
	{#if drawer}
		<button class="scrim" aria-label="Close navigation" onclick={closeDrawer}></button>
	{/if}
	<div class="main">
		<Topbar bind:menuButton onmenu={() => (drawer ? closeDrawer() : (drawer = true))} />
		{#if locked}
			<KeyGate />
		{:else if app.problem && app.problem.status === 0 && !app.node}
			<div class="body">
				<div class="empty" role="alert">
					<strong>The server did not answer</strong>
					<p>It may be down or restarting, the console keeps trying every few seconds.</p>
				</div>
			</div>
		{:else}
			{@render children()}
		{/if}
	</div>
</div>
<Toasts />

<svelte:window
	onkeydown={(e) => {
		if (e.key === 'Escape' && drawer) closeDrawer();
	}}
/>

<style>
	.app {
		display: grid;
		grid-template-columns: 232px minmax(0, 1fr);
		height: 100vh;
		height: 100dvh;
	}
	.side-wrap {
		min-height: 0;
	}
	.main {
		display: flex;
		flex-direction: column;
		min-width: 0;
		min-height: 0;
	}
	.scrim {
		display: none;
	}
	@media (max-width: 900px) {
		.app {
			grid-template-columns: minmax(0, 1fr);
		}
		.side-wrap {
			position: fixed;
			z-index: 30;
			inset: 0 auto 0 0;
			width: 264px;
			transform: translateX(-100%);
			transition: transform 220ms var(--ease-out);
			box-shadow: 0 0 0 1px var(--line);
		}
		.drawer .side-wrap {
			transform: none;
			box-shadow: 12px 0 32px rgba(0, 0, 0, 0.45);
		}
		.scrim {
			display: block;
			position: fixed;
			z-index: 20;
			inset: 0;
			border: 0;
			background: rgba(5, 6, 9, 0.6);
		}
	}
</style>
