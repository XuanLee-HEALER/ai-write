import type { PageLoad } from './$types';
import { createApi } from '$lib/api/client';

/**
 * Article (authoring) shell load: the article content plus its sibling list (for
 * the navigator + prev/next), commit history, and contribution split — fetched
 * SSR-first for a shareable `/a/{theme}/{file}` URL. Each piece degrades
 * independently so a partial backend still renders the shell.
 */
export const load: PageLoad = async ({ params, fetch }) => {
	const api = createApi(fetch);
	const { theme, file } = params;

	const [content, rich, articles, history, contributions, blame] = await Promise.all([
		api.article(theme, file).catch((e) => ({
			theme,
			file,
			content: '',
			error: e instanceof Error ? e.message : 'load failed'
		})),
		// Authorship runs (B2). Optional: the body renders plain when absent, and
		// the authorship toggle is hidden — so this degrades independently.
		api.articleRich(theme, file).catch(() => null),
		api.articles(theme).catch(() => ({ theme, articles: [] })),
		api.history(theme, file).catch(() => ({ history: [] })),
		api.contributions(theme, file).catch(() => ({ contributions: [] })),
		api.blame(theme, file).catch(() => ({ blame: [] }))
	]);

	return {
		theme,
		file,
		content: 'content' in content ? content.content : '',
		contentError: 'error' in content ? (content.error as string) : null,
		rich,
		articles: articles.articles,
		history: history.history,
		contributions: contributions.contributions,
		blame: blame.blame
	};
};
