import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

// While developing, the console runs on Vite and sends API calls to a nocturnum server,
// http://127.0.0.1:6333 unless NOCTURNUM_URL says otherwise.
const target = process.env.NOCTURNUM_URL ?? 'http://127.0.0.1:6333';
const proxy = Object.fromEntries(
	['/v1', '/metrics', '/livez', '/readyz'].map((path) => [path, { target, changeOrigin: true }])
);

export default defineConfig({
	plugins: [
		sveltekit({
			compilerOptions: {
				// Force runes mode for the project, except for libraries. Can be removed in svelte 6.
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true
			},
			// The server sends index.html for every console path, so the app routes itself.
			adapter: adapter({ fallback: 'index.html', precompress: false })
		})
	],
	server: { proxy }
});
