<script lang="ts" module>
	// One authored set, drawn on a 14 px grid with a 1.4 px stroke, round caps.
	const paths: Record<string, string> = {
		plus: '<path d="M7 2.5v9M2.5 7h9"/>',
		search: '<circle cx="6" cy="6" r="4.25"/><path d="M9.2 9.2L12 12"/>',
		chevron: '<path d="M5.5 3L9.5 7l-4 4"/>',
		down: '<path d="M3.5 5.5L7 9l3.5-3.5"/>',
		left: '<path d="M8.5 3L4.5 7l4 4"/>',
		camera: '<path d="M2 4.5h10v7H2z"/><path d="M4.5 4.5l1-2h3l1 2"/><circle cx="7" cy="8" r="1.6"/>',
		archive: '<path d="M2 4.5h10v7H2z"/><path d="M4.5 4.5l1-2h3l1 2"/>',
		pulse: '<path d="M1.5 7.5h2.5l1.5-4 3 7 1.5-3h2.5"/>',
		key: '<circle cx="5" cy="7" r="2.5"/><path d="M7.5 7H12M10.5 7v2"/>',
		layers: '<path d="M7 2l5 2.5-5 2.5-5-2.5z"/><path d="M2 7.2L7 9.7l5-2.5"/><path d="M2 9.7l5 2.5 5-2.5"/>',
		more: '<circle cx="3" cy="7" r=".6"/><circle cx="7" cy="7" r=".6"/><circle cx="11" cy="7" r=".6"/>',
		copy: '<rect x="4.5" y="4.5" width="7" height="7" rx="1.2"/><path d="M9.5 4.5V3.2c0-.7-.5-1.2-1.2-1.2H3.2C2.5 2 2 2.5 2 3.2v5.1c0 .7.5 1.2 1.2 1.2h1.3"/>',
		check: '<path d="M3 7.3l2.6 2.6L11 4.5"/>',
		warn: '<path d="M7 2l5.5 9.5h-11z"/><path d="M7 5.8v2.4"/><circle cx="7" cy="10" r=".3"/>',
		trash: '<path d="M2.5 4h9M5.5 4V2.5h3V4M3.8 4l.6 7.5h5.2l.6-7.5"/>',
		edit: '<path d="M9.5 2.5l2 2-6.5 6.5H3V9z"/>',
		download: '<path d="M7 2v7M4 6.5L7 9.5l3-3M2.5 12h9"/>',
		restore: '<path d="M2.5 7a4.5 4.5 0 1 0 1.4-3.3"/><path d="M2.5 2.2v2.6h2.6"/>',
		play: '<path d="M4 2.8v8.4L11 7z"/>',
		x: '<path d="M3.5 3.5l7 7M10.5 3.5l-7 7"/>',
		clock: '<circle cx="7" cy="7" r="5"/><path d="M7 4.2V7l2 1.4"/>',
		menu: '<path d="M2.5 4h9M2.5 7h9M2.5 10h9"/>',
		external: '<path d="M6 3H3v8h8V8"/><path d="M8 2.5h3.5V6M11.5 2.5L6.5 7.5"/>',
		filter: '<path d="M2 3h10L8 7.5V11l-2 1V7.5z"/>',
		db: '<ellipse cx="7" cy="3.5" rx="4.5" ry="1.6"/><path d="M2.5 3.5v7c0 .9 2 1.6 4.5 1.6s4.5-.7 4.5-1.6v-7"/><path d="M2.5 7c0 .9 2 1.6 4.5 1.6s4.5-.7 4.5-1.6"/>',
		logout: '<path d="M5.5 2.5h-3v9h3M8 4.5L10.5 7 8 9.5M10.5 7H5"/>'
	};
</script>

<script lang="ts">
	let {
		name,
		size = 14,
		label
	}: { name: keyof typeof paths | string; size?: number; label?: string } = $props();
</script>

<svg
	width={size}
	height={size}
	viewBox="0 0 14 14"
	fill="none"
	stroke="currentColor"
	stroke-width="1.4"
	stroke-linecap="round"
	stroke-linejoin="round"
	role={label ? 'img' : undefined}
	aria-label={label}
	aria-hidden={label ? undefined : 'true'}
>
	{@html paths[name] ?? ''}
</svg>
