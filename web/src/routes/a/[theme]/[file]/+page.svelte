<script lang="ts">
	import { onMount, tick, untrack } from 'svelte';
	import { workspace } from '$lib/stores/workspace.svelte';
	import { layout } from '$lib/stores/layout.svelte';
	import { connection } from '$lib/stores/connection.svelte';
	import { toast } from '$lib/stores/toast';
	import { createApi, ApiRequestError } from '$lib/api/client';
	import { authorColor } from '$lib/author';
	import {
		commandFromFeed,
		buildVersionRows,
		buildBlameRows,
		buildContributionBars,
		buildStyledParas,
		buildLegendChips,
		toggleTick,
		diffPair,
		parseUnifiedDiff,
		toSideBySide,
		diffStat,
		modelId,
		type AuthorDisplay,
		type CommandRow,
		type DiffLine,
		type SideRow
	} from '$lib/article';
	import { deriveTxn } from '$lib/txn';
	import * as Tabs from '$lib/components/ui/tabs/index.js';
	import type { PageData } from './$types';

	// Article authoring view (EDITOR + INSPECTOR of AI-Write.dc.html, ui-ux-design
	// §6.3). Desktop: editor canvas (view/edit, format toolbar, AI-edit/diff,
	// three/merged layout) + a two-column Inspector (Activity command stream,
	// Versions timeline/diff/blame/signature). Mobile collapses the Inspector into
	// a segmented [正文 · 活动 · 版本].
	//
	// Authorship body coloring is F3; live coordinator txn banners + the
	// COORDINATOR block in the Activity inspector are F4 (the busy/queued/your-turn
	// state machine derived from the B3 SSE feed, plus request-edit / cancel).
	let { data }: { data: PageData } = $props();

	const api = createApi();

	// Sync the loaded article into the Navigator store. The store mutations read +
	// write the store's own `topics` state, so they are untracked — this effect
	// depends only on the load data, not on the state it writes (which would loop).
	$effect(() => {
		const theme = data.theme;
		const articles = data.articles;
		const file = data.file;
		untrack(() => {
			workspace.setArticles(theme, articles);
			workspace.syncSelection({ kind: 'article', theme, file });
		});
	});

	const currentFile = $derived(data.file);
	const title = $derived(data.articles.find((a) => a.file === data.file)?.title ?? data.file);

	/* ---- editor mode + layout ---------------------------------------------- */

	// WYSIWYG, two modes only (decision §11·1): no third state.
	let mode = $state<'view' | 'edit'>('view');
	// Desktop inspector layout (three narrow columns vs one merged tabbed pane).
	let layoutMode = $state<'three' | 'merged'>('three');
	let mergedTab = $state<'versions' | 'activity'>('versions');
	// Mobile segmented Inspector destinations.
	let seg = $state<'body' | 'activity' | 'versions'>('body');

	/* ---- editable body ------------------------------------------------------ */

	// The canvas binds to a working copy of the content. The *displayed* committed
	// text is `data.content` (so SSR renders the real body) unless a local save has
	// produced newer text for this exact file — tracked by `savedOverride`. The
	// editable `draft` is seeded from that committed text in an effect.
	let saving = $state(false);
	let savedOverride = $state<{ file: string; text: string } | null>(null);
	const savedContent = $derived(
		savedOverride && savedOverride.file === data.file ? savedOverride.text : data.content
	);
	let draft = $state('');
	$effect(() => {
		data.file; // dependency: reset working copy on article switch
		draft = savedContent;
		mode = 'view';
	});
	const dirty = $derived(mode === 'edit' && draft !== savedContent);

	let editorEl = $state<HTMLDivElement | null>(null);

	async function enterEdit() {
		mode = 'edit';
		await tick();
		editorEl?.focus();
	}

	function cancelEdit() {
		draft = savedContent;
		mode = 'view';
	}

	async function save() {
		if (saving) return;
		const text = (editorEl?.innerText ?? draft).trim();
		saving = true;
		try {
			const res = await api.putArticle(data.theme, data.file, text);
			savedOverride = { file: data.file, text };
			draft = text;
			mode = 'view';
			toast.show(res.committed ? `已提交 · ${res.committed}` : '已保存(无改动)');
		} catch (e) {
			toast.error(e instanceof ApiRequestError ? e.message : '保存失败');
		} finally {
			saving = false;
		}
	}

	/** Rich-text format command (B/I/headings/quote/undo/redo) — edit mode only. */
	function fmt(cmd: string, value?: string) {
		if (mode !== 'edit') return;
		editorEl?.focus();
		// document.execCommand is deprecated but remains the pragmatic WYSIWYG
		// primitive for a contenteditable canvas; the eventual editor swap is a
		// seam behind this one call site.
		document.execCommand(cmd, false, value);
		draft = editorEl?.innerHTML ?? draft;
	}

	/* ---- authorship coloring (F3) ------------------------------------------ */

	// Author runs (B2 `?format=rich`); null when the backend can't supply them.
	const rich = $derived(data.rich);
	const hasRich = $derived(!!rich && rich.blocks.length > 0);

	// DEFAULT OFF (decision §11·2); only meaningful + toggleable in view mode.
	let authorship = $state(false);
	let authorDisplay = $state<AuthorDisplay>('color');
	// Authorship is shown only while reading with the toggle on and runs present.
	const showColored = $derived(authorship && mode === 'view' && hasRich);
	// Toolbar toggle is enabled only while reading with runs available.
	const authEnabled = $derived(mode === 'view' && hasRich);
	const styledParas = $derived(
		rich ? buildStyledParas(rich.blocks, authorDisplay) : []
	);
	const legendChips = $derived(buildLegendChips(rich?.authors, authorDisplay));

	// Entering edit mode disables coloring (§11·2: edit mode is never colored).
	$effect(() => {
		if (mode === 'edit' && authorship) authorship = false;
	});
	// Reset the toggle when switching articles (rich data is per-article).
	$effect(() => {
		data.file;
		authorship = false;
	});

	function toggleAuthorship() {
		if (mode !== 'view' || !hasRich) return;
		authorship = !authorship;
	}

	const modeMeta = $derived(
		mode === 'edit' ? 'edit · 编辑事务在你' : showColored ? 'view · 作者着色 on' : 'view'
	);

	/* ---- activity: command stream filtered to this article ------------------ */

	let commands = $state<CommandRow[]>([]);
	// Reset + backfill the stream whenever the article changes; the live
	// subscription reads the current file via this closure, so it stays correct
	// across client navigations (the component is reused on the same route).
	$effect(() => {
		const file = data.file;
		commands = connection.feed
			.map((it) => commandFromFeed(it, file))
			.filter((c): c is CommandRow => c !== null)
			.slice(0, 80);
	});
	onMount(() => {
		connection.connect();
		return connection.on((item) => {
			const row = commandFromFeed(item, currentFile, { fresh: true });
			if (row) commands = [row, ...commands].slice(0, 80);
		});
	});

	/* ---- versions: timeline + diff ------------------------------------------ */

	let timelineStyle = $state<'rail' | 'list'>('rail');
	let ticked = $state<string[]>([]);
	const versionRows = $derived(buildVersionRows(data.history, new Set(ticked)));

	function tick2(id: string) {
		ticked = toggleTick(ticked, id);
	}

	// Diff is fetched lazily when exactly two versions are ticked.
	let diffMode = $state<'side' | 'inline'>('side');
	let diffLines = $state<DiffLine[]>([]);
	let diffSide = $state<SideRow[]>([]);
	let diffLoading = $state(false);
	let diffError = $state<string | null>(null);
	const pair = $derived(diffPair(ticked, data.history));
	const stat = $derived(diffStat(diffLines));

	$effect(() => {
		const p = pair;
		if (!p) {
			diffLines = [];
			diffSide = [];
			diffError = null;
			return;
		}
		let cancelled = false;
		diffLoading = true;
		diffError = null;
		api
			.diff(data.theme, data.file, p.from, p.to)
			.then((res) => {
				if (cancelled) return;
				const lines = parseUnifiedDiff(res.diff);
				diffLines = lines;
				diffSide = toSideBySide(lines);
			})
			.catch((e) => {
				if (cancelled) return;
				diffError = e instanceof ApiRequestError ? e.message : 'diff 加载失败';
				diffLines = [];
				diffSide = [];
			})
			.finally(() => {
				if (!cancelled) diffLoading = false;
			});
		return () => {
			cancelled = true;
		};
	});

	/* ---- blame + signature -------------------------------------------------- */

	const blameRows = $derived(buildBlameRows(data.blame));
	const contributionBars = $derived(buildContributionBars(data.contributions));
	let showBlame = $state(false);

	/* ---- AI-edit dispatch (Topic-面 orchestration seam) --------------------- */

	function requestAiEdit() {
		// "请 AI 编辑本段" dispatches to the Topic-side orchestration flow (separate
		// from the human edit-txn request below); that wiring lands with the chat
		// surface. Surfaces intent without a side effect on the article for now.
		toast.info('「请 AI 编辑本段」将在编排面接入。');
	}

	/* ---- F4: live coordinator transaction ----------------------------------- */

	// Optimistic local intents layered on top of the replayed SSE feed: `requested`
	// shows `queued` the instant the human presses request-edit (before TxnQueued
	// echoes); `canceled` suppresses a stale queue until release/handoff. Both are
	// cleared when txn state actually moves, so server truth always wins.
	let txnRequested = $state(false);
	let txnCanceled = $state(false);

	// connection.feed is reactive; this re-derives on every incoming Txn* event.
	const txn = $derived(deriveTxn(connection.feed, data.file, {
		requested: txnRequested,
		canceled: txnCanceled
	}));

	// Reset optimistic intents on article switch and whenever the holder hands off
	// to the human (your-turn) or no AI holds the txn (idle) — the intent is spent.
	$effect(() => {
		data.file;
		txnRequested = false;
		txnCanceled = false;
	});
	$effect(() => {
		if (txn.state === 'your-turn' || txn.state === 'idle') {
			txnRequested = false;
			txnCanceled = false;
		}
	});

	// The AI holds the edit txn → the canvas is read-only (§7.3). Force view mode
	// so the editable contenteditable can never be reached while busy.
	const canvasLocked = $derived(txn.readOnly);
	$effect(() => {
		if (canvasLocked && mode === 'edit') mode = 'view';
	});

	// Request the edit txn: insert at the head of the queue without interrupting
	// the AI's current commit (non-preemptive, §11·3). Optimistically queued.
	async function requestEdit() {
		if (txn.state !== 'ai-busy') return;
		txnCanceled = false;
		txnRequested = true;
		try {
			const res = await api.requestEdit(data.theme, data.file);
			toast.show(
				res.ahead > 0
					? `已插入队首 · 前面还有 ${res.ahead} 个提交`
					: '已插入队首 · 当前 AI 提交后轮到你'
			);
		} catch (e) {
			txnRequested = false;
			toast.error(e instanceof ApiRequestError ? e.message : '请求编辑失败');
		}
	}

	// Cancel the queued request — allowed to reverse (§11·3). Optimistically idle.
	async function cancelQueue() {
		if (txn.state !== 'queued') return;
		txnRequested = false;
		txnCanceled = true;
		try {
			await api.cancelRequestEdit(data.theme, data.file);
			toast.show('已取消排队');
		} catch (e) {
			txnCanceled = false;
			toast.error(e instanceof ApiRequestError ? e.message : '取消失败');
		}
	}
</script>

<div class="flex min-h-0 min-w-0 flex-1">
	{#if layout.inspectorSegmented}
		<!-- mobile: segmented [正文 · 活动 · 版本] -->
		<section class="flex min-h-0 min-w-0 flex-1 flex-col">
			<Tabs.Root bind:value={seg} class="flex min-h-0 flex-1 flex-col gap-0">
				<Tabs.List
					class="bg-cream border-rule-soft flex-none justify-start rounded-none border-b px-2"
				>
					<Tabs.Trigger value="body">正文</Tabs.Trigger>
					<Tabs.Trigger value="activity">活动</Tabs.Trigger>
					<Tabs.Trigger value="versions">版本</Tabs.Trigger>
				</Tabs.List>
				<Tabs.Content value="body" class="m-0 flex min-h-0 flex-1 flex-col overflow-hidden">
					{@render toolbar()}
					{@render txnBanner()}
					<div data-scroll class="min-h-0 flex-1 overflow-auto">{@render canvas()}</div>
				</Tabs.Content>
				<Tabs.Content value="activity" class="m-0 min-h-0 flex-1 overflow-auto">
					{@render activity()}
				</Tabs.Content>
				<Tabs.Content value="versions" class="m-0 min-h-0 flex-1 overflow-auto">
					{@render versions()}
				</Tabs.Content>
			</Tabs.Root>
		</section>
	{:else}
		<!-- desktop: 正文 + Inspector (three columns or one merged tabbed pane) -->
		<section class="bg-paper flex min-h-0 min-w-0 flex-1 flex-col">
			{@render toolbar()}
			{@render txnBanner()}
			<div data-scroll class="min-h-0 flex-1 overflow-auto">{@render canvas()}</div>
		</section>

		{#if layoutMode === 'three'}
			<aside
				data-scroll
				class="border-rule bg-cream hidden min-h-0 w-[296px] flex-none overflow-auto border-l xl:block"
				aria-label="协作活动 · 本文"
			>
				{@render activity()}
			</aside>
			<aside
				data-scroll
				class="border-rule bg-cream hidden min-h-0 w-[320px] flex-none overflow-auto border-l lg:block"
				aria-label="版本 · 署名"
			>
				{@render versions()}
			</aside>
		{:else}
			<!-- merged inspector: one pane, version/activity tabs -->
			<aside
				class="border-rule bg-cream hidden min-h-0 w-[358px] flex-none flex-col border-l lg:flex"
				aria-label="检视区"
			>
				<div class="border-rule flex flex-none border-b">
					<button
						type="button"
						class="font-disp h-[42px] flex-1 text-[12px] tracking-[.04em]"
						class:bg-cream={mergedTab === 'versions'}
						class:text-ink={mergedTab === 'versions'}
						class:bg-cream2={mergedTab !== 'versions'}
						class:text-ink-faint={mergedTab !== 'versions'}
						style:box-shadow={mergedTab === 'versions' ? 'inset 0 -2px 0 var(--color-accent)' : 'none'}
						onclick={() => (mergedTab = 'versions')}>版本 · 署名</button
					>
					<button
						type="button"
						class="font-disp h-[42px] flex-1 text-[12px] tracking-[.04em]"
						class:bg-cream={mergedTab === 'activity'}
						class:text-ink={mergedTab === 'activity'}
						class:bg-cream2={mergedTab !== 'activity'}
						class:text-ink-faint={mergedTab !== 'activity'}
						style:box-shadow={mergedTab === 'activity' ? 'inset 0 -2px 0 var(--color-accent)' : 'none'}
						onclick={() => (mergedTab = 'activity')}>活动流</button
					>
				</div>
				<div data-scroll class="min-h-0 flex-1 overflow-auto">
					{#if mergedTab === 'activity'}
						{@render activity()}
					{:else}
						{@render versions()}
					{/if}
				</div>
			</aside>
		{/if}
	{/if}
</div>

<!-- ---- snippets ---------------------------------------------------------- -->

{#snippet toolbar()}
	<div
		class="border-rule-soft bg-paper flex flex-none flex-wrap items-center gap-[12px] border-b px-[16px] py-[9px] sm:px-[22px]"
	>
		<!-- view / edit segmented -->
		<div class="border-rule inline-flex overflow-hidden rounded-[4px] border">
			<button
				type="button"
				class="h-[30px] px-[14px] font-serif text-[13px]"
				class:bg-ink={mode === 'view'}
				class:text-paper={mode === 'view'}
				class:bg-card={mode !== 'view'}
				class:text-ink-soft={mode !== 'view'}
				onclick={() => (mode === 'edit' ? cancelEdit() : (mode = 'view'))}>阅读</button
			>
			<button
				type="button"
				disabled={canvasLocked}
				title={canvasLocked ? 'AI 持有编辑事务 · 正文暂为只读' : undefined}
				class="h-[30px] px-[14px] font-serif text-[13px]"
				class:bg-ink={mode === 'edit'}
				class:text-paper={mode === 'edit'}
				class:bg-card={mode !== 'edit'}
				class:text-ink-soft={mode !== 'edit' && !canvasLocked}
				class:text-ink-ghost={canvasLocked}
				class:cursor-not-allowed={canvasLocked}
				class:opacity-50={canvasLocked}
				onclick={enterEdit}>编辑</button
			>
		</div>

		<!-- authorship coloring toggle — default OFF, view-only (§11·2). Disabled in
		     edit mode, and when no author runs are available. -->
		<button
			type="button"
			aria-pressed={showColored}
			disabled={!authEnabled}
			onclick={toggleAuthorship}
			title={mode !== 'view'
				? '编辑模式不着色(§11·2)'
				: hasRich
					? '正文按作者游程着色(仅阅读模式可开)'
					: '暂无作者游程数据'}
			class="inline-flex h-[30px] items-center gap-[7px] rounded-[4px] border px-[12px] font-serif text-[13px] transition-colors"
			class:border-accent={showColored}
			class:text-accent={showColored}
			class:bg-accent-tint={showColored}
			class:border-rule={!showColored}
			class:bg-card={!showColored}
			class:text-ink-soft={!showColored && authEnabled}
			class:text-ink-ghost={!authEnabled}
			class:cursor-not-allowed={!authEnabled}
			class:opacity-50={!authEnabled}
			style:background-color={showColored ? 'var(--accent-tint)' : undefined}
		>
			<span
				class="h-[10px] w-[14px] rounded-[2px]"
				class:bg-rule-soft={!showColored}
				style:background={showColored
					? 'linear-gradient(90deg, var(--color-author-you) 0 33%, var(--color-author-chat) 33% 66%, var(--color-author-reasoner) 66%)'
					: undefined}
			></span>作者着色
		</button>

		<!-- display-mode segmented: color / texture / label (only when coloring on) -->
		{#if showColored}
			<div class="border-rule inline-flex overflow-hidden rounded-[4px] border">
				{@render adSeg('color', '色')}
				{@render adSeg('texture', '纹')}
				{@render adSeg('label', '签')}
			</div>
		{/if}

		<div class="bg-rule-soft h-[20px] w-px"></div>

		<!-- format group: active only in edit -->
		<div
			class="flex items-center gap-[2px]"
			class:opacity-40={mode !== 'edit'}
			class:pointer-events-none={mode !== 'edit'}
		>
			{@render fmtBtn('粗体', () => fmt('bold'), 'font-bold', 'B')}
			{@render fmtBtn('斜体', () => fmt('italic'), 'italic', 'I')}
			{@render fmtBtn('标题', () => fmt('formatBlock', 'H2'), 'font-disp', 'H₂')}
			{@render fmtBtn('引用', () => fmt('formatBlock', 'BLOCKQUOTE'), 'font-disp', '„')}
			<span class="bg-rule-soft mx-[4px] h-[18px] w-px"></span>
			{@render fmtBtn('撤销', () => fmt('undo'), '', '↶')}
			{@render fmtBtn('重做', () => fmt('redo'), '', '↷')}
		</div>

		<div class="flex-1"></div>

		{#if mode === 'edit'}
			<button
				type="button"
				onclick={cancelEdit}
				class="border-rule bg-card text-ink-soft hover:bg-cream2 h-[30px] rounded-[4px] border px-[13px] font-serif text-[13px]"
				>取消</button
			>
			<button
				type="button"
				onclick={save}
				disabled={saving || !dirty}
				class="bg-accent h-[30px] rounded-[4px] px-[13px] font-serif text-[13px] text-white transition-opacity disabled:cursor-not-allowed disabled:opacity-45"
				>{saving ? '提交中…' : '保存(提交)'}</button
			>
		{:else}
			<button
				type="button"
				onclick={requestAiEdit}
				class="border-rule bg-card text-accent hover:bg-cream2 h-[30px] rounded-[4px] border px-[13px] font-serif text-[13px]"
				>请 AI 编辑本段</button
			>
			<button
				type="button"
				onclick={() => {
					if (layout.inspectorSegmented) seg = 'versions';
					else if (layoutMode === 'merged') mergedTab = 'versions';
				}}
				class="border-rule bg-card text-ink-soft hover:bg-cream2 h-[30px] rounded-[4px] border px-[13px] font-serif text-[13px]"
				>diff</button
			>
		{/if}

		<!-- layout toggle (three / merged) — desktop only -->
		<div class="hidden items-center gap-[7px] pl-[4px] lg:inline-flex">
			<span class="text-ink-ghost font-mono text-[10px] tracking-[.1em]">布局</span>
			<div class="border-rule inline-flex overflow-hidden rounded-[4px] border">
				<button
					type="button"
					class="h-[26px] px-[11px] font-serif text-[12px]"
					class:bg-ink={layoutMode === 'three'}
					class:text-paper={layoutMode === 'three'}
					class:bg-card={layoutMode !== 'three'}
					class:text-ink-soft={layoutMode !== 'three'}
					onclick={() => (layoutMode = 'three')}>三栏</button
				>
				<button
					type="button"
					class="h-[26px] px-[11px] font-serif text-[12px]"
					class:bg-ink={layoutMode === 'merged'}
					class:text-paper={layoutMode === 'merged'}
					class:bg-card={layoutMode !== 'merged'}
					class:text-ink-soft={layoutMode !== 'merged'}
					onclick={() => (layoutMode = 'merged')}>合并</button
				>
			</div>
		</div>
	</div>
{/snippet}

{#snippet txnBanner()}
	<!-- F4 live txn banners (B3 events). busy: AI holds the edit txn, canvas
	     read-only, request-edit head-of-queue. your-turn: handoff landed, canvas
	     editable. Non-preemptive + cancelable (§11·3). -->
	{#if txn.state === 'ai-busy' || txn.state === 'queued'}
		<div
			class="flex flex-none items-center gap-[11px] border-b px-[16px] py-[9px] text-[13px] sm:px-[22px]"
			style:background="oklch(0.96 0.02 62)"
			style:border-color="oklch(0.88 0.04 62)"
			style:color="oklch(0.4 0.06 50)"
			aria-live="polite"
		>
			<span
				class="h-[8px] w-[8px] flex-none rounded-full"
				style:background="oklch(0.55 0.1 62)"
				style:animation="softpulse 1.6s ease-in-out infinite"
			></span>
			{#if txn.state === 'queued'}
				<span
					><strong class="font-semibold">{txn.holderLabel}</strong> 持有编辑事务 · 你已在队首{txn.ahead
						> 0
						? ` · 前面 ${txn.ahead} 个提交`
						: ' · 当前提交后轮到你'}</span
				>
				<span class="flex-1"></span>
				<button
					type="button"
					onclick={cancelQueue}
					class="bg-card h-[27px] rounded-[4px] border px-[12px] font-serif text-[12px]"
					style:border-color="oklch(0.78 0.05 62)"
					style:color="oklch(0.38 0.07 50)">取消排队</button
				>
			{:else}
				<span><strong class="font-semibold">{txn.holderLabel}</strong> 持有编辑事务 · 正文暂为只读</span>
				<span class="flex-1"></span>
				<button
					type="button"
					onclick={requestEdit}
					class="bg-card h-[27px] rounded-[4px] border px-[12px] font-serif text-[12px]"
					style:border-color="oklch(0.78 0.05 62)"
					style:color="oklch(0.38 0.07 50)">请求编辑(插队首)</button
				>
			{/if}
		</div>
	{:else if txn.state === 'your-turn'}
		<div
			class="flex flex-none items-center gap-[11px] border-b px-[16px] py-[9px] text-[13px] sm:px-[22px]"
			style:background="oklch(0.96 0.025 150)"
			style:border-color="oklch(0.86 0.05 150)"
			style:color="oklch(0.36 0.06 150)"
			aria-live="polite"
		>
			<span class="h-[8px] w-[8px] flex-none rounded-full" style:background="oklch(0.55 0.1 150)"></span>
			<span><strong class="font-semibold">轮到你了</strong> · 编辑事务已交给你,正文进入可编辑态</span>
		</div>
	{/if}
{/snippet}

{#snippet fmtBtn(label: string, run: () => void, extra: string, glyph: string)}
	<button
		type="button"
		aria-label={label}
		title={label}
		onclick={run}
		class="border-rule bg-card text-ink-soft hover:bg-cream2 grid h-[30px] w-[30px] place-items-center rounded-[4px] border font-serif text-[14px] {extra}"
		>{glyph}</button
	>
{/snippet}

{#snippet adSeg(value: AuthorDisplay, glyph: string)}
	<button
		type="button"
		aria-pressed={authorDisplay === value}
		onclick={() => (authorDisplay = value)}
		class="h-[28px] px-[10px] font-serif text-[12px]"
		class:bg-ink={authorDisplay === value}
		class:text-paper={authorDisplay === value}
		class:bg-card={authorDisplay !== value}
		class:text-ink-soft={authorDisplay !== value}>{glyph}</button
	>
{/snippet}

{#snippet canvas()}
	<article class="mx-auto max-w-[680px] px-[16px] pt-[40px] pb-[80px] sm:px-[22px]">
		<div class="font-disp text-ink-ghost mb-[14px] text-[11px] tracking-[.24em] uppercase">
			{data.theme}
		</div>
		<h1 class="font-disp m-0 mb-[12px] text-[26px] leading-[1.25] font-semibold sm:text-[30px]">
			{title}
		</h1>
		<div
			class="border-rule-soft text-ink-faint mb-[8px] flex items-center gap-[14px] border-b pb-[18px] font-mono text-[11.5px]"
		>
			<span>HEAD{data.history[0] ? ' · ' + data.history[0].id.slice(0, 8) : ''}</span>
			<span>·</span><span>{modeMeta}</span><span>·</span><span>measure 68ch</span>
		</div>

		{#if showColored}
			<!-- authorship-colored body: each run colored by author (B2 runs) -->
			<div class="text-ink mt-[18px] text-[17px] leading-[1.95]">
				{#each styledParas as para, pi (pi)}
					<p
						class="text-justify"
						style:margin={pi === styledParas.length - 1 ? '0' : '0 0 1.15rem'}
						style:text-wrap="pretty"
					>
						{#each para.runs as run, ri (ri)}<span style={run.css}
								>{run.text}{#if run.showLabel}<sub
										class="font-mono align-super text-[8px] font-semibold tracking-[.02em]"
										style:color={run.style.color}
										style:margin-left="1px">{run.short}</sub
									>{/if}</span
							>{/each}
					</p>
				{/each}
			</div>
			{@render legendRow()}
		{:else if data.contentError}
			<p class="text-ink-faint mt-[18px] text-[14px] italic">无法加载正文:{data.contentError}</p>
		{:else if mode === 'edit'}
			<div
				bind:this={editorEl}
				contenteditable="true"
				role="textbox"
				tabindex="0"
				aria-multiline="true"
				aria-label="正文编辑器"
				oninput={() => (draft = editorEl?.innerHTML ?? draft)}
				class="border-rule bg-card text-ink mt-[18px] rounded-[4px] border px-[20px] py-[18px] text-[17px] leading-[1.95] whitespace-pre-wrap shadow-[0_1px_0_var(--color-rule-soft)] focus:outline-none"
			>
				{savedContent}
			</div>
		{:else if savedContent.trim()}
			<div class="text-ink mt-[18px] text-[17px] leading-[1.95] whitespace-pre-wrap">
				{savedContent}
			</div>
		{:else}
			<p class="text-ink-ghost mt-[18px] text-[14px] italic">
				这篇文章还没有正文。切到「编辑」开始写。
			</p>
		{/if}
	</article>
{/snippet}

{#snippet legendRow()}
	<!-- authorship legend: one chip per author present (color/texture/label) -->
	<div class="border-rule-soft mt-[22px] flex flex-wrap items-center gap-x-[16px] gap-y-[7px] border-t pt-[14px]">
		<span class="text-ink-ghost font-mono text-[10px] tracking-[.1em] uppercase">图例</span>
		{#each legendChips as chip (chip.key)}
			<span class="text-ink-soft inline-flex items-center gap-[6px] font-mono text-[11px]">
				<span style={chip.swatchCss}></span>{chip.label}
			</span>
		{/each}
	</div>
{/snippet}

{#snippet activity()}
	<div class="px-[16px] pt-[14px] pb-[10px]">
		<span class="font-disp text-ink-faint text-[11px] tracking-[.22em] uppercase"
			>协作活动 · 本文</span
		>
	</div>

	<!-- F4 coordinator transaction block (B3 events): holder row + queue row
	     (cancel) / your-turn note / request-edit. Matches AI-Write.dc.html txnBlock. -->
	<div class="px-[16px]">
		<div class="border-rule bg-card overflow-hidden rounded-[6px] border">
			<div class="bg-cream2 text-ink-faint px-[11px] py-[7px] font-mono text-[10px] tracking-[.08em]">
				COORDINATOR · 事务
			</div>

			<!-- holder row -->
			<div class="flex items-center gap-[8px] px-[11px] py-[9px]">
				<span
					class="h-[8px] w-[8px] flex-none rounded-full"
					style:background={txn.state === 'your-turn' ? 'oklch(0.55 0.1 150)' : 'oklch(0.55 0.1 62)'}
					style:animation={txn.state === 'ai-busy' || txn.state === 'queued'
						? 'softpulse 1.6s ease-in-out infinite'
						: 'none'}
				></span>
				<span class="text-ink-soft text-[12.5px]">
					{#if txn.state === 'idle'}
						持锁:<span class="text-ink-faint">空闲 · 无人持事务</span>
					{:else if txn.state === 'your-turn'}
						持锁:<strong class="font-semibold" style:color="oklch(0.36 0.06 150)"> 你</strong>
					{:else}
						持锁:<strong class="font-semibold" style:color={txn.holderStyle?.color}>
							{txn.holderLabel}</strong
						>
					{/if}
				</span>
				<span class="text-ink-ghost ml-auto font-mono text-[10px]">
					{txn.state === 'your-turn'
						? 'editing'
						: txn.state === 'idle'
							? 'idle'
							: 'committing'}
				</span>
			</div>

			<!-- queued row (cancelable) -->
			{#if txn.state === 'queued'}
				<div
					class="border-rule-soft flex items-center gap-[8px] border-t px-[11px] py-[9px]"
					style:background="oklch(0.97 0.012 255)"
				>
					<span
						class="h-[7px] w-[7px] flex-none rounded-full"
						style:border="1.5px solid oklch(0.44 0.075 255)"
					></span>
					<span class="text-[12.5px]" style:color="oklch(0.4 0.06 255)"
						>你 · 排队中 · 当前提交后轮到你</span
					>
					<button
						type="button"
						onclick={cancelQueue}
						class="border-rule bg-card text-ink-soft hover:bg-cream2 ml-auto h-[24px] rounded-[4px] border px-[9px] font-serif text-[11px]"
						>取消</button
					>
				</div>
			{/if}

			<!-- your-turn note -->
			{#if txn.state === 'your-turn'}
				<div
					class="border-rule-soft border-t px-[11px] py-[8px] text-[11.5px]"
					style:color="oklch(0.4 0.05 150)"
				>
					队列空 · 编辑权在你,落定即一次 commit
				</div>
			{/if}

			<!-- request-edit (head-of-queue, non-preemptive) -->
			{#if txn.state === 'ai-busy'}
				<button
					type="button"
					onclick={requestEdit}
					class="border-rule-soft text-accent hover:bg-cream2 block w-full border-t px-[11px] py-[8px] text-left font-serif text-[12px]"
					>请求编辑 · 插入队首(不打断当前提交)</button
				>
			{/if}
		</div>
	</div>

	<div class="px-[16px] pt-[6px]">
		<div class="text-ink-ghost mt-[6px] mb-[8px] font-mono text-[10px] tracking-[.1em]">
			COMMAND STREAM
		</div>
	</div>
	<div class="px-[16px] pb-[22px]" aria-live="polite">
		{#if commands.length === 0}
			<p class="text-ink-ghost text-[12px] italic leading-[1.5]">
				暂无本文活动。AI 编辑或提交本文时,round / tool / commit 事件会实时出现在这里。
			</p>
		{:else}
			{#each commands as e (e.id)}
				<div
					class="border-rule-soft border-b py-[9px]"
					style:animation={e.fresh ? 'slidein .3s ease' : undefined}
				>
					<div class="mb-[3px] flex items-center gap-[8px]">
						<span class="h-[7px] w-[7px] flex-none rounded-full" style:background={e.style.color}
						></span>
						<span class="font-mono text-[11px]" style:color={e.style.color}>{e.author}</span>
						<span class="text-ink-ghost ml-auto font-mono text-[10px]">{e.time}</span>
					</div>
					<div class="text-ink-soft pl-[16px] text-[13px] leading-[1.5]">{e.text}</div>
				</div>
			{/each}
		{/if}
	</div>
{/snippet}

{#snippet versions()}
	<!-- header + timeline/list toggle -->
	<div class="flex items-center justify-between px-[16px] pt-[14px] pb-[8px]">
		<span class="font-disp text-ink-faint text-[11px] tracking-[.22em] uppercase">版本 · 署名</span>
		<div class="border-rule inline-flex overflow-hidden rounded-[4px] border">
			<button
				type="button"
				class="h-[24px] px-[9px] font-serif text-[11px]"
				class:bg-ink={timelineStyle === 'rail'}
				class:text-paper={timelineStyle === 'rail'}
				class:bg-card={timelineStyle !== 'rail'}
				class:text-ink-soft={timelineStyle !== 'rail'}
				onclick={() => (timelineStyle = 'rail')}>时间线</button
			>
			<button
				type="button"
				class="h-[24px] px-[9px] font-serif text-[11px]"
				class:bg-ink={timelineStyle === 'list'}
				class:text-paper={timelineStyle === 'list'}
				class:bg-card={timelineStyle !== 'list'}
				class:text-ink-soft={timelineStyle !== 'list'}
				onclick={() => (timelineStyle = 'list')}>列表</button
			>
		</div>
	</div>

	<!-- commit timeline -->
	<div class="px-[16px] pt-[8px] pb-[4px]">
		{#if versionRows.length === 0}
			<p class="text-ink-ghost text-[12px] italic">暂无版本记录。</p>
		{:else}
			{#each versionRows as v, i (v.id)}
				<button
					type="button"
					onclick={() => tick2(v.id)}
					title="勾选两个版本以对比 diff"
					class="relative flex w-full gap-[11px] rounded-[4px] text-left {timelineStyle ===
					'rail'
						? 'pb-[16px]'
						: 'border-rule-soft items-start border-b py-[10px]'}"
					class:bg-accent-tint={v.ticked}
					style:--accent-tint="var(--color-accent)"
					style:background={v.ticked ? 'rgba(122,46,46,.07)' : undefined}
				>
					{#if timelineStyle === 'rail' && i < versionRows.length - 1}
						<span
							class="bg-rule-soft absolute top-[14px] bottom-[-2px] w-[1.5px]"
							style:left={v.head ? '5px' : '4px'}
						></span>
					{/if}
					<span
						class="mt-[3px] flex-none rounded-full"
						style:width={v.head ? '11px' : '9px'}
						style:height={v.head ? '11px' : '9px'}
						style:background={v.style.color}
						style:box-shadow={v.head ? `0 0 0 3px ${v.style.tint}` : 'none'}
						style:z-index="1"
					></span>
					<span class="min-w-0 flex-1">
						<span class="text-ink block text-[13px] leading-[1.45]">{v.message}</span>
						<span class="mt-[3px] flex items-center gap-[8px] font-mono text-[10px]">
							<span style:color={v.style.color}>{v.authorLabel}</span>
							<span class="text-ink-ghost">· {v.time}</span>
							{#if v.head}
								<span
									class="text-accent border-accent rounded-[3px] border px-[4px] text-[9px] tracking-[.08em]"
									>HEAD</span
								>
							{/if}
							{#if v.ticked}
								<span class="text-accent ml-auto text-[9px]">✓ 已选</span>
							{/if}
						</span>
					</span>
				</button>
			{/each}
		{/if}
	</div>

	<!-- diff viewer (tick two versions) -->
	{@render diffPanel()}

	<!-- blame gutter -->
	{@render blamePanel()}

	<!-- signature card -->
	{@render signatureCard()}
{/snippet}

{#snippet diffPanel()}
	<div class="mx-[16px] mt-[8px]">
		{#if !pair}
			<div
				class="border-rule bg-card flex items-start gap-[9px] rounded-[6px] border px-[12px] py-[10px]"
			>
				<span class="text-accent mt-[1px] font-mono text-[10px]">diff</span>
				<span class="text-ink-faint text-[11.5px] leading-[1.5]"
					>勾选两个版本即可 side-by-side / inline 对比;revert 自身也是一次 commit,永不重写历史。</span
				>
			</div>
		{:else}
			<div class="border-rule bg-card overflow-hidden rounded-[6px] border">
				<div
					class="border-rule-soft flex items-center gap-[8px] border-b px-[11px] py-[7px]"
				>
					<span class="text-ink-faint font-mono text-[10px] tracking-[.06em]">
						diff · {pair.from.slice(0, 7)} → {pair.to.slice(0, 7)}
					</span>
					{#if diffLines.length}
						<span class="font-mono text-[10px]" style:color="var(--color-author-chat)"
							>+{stat.added}</span
						>
						<span class="text-accent font-mono text-[10px]">−{stat.removed}</span>
					{/if}
					<span class="flex-1"></span>
					<div class="border-rule inline-flex overflow-hidden rounded-[4px] border">
						<button
							type="button"
							class="h-[22px] px-[8px] font-serif text-[10px]"
							class:bg-ink={diffMode === 'side'}
							class:text-paper={diffMode === 'side'}
							class:bg-card={diffMode !== 'side'}
							class:text-ink-soft={diffMode !== 'side'}
							onclick={() => (diffMode = 'side')}>并排</button
						>
						<button
							type="button"
							class="h-[22px] px-[8px] font-serif text-[10px]"
							class:bg-ink={diffMode === 'inline'}
							class:text-paper={diffMode === 'inline'}
							class:bg-card={diffMode !== 'inline'}
							class:text-ink-soft={diffMode !== 'inline'}
							onclick={() => (diffMode = 'inline')}>行内</button
						>
					</div>
				</div>
				{#if diffLoading}
					<div class="text-ink-ghost px-[11px] py-[10px] text-[11px] italic">diff 加载中…</div>
				{:else if diffError}
					<div class="text-accent px-[11px] py-[10px] text-[11px]">{diffError}</div>
				{:else if diffLines.length === 0}
					<div class="text-ink-ghost px-[11px] py-[10px] text-[11px] italic">两版之间无差异。</div>
				{:else if diffMode === 'inline'}
					<div data-scroll class="max-h-[280px] overflow-auto py-[4px] font-mono text-[11px]">
						{#each diffLines as l, li (li)}
							{@render inlineDiffLine(l)}
						{/each}
					</div>
				{:else}
					<div data-scroll class="max-h-[280px] overflow-auto font-mono text-[11px]">
						{#each diffSide as row, ri (ri)}
							{@render sideDiffRow(row)}
						{/each}
					</div>
				{/if}
			</div>
		{/if}
	</div>
{/snippet}

{#snippet inlineDiffLine(l: DiffLine)}
	{#if l.kind === 'meta'}
		<div class="text-ink-ghost px-[11px] leading-[1.7]">{l.text}</div>
	{:else if l.kind === 'hunk'}
		<div class="text-ink-faint bg-cream2 px-[11px] leading-[1.7]">{l.text}</div>
	{:else if l.kind === 'add'}
		<div
			class="px-[11px] leading-[1.7]"
			style:background="oklch(0.95 0.038 152 / .6)"
			style:color="oklch(0.36 0.07 152)"
		>
			<span class="select-none opacity-60">+ </span>{l.text}
		</div>
	{:else if l.kind === 'del'}
		<div
			class="px-[11px] leading-[1.7]"
			style:background="rgba(122,46,46,.07)"
			style:color="var(--color-accent)"
		>
			<span class="select-none opacity-60">− </span>{l.text}
		</div>
	{:else}
		<div class="text-ink-soft px-[11px] leading-[1.7]"><span class="opacity-0">  </span>{l.text}</div>
	{/if}
{/snippet}

{#snippet sideDiffRow(row: SideRow)}
	{#if row.left?.kind === 'hunk' || row.left?.kind === 'meta'}
		<div
			class="leading-[1.7]"
			class:bg-cream2={row.left.kind === 'hunk'}
			class:text-ink-faint={row.left.kind === 'hunk'}
			class:text-ink-ghost={row.left.kind === 'meta'}
		>
			<span class="block px-[11px]">{row.left.text}</span>
		</div>
	{:else}
		<div class="grid grid-cols-2">
			<span
				class="text-ink-soft min-w-0 truncate border-r border-[var(--color-rule-soft)] px-[8px] leading-[1.7]"
				style:background={row.left?.kind === 'del' ? 'rgba(122,46,46,.07)' : undefined}
				style:color={row.left?.kind === 'del' ? 'var(--color-accent)' : undefined}
				>{row.left ? (row.left.kind === 'del' ? '− ' : '') + row.left.text : ''}</span
			>
			<span
				class="text-ink-soft min-w-0 truncate px-[8px] leading-[1.7]"
				style:background={row.right?.kind === 'add' ? 'oklch(0.95 0.038 152 / .6)' : undefined}
				style:color={row.right?.kind === 'add' ? 'oklch(0.36 0.07 152)' : undefined}
				>{row.right ? (row.right.kind === 'add' ? '+ ' : '') + row.right.text : ''}</span
			>
		</div>
	{/if}
{/snippet}

{#snippet blamePanel()}
	<div class="mx-[16px] mt-[8px]">
		<button
			type="button"
			onclick={() => (showBlame = !showBlame)}
			class="border-rule bg-card hover:bg-cream2 flex w-full items-center gap-[9px] rounded-[6px] border px-[12px] py-[10px] text-left"
		>
			<span class="text-accent font-mono text-[10px]">blame</span>
			<span class="text-ink-faint flex-1 text-[11.5px] leading-[1.5]"
				>逐行作者归属(经 git blame 还原)。{blameRows.length
					? `${blameRows.length} 行`
					: '无数据'}</span
			>
			<span class="text-ink-ghost font-mono text-[11px]">{showBlame ? '▾' : '▸'}</span>
		</button>
		{#if showBlame && blameRows.length}
			<div
				data-scroll
				class="border-rule bg-card mt-[6px] max-h-[220px] overflow-auto rounded-[6px] border"
			>
				{#each blameRows as b (b.lineNo)}
					<div class="border-rule-soft flex items-center gap-[8px] border-b px-[10px] py-[4px]">
						<span class="text-ink-ghost w-[28px] text-right font-mono text-[10px]">{b.lineNo}</span>
						<span class="h-[8px] w-[8px] flex-none rounded-full" style:background={b.style.color}
						></span>
						<span class="font-mono text-[10px]" style:color={b.style.color}>{b.authorLabel}</span>
						<span class="text-ink-ghost ml-auto font-mono text-[10px]">{b.shortSha}</span>
					</div>
				{/each}
			</div>
		{/if}
	</div>
{/snippet}

{#snippet signatureCard()}
	<div class="border-rule bg-card mx-[16px] my-[16px] overflow-hidden rounded-[8px] border">
		<div
			class="border-rule-soft font-disp text-ink-faint border-b px-[13px] py-[9px] text-[11px] tracking-[.18em] uppercase"
		>
			共有署名 · 证据
		</div>
		{#if contributionBars.length}
			<div class="flex flex-col gap-[9px] px-[13px] py-[12px]">
				{#each contributionBars as c (c.author)}
					<div class="flex items-center gap-[9px]">
						<span class="w-[112px] truncate font-mono text-[10px]" style:color={c.color}
							>{c.label}</span
						>
						<span class="bg-rule-soft h-[7px] flex-1 overflow-hidden rounded-[4px]">
							<span class="block h-full" style:width="{c.pct}%" style:background={c.color}></span>
						</span>
						<span class="text-ink-soft w-[30px] text-right font-mono text-[11px]">{c.pct}%</span>
					</div>
				{/each}
			</div>
			<!-- dated model ids -->
			<div class="flex flex-col gap-[4px] px-[13px] pb-[12px]">
				{#each contributionBars as c (c.author)}
					<div class="text-ink-faint font-mono text-[10px]">{modelId(c.author)}</div>
				{/each}
			</div>
		{:else}
			<div class="text-ink-ghost px-[13px] py-[12px] text-[12px] italic">暂无贡献数据。</div>
		{/if}
		<div class="border-rule-soft text-ink-ghost border-t px-[13px] py-[9px] text-[11px] leading-[1.5]">
			诚实边界:不提供「已验证人类原创」徽章;只呈现 commit 作者分布这一事实(内核 §11)。
		</div>
	</div>
{/snippet}
