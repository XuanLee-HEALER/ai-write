/**
 * Typed client for the AI-Write domain API (`docs/api-contract.md`).
 *
 * Every call accepts an optional `fetch` so SvelteKit `load` functions can pass
 * their request-scoped `fetch` (preserving cookies/relative URLs during SSR);
 * in the browser the global `fetch` is used. All paths are under `/api`, which
 * the Vite dev proxy / Hono prod server reverse-proxy to axum.
 */
import type {
	ArticleContent,
	ArticleRich,
	ArticlesResponse,
	BlameResponse,
	ChatRequest,
	ChatResponse,
	ContributionsResponse,
	DiffResponse,
	HistoryResponse,
	PutArticleResponse,
	RequestEditResponse,
	SkillsResponse,
	ThemeConfig,
	ThemesResponse,
	UndoResponse
} from './types';

type Fetch = typeof fetch;

/** Thrown by every client call on a non-2xx response. Carries the parsed
 * `{error}` message (contract error convention) and the HTTP status. */
export class ApiRequestError extends Error {
	readonly status: number;
	constructor(message: string, status: number) {
		super(message);
		this.name = 'ApiRequestError';
		this.status = status;
	}
}

async function request<T>(
	path: string,
	init: RequestInit & { fetch?: Fetch } = {}
): Promise<T> {
	const { fetch: f = fetch, ...rest } = init;
	let res: Response;
	try {
		res = await f(`/api${path}`, {
			...rest,
			headers: {
				...(rest.body ? { 'content-type': 'application/json' } : {}),
				...rest.headers
			}
		});
	} catch (cause) {
		throw new ApiRequestError(
			cause instanceof Error ? cause.message : 'network error',
			0
		);
	}
	if (!res.ok) {
		let message = `${res.status} ${res.statusText}`;
		try {
			const body = (await res.json()) as { error?: string };
			if (body?.error) message = body.error;
		} catch {
			/* non-JSON error body; keep status line */
		}
		throw new ApiRequestError(message, res.status);
	}
	if (res.status === 204) return undefined as T;
	return (await res.json()) as T;
}

const json = (data: unknown): RequestInit => ({
	method: 'POST',
	body: JSON.stringify(data)
});

/** Build the `/api` client. Pass a request-scoped `fetch` inside a `load`. */
export function createApi(f: Fetch = fetch) {
	const opts = (extra: RequestInit = {}) => ({ fetch: f, ...extra });

	return {
		/** `GET /api/themes`. */
		themes: () => request<ThemesResponse>('/themes', opts()),

		/** `GET /api/themes/{theme}/articles` (reading order). */
		articles: (theme: string) =>
			request<ArticlesResponse>(`/themes/${enc(theme)}/articles`, opts()),

		/** `GET /api/themes/{theme}/config`. */
		themeConfig: (theme: string) =>
			request<ThemeConfig>(`/themes/${enc(theme)}/config`, opts()),

		/** `PUT /api/themes/{theme}/config`. */
		putThemeConfig: (theme: string, config: ThemeConfig) =>
			request<ThemeConfig>(`/themes/${enc(theme)}/config`, {
				...opts(),
				method: 'PUT',
				body: JSON.stringify(config)
			}),

		/** `POST /api/themes/{theme}/reorder`. */
		reorder: (theme: string, order: string[]) =>
			request<unknown>(`/themes/${enc(theme)}/reorder`, {
				...opts(),
				...json({ order })
			}),

		/** `POST /api/themes/{theme}/articles/{file}/parent`. */
		reparent: (theme: string, file: string, parent: string | null) =>
			request<unknown>(
				`/themes/${enc(theme)}/articles/${enc(file)}/parent`,
				{ ...opts(), ...json({ parent }) }
			),

		/** `GET /api/skills`. */
		skills: () => request<SkillsResponse>('/skills', opts()),

		/** `GET /api/articles/{theme}/{file}` (plain content). */
		article: (theme: string, file: string) =>
			request<ArticleContent>(`/articles/${enc(theme)}/${enc(file)}`, opts()),

		/** `GET /api/articles/{theme}/{file}?format=rich` (author runs, B2). */
		articleRich: (theme: string, file: string) =>
			request<ArticleRich>(
				`/articles/${enc(theme)}/${enc(file)}?format=rich`,
				opts()
			),

		/** `PUT /api/articles/{theme}/{file}` (human authoring, B1·1). */
		putArticle: (theme: string, file: string, text: string) =>
			request<PutArticleResponse>(`/articles/${enc(theme)}/${enc(file)}`, {
				...opts(),
				method: 'PUT',
				body: JSON.stringify({ text })
			}),

		/** `GET /api/articles/{theme}/{file}/history` (newest-first). */
		history: (theme: string, file: string) =>
			request<HistoryResponse>(
				`/articles/${enc(theme)}/${enc(file)}/history`,
				opts()
			),

		/** `GET /api/articles/{theme}/{file}/diff?from&to` (unified patch). */
		diff: (theme: string, file: string, from: string, to: string) =>
			request<DiffResponse>(
				`/articles/${enc(theme)}/${enc(file)}/diff?from=${enc(from)}&to=${enc(to)}`,
				opts()
			),

		/** `GET /api/articles/{theme}/{file}/blame` (line-level authorship). */
		blame: (theme: string, file: string) =>
			request<BlameResponse>(
				`/articles/${enc(theme)}/${enc(file)}/blame`,
				opts()
			),

		/** `GET /api/articles/{theme}/{file}/contributions` (signature card). */
		contributions: (theme: string, file: string) =>
			request<ContributionsResponse>(
				`/articles/${enc(theme)}/${enc(file)}/contributions`,
				opts()
			),

		/** `POST /api/articles/{theme}/{file}/undo`. */
		undo: (theme: string, file: string) =>
			request<UndoResponse>(`/articles/${enc(theme)}/${enc(file)}/undo`, {
				...opts(),
				method: 'POST'
			}),

		/** `POST /api/articles/{theme}/{file}/request-edit` (head-of-queue, B3). */
		requestEdit: (theme: string, file: string) =>
			request<RequestEditResponse>(
				`/articles/${enc(theme)}/${enc(file)}/request-edit`,
				{ ...opts(), method: 'POST' }
			),

		/** `DELETE /api/articles/{theme}/{file}/request-edit` (cancel queue). */
		cancelRequestEdit: (theme: string, file: string) =>
			request<unknown>(
				`/articles/${enc(theme)}/${enc(file)}/request-edit`,
				{ ...opts(), method: 'DELETE' }
			),

		/** `POST /api/themes/{theme}/chat` (orchestration, B1·2). */
		chat: (theme: string, body: ChatRequest) =>
			request<ChatResponse>(`/themes/${enc(theme)}/chat`, {
				...opts(),
				...json(body)
			})
	};
}

/** A default browser-scoped client; in `load` build a fresh one with `createApi(fetch)`. */
export const api = createApi();

/** Path-segment encode that keeps the value safe inside a URL path. */
function enc(s: string): string {
	return encodeURIComponent(s);
}

export type Api = ReturnType<typeof createApi>;
