<script lang="ts">
	import { fly } from 'svelte/transition';
	import { toasts } from '#lib/state.svelte.ts';
	import Icon from './Icon.svelte';
</script>

<div class="toasts" role="status" aria-live="polite">
	{#each toasts.list as t (t.id)}
		<div class="toast {t.tone}" transition:fly={{ y: 8, duration: 180 }}>
			<Icon name={t.tone === 'ok' ? 'check' : 'warn'} />
			<span>{t.text}</span>
			<button class="icon-btn" aria-label="Dismiss" onclick={() => toasts.dismiss(t.id)}><Icon name="x" size={12} /></button>
		</div>
	{/each}
</div>

<style>
	.toasts {
		position: fixed;
		right: 20px;
		bottom: 20px;
		z-index: 50;
		display: grid;
		gap: 8px;
		justify-items: end;
		pointer-events: none;
	}
	.toast {
		pointer-events: auto;
		display: flex;
		align-items: flex-start;
		gap: 10px;
		max-width: min(440px, calc(100vw - 40px));
		padding: 9px 8px 9px 12px;
		background: var(--raised);
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4);
		line-height: 1.45;
	}
	.toast > :global(svg) {
		margin-top: 2px;
		flex: none;
	}
	.toast.ok > :global(svg) {
		color: var(--ok);
	}
	.toast.bad > :global(svg) {
		color: var(--bad);
	}
	.toast span {
		flex: 1;
		text-wrap: pretty;
	}
</style>
