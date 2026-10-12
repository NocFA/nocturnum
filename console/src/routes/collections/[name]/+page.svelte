<script lang="ts">
	import { current } from '#lib/collection.svelte.ts';
	import ConfigPanel from '#lib/components/ConfigPanel.svelte';
	import SegmentsPanel from '#lib/components/SegmentsPanel.svelte';
	import ResourcesPanel from '#lib/components/ResourcesPanel.svelte';
	import QualityPanel from '#lib/components/QualityPanel.svelte';
	import PointsBrowser from '#lib/components/PointsBrowser.svelte';

	const view = current();
</script>

<main class="body">
	{#if view.error && view.error.status !== 404 && !view.status}
		<p class="error-line" role="alert">{view.error.message}</p>
	{/if}
	<div class="grid">
		<div class="stack left">
			<div class="cfg"><ConfigPanel {view} /></div>
			<div class="seg"><SegmentsPanel {view} /></div>
			<div class="pair">
				<ResourcesPanel {view} />
				<QualityPanel {view} />
			</div>
		</div>
		<div class="stack">
			<PointsBrowser name={view.name} compact />
		</div>
	</div>
</main>

<style>
	.grid {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(360px, 468px);
		gap: 20px;
		align-items: start;
	}
	.pair {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 16px;
		align-items: stretch;
	}
	@media (max-width: 1240px) {
		.grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
	@media (max-width: 700px) {
		.pair {
			grid-template-columns: minmax(0, 1fr);
		}
		/* On a phone the engine's health comes first and the settings last. */
		.left .seg {
			order: -2;
		}
		.left .pair {
			order: -1;
		}
	}
	.cfg,
	.seg {
		min-width: 0;
	}
</style>
