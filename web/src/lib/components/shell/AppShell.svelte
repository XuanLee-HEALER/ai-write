<script lang="ts">
	import type { Snippet } from 'svelte';
	import { layout } from '$lib/stores/layout.svelte';
	import { palette } from '$lib/stores/palette.svelte';
	import Navigator from '$lib/components/navigator/Navigator.svelte';
	import ConnectionIndicator from './ConnectionIndicator.svelte';
	import MenuIcon from '@lucide/svelte/icons/menu';
	import XIcon from '@lucide/svelte/icons/x';
	import LibraryIcon from '@lucide/svelte/icons/library-big';
	import PanelIcon from '@lucide/svelte/icons/panels-top-left';

	// App Shell + responsive three-region collapse ladder (ui-ux-design-draft §4.2,
	// §5). Navigator: persistent rail (lg/xl/2xl) → overlay drawer (md) →
	// full-screen drawer + bottom tab bar (sm/xs). The work surface fills the rest;
	// the Inspector collapse is handled inside the article/topic shells.
	let { children }: { children: Snippet } = $props();
</script>

<div class="bg-paper text-ink flex h-[100dvh] min-h-0 flex-col overflow-hidden">
	<!-- ============ TOP BAR ============ -->
	<header
		class="border-rule bg-cream relative z-10 flex h-[54px] flex-none items-center gap-[12px] border-b px-[12px] sm:gap-[18px] sm:px-[18px]"
		style:padding-top="env(safe-area-inset-top)"
	>
		<!-- nav toggle (drawer form factors) -->
		{#if layout.navIsDrawer}
			<button
				type="button"
				class="border-rule text-ink-soft hover:bg-cream2 grid h-[32px] w-[32px] flex-none place-items-center rounded-[3px] border"
				onclick={() => layout.toggleNav()}
				aria-label="打开导航"
			>
				<MenuIcon class="size-[16px]" />
			</button>
		{:else}
			<!-- persistent-rail collapse toggle (desktop) -->
			<button
				type="button"
				class="border-rule text-ink-soft hover:bg-cream2 hidden h-[32px] w-[32px] flex-none place-items-center rounded-[3px] border lg:grid"
				onclick={() => (layout.navCollapsed = !layout.navCollapsed)}
				aria-label={layout.navCollapsed ? '展开导航' : '折叠导航'}
				title={layout.navCollapsed ? '展开导航' : '折叠导航'}
			>
				<PanelIcon class="size-[16px]" />
			</button>
		{/if}

		<div class="flex items-baseline gap-[10px]">
			<a href="/" class="font-disp text-[18px] font-semibold tracking-[.01em]">AI-Write</a>
			<span class="font-disp text-ink-ghost hidden text-[11px] tracking-[.26em] uppercase sm:inline"
				>workspace</span
			>
		</div>

		<button
			type="button"
			class="border-rule text-ink-soft hidden h-[30px] items-center gap-[7px] rounded-[3px] border bg-transparent px-[11px] text-[13px] md:flex"
		>
			<span class="bg-accent h-[7px] w-[7px] rounded-full"></span>
			Mouselee 的工作区
			<span class="text-ink-ghost text-[11px]">▾</span>
		</button>

		<div class="flex-1"></div>

		<ConnectionIndicator />

		<!-- command palette entry -->
		<button
			type="button"
			class="border-rule bg-card text-ink-faint hover:bg-cream2 flex h-[30px] items-center gap-[9px] rounded-[3px] border py-0 pr-[11px] pl-[12px]"
			onclick={() => palette.show()}
			aria-label="打开命令面板"
		>
			<span class="font-serif hidden text-[13px] sm:inline">搜索 / 命令</span>
			<kbd
				class="border-rule bg-cream2 text-ink rounded-[3px] border px-[5px] py-px font-mono text-[11px]"
				>⌘K</kbd
			>
		</button>
	</header>

	<!-- ============ BODY ============ -->
	<div class="flex min-h-0 flex-1">
		<!-- persistent Navigator rail (lg and up) -->
		{#if layout.navPersistent}
			<nav
				class="border-rule bg-cream flex min-h-0 w-[264px] flex-none flex-col border-r"
				aria-label="工作区导航"
			>
				<Navigator />
			</nav>
		{/if}

		<!-- work surface -->
		<main class="bg-paper flex min-h-0 min-w-0 flex-1 flex-col">
			{@render children()}
		</main>
	</div>

	<!-- ============ BOTTOM TAB BAR (phone) ============ -->
	{#if layout.hasBottomTabs}
		<nav
			class="border-rule bg-cream flex flex-none border-t"
			style:padding-bottom="env(safe-area-inset-bottom)"
			aria-label="主导航"
		>
			<button
				type="button"
				class="flex h-[52px] flex-1 flex-col items-center justify-center gap-[3px] text-[11px]"
				class:text-accent={layout.mobileTab === 'workspace'}
				class:text-ink-faint={layout.mobileTab !== 'workspace'}
				onclick={() => layout.openNav()}
			>
				<LibraryIcon class="size-[18px]" />
				工作区
			</button>
			<button
				type="button"
				class="flex h-[52px] flex-1 flex-col items-center justify-center gap-[3px] text-[11px]"
				class:text-accent={layout.mobileTab === 'surface'}
				class:text-ink-faint={layout.mobileTab !== 'surface'}
				onclick={() => layout.closeNav()}
			>
				<PanelIcon class="size-[18px]" />
				当前
			</button>
		</nav>
	{/if}
</div>

<!-- ============ NAVIGATOR DRAWER (md and below) ============ -->
{#if layout.navIsDrawer && layout.navOpen}
	<div class="fixed inset-0 z-40 flex" role="dialog" aria-modal="true" aria-label="工作区导航">
		<button
			type="button"
			class="absolute inset-0 bg-[rgba(27,26,24,.32)]"
			onclick={() => layout.closeNav()}
			aria-label="关闭导航"
			tabindex="-1"
		></button>
		<nav
			class="border-rule bg-cream relative flex min-h-0 flex-col border-r shadow-[0_24px_64px_rgba(27,26,24,.3)]"
			class:w-[284px]={layout.bp === 'md'}
			class:w-full={layout.bp !== 'md'}
			style:padding-top="env(safe-area-inset-top)"
			style:animation="slidein .2s ease"
		>
			<div class="flex items-center justify-end px-[10px] pt-[8px]">
				<button
					type="button"
					class="text-ink-faint hover:bg-cream2 grid h-[30px] w-[30px] place-items-center rounded"
					onclick={() => layout.closeNav()}
					aria-label="关闭导航"
				>
					<XIcon class="size-[16px]" />
				</button>
			</div>
			<Navigator />
		</nav>
	</div>
{/if}
