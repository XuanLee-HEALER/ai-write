/**
 * Live coordinator transaction state for one article (F4).
 *
 * The coordinator serializes every edit through a single non-preemptive,
 * head-of-queue transaction (ui-ux-design §7.3 / §11·3): when an AI writer
 * holds the edit txn the canvas is read-only; a human who wants to edit
 * *requests* the txn, which inserts them at the head of the queue **without
 * interrupting the AI's current commit**. When that commit lands, the
 * coordinator hands off (`HandoffToHuman`) and the canvas becomes editable.
 *
 * This module is the pure reducer over the B3 SSE feed (`TxnAcquired`,
 * `TxnQueued`, `TxnReleased`, `HandoffToHuman`) plus the two local intents the
 * UI raises optimistically (request / cancel). The component owns the reactive
 * `$state` and the `request-edit` API calls; this stays framework-free and
 * unit-testable.
 *
 * Semantics, per the contract:
 * - Only events whose path/file targets *this* article move its state.
 * - A human writer holding the txn is "your-turn", not "ai-busy" (the canvas is
 *   theirs). An AI writer holding it is "ai-busy" (read-only canvas).
 * - `queued` is reachable only locally (the human pressed "request edit"); the
 *   `TxnQueued` event with a human writer reaffirms it. It is cancelable.
 */
import type { FeedItemLike } from '$lib/api/types';
import { authorColor, type AuthorStyle } from '$lib/author';

/** The four coordinator txn states surfaced to the article view. */
export type TxnState = 'idle' | 'ai-busy' | 'queued' | 'your-turn';

/** Derived view of the coordinator transaction for the current article. */
export interface TxnView {
	/** Coarse state driving banners + the COORDINATOR block. */
	state: TxnState;
	/** The current txn holder's author tag, or `null` when no one holds it. */
	holder: string | null;
	/** Resolved style for the holder (dot + label color). */
	holderStyle: AuthorStyle | null;
	/** Holder display label (already resolves `human` → 你). */
	holderLabel: string;
	/** True while an AI writer holds the txn → canvas must be read-only. */
	readOnly: boolean;
	/** True once the txn is the human's → canvas editable, "your turn" banner. */
	yourTurn: boolean;
	/** True while the human's request is queued behind the AI commit (cancelable). */
	queued: boolean;
	/** Number of writers ahead of the human in the queue (0 when next). */
	ahead: number;
}

/** Is this author tag the human collaborator? */
function isHuman(tag: string): boolean {
	return authorColor(tag).key === 'you';
}

/** Display label for a holder tag (`human` → 你, else the model label). */
function holderLabelOf(tag: string | null): string {
	if (!tag) return '—';
	const style = authorColor(tag);
	return style.key === 'you' ? '你' : style.label;
}

/** Strip a possible `theme/` prefix so `theme/file` and `file` both match. */
function targetsFile(ref: string | undefined, file: string): boolean {
	if (!ref) return false;
	return ref === file || ref.split('/').pop() === file;
}

function eventOf(item: FeedItemLike): Record<string, unknown> {
	return item.event as unknown as Record<string, unknown>;
}

/**
 * Fold the SSE feed into the coordinator txn state for `file`.
 *
 * `feed` is newest-first (as the connection store keeps it); we replay it
 * oldest-first so the latest event wins. `local` carries the two optimistic
 * intents the UI raises before the matching server event arrives:
 * - `requested`: the human pressed "request edit" → show `queued` even before
 *   `TxnQueued` echoes back.
 * - `canceled`: the human canceled the queue → suppress a stale `TxnQueued`.
 *
 * Both intents are cleared by the component once a terminal event (handoff /
 * release) is observed, so they never mask fresh server truth.
 */
export function deriveTxn(
	feed: readonly FeedItemLike[],
	file: string,
	local: { requested: boolean; canceled: boolean } = { requested: false, canceled: false }
): TxnView {
	let holder: string | null = null;
	let queuedHuman = false;
	let ahead = 0;
	let handedOff = false;

	// Replay oldest-first; the feed is stored newest-first.
	for (let i = feed.length - 1; i >= 0; i--) {
		const ev = eventOf(feed[i]);
		const kind = feed[i].kind;
		if (kind === 'TxnAcquired') {
			const t = ev.TxnAcquired as { writer: string; paths: string[] };
			if (t.paths?.some((p) => targetsFile(p, file))) {
				holder = t.writer;
				handedOff = false;
				// Acquiring clears any prior queue intent — the holder changed.
				if (isHuman(t.writer)) {
					queuedHuman = false;
					ahead = 0;
				}
			}
		} else if (kind === 'TxnQueued') {
			const t = ev.TxnQueued as { writer: string; ahead: number };
			// The contract attaches no file to TxnQueued; only the human queue is
			// surfaced per-article (a human can only be queued on what they opened).
			if (isHuman(t.writer)) {
				queuedHuman = true;
				ahead = t.ahead ?? 0;
			}
		} else if (kind === 'TxnReleased') {
			const t = ev.TxnReleased as { writer: string };
			if (holder !== null && t.writer === holder) holder = null;
		} else if (kind === 'HandoffToHuman') {
			const t = ev.HandoffToHuman as { theme: string; file: string };
			if (targetsFile(t.file, file)) {
				holder = 'human';
				queuedHuman = false;
				ahead = 0;
				handedOff = true;
			}
		}
	}

	// Layer the local optimistic intents on top of the replayed server truth.
	if (local.canceled) {
		queuedHuman = false;
		ahead = 0;
	} else if (local.requested && holder !== null && !isHuman(holder) && !handedOff) {
		queuedHuman = true;
	}

	const holderIsHuman = holder !== null && isHuman(holder);
	const holderIsAi = holder !== null && !holderIsHuman;

	let state: TxnState;
	if (holderIsHuman) state = 'your-turn';
	else if (holderIsAi && queuedHuman) state = 'queued';
	else if (holderIsAi) state = 'ai-busy';
	else state = 'idle';

	return {
		state,
		holder,
		holderStyle: holder ? authorColor(holder) : null,
		holderLabel: holderLabelOf(holder),
		readOnly: holderIsAi,
		yourTurn: holderIsHuman,
		queued: state === 'queued',
		ahead
	};
}
