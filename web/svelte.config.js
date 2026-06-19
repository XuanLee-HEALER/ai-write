import adapter from '@sveltejs/adapter-node';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
const config = {
	preprocess: vitePreprocess(),

	kit: {
		// adapter-node produces ./build/handler.js, which the Hono/Bun production
		// server (web/server.ts) mounts; in dev Vite serves the app directly.
		adapter: adapter(),
		alias: {
			// shadcn-svelte / project convention: import components via $lib/components.
			'$lib/components': 'src/lib/components',
			'$lib/utils': 'src/lib/utils'
		}
	},

	compilerOptions: {
		// Force runes mode project-wide (libraries under node_modules opt out).
		runes: true
	}
};

export default config;
