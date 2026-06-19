/**
 * Author identity → visual encoding, per the shared palette in
 * `docs/api-contract.md` (§颜色) and `AI-Write.dc.html`.
 *
 * Authorship must not rely on hue alone (a11y, §11·2 / WCAG AA), so each author
 * also carries a `deco` (border style) and a `short` label that callers layer on
 * top of color — matching the canvas's color / texture / label expressions.
 */

/** A resolved author's visual identity. */
export interface AuthorStyle {
	/** Stable key (`'you' | 'chat' | 'reasoner' | 'other'`). */
	key: 'you' | 'chat' | 'reasoner' | 'other';
	/** Display label (human-friendly). */
	label: string;
	/** Short 2-char badge label for the `label` expression. */
	short: string;
	/** Primary oklch color. */
	color: string;
	/** A pale tint of `color`, for `label`/background expression. */
	tint: string;
	/** Underline / border-bottom style distinguishing this author non-chromatically. */
	deco: 'solid' | 'dotted' | 'dashed';
	/** The repeating-gradient angle (deg) for the `texture` expression. */
	angle: number;
	/** The oklch hue of `color`, reused by the texture gradient. */
	hue: number;
}

const YOU: AuthorStyle = {
	key: 'you',
	label: '你',
	short: '你',
	color: 'oklch(0.44 0.075 255)',
	tint: 'oklch(0.95 0.03 255)',
	deco: 'solid',
	angle: 45,
	hue: 255
};
const CHAT: AuthorStyle = {
	key: 'chat',
	label: 'deepseek-chat',
	short: 'DC',
	color: 'oklch(0.45 0.08 152)',
	tint: 'oklch(0.95 0.038 152)',
	deco: 'dotted',
	angle: -45,
	hue: 152
};
const REASONER: AuthorStyle = {
	key: 'reasoner',
	label: 'deepseek-reasoner',
	short: 'DR',
	color: 'oklch(0.47 0.09 62)',
	tint: 'oklch(0.95 0.05 62)',
	deco: 'dashed',
	angle: 90,
	hue: 62
};

/** Reserved fallback hues for unknown models (deterministic by id hash). */
const FALLBACK_HUES = [300, 200, 20, 110, 340, 250];

/**
 * Resolve an author tag to its visual identity.
 *
 * `tag` is the contract's author name: `"human"` (the human collaborator),
 * `"deepseek-chat*"`, `"deepseek-reasoner*"`, or any other `"<model-id>/<label>"`.
 * The full `"<name> <email>"` author string is also accepted — only the name
 * portion is matched. Unknown models hash to a reserved fallback hue so they are
 * stable and mutually distinguishable.
 */
export function authorColor(tag: string): AuthorStyle {
	const name = (tag ?? '').trim().split(/\s+/)[0]?.toLowerCase() ?? '';

	if (name === 'human' || name === 'you' || name === '你' || name === '人') {
		return YOU;
	}
	if (name.includes('deepseek-reasoner') || name.includes('reasoner')) {
		return REASONER;
	}
	if (name.includes('deepseek-chat') || name.includes('chat')) {
		return CHAT;
	}

	const hue = FALLBACK_HUES[hashString(name) % FALLBACK_HUES.length];
	const short = (name.replace(/[^a-z0-9]/gi, '').slice(0, 2) || '??').toUpperCase();
	return {
		key: 'other',
		label: tag || 'unknown',
		short,
		color: `oklch(0.5 0.09 ${hue})`,
		tint: `oklch(0.95 0.04 ${hue})`,
		deco: 'dotted',
		angle: 0,
		hue
	};
}

/** The three known author identities, for legends and signature cards. */
export const KNOWN_AUTHORS: AuthorStyle[] = [YOU, CHAT, REASONER];

/** A small, stable FNV-1a string hash (positive int). */
function hashString(s: string): number {
	let h = 0x811c9dc5;
	for (let i = 0; i < s.length; i++) {
		h ^= s.charCodeAt(i);
		h = Math.imul(h, 0x01000193);
	}
	return h >>> 0;
}
