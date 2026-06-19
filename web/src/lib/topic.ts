/**
 * Topic-orchestration view-model helpers (F1).
 *
 * The TOPIC view (`/t/{topic}`) is the planning layer: a master dialogue whose
 * key payload is the *structured product* — the set of articles the master
 * created/dispatched, each with its hierarchy, writer, and report status. This
 * module derives that view-model from the contract's article rows + the live
 * orchestration result (`POST /api/themes/{theme}/chat` → `plan`), and maps the
 * SSE event feed into the inspector's typed AI-operation rows.
 *
 * Pure + framework-free so it stays unit-testable and out of the component.
 */
import type {
	ArticleRow,
	ChatResponse,
	FeedItemLike,
	PlanDispatched
} from '$lib/api/types';
import { authorColor, type AuthorStyle } from '$lib/author';

/** One row of the master plan's structured product (one created article). */
export interface PlanRow {
	/** Article file id (stable key). */
	file: string;
	/** Display title. */
	title: string;
	/** Nesting depth (0 = top level) → indent. */
	depth: number;
	/** Writer label, e.g. `人`, `deepseek-chat`, `人 + reasoner`. */
	writer: string;
	/** Author style for the row dot (derived from the writer tag). */
	style: AuthorStyle;
	/** Report status badge text. */
	badge: string;
	/** Badge tone, picked from the badge text. */
	tone: 'done' | 'editing' | 'pending';
}

/** A writer-model choice for the topic input + inspector default. */
export interface WriterModel {
	id: string;
	label: string;
}

/**
 * The writer models the master can dispatch to. Mirrors the DeepSeek V4 family
 * the kernel targets (`deepseek-v4-pro` / `-flash`, surfaced as chat/reasoner).
 * The contract treats `slave_model` as a free string, so this list is a
 * convenience default — unknown values from config still round-trip.
 */
export const WRITER_MODELS: WriterModel[] = [
	{ id: 'deepseek-chat', label: 'deepseek-chat · 行文' },
	{ id: 'deepseek-reasoner', label: 'deepseek-reasoner · 内省' }
];

/** Resolve a `slave_model` string to a label, tolerating unknown ids. */
export function writerModelLabel(id: string | null | undefined): string {
	if (!id) return '未设置';
	return WRITER_MODELS.find((m) => m.id === id)?.label ?? id;
}

/**
 * Classify a report-status badge into a visual tone.
 *
 * `done` covers settled / report-✓ states, `editing` an in-flight human edit,
 * else `pending`. Matches the canvas badge styling in `AI-Write.dc.html`.
 */
export function badgeTone(badge: string): PlanRow['tone'] {
	const b = badge.toLowerCase();
	if (b.includes('✓') || b.includes('定稿') || b.includes('done') || b.includes('ok')) {
		return 'done';
	}
	if (b.includes('编辑') || b.includes('edit') || b.includes('live')) return 'editing';
	return 'pending';
}

/**
 * Build the master plan's structured-product rows.
 *
 * Articles are the durable record of what the master created; the live `chat`
 * result's `dispatched` list (when present) overlays each article's current
 * writer + report status. When no chat has run yet, articles still render with a
 * neutral `pending` badge so the plan is never blank.
 */
export function buildPlanRows(
	articles: ArticleRow[],
	dispatched: PlanDispatched[] = []
): PlanRow[] {
	const byFile = new Map(dispatched.map((d) => [d.file, d]));
	return articles.map((a) => {
		const d = byFile.get(a.file);
		const writer = d?.writer ?? '—';
		const badge = d ? reportBadge(d) : '待派发';
		return {
			file: a.file,
			title: a.title || a.file,
			depth: a.depth,
			writer,
			style: authorColor(writer),
			badge,
			tone: badgeTone(badge)
		};
	});
}

/** Human-facing report badge for a dispatched writer entry. */
function reportBadge(d: PlanDispatched): string {
	const s = (d.status ?? '').toLowerCase();
	if (s.includes('report') || s.includes('done') || s.includes('ok') || s.includes('✓')) {
		return 'report ✓';
	}
	if (s.includes('edit')) return '编辑中';
	if (s.includes('settle') || s.includes('final') || s.includes('定稿')) return '已定稿';
	return d.status || '进行中';
}

/** A typed AI-operation row for the inspector feed, derived from one SSE event. */
export interface OpRow {
	id: number;
	/** Short uppercase tag: `round` / `tool` / `commit` / `slave`. */
	tag: 'round' | 'tool' | 'commit' | 'slave' | 'master' | 'done';
	/** One-line human description. */
	text: string;
	/** `HH:MM` arrival time. */
	time: string;
	/** Tag color (oklch / token), per the canvas. */
	color: string;
}

const OP_TAG_COLOR: Record<OpRow['tag'], string> = {
	commit: 'var(--color-accent)',
	tool: 'var(--color-author-chat)',
	round: 'var(--color-ink-faint)',
	slave: 'var(--color-author-reasoner)',
	master: 'var(--color-accent)',
	done: 'var(--color-live)'
};

/** Tailwind/CSS color value for an op tag. */
export function opTagColor(tag: OpRow['tag']): string {
	return OP_TAG_COLOR[tag];
}

/** Format a millisecond timestamp as `H:MM` (matching the canvas op times). */
function hhmm(at: number): string {
	const d = new Date(at);
	return `${d.getHours()}:${String(d.getMinutes()).padStart(2, '0')}`;
}

/**
 * Map one SSE feed item to an inspector op row, or `null` to drop it.
 *
 * The TOPIC inspector surfaces the *orchestration* events (rounds, tool calls,
 * commits, slave lifecycle, master messages, finish) — the same typed rows the
 * canvas shows. Lower-level transaction events (B3 `Txn*`) are the article
 * view's concern and are filtered out here.
 */
export function opFromFeed(item: FeedItemLike): OpRow | null {
	const ev = item.event as Record<string, unknown>;
	const time = hhmm(item.at);
	const mk = (tag: OpRow['tag'], text: string): OpRow => ({
		id: item.id,
		tag,
		text,
		time,
		color: opTagColor(tag)
	});

	switch (item.kind) {
		case 'RoundStarted': {
			const round = (ev.RoundStarted as { round: number }).round;
			return mk('round', `round ${round} · master 推进`);
		}
		case 'ModelMessage': {
			const text = (ev.ModelMessage as { text: string }).text;
			return mk('master', truncate(text, 80));
		}
		case 'ToolCalled': {
			const name = (ev.ToolCalled as { name: string }).name;
			return mk('tool', `${name} · 调用`);
		}
		case 'ToolResult': {
			const r = ev.ToolResult as { name: string; ok: boolean; summary: string };
			return mk('tool', `${r.name} · ${r.ok ? '完成' : '失败'}${r.summary ? ' · ' + truncate(r.summary, 60) : ''}`);
		}
		case 'EditCommitted': {
			const c = ev.EditCommitted as { article: string; author: string; sha: string };
			return mk('commit', `${c.article} · 提交 ${c.sha}`);
		}
		case 'SlaveSpawned': {
			const s = ev.SlaveSpawned as { file: string; writer: string };
			return mk('slave', `writer 启动 · ${s.writer} → ${s.file}`);
		}
		case 'SlaveReported': {
			const s = ev.SlaveReported as { status: string; summary: string };
			return mk('slave', `report ${s.status} · ${truncate(s.summary, 60)}`);
		}
		case 'Finished': {
			const f = ev.Finished as { outcome: string };
			return mk('done', `编排结束 · ${f.outcome}`);
		}
		default:
			// Txn* (B3) + SessionStarted are not topic-level operations.
			return null;
	}
}

function truncate(s: string, n: number): string {
	const t = (s ?? '').trim();
	return t.length > n ? t.slice(0, n - 1) + '…' : t;
}

/** A master-dialogue turn: the human goal, then the master plan reply. */
export interface DialogueTurn {
	/** Human goal text (the topic-level goal that was dispatched). */
	goal: string;
	/** Master's planning prose (from the chat result), if any. */
	masterMessage: string;
	/** The structured product rows produced by this turn. */
	rows: PlanRow[];
	/** Outcome string from the chat result (drives a subtle status line). */
	outcome: string | null;
}

/** Build the master reply turn from a chat result + the current article rows. */
export function turnFromChat(
	goal: string,
	articles: ArticleRow[],
	res: ChatResponse | null
): DialogueTurn {
	return {
		goal,
		masterMessage: res?.message ?? '',
		rows: buildPlanRows(articles, res?.plan?.dispatched ?? []),
		outcome: res?.outcome ?? null
	};
}
