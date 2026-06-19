<script lang="ts">
	import { workspace, type FlatRow } from '$lib/stores/workspace.svelte';
	import { connection } from '$lib/stores/connection.svelte';
	import { layout } from '$lib/stores/layout.svelte';
	import TreeRow from './TreeRow.svelte';
	import PlusIcon from '@lucide/svelte/icons/plus';
	import SearchIcon from '@lucide/svelte/icons/search';

	// Workspace Navigator (ui-ux-design-draft §6.1): topic/article hierarchy via
	// depth indent + disclosure, title/body search filter, click-to-navigate, and
	// drag reorder / reparent with a menu fallback. Reads the live tree + SSE feed
	// from the stores.

	// Articles with in-flight AI activity, derived from the SSE feed (slave spawn /
	// commit events carry theme+file).
	const liveSet = $derived.by(() => {
		const set = new Set<string>();
		for (const item of connection.feed) {
			const ev = item.event as Record<string, { theme?: string; file?: string; article?: string }>;
			const v = ev[item.kind];
			if (!v) continue;
			if (v.theme && v.file) set.add(`${v.theme}/${v.file}`);
			if (v.article) set.add(v.article);
		}
		return set;
	});

	const rows = $derived(workspace.rows);

	// --- drag state ---
	let dragKey = $state<string | null>(null);
	let overKey = $state<string | null>(null);
	let overMode = $state<'before' | 'after' | 'into' | null>(null);

	const keyOf = (r: FlatRow) => (r.type === 'topic' ? `t:${r.theme}` : `a:${r.theme}/${r.file}`);

	function onRowDragStart(r: FlatRow, e: DragEvent) {
		if (r.type !== 'article' || !r.file) return;
		dragKey = keyOf(r);
		e.dataTransfer?.setData('text/plain', r.file);
		if (e.dataTransfer) e.dataTransfer.effectAllowed = 'move';
	}

	function onRowDragOver(r: FlatRow, e: DragEvent) {
		if (r.type !== 'article' || !dragKey || keyOf(r) === dragKey) return;
		const dragRow = rows.find((x) => keyOf(x) === dragKey);
		if (!dragRow || dragRow.theme !== r.theme) return; // reorder within a theme
		e.preventDefault();
		if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
		const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
		const ratio = (e.clientY - rect.top) / rect.height;
		overKey = keyOf(r);
		overMode = ratio < 0.28 ? 'before' : ratio > 0.72 ? 'after' : 'into';
	}

	function onRowDrop(r: FlatRow, e: DragEvent) {
		e.preventDefault();
		const dragRow = rows.find((x) => keyOf(x) === dragKey);
		if (!dragRow || !dragRow.file || r.type !== 'article' || !r.file) {
			resetDrag();
			return;
		}
		if (dragRow.theme !== r.theme) {
			resetDrag();
			return;
		}
		if (overMode === 'into') {
			// reparent the dragged article under the drop-target article
			void workspace.reparent(r.theme, dragRow.file, r.file);
		} else {
			// reorder: move dragged file before/after the target in the theme order
			const node = workspace.topics.find((t) => t.theme === r.theme);
			if (node) {
				const order = node.articles.map((a) => a.file).filter((f) => f !== dragRow.file);
				const idx = order.indexOf(r.file);
				const insertAt = overMode === 'after' ? idx + 1 : idx;
				order.splice(insertAt, 0, dragRow.file);
				void workspace.reorder(r.theme, order);
			}
		}
		resetDrag();
	}

	function resetDrag() {
		dragKey = null;
		overKey = null;
		overMode = null;
	}

	// --- menu fallback actions ---
	function siblings(r: FlatRow) {
		const node = workspace.topics.find((t) => t.theme === r.theme);
		return node ? node.articles.map((a) => a.file) : [];
	}
	function move(r: FlatRow, delta: number) {
		if (!r.file) return;
		const order = siblings(r);
		const i = order.indexOf(r.file);
		const j = i + delta;
		if (i < 0 || j < 0 || j >= order.length) return;
		[order[i], order[j]] = [order[j], order[i]];
		void workspace.reorder(r.theme, order);
	}
	function reparentUnderPrev(r: FlatRow) {
		if (!r.file) return;
		const order = siblings(r);
		const i = order.indexOf(r.file);
		if (i <= 0) return;
		void workspace.reparent(r.theme, r.file, order[i - 1]);
	}
	function promote(r: FlatRow) {
		if (!r.file) return;
		void workspace.reparent(r.theme, r.file, null);
	}
</script>

<div class="flex min-h-0 flex-1 flex-col">
	<!-- header -->
	<div class="flex flex-none items-center justify-between px-[16px] pt-[14px] pb-[10px]">
		<span class="font-disp text-ink-faint text-[11px] tracking-[.24em] uppercase">Workspace</span>
		<button
			type="button"
			title="新建主题"
			class="border-rule text-ink-soft hover:bg-cream2 grid h-[24px] w-[24px] place-items-center rounded-[3px] border"
			aria-label="新建主题"
		>
			<PlusIcon class="size-[14px]" />
		</button>
	</div>

	<!-- search -->
	<div class="px-[14px] pb-[12px]">
		<div
			class="border-rule bg-card flex h-[32px] items-center gap-[8px] rounded-[3px] border px-[10px]"
		>
			<SearchIcon class="text-ink-ghost size-[13px]" />
			<input
				bind:value={workspace.filter}
				placeholder="检索标题 / 正文"
				class="text-ink min-w-0 flex-1 border-none bg-transparent font-serif text-[13px] outline-none"
				aria-label="检索标题或正文"
			/>
		</div>
	</div>

	<!-- tree -->
	<div data-scroll class="min-h-0 flex-1 overflow-auto px-[8px] pt-[2px] pb-[16px]" role="tree">
		{#if rows.length === 0}
			<p class="text-ink-ghost px-[10px] py-3 text-[13px] italic">
				{workspace.filter ? '无匹配项' : '暂无主题 · 新建或与 master 对话生成结构'}
			</p>
		{/if}
		{#each rows as row (keyOf(row))}
			<TreeRow
				{row}
				expanded={!!workspace.expanded[row.theme]}
				live={row.type === 'article' && row.file ? liveSet.has(`${row.theme}/${row.file}`) : false}
				dragging={dragKey === keyOf(row)}
				dropTarget={overKey === keyOf(row) ? overMode : null}
				onselect={() => {
					if (row.type === 'topic') workspace.selectTopic(row.theme);
					else if (row.file) workspace.selectArticle(row.theme, row.file);
					if (layout.navIsDrawer) layout.closeNav();
				}}
				ontoggle={() => workspace.toggle(row.theme)}
				onmoveup={() => move(row, -1)}
				onmovedown={() => move(row, 1)}
				onreparent={() => reparentUnderPrev(row)}
				onpromote={() => promote(row)}
				ondragstart={(e) => onRowDragStart(row, e)}
				ondragover={(e) => onRowDragOver(row, e)}
				ondrop={(e) => onRowDrop(row, e)}
				ondragend={resetDrag}
				ondragleave={() => {
					if (overKey === keyOf(row)) {
						overKey = null;
						overMode = null;
					}
				}}
			/>
		{/each}
	</div>

	<!-- identity footer -->
	<div
		class="border-rule text-ink-faint flex flex-none items-center gap-[9px] border-t px-[16px] py-[11px] text-[12px]"
	>
		<span class="h-[8px] w-[8px] rounded-full" style:background="var(--color-author-you)"></span>
		<span class="font-serif">以「你」的身份协作</span>
	</div>
</div>
