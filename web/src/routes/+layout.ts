import type { LayoutLoad } from './$types';
import { createApi } from '$lib/api/client';

/**
 * Load the workspace's topic list for the Navigator on every route (SSR + client).
 *
 * The backend is built in parallel and may be unreachable in dev; a failure here
 * must not blank the shell, so we degrade to an empty list and surface the error
 * for the Navigator to show inline.
 */
export const load: LayoutLoad = async ({ fetch }) => {
	const api = createApi(fetch);
	try {
		const { themes } = await api.themes();
		return { themes, themesError: null as string | null };
	} catch (e) {
		return { themes: [] as string[], themesError: e instanceof Error ? e.message : 'unreachable' };
	}
};
