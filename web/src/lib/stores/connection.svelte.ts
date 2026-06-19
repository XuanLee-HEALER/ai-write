/**
 * SSE event feed + connection state for `GET /api/events`.
 *
 * A single browser-side `EventSource` subscribes to the domain event stream and
 * fans events into a bounded in-memory feed. Connection state drives the top-bar
 * indicator (`live` / `reconnecting` / `offline`); the contract says SSE is
 * best-effort, so on drop we auto-reconnect with backoff and never clear what is
 * already rendered.
 */
import { browser } from '$app/environment';
import type { SseEvent, SseEventKind } from '$lib/api/types';

export type ConnState = 'connecting' | 'live' | 'reconnecting' | 'offline';

/** A received event, tagged with arrival time and a monotonic id for keying. */
export interface FeedItem {
	id: number;
	kind: SseEventKind;
	event: SseEvent;
	at: number;
}

const MAX_FEED = 400;
const MAX_BACKOFF = 15_000;

class ConnectionStore {
	/** Current connection state (reactive). */
	state = $state<ConnState>('offline');
	/** Newest-first event feed (bounded to {@link MAX_FEED}). */
	feed = $state<FeedItem[]>([]);
	/** Count of events seen since start (never reset), for "new since" badges. */
	seen = $state(0);

	#es: EventSource | null = null;
	#backoff = 1000;
	#retry: ReturnType<typeof setTimeout> | null = null;
	#nextId = 1;
	#started = false;
	#listeners = new Set<(item: FeedItem) => void>();

	/** Open the stream. Idempotent; a no-op during SSR. */
	connect() {
		if (!browser || this.#started) return;
		this.#started = true;
		this.#open();
	}

	/** Close the stream and stop reconnecting. */
	disconnect() {
		this.#started = false;
		if (this.#retry) clearTimeout(this.#retry);
		this.#retry = null;
		this.#es?.close();
		this.#es = null;
		this.state = 'offline';
	}

	/** Subscribe to every incoming event (for per-view routing/filtering). Returns an unsubscribe. */
	on(fn: (item: FeedItem) => void): () => void {
		this.#listeners.add(fn);
		return () => this.#listeners.delete(fn);
	}

	#open() {
		if (!browser) return;
		this.state = this.feed.length ? 'reconnecting' : 'connecting';
		const es = new EventSource('/api/events');
		this.#es = es;

		es.onopen = () => {
			this.state = 'live';
			this.#backoff = 1000;
		};
		es.onmessage = (msg) => this.#ingest(msg.data);
		es.onerror = () => {
			// EventSource reconnects on its own, but we want explicit state +
			// bounded backoff and to surface `offline` once the browser gives up.
			es.close();
			this.#es = null;
			this.state = 'reconnecting';
			if (this.#started) this.#scheduleRetry();
			else this.state = 'offline';
		};
	}

	#scheduleRetry() {
		if (this.#retry) clearTimeout(this.#retry);
		this.#retry = setTimeout(() => this.#open(), this.#backoff);
		this.#backoff = Math.min(this.#backoff * 2, MAX_BACKOFF);
	}

	#ingest(raw: string) {
		let event: SseEvent;
		try {
			event = JSON.parse(raw) as SseEvent;
		} catch {
			return; // ignore keep-alives / malformed lines
		}
		const kind = Object.keys(event)[0] as SseEventKind | undefined;
		if (!kind) return;
		const item: FeedItem = { id: this.#nextId++, kind, event, at: Date.now() };
		this.feed = [item, ...this.feed].slice(0, MAX_FEED);
		this.seen += 1;
		for (const fn of this.#listeners) fn(item);
	}
}

/** The app-wide SSE connection. Connected once from the root layout (client). */
export const connection = new ConnectionStore();
