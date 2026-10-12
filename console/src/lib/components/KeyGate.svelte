<script lang="ts">
	import { app } from '#lib/state.svelte.ts';

	let key = $state('');
	let busy = $state(false);

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		busy = true;
		await app.setKey(key.trim() || null);
		busy = false;
	}

	const refused = $derived(app.problem?.status === 403);
</script>

<div class="body">
	<form class="gate panel" onsubmit={submit}>
		<div class="panel-h"><h2>{refused ? 'This browser is refused' : 'An API key is needed'}</h2></div>
		<div class="panel-body stack">
			{#if refused}
				<p class="muted">
					The server answered <code class="mono">{app.problem?.message}</code>
				</p>
				<p class="dim">
					With no keys set the server only serves the host names it knows, so add this one to
					<code class="mono">server.allowed_hosts</code> in the config, or set a key and enter it here.
				</p>
			{:else}
				<p class="muted">
					This server has keys set, so the console needs one to read anything. The key stays in this
					browser's storage and goes to the server in the <code class="mono">Authorization</code> header.
				</p>
				{#if app.hasKey}
					<p class="error-line">The key stored in this browser was not accepted.</p>
				{/if}
			{/if}
			<label class="field">
				<span>Key</span>
				<!-- svelte-ignore a11y_autofocus -->
				<input class="input mono" type="password" autocomplete="current-password" bind:value={key} autofocus required={!refused} />
				<small>The admin key can do everything, the read-only key can browse and search.</small>
			</label>
			<div class="row">
				<button class="btn primary" type="submit" disabled={busy || !key.trim()}>Use this key</button>
				{#if app.hasKey}
					<button class="btn ghost" type="button" onclick={() => app.setKey(null)}>Forget the stored key</button>
				{/if}
			</div>
		</div>
	</form>
</div>

<style>
	.gate {
		max-width: 520px;
		margin: 6vh auto 0;
	}
	p {
		margin: 0;
		text-wrap: pretty;
	}
	.row {
		display: flex;
		gap: 8px;
	}
	code {
		color: var(--text);
	}
</style>
