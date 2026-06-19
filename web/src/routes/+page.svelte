<script lang="ts">
	import { onMount } from 'svelte';
	import { workspace } from '$lib/stores/workspace.svelte';

	// Welcome / empty state (ui-ux-design-draft §6.1 empty态): guidance, not a
	// blank. Clears any stale selection so the Navigator shows nothing active.
	onMount(() => workspace.syncSelection({ kind: 'none' }));
</script>

<div class="flex flex-1 flex-col items-center justify-center px-6 text-center">
	<div class="font-disp text-ink-ghost mb-3 text-[11px] tracking-[.26em] uppercase">Workspace</div>
	<h1 class="font-disp text-ink mb-3 text-[26px] font-semibold">选择一个主题开始</h1>
	<p class="text-ink-faint max-w-[46ch] text-[14px] leading-[1.7]">
		在左侧选择一个主题与 master 对话下达主题级目标,或选择一篇文章阅读与协作编辑。每一个提交都是可交付的成品,历史是唯一的状态轴。
	</p>
</div>
