<script lang="ts">
	import { app, toasts } from '#lib/state.svelte.ts';
	import { storedKey } from '#lib/api.ts';
	import Icon from '#lib/components/Icon.svelte';

	let key = $state('');
	let busy = $state(false);

	async function use(e: SubmitEvent) {
		e.preventDefault();
		busy = true;
		await app.setKey(key.trim());
		busy = false;
		key = '';
		if (!app.problem) toasts.show(`Key accepted, this browser is now ${app.role === 'admin' ? 'admin' : 'read-only'}`);
	}

	async function forget() {
		await app.setKey(null);
		toasts.show('The key is gone from this browser');
	}

	const stored = $derived(app.hasKey ? storedKey() : null);
	const masked = $derived(stored ? `${stored.slice(0, 4)}${'•'.repeat(Math.max(4, Math.min(16, stored.length - 8)))}${stored.slice(-4)}` : null);
	const role = $derived(app.role);
</script>

<svelte:head><title>API keys · nocturnum</title></svelte:head>

<div class="page-head flat">
	<div class="head-row">
		<div>
			<div class="title"><h1>API keys</h1></div>
			<div class="meta"><span>Keys are set in the server's config, this page picks which one this browser uses</span></div>
		</div>
	</div>
</div>

<main class="body">
	<div class="grid">
		<section class="panel" aria-labelledby="h-this">
			<div class="panel-h"><h2 id="h-this">This browser</h2></div>
			<div class="panel-body stack">
				<dl class="kv">
					<dt>Access</dt>
					<dd>
						{#if role === 'admin'}<span class="pill ok"><i class="dot"></i>admin</span>
						{:else if role === 'read-only'}<span class="pill idle"><i class="dot"></i>read-only</span>
						{:else if role === 'anonymous'}<span class="pill warn"><i class="dot"></i>no keys on the server</span>
						{:else}<span class="dim">unknown</span>{/if}
					</dd>
					<dt>Stored key</dt>
					<dd class="mono">{masked ?? 'none'}</dd>
				</dl>
				<form class="stack" onsubmit={use}>
					<label class="field">
						<span>{stored ? 'Switch to another key' : 'Use a key'}</span>
						<input class="input mono" type="password" autocomplete="off" bind:value={key} placeholder="paste the admin or read-only key" />
						<small>It stays in this browser's local storage and goes to the server in the Authorization header, never in a URL.</small>
					</label>
					<div class="row">
						<button class="btn primary" disabled={busy || !key.trim()}>Use this key</button>
						{#if stored}<button type="button" class="btn ghost" onclick={forget}><Icon name="logout" />Forget the stored key</button>{/if}
					</div>
				</form>
			</div>
		</section>

		<section class="panel" aria-labelledby="h-server">
			<div class="panel-h"><h2 id="h-server">On the server</h2></div>
			<div class="panel-body stack">
				<dl class="kv">
					<dt>Admin key</dt><dd>{app.node ? (app.node.keys.admin ? 'set' : 'not set') : '…'}</dd>
					<dt>Read-only key</dt><dd>{app.node ? (app.node.keys.read_only ? 'set' : 'not set') : '…'}</dd>
				</dl>
				<p class="muted">
					The admin key can do everything, and the read-only key can browse, search, check recall and download
					snapshots. Set them in the <code>[auth]</code> section of the config and restart:
				</p>
				<pre class="snippet">[auth]
admin_key = "…"       # at least 16 visible characters
read_only_key = "…"   # needs admin_key, and must differ from it</pre>
				{#if role === 'anonymous'}
					<p class="warn-line">
						<Icon name="warn" />With no keys the server listens on loopback unless <code>auth.allow_anonymous</code> is
						set, and answers only requests whose host is an IP address, localhost or a name in
						<code>server.allowed_hosts</code>, so set a key before opening it to the tailnet.
					</p>
				{/if}
			</div>
		</section>
	</div>
</main>

<style>
	.grid {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 20px;
		align-items: start;
		max-width: 1100px;
	}
	.kv dd {
		display: flex;
		justify-content: flex-end;
	}
	p {
		margin: 0;
		line-height: 1.55;
		text-wrap: pretty;
	}
	code {
		font-family: var(--mono);
		color: var(--text);
	}
	.row {
		display: flex;
		gap: 8px;
	}
	.snippet {
		margin: 0;
		padding: 12px 14px;
		background: var(--ground);
		border: 1px solid var(--line);
		border-radius: var(--r);
		font-family: var(--mono);
		font-size: 12px;
		color: var(--text-2);
		overflow-x: auto;
	}
	.warn-line {
		display: flex;
		gap: 8px;
		color: var(--warn);
		font-size: 12px;
	}
	.warn-line :global(svg) {
		flex: none;
		margin-top: 3px;
	}
	@media (max-width: 900px) {
		.grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
