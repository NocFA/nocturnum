<script lang="ts">
	import { current } from '#lib/collection.svelte.ts';
	import PointsBrowser from '#lib/components/PointsBrowser.svelte';

	const view = current();
</script>

<main class="body">
	<PointsBrowser name={view.name} />
	<p class="help">
		The top search filters this table: an id such as <code>41207</code>, <code>field:value</code>, <code>price&gt;10</code>
		or plain words found in the payloads on this page.
	</p>
</main>

<style>
	.help {
		margin: 14px 2px 0;
		color: var(--text-3);
		font-size: 12px;
		line-height: 1.6;
	}
	code {
		font-family: var(--mono);
		color: var(--text-2);
		background: var(--raised);
		border: 1px solid var(--line);
		border-radius: 3px;
		padding: 0 4px;
	}
</style>
