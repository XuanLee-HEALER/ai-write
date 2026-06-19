import type { HandleFetch } from '@sveltejs/kit';

/**
 * During SSR, a `load`'s relative `fetch('/api/...')` is resolved by SvelteKit
 * itself against the request origin — it does NOT re-enter the Hono/Vite `/api`
 * reverse proxy (that only sees real network requests from the browser). So on
 * the server we rewrite `/api/*` to the absolute axum origin (`AI_WRITE_API`,
 * default http://127.0.0.1:8080), giving SSR loads a direct line to the backend.
 * Browser-side fetches are unaffected and keep using the proxied relative path.
 */
const API_TARGET = (process.env.AI_WRITE_API ?? 'http://127.0.0.1:8080').replace(/\/$/, '');

export const handleFetch: HandleFetch = async ({ request, fetch }) => {
	const url = new URL(request.url);
	if (url.pathname.startsWith('/api/')) {
		const target = `${API_TARGET}${url.pathname}${url.search}`;
		return fetch(new Request(target, request));
	}
	return fetch(request);
};
