import type { PageLoad } from './$types';
import { createApi } from '$lib/api/client';

/**
 * Topic (orchestration) shell load: the topic's articles, theme config, and the
 * skill list, fetched SSR-first for a shareable `/t/{topic}` URL. Each piece
 * degrades independently so a partial backend still renders the shell.
 *
 * The master dialogue's structured product (chat plan) is reconstructed from the
 * article hierarchy + each article's HEAD commit author/report (see
 * `buildPlanRows` in the page), so the orchestration view stays meaningful even
 * before the next `chat` round runs.
 */
export const load: PageLoad = async ({ params, fetch }) => {
	const api = createApi(fetch);
	const theme = params.topic;

	const [articles, config, skills] = await Promise.all([
		api.articles(theme).catch(() => ({ theme, articles: [] })),
		api.themeConfig(theme).catch(() => null),
		api.skills().catch(() => ({ skills: [] }))
	]);

	return {
		theme,
		articles: articles.articles,
		config,
		skills: skills.skills
	};
};
