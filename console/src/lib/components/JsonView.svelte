<script lang="ts">
	import type { Line } from '#lib/json.ts';

	let { lines, label, numbered = true }: { lines: Line[]; label: string; numbered?: boolean } = $props();
</script>

<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<pre class="code" class:numbered tabindex="0" aria-label={label}>{#each lines as line, i (i)}<span class="ln">{#each line as tok, j (j)}<span class="j-{tok.t}">{tok.v}</span>{/each}</span>{/each}</pre>

<style>
	.code {
		margin: 0;
		padding: 9px 0;
		overflow: auto;
		font-family: var(--mono);
		font-size: 12.5px;
		line-height: 20px;
		color: var(--text-2);
		counter-reset: ln;
		tab-size: 2;
	}
	.code:focus-visible {
		outline-offset: -2px;
	}
	.ln {
		display: block;
		padding: 0 16px;
		white-space: pre;
	}
	.numbered .ln {
		padding-left: 0;
	}
	.numbered .ln::before {
		counter-increment: ln;
		content: counter(ln);
		display: inline-block;
		width: 40px;
		padding-right: 14px;
		text-align: right;
		color: #3c4354;
		user-select: none;
		font-variant-numeric: tabular-nums;
	}
	.j-key {
		color: #9cb4e0;
	}
	.j-str {
		color: #a6d18f;
	}
	.j-num {
		color: #e0b47a;
	}
	.j-bool {
		color: #e58aa6;
	}
	.j-null {
		color: #e58aa6;
		font-style: italic;
	}
	.j-p {
		color: #5d6577;
	}
	.j-note {
		color: #5d6577;
		font-style: italic;
	}
</style>
