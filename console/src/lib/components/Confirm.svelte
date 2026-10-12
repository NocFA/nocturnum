<script lang="ts">
	// A dialog for an action that cannot be undone, which can ask for a word to be typed first.
	import type { Snippet } from 'svelte';

	let {
		open = $bindable(false),
		title,
		action,
		require,
		danger = true,
		onconfirm,
		children
	}: {
		open: boolean;
		title: string;
		action: string;
		require?: string;
		danger?: boolean;
		onconfirm: () => Promise<void> | void;
		children: Snippet;
	} = $props();

	const uid = $props.id();
	let dialog: HTMLDialogElement | undefined = $state();
	let typed = $state('');
	let busy = $state(false);

	$effect(() => {
		if (!dialog) return;
		if (open && !dialog.open) {
			typed = '';
			dialog.showModal();
		} else if (!open && dialog.open) {
			dialog.close();
		}
	});

	async function confirm(e: SubmitEvent) {
		e.preventDefault();
		busy = true;
		try {
			await onconfirm();
			open = false;
		} finally {
			busy = false;
		}
	}
</script>

<dialog bind:this={dialog} onclose={() => (open = false)} aria-labelledby="{uid}-title">
	<form onsubmit={confirm}>
		<h2 id="{uid}-title">{title}</h2>
		<div class="text">{@render children()}</div>
		{#if require}
			<label class="field">
				<span>Type <code>{require}</code> to confirm</span>
				<input class="input mono" bind:value={typed} autocomplete="off" spellcheck="false" />
			</label>
		{/if}
		<div class="row">
			<button type="button" class="btn ghost" onclick={() => (open = false)}>Cancel</button>
			<button
				type="submit"
				class="btn {danger ? 'danger solid' : 'primary'}"
				disabled={busy || (require !== undefined && typed !== require)}
			>
				{busy ? 'Working' : action}
			</button>
		</div>
	</form>
</dialog>

<style>
	dialog {
		width: min(460px, calc(100vw - 32px));
		padding: 0;
		background: var(--surface);
		color: var(--text);
		border: 1px solid var(--line-2);
		border-radius: var(--r-lg);
		box-shadow: 0 12px 28px rgba(0, 0, 0, 0.45);
	}
	dialog::backdrop {
		background: rgba(5, 6, 9, 0.66);
	}
	dialog[open] {
		animation: rise 200ms var(--ease-out);
	}
	@keyframes rise {
		from {
			opacity: 0;
			transform: translateY(6px);
		}
	}
	form {
		padding: 18px 20px 16px;
		display: grid;
		gap: 14px;
	}
	h2 {
		margin: 0;
		font-size: 15px;
		font-weight: 600;
	}
	.text {
		display: grid;
		gap: 8px;
		color: var(--text-2);
		line-height: 1.55;
	}
	.text :global(p) {
		margin: 0;
	}
	code {
		font-family: var(--mono);
		color: var(--text);
	}
	.row {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
		margin-top: 4px;
	}
</style>
