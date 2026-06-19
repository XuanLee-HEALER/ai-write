/**
 * Article-authoring view-model helpers (F2).
 *
 * The ARTICLE view (`/a/{theme}/{file}`) is the single-article work surface: an
 * editor canvas plus an Inspector with two faces — Activity (the SSE command
 * stream filtered to *this* article) and Versions (commit timeline, diff,
 * blame, signature card). This module derives those view-models from the
 * contract types so the component stays declarative.
 *
 * Pure + framework-free so it stays unit-testable and out of the component.
 * Live coordinator txn banners (F4) are intentionally out of scope here — a
 * clean seam is left for them. Authorship body coloring (F3) lives below.
 */
import type {
	BlameLine,
	Contribution,
	FeedItemLike,
	HistoryEntry,
	RichBlock
} from '$lib/api/types';
import { authorColor, type AuthorStyle } from '$lib/author';

/* ------------------------------------------------------------------ */
/* Activity — command stream filtered to one article.                  */
/* ------------------------------------------------------------------ */

/** One row in the article's collaboration command stream. */
export interface CommandRow {
	id: number;
	/** Author style (dot + label color), resolved from the event author tag. */
	style: AuthorStyle;
	/** Author display label. */
	author: string;
	/** One-line human description. */
	text: string;
	/** Relative-ish arrival time (`H:MM`). */
	time: string;
	/** True for events that just arrived (drives the slide-in animation). */
	fresh: boolean;
}

/** Format a millisecond timestamp as `H:MM`. */
function hhmm(at: number): string {
	const d = new Date(at);
	return `${d.getHours()}:${String(d.getMinutes()).padStart(2, '0')}`;
}

function truncate(s: string, n: number): string {
	const t = (s ?? '').trim();
	return t.length > n ? t.slice(0, n - 1) + '…' : t;
}

/** Strip a possible theme prefix so `theme/file` and `file` both match `file`. */
function sameArticle(ref: string | undefined, file: string): boolean {
	if (!ref) return false;
	const tail = ref.split('/').pop();
	return ref === file || tail === file;
}

/**
 * Map one SSE feed item to a command-stream row for `file`, or `null` to drop.
 *
 * The article Activity surfaces the *content* events that touch this article —
 * edits committed to it and the model rounds/tools driving those edits. It is
 * filtered to the current article by the `article` / `paths` fields the
 * contract attaches to `EditCommitted` and the B3 `Txn*` events. Pure
 * orchestration chatter for other files is dropped.
 *
 * `now` defaults to the item arrival time; `fresh` marks the just-arrived row
 * for the slide-in animation when the caller knows it is live (not a backfill).
 */
export function commandFromFeed(
	item: FeedItemLike,
	file: string,
	opts: { fresh?: boolean } = {}
): CommandRow | null {
	const ev = item.event as Record<string, unknown>;
	const time = hhmm(item.at);
	const fresh = opts.fresh ?? false;
	const mk = (authorTag: string, text: string): CommandRow => {
		const style = authorColor(authorTag);
		return { id: item.id, style, author: style.label, text, time, fresh };
	};

	switch (item.kind) {
		case 'EditCommitted': {
			const c = ev.EditCommitted as { article: string; author: string; sha: string };
			if (!sameArticle(c.article, file)) return null;
			return mk(c.author, `提交 · ${truncate(c.sha, 12)}`);
		}
		case 'ToolCalled': {
			// Tool calls are only attributed when their path targets this article.
			const t = ev.ToolCalled as { name: string; args: unknown };
			const path = pathFromArgs(t.args);
			if (path && !sameArticle(path, file)) return null;
			if (!path) return null; // un-targeted tool calls belong to the topic feed
			return mk('', `${t.name} · ${file}`);
		}
		case 'TxnAcquired': {
			const t = ev.TxnAcquired as { writer: string; paths: string[] };
			if (!t.paths?.some((p) => sameArticle(p, file))) return null;
			return mk(t.writer, '取得编辑事务 · 进入临界区');
		}
		case 'TxnReleased': {
			const t = ev.TxnReleased as { writer: string };
			return mk(t.writer, '提交完成 · 释放事务');
		}
		case 'HandoffToHuman': {
			const t = ev.HandoffToHuman as { theme: string; file: string };
			if (!sameArticle(t.file, file)) return null;
			return mk('human', '轮到你了 · 编辑事务已交给你');
		}
		default:
			// RoundStarted / ModelMessage / Slave* / Finished / TxnQueued are not
			// per-article content rows (some belong to F4's coordinator block).
			return null;
	}
}

/** Best-effort extraction of a file path from a tool call's args object. */
function pathFromArgs(args: unknown): string | null {
	if (!args || typeof args !== 'object') return null;
	const o = args as Record<string, unknown>;
	for (const k of ['file', 'path', 'article', 'target']) {
		const v = o[k];
		if (typeof v === 'string' && v) return v;
	}
	return null;
}

/* ------------------------------------------------------------------ */
/* Versions — commit timeline rows.                                    */
/* ------------------------------------------------------------------ */

/** One commit row in the version timeline / list. */
export interface VersionRow {
	id: string;
	style: AuthorStyle;
	/** Author label: `你` for the human, else the dated model id. */
	authorLabel: string;
	message: string;
	time: string;
	/** True for the newest commit (HEAD badge + larger dot). */
	head: boolean;
	/** Whether the user has ticked this version for diff. */
	ticked: boolean;
}

/** Build version rows from history (newest-first), marking HEAD + ticked set. */
export function buildVersionRows(
	history: HistoryEntry[],
	ticked: ReadonlySet<string>
): VersionRow[] {
	return history.map((v, i) => {
		const style = authorColor(v.author);
		return {
			id: v.id,
			style,
			authorLabel: style.key === 'you' ? '你' : modelId(v.author),
			message: v.message,
			time: v.time,
			head: i === 0,
			ticked: ticked.has(v.id)
		};
	});
}

/** Extract the bare model id (drops the `<email>` portion of an author string). */
export function modelId(author: string): string {
	return (author ?? '').trim().split(/\s+/)[0] || author;
}

/**
 * Toggle a version id in the diff selection, keeping at most two ticked.
 *
 * Ticking a third version drops the oldest of the current pair (FIFO), so the
 * two ticks always describe a `from`/`to` pair the diff viewer can request.
 */
export function toggleTick(current: readonly string[], id: string): string[] {
	if (current.includes(id)) return current.filter((x) => x !== id);
	const next = [...current, id];
	return next.length > 2 ? next.slice(next.length - 2) : next;
}

/**
 * Resolve a `[a, b]` tick pair into a chronological `{from, to}` for the diff
 * request, where `from` is the older commit. `order` is the history order
 * (newest-first), so the later index is older.
 */
export function diffPair(
	ticked: readonly string[],
	order: HistoryEntry[]
): { from: string; to: string } | null {
	if (ticked.length !== 2) return null;
	const idx = (id: string) => order.findIndex((v) => v.id === id);
	const [a, b] = ticked;
	const ia = idx(a);
	const ib = idx(b);
	if (ia < 0 || ib < 0) return null;
	// Larger index = older = `from`.
	return ia > ib ? { from: a, to: b } : { from: b, to: a };
}

/* ------------------------------------------------------------------ */
/* Diff — unified patch → side-by-side / inline rows.                  */
/* ------------------------------------------------------------------ */

/** A parsed line of a unified diff. */
export interface DiffLine {
	kind: 'add' | 'del' | 'ctx' | 'hunk' | 'meta';
	text: string;
	/** Old-file line number (null for adds / headers). */
	oldNo: number | null;
	/** New-file line number (null for deletes / headers). */
	newNo: number | null;
}

/** A side-by-side diff row pairing an old and a new line. */
export interface SideRow {
	left: DiffLine | null;
	right: DiffLine | null;
}

/**
 * Parse a unified patch (`GET …/diff` → `{diff}`) into typed lines.
 *
 * File headers (`diff`, `index`, `---`, `+++`) collapse to `meta`; `@@` hunk
 * headers become `hunk` and reset the running line counters. Add/del/context
 * lines carry the old/new line numbers so a blame-style gutter is possible.
 */
export function parseUnifiedDiff(patch: string): DiffLine[] {
	const out: DiffLine[] = [];
	let oldNo = 0;
	let newNo = 0;
	for (const raw of (patch ?? '').split('\n')) {
		if (raw.startsWith('@@')) {
			const m = /@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
			if (m) {
				oldNo = parseInt(m[1], 10);
				newNo = parseInt(m[2], 10);
			}
			out.push({ kind: 'hunk', text: raw, oldNo: null, newNo: null });
			continue;
		}
		if (
			raw.startsWith('diff ') ||
			raw.startsWith('index ') ||
			raw.startsWith('--- ') ||
			raw.startsWith('+++ ') ||
			raw.startsWith('new file') ||
			raw.startsWith('deleted file')
		) {
			out.push({ kind: 'meta', text: raw, oldNo: null, newNo: null });
			continue;
		}
		if (raw.startsWith('+')) {
			out.push({ kind: 'add', text: raw.slice(1), oldNo: null, newNo: newNo++ });
			continue;
		}
		if (raw.startsWith('-')) {
			out.push({ kind: 'del', text: raw.slice(1), oldNo: oldNo++, newNo: null });
			continue;
		}
		if (raw === '' && out.length === 0) continue; // skip leading blank
		// context (leading space) or stray line
		out.push({
			kind: 'ctx',
			text: raw.startsWith(' ') ? raw.slice(1) : raw,
			oldNo: oldNo++,
			newNo: newNo++
		});
	}
	return out;
}

/**
 * Fold parsed diff lines into side-by-side rows.
 *
 * Context/hunk/meta lines span both columns; a run of deletions is zipped with
 * the following run of additions so changed lines sit on the same row (the
 * common GitHub-style two-column view), with `null` padding the shorter side.
 */
export function toSideBySide(lines: DiffLine[]): SideRow[] {
	const rows: SideRow[] = [];
	let i = 0;
	while (i < lines.length) {
		const l = lines[i];
		if (l.kind === 'ctx' || l.kind === 'hunk' || l.kind === 'meta') {
			rows.push({ left: l, right: l });
			i++;
			continue;
		}
		// gather a del-run then an add-run
		const dels: DiffLine[] = [];
		const adds: DiffLine[] = [];
		while (i < lines.length && lines[i].kind === 'del') dels.push(lines[i++]);
		while (i < lines.length && lines[i].kind === 'add') adds.push(lines[i++]);
		const n = Math.max(dels.length, adds.length);
		for (let j = 0; j < n; j++) {
			rows.push({ left: dels[j] ?? null, right: adds[j] ?? null });
		}
	}
	return rows;
}

/** Tally of changed lines in a diff (for the diff header summary). */
export interface DiffStat {
	added: number;
	removed: number;
}

/** Count added / removed lines in a parsed diff. */
export function diffStat(lines: DiffLine[]): DiffStat {
	let added = 0;
	let removed = 0;
	for (const l of lines) {
		if (l.kind === 'add') added++;
		else if (l.kind === 'del') removed++;
	}
	return { added, removed };
}

/* ------------------------------------------------------------------ */
/* Blame — line gutter.                                                */
/* ------------------------------------------------------------------ */

/** One rendered blame gutter line (the text body lives in the canvas). */
export interface BlameRow {
	lineNo: number;
	style: AuthorStyle;
	authorLabel: string;
	shortSha: string;
}

/** Map blame lines to gutter rows with resolved author styles. */
export function buildBlameRows(blame: BlameLine[]): BlameRow[] {
	return blame.map((b) => {
		const style = authorColor(b.author);
		return {
			lineNo: b.line_no,
			style,
			authorLabel: style.key === 'you' ? '你' : style.short,
			shortSha: b.short_sha
		};
	});
}

/* ------------------------------------------------------------------ */
/* Signature card — contribution bars.                                 */
/* ------------------------------------------------------------------ */

/** One contribution bar in the signature card. */
export interface ContributionBar {
	author: string;
	/** Display label (the dated model id, or `你`). */
	label: string;
	/** The bar/text color. */
	color: string;
	pct: number;
	lines: number;
}

/** Build signature-card bars from the contributions aggregate. */
export function buildContributionBars(contributions: Contribution[]): ContributionBar[] {
	return contributions.map((c) => {
		const style = authorColor(c.author);
		return {
			author: c.author,
			label: style.key === 'you' ? '你' : c.label || modelId(c.author),
			color: style.color,
			pct: c.pct,
			lines: c.lines
		};
	});
}

/* ------------------------------------------------------------------ */
/* Authorship body coloring (F3) — rich runs → styled paragraphs.      */
/* ------------------------------------------------------------------ */

/**
 * The three authorship display expressions (ui-ux §6.3 / `AI-Write.dc.html`
 * `runStyle`). Authorship must not rely on hue alone (a11y / WCAG AA), so each
 * mode layers a non-chromatic cue on top of the per-author color:
 * - `color`   — per-author underline (`deco`: solid / dotted / dashed).
 * - `texture` — a per-author diagonal repeating gradient (distinct `angle`).
 * - `label`   — a pale author tint + a superscript short author label.
 */
export type AuthorDisplay = 'color' | 'texture' | 'label';

/** One author run, resolved to its style + inline CSS for the chosen display. */
export interface StyledRun {
	text: string;
	style: AuthorStyle;
	/** Inline CSS for the run `<span>`, encoding the active display mode. */
	css: string;
	/** Short author label, rendered as a superscript only in `label` mode. */
	short: string;
	/** Whether to render the superscript label (true only in `label` mode). */
	showLabel: boolean;
}

/** One rendered paragraph of styled author runs. */
export interface StyledPara {
	/** Block kind passed through from the contract (`paragraph` / `heading` / …). */
	kind: string;
	runs: StyledRun[];
}

/**
 * Inline CSS for one author run under a display mode, matching the canvas's
 * `runStyle`. Returns a `style=`-ready string (so the component stays declarative
 * and never branches on mode in markup).
 */
export function runInlineStyle(style: AuthorStyle, mode: AuthorDisplay): string {
	if (mode === 'texture') {
		const grad = `repeating-linear-gradient(${style.angle}deg, oklch(0.5 0.09 ${style.hue} / .18) 0 2px, transparent 2px 5px)`;
		return [
			`background-image:${grad}`,
			'color:var(--color-ink)',
			'-webkit-box-decoration-break:clone',
			'box-decoration-break:clone',
			'padding:1px 0'
		].join(';');
	}
	if (mode === 'label') {
		return [
			`background:${style.tint}`,
			'color:var(--color-ink)',
			'-webkit-box-decoration-break:clone',
			'box-decoration-break:clone',
			'padding:1px 2px',
			'border-radius:2px'
		].join(';');
	}
	// color: per-author underline.
	return [`color:${style.color}`, `border-bottom:1.5px ${style.deco} ${style.color}`, 'padding-bottom:1.5px'].join(
		';'
	);
}

/**
 * Map the rich blocks (`GET …?format=rich`) into paragraphs of styled runs for
 * the chosen display mode. Each run's author tag resolves through
 * {@link authorColor}; empty runs are dropped so adjacent same-author text reads
 * cleanly. The result is render-ready, so the component just iterates.
 */
export function buildStyledParas(blocks: RichBlock[], mode: AuthorDisplay): StyledPara[] {
	return (blocks ?? []).map((block) => ({
		kind: block.kind,
		runs: (block.runs ?? [])
			.filter((r) => r.text !== '')
			.map((r) => {
				const style = authorColor(r.author);
				return {
					text: r.text,
					style,
					css: runInlineStyle(style, mode),
					short: style.short,
					showLabel: mode === 'label'
				};
			})
	}));
}

/** One legend chip (author swatch + label) for the authorship legend row. */
export interface LegendChip {
	key: string;
	label: string;
	/** Inline CSS for the swatch box, encoding the active display mode. */
	swatchCss: string;
}

/**
 * Build the authorship legend chips for the *authors present in this article*.
 *
 * Authors are taken from the rich response's `authors` list (falling back to the
 * three known identities when absent), de-duplicated by resolved style key so
 * `human`/`你` collapse to one chip. The swatch mirrors each display mode: a
 * gradient tile for `texture`, a tinted/outlined tile for `label`, a solid color
 * tile for `color`.
 */
export function buildLegendChips(
	authors: { id: string; label: string }[] | undefined,
	mode: AuthorDisplay
): LegendChip[] {
	const source =
		authors && authors.length
			? authors
			: KNOWN_AUTHOR_TAGS.map((id) => ({ id, label: '' }));
	const seen = new Set<string>();
	const chips: LegendChip[] = [];
	for (const a of source) {
		const style = authorColor(a.id);
		if (seen.has(style.key)) continue;
		seen.add(style.key);
		chips.push({
			key: style.key,
			label: a.label || style.label,
			swatchCss: legendSwatchCss(style, mode)
		});
	}
	return chips;
}

/** The author tags whose legend appears by default (the three known families). */
const KNOWN_AUTHOR_TAGS = ['human', 'deepseek-chat', 'deepseek-reasoner'];

/** Inline CSS for a legend swatch tile under a display mode (matches the canvas). */
function legendSwatchCss(style: AuthorStyle, mode: AuthorDisplay): string {
	const box = 'width:13px;height:13px;border-radius:2px;flex:0 0 auto';
	if (mode === 'texture') {
		const grad = `repeating-linear-gradient(${style.angle}deg, oklch(0.5 0.09 ${style.hue} / .45) 0 2px, transparent 2px 5px)`;
		return `${box};background-image:${grad};border:1px solid var(--color-rule-soft)`;
	}
	if (mode === 'label') {
		return `${box};background:${style.tint};border:1px solid ${style.color}`;
	}
	return `${box};background:${style.color}`;
}
