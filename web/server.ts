/**
 * Production server: a Hono app on Bun that
 *
 *   1. reverse-proxies `/api/*` (REST + SSE `/api/events`) to the Rust axum
 *      domain API (`AI_WRITE_API`, default http://127.0.0.1:8080), streaming
 *      responses so Server-Sent Events flush in real time; and
 *   2. serves the SvelteKit `adapter-node` build for every other path.
 *
 * ## Integration choice
 * The project keeps `@sveltejs/adapter-node` (so `just web-build` is unchanged and
 * the SvelteKit handler is a plain Node middleware). Bun provides Node-http
 * compatibility, so we run Hono through `@hono/node-server`, whose `HttpBindings`
 * expose the raw Node `incoming`/`outgoing` — we hand those straight to the
 * adapter-node `handler`. This is cleaner than swapping to `svelte-adapter-bun`:
 * no adapter change, one process, Hono owns routing + the streaming `/api` proxy.
 *
 * Run with `bun run server.ts` (script: `bun start`, recipe: `just web-serve`).
 * Requires `just web-build` to have produced `./build` first.
 */
import { Hono } from 'hono';
import { serve, type HttpBindings } from '@hono/node-server';
import { handler } from './build/handler.js';

type Bindings = HttpBindings;

const API_TARGET = (process.env.AI_WRITE_API ?? 'http://127.0.0.1:8080').replace(/\/$/, '');
const PORT = Number(process.env.PORT ?? 3000);
const HOST = process.env.HOST ?? '0.0.0.0';

const app = new Hono<{ Bindings: Bindings }>();

// ---- /api reverse proxy (REST + streaming SSE) ----
app.all('/api/*', async (c) => {
	const url = new URL(c.req.url);
	const target = `${API_TARGET}${url.pathname}${url.search}`;

	// Forward the request verbatim. `accept-encoding: identity` keeps the upstream
	// from compressing — essential so SSE chunks are not buffered by a codec.
	const headers = new Headers(c.req.raw.headers);
	headers.set('accept-encoding', 'identity');
	headers.delete('host');

	const method = c.req.method;
	const hasBody = method !== 'GET' && method !== 'HEAD';

	let upstream: Response;
	try {
		upstream = await fetch(target, {
			method,
			headers,
			body: hasBody ? c.req.raw.body : undefined,
			// allow a streaming request body under the fetch spec
			// @ts-expect-error duplex is valid at runtime on Bun/undici
			duplex: hasBody ? 'half' : undefined,
			redirect: 'manual'
		});
	} catch (e) {
		return c.json(
			{ error: `upstream unreachable: ${e instanceof Error ? e.message : 'error'}` },
			502
		);
	}

	// Re-emit upstream status + headers, passing the body through as a stream so
	// `text/event-stream` responses flush incrementally instead of buffering.
	const respHeaders = new Headers(upstream.headers);
	respHeaders.delete('content-encoding');
	respHeaders.delete('content-length');
	return new Response(upstream.body, {
		status: upstream.status,
		statusText: upstream.statusText,
		headers: respHeaders
	});
});

// ---- everything else → SvelteKit adapter-node handler ----
app.all('*', (c) => {
	const { incoming, outgoing } = c.env;
	return new Promise<Response>((resolve) => {
		outgoing.on('finish', () => resolve(c.body(null)));
		// adapter-node writes the full response onto the raw Node res.
		handler(incoming, outgoing);
	});
});

serve({ fetch: app.fetch, port: PORT, hostname: HOST }, (info) => {
	console.log(`AI-Write web server on http://${HOST}:${info.port} → API ${API_TARGET}`);
});
