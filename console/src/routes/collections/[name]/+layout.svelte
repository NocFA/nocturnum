<script lang="ts">
	import { page } from '$app/state';
	import CollectionShell from '#lib/components/CollectionShell.svelte';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();
</script>

{#key page.params.name}
	<CollectionShell name={page.params.name ?? ''}>
		{@render children()}
	</CollectionShell>
{/key}
