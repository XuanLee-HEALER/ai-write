import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

// The Rust axum domain API. In dev, Vite proxies /api (REST + SSE) to it so the
// browser only ever talks to the SvelteKit origin (no CORS). In production the
// Hono/Bun server (web/server.ts) performs the same proxy.
const API_TARGET = process.env.AI_WRITE_API ?? 'http://127.0.0.1:8080';

export default defineConfig({
	plugins: [tailwindcss(), sveltekit()],
	server: {
		proxy: {
			'/api': {
				target: API_TARGET,
				changeOrigin: true,
				ws: true,
				// SSE (/api/events) must stream, not buffer.
				configure: (proxy) => {
					proxy.on('proxyReq', (proxyReq) => {
						proxyReq.setHeader('accept-encoding', 'identity');
					});
				}
			}
		}
	}
});
