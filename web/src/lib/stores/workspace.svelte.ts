/**
 * Workspace tree + current selection.
 *
 * Models the IA from `ui-ux-design-draft.md §2`: Workspace → Topic (theme) →
 * Article (file, with parent/depth hierarchy and an explicit, draggable reading
 * order). The tree is hydrated from SSR `load` data and lazily fills per-theme
 * article lists when a topic expands. Structure edits (reorder / reparent) are
 * applied optimistically and persisted via the API, with rollback on failure.
 */
import { goto } from '$app/navigation';
import { api } from '$lib/api/client';
import type { ArticleRow } from '$lib/api/types';
import { toast } from './toast';

/** A topic node and its loaded articles (reading order). */
export interface TopicNode {
	theme: string;
	articles: ArticleRow[];
	loaded: boolean;
	loading: boolean;
	error: string | null;
}

/** The active selection — nothing, a topic, or an article within a topic. */
export type Selection =
	| { kind: 'none' }
	| { kind: 'topic'; theme: string }
	| { kind: 'article'; theme: string; file: string };

/** A flattened row the navigator renders (topic or article). */
export interface FlatRow {
	type: 'topic' | 'article';
	theme: string;
	file?: string;
	title: string;
	depth: number;
	parent: string | null;
	/** Author tag of the article's HEAD commit, when known (drives the dot color). */
	author?: string;
	selected: boolean;
}

class WorkspaceStore {
	topics = $state<TopicNode[]>([]);
	selection = $state<Selection>({ kind: 'none' });
	expanded = $state<Record<string, boolean>>({});
	filter = $state('');

	/** Seed the topic list (from the root SSR load). Preserves loaded articles. */
	setThemes(themes: string[]) {
		const prev = new Map(this.topics.map((t) => [t.theme, t]));
		this.topics = themes.map(
			(theme) =>
				prev.get(theme) ?? {
					theme,
					articles: [],
					loaded: false,
					loading: false,
					error: null
				}
		);
	}

	/** Seed one theme's articles (from a topic/article SSR load). */
	setArticles(theme: string, articles: ArticleRow[]) {
		this.#patchTopic(theme, { articles, loaded: true, loading: false, error: null });
		this.expanded[theme] = true;
	}

	/** Toggle a topic's disclosure, lazily loading its articles the first time. */
	async toggle(theme: string) {
		const open = !this.expanded[theme];
		this.expanded[theme] = open;
		if (open) await this.ensureArticles(theme);
	}

	/** Ensure a theme's articles are loaded (browser fetch). */
	async ensureArticles(theme: string) {
		const node = this.topics.find((t) => t.theme === theme);
		if (!node || node.loaded || node.loading) return;
		this.#patchTopic(theme, { loading: true, error: null });
		try {
			const { articles } = await api.articles(theme);
			this.#patchTopic(theme, { articles, loaded: true, loading: false });
		} catch (e) {
			this.#patchTopic(theme, {
				loading: false,
				error: e instanceof Error ? e.message : 'load failed'
			});
		}
	}

	/** Select + navigate to a topic (orchestration shell). */
	selectTopic(theme: string) {
		this.selection = { kind: 'topic', theme };
		this.expanded[theme] = true;
		void this.ensureArticles(theme);
		void goto(`/t/${encodeURIComponent(theme)}`);
	}

	/** Select + navigate to an article (authoring shell). */
	selectArticle(theme: string, file: string) {
		this.selection = { kind: 'article', theme, file };
		this.expanded[theme] = true;
		void goto(`/a/${encodeURIComponent(theme)}/${encodeURIComponent(file)}`);
	}

	/** Mark the current selection without navigating (called from page loads). */
	syncSelection(sel: Selection) {
		this.selection = sel;
		if (sel.kind !== 'none') this.expanded[sel.theme] = true;
	}

	/**
	 * Persist a new reading order for `theme` (drag reorder). Optimistic: the
	 * tree reorders immediately and rolls back if `POST /reorder` fails.
	 */
	async reorder(theme: string, order: string[]) {
		const node = this.topics.find((t) => t.theme === theme);
		if (!node) return;
		const prev = node.articles;
		const byFile = new Map(prev.map((a) => [a.file, a]));
		const next = order.map((f) => byFile.get(f)).filter((a): a is ArticleRow => !!a);
		this.#patchTopic(theme, { articles: next });
		try {
			await api.reorder(theme, order);
		} catch (e) {
			this.#patchTopic(theme, { articles: prev });
			toast.error(e instanceof Error ? e.message : '重排失败');
		}
	}

	/**
	 * Persist a new parent for an article (drag reparent). Optimistic, with a
	 * depth recompute and rollback on failure.
	 */
	async reparent(theme: string, file: string, parent: string | null) {
		const node = this.topics.find((t) => t.theme === theme);
		if (!node) return;
		const prev = node.articles;
		const next = prev.map((a) =>
			a.file === file ? { ...a, parent, depth: this.#depthOf(prev, parent) + 1 } : a
		);
		this.#patchTopic(theme, { articles: next });
		try {
			await api.reparent(theme, file, parent);
		} catch (e) {
			this.#patchTopic(theme, { articles: prev });
			toast.error(e instanceof Error ? e.message : '改父级失败');
		}
	}

	/** The navigator's flattened, filtered rows (topics + expanded articles). */
	get rows(): FlatRow[] {
		const q = this.filter.trim().toLowerCase();
		const out: FlatRow[] = [];
		for (const t of this.topics) {
			const topicMatch = !q || t.theme.toLowerCase().includes(q);
			const articleRows = this.expanded[t.theme]
				? t.articles.filter((a) => !q || a.title.toLowerCase().includes(q))
				: [];
			// Hide a topic entirely only when it neither matches nor has matches.
			if (q && !topicMatch && articleRows.length === 0) continue;
			out.push({
				type: 'topic',
				theme: t.theme,
				title: t.theme,
				depth: 0,
				parent: null,
				selected: this.selection.kind === 'topic' && this.selection.theme === t.theme
			});
			for (const a of articleRows) {
				out.push({
					type: 'article',
					theme: t.theme,
					file: a.file,
					title: a.title,
					depth: a.depth,
					parent: a.parent,
					selected:
						this.selection.kind === 'article' &&
						this.selection.theme === t.theme &&
						this.selection.file === a.file
				});
			}
		}
		return out;
	}

	#depthOf(articles: ArticleRow[], file: string | null): number {
		if (!file) return -1; // top-level child becomes depth 0
		return articles.find((a) => a.file === file)?.depth ?? 0;
	}

	#patchTopic(theme: string, patch: Partial<TopicNode>) {
		this.topics = this.topics.map((t) => (t.theme === theme ? { ...t, ...patch } : t));
	}
}

/** The app-wide workspace store. */
export const workspace = new WorkspaceStore();
