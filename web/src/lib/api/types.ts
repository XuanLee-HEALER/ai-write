/**
 * Wire types for the AI-Write domain API, built to `docs/api-contract.md`.
 *
 * The backend (axum) implements the same contract in parallel; these types are
 * the frontend's view of that contract, not a generated mirror of the Rust. All
 * requests are made under the `/api` prefix (Vite proxy in dev, Hono/Bun reverse
 * proxy in prod).
 */

/** A topic (= directory) in the workspace. The bare-list endpoint returns names. */
export type ThemeName = string;

/** One article row from `GET /api/themes/{theme}/articles` (reading order). */
export interface ArticleRow {
	/** File name within the theme, e.g. `ch2.md`. */
	file: string;
	/** Human title (first heading / front-matter), for display. */
	title: string;
	/** Parent article file, or `null` for a top-level article. */
	parent: string | null;
	/** Nesting depth (0 = top level), drives indent in the navigator. */
	depth: number;
}

/** `GET /api/themes/{theme}/articles`. */
export interface ArticlesResponse {
	theme: string;
	articles: ArticleRow[];
}

/** `GET /api/themes` → `{themes}`. */
export interface ThemesResponse {
	themes: ThemeName[];
}

/** Theme configuration (B1 extension: multi-skill stack + slave model). */
export interface ThemeConfig {
	description: string;
	/** Single default skill (kept for back-compat with `default_skill_ids`). */
	default_skill: string | null;
	/** Ordered multi-skill stack (last wins); preferred over `default_skill`. */
	default_skill_ids?: string[];
	slave_model: string | null;
}

/** One skill from `GET /api/skills`. */
export interface Skill {
	id: string;
	name: string;
	description: string;
}

export interface SkillsResponse {
	skills: Skill[];
}

/** `GET /api/articles/{theme}/{file}` (plain content, back-compat default). */
export interface ArticleContent {
	theme: string;
	file: string;
	content: string;
}

/** One authored run (word-level author run) within a rich block (B2). */
export interface RichRun {
	text: string;
	/** Author tag: `"human"` or `"<model-id>/<label>"`. Drives {@link authorColor}. */
	author: string;
}

export interface RichBlock {
	/** Block kind, e.g. `"paragraph"` / `"heading"`. */
	kind: string;
	runs: RichRun[];
}

/** `GET /api/articles/{theme}/{file}?format=rich` (B2 author coloring). */
export interface ArticleRich {
	theme: string;
	file: string;
	blocks: RichBlock[];
	authors: { id: string; label: string }[];
}

/** One commit in an article's history (newest-first). */
export interface HistoryEntry {
	id: string;
	/** `"<name> <email>"`; `name` is `"human"` or `"<model-id>/<label>"`. */
	author: string;
	message: string;
	time: string;
}

export interface HistoryResponse {
	history: HistoryEntry[];
}

/** `GET /api/articles/{theme}/{file}/diff?from&to`. */
export interface DiffResponse {
	diff: string;
}

/** One blame line from `GET /api/articles/{theme}/{file}/blame`. */
export interface BlameLine {
	line_no: number;
	author: string;
	short_sha: string;
}

export interface BlameResponse {
	blame: BlameLine[];
}

/** `POST /api/articles/{theme}/{file}/undo`. */
export type UndoResponse =
	| { undone: true; committed: string }
	| { undone: false; reason: string };

/** One author's aggregated contribution share (B1·3, signature card). */
export interface Contribution {
	/** Author tag (`"human"` / `"<model-id>/<label>"`). */
	author: string;
	/** Display label. */
	label: string;
	/** Integer percent; the set sums to 100. */
	pct: number;
	lines: number;
}

export interface ContributionsResponse {
	contributions: Contribution[];
}

/** Body for `POST /api/themes/{theme}/chat` (B1·2 multi-skill orchestration). */
export interface ChatRequest {
	goal: string;
	/** Single skill (back-compat). */
	skill_id?: string;
	/** Ordered skill stack; preferred over `skill_id`. */
	skill_ids?: string[];
	slave_model?: string;
}

/** A planned article creation in the orchestration result. */
export interface PlanCreated {
	theme: string;
	file: string;
	title: string;
	parent: string | null;
}

/** A dispatched writer entry in the orchestration result. */
export interface PlanDispatched {
	file: string;
	writer: string;
	status: string;
	summary: string;
}

/** `POST /api/themes/{theme}/chat` response (B1·2). */
export interface ChatResponse {
	outcome: string;
	message: string;
	reports: unknown[];
	plan?: {
		created: PlanCreated[];
		dispatched: PlanDispatched[];
	};
}

/** `PUT /api/articles/{theme}/{file}` (B1·1 human authoring). */
export interface PutArticleResponse {
	theme: string;
	file: string;
	committed: string | null;
}

/** `POST /api/articles/{theme}/{file}/request-edit` (B3 head-of-queue). */
export interface RequestEditResponse {
	queued: true;
	ahead: number;
}

/** A `{error}` body returned on any non-2xx (error convention). */
export interface ApiError {
	error: string;
}

/* ------------------------------------------------------------------ */
/* SSE events (`GET /api/events`) — externally-tagged enum, one per line. */
/* ------------------------------------------------------------------ */

export type SseEvent =
	| { SessionStarted: { role: string; system_excerpt: string } }
	| { RoundStarted: { round: number } }
	| { ModelMessage: { text: string } }
	| { ToolCalled: { name: string; args: unknown } }
	| { ToolResult: { name: string; ok: boolean; summary: string } }
	| { EditCommitted: { article: string; author: string; sha: string } }
	| { SlaveSpawned: { theme: string; file: string; writer: string } }
	| { SlaveReported: { status: string; summary: string } }
	| { Finished: { outcome: string } }
	// B3 coordinator transaction events
	| { TxnAcquired: { writer: string; paths: string[] } }
	| { TxnQueued: { writer: string; ahead: number } }
	| { TxnReleased: { writer: string } }
	| { HandoffToHuman: { theme: string; file: string } };

/** The discriminant key of an {@link SseEvent} (its single property name). */
export type SseEventKind = keyof UnionToIntersection<SseEvent>;

/**
 * The minimal shape a received SSE feed item exposes to view-model code.
 *
 * `connection.svelte.ts`'s `FeedItem` structurally satisfies this; keeping a
 * narrow alias here lets the pure topic/article view-model helpers depend on the
 * wire types only, not on the runes-based store.
 */
export interface FeedItemLike {
	id: number;
	kind: SseEventKind;
	event: SseEvent;
	/** Arrival time (ms epoch). */
	at: number;
}

type UnionToIntersection<U> = (U extends unknown ? (k: U) => void : never) extends (
	k: infer I
) => void
	? I
	: never;
