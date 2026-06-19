<script lang="ts">
	import type { FlatRow } from '$lib/stores/workspace.svelte';
	import { authorColor } from '$lib/author';
	import * as DropdownMenu from '$lib/components/ui/dropdown-menu/index.js';
	import EllipsisIcon from '@lucide/svelte/icons/ellipsis-vertical';
	import GripIcon from '@lucide/svelte/icons/grip-vertical';

	// One navigator row: topic (square + disclosure caret) or article (author dot,
	// indented by depth). Drag handle + an explicit "更多" menu provide the
	// touch/keyboard fallback for reorder/reparent (ui-ux-design-draft §4.3).
	type Props = {
		row: FlatRow;
		expanded: boolean;
		live?: boolean;
		dragging?: boolean;
		dropTarget?: 'before' | 'after' | 'into' | null;
		onselect: () => void;
		ontoggle: () => void;
		onmoveup?: () => void;
		onmovedown?: () => void;
		onreparent?: () => void;
		onpromote?: () => void;
		ondragstart?: (e: DragEvent) => void;
		ondragover?: (e: DragEvent) => void;
		ondrop?: (e: DragEvent) => void;
		ondragend?: (e: DragEvent) => void;
		ondragleave?: (e: DragEvent) => void;
	};

	let {
		row,
		expanded,
		live = false,
		dragging = false,
		dropTarget = null,
		onselect,
		ontoggle,
		onmoveup,
		onmovedown,
		onreparent,
		onpromote,
		ondragstart,
		ondragover,
		ondrop,
		ondragend,
		ondragleave
	}: Props = $props();

	const isTopic = $derived(row.type === 'topic');
	const indent = $derived(10 + row.depth * 15);
	const author = $derived(row.author ? authorColor(row.author) : null);
</script>

<div
	class="group/row relative my-px flex h-[32px] cursor-pointer items-center gap-[7px] rounded-[4px] pr-[6px] select-none"
	class:bg-[color:var(--accent-tint)]={row.selected}
	style:padding-left="{indent}px"
	style:box-shadow={row.selected ? 'inset 2px 0 0 var(--color-accent)' : 'none'}
	style:opacity={dragging ? '0.4' : '1'}
	style:border-top={dropTarget === 'before' ? '2px solid var(--color-accent)' : '2px solid transparent'}
	style:border-bottom={dropTarget === 'after' ? '2px solid var(--color-accent)' : '2px solid transparent'}
	style:background={dropTarget === 'into' ? 'var(--accent-tint)' : undefined}
	role="treeitem"
	aria-selected={row.selected}
	aria-expanded={isTopic ? expanded : undefined}
	tabindex="0"
	draggable={!isTopic}
	onclick={onselect}
	onkeydown={(e) => {
		if (e.key === 'Enter' || e.key === ' ') {
			e.preventDefault();
			onselect();
		}
	}}
	ondragstart={(e) => ondragstart?.(e)}
	ondragover={(e) => ondragover?.(e)}
	ondrop={(e) => ondrop?.(e)}
	ondragend={(e) => ondragend?.(e)}
	ondragleave={(e) => ondragleave?.(e)}
>
	<!-- disclosure caret (topics only) -->
	<button
		type="button"
		class="text-ink-ghost w-[13px] flex-none text-center text-[10px]"
		class:invisible={!isTopic}
		onclick={(e) => {
			e.stopPropagation();
			ontoggle();
		}}
		tabindex="-1"
		aria-hidden={!isTopic}
	>
		{isTopic ? (expanded ? '▾' : '▸') : ''}
	</button>

	<!-- identity dot: topic = square outline, article = author color circle -->
	{#if isTopic}
		<span class="border-ink-faint h-[9px] w-[9px] flex-none rounded-[2px] border-[1.5px]"></span>
	{:else}
		<span
			class="h-[8px] w-[8px] flex-none rounded-full"
			style:background={author ? author.color : 'var(--color-ink-ghost)'}
		></span>
	{/if}

	<!-- title -->
	<span
		class="min-w-0 flex-1 overflow-hidden text-ellipsis whitespace-nowrap"
		class:font-disp={isTopic}
		class:font-serif={!isTopic}
		style:font-size={isTopic ? '14px' : '13.5px'}
		style:font-weight={isTopic || row.selected ? '600' : '400'}
		style:color={row.selected ? 'var(--color-ink)' : 'var(--color-ink-soft)'}
	>
		{row.title}
	</span>

	<!-- live badge -->
	{#if live}
		<span
			class="h-[6px] w-[6px] flex-none rounded-full"
			style:background="var(--color-live)"
			style:box-shadow="0 0 0 3px color-mix(in oklch, var(--color-live) 18%, transparent)"
			title="有进行中的 AI 活动"
		></span>
	{/if}

	{#if !isTopic}
		<!-- touch/keyboard reorder fallback menu -->
		<DropdownMenu.Root>
			<DropdownMenu.Trigger
				class="text-ink-ghost hover:text-ink-soft flex-none rounded p-[2px] opacity-0 group-hover/row:opacity-100 focus-visible:opacity-100 data-[state=open]:opacity-100"
				onclick={(e) => e.stopPropagation()}
				aria-label="更多操作"
			>
				<EllipsisIcon class="size-[14px]" />
			</DropdownMenu.Trigger>
			<DropdownMenu.Content class="bg-card border-rule">
				<DropdownMenu.Item onclick={() => onmoveup?.()}>上移</DropdownMenu.Item>
				<DropdownMenu.Item onclick={() => onmovedown?.()}>下移</DropdownMenu.Item>
				<DropdownMenu.Separator />
				<DropdownMenu.Item onclick={() => onreparent?.()}>降级为上一篇的子级</DropdownMenu.Item>
				<DropdownMenu.Item onclick={() => onpromote?.()} disabled={row.parent === null}
					>提升为顶层</DropdownMenu.Item
				>
			</DropdownMenu.Content>
		</DropdownMenu.Root>

		<!-- drag handle -->
		<span
			class="text-ink-ghost flex-none cursor-grab opacity-0 group-hover/row:opacity-100"
			title="拖拽重排 / 改父级"
			aria-hidden="true"
		>
			<GripIcon class="size-[13px]" />
		</span>
	{/if}
</div>
