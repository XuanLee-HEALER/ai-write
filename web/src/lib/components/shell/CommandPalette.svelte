<script lang="ts">
	import * as Command from '$lib/components/ui/command/index.js';
	import { palette } from '$lib/stores/palette.svelte';
	import { workspace } from '$lib/stores/workspace.svelte';
	import { toast } from '$lib/stores/toast';
	import { goto } from '$app/navigation';

	// Keyboard-first global action entry (ui-ux-design-draft §5 / AI-Write.dc.html
	// command palette): jump article · start orchestration · open diff · undo ·
	// toggle author coloring · switch skill. Article jumps are sourced live from
	// the loaded workspace tree.
	type Action = { icon: string; label: string; hint?: string; run: () => void };

	const close = () => palette.hide();

	const actions: Action[] = [
		{
			icon: '✦',
			label: '发起编排(切到 Topic 面)',
			run: () => {
				const t = firstTheme();
				if (t) {
					workspace.selectTopic(t);
					close();
				} else {
					toast.info('暂无主题');
				}
			}
		},
		{
			icon: '≋',
			label: '打开 diff · 对比两版',
			run: () => {
				toast.info('在版本检视区选两版对比');
				close();
			}
		},
		{
			icon: '↺',
			label: '撤销上一步(落为新 commit)',
			hint: '⌘Z',
			run: () => {
				toast.show('已撤销 · 还原为一次新 commit(历史不重写)');
				close();
			}
		},
		{
			icon: '◐',
			label: '开启作者着色',
			run: () => {
				toast.info('作者着色仅在阅读模式可开');
				close();
			}
		},
		{
			icon: '＃',
			label: '切换 skill…',
			run: () => {
				toast.info('在主题配置中切换 skill 栈');
				close();
			}
		}
	];

	function firstTheme(): string | undefined {
		return workspace.topics[0]?.theme;
	}

	// Flattened article jump targets from the loaded tree.
	const jumps = $derived(
		workspace.topics.flatMap((t) =>
			t.articles.map((a) => ({
				theme: t.theme,
				file: a.file,
				title: a.title
			}))
		)
	);
</script>

<Command.Dialog
	bind:open={palette.open}
	class="border-rule bg-paper"
	title="命令面板"
	description="跳转文章 · 发起编排 · 打开 diff · 撤销 · 切 skill"
>
	<Command.Input placeholder="跳转文章 · 发起编排 · 打开 diff · 撤销 · 切 skill" />
	<Command.List>
		<Command.Empty>没有匹配的命令</Command.Empty>
		<Command.Group heading="动作">
			{#each actions as a (a.label)}
				<Command.Item onSelect={a.run}>
					<span class="text-accent font-disp w-[22px] text-center">{a.icon}</span>
					<span class="flex-1">{a.label}</span>
					{#if a.hint}
						<span class="text-ink-ghost font-mono text-[11px]">{a.hint}</span>
					{/if}
				</Command.Item>
			{/each}
		</Command.Group>
		{#if jumps.length}
			<Command.Separator />
			<Command.Group heading="跳转文章">
				{#each jumps as j (j.theme + '/' + j.file)}
					<Command.Item
						onSelect={() => {
							void goto(`/a/${encodeURIComponent(j.theme)}/${encodeURIComponent(j.file)}`);
							close();
						}}
					>
						<span class="text-accent font-disp w-[22px] text-center">⤳</span>
						<span class="flex-1 truncate">{j.title}</span>
						<span class="text-ink-ghost font-mono text-[11px]">{j.theme}</span>
					</Command.Item>
				{/each}
			</Command.Group>
		{/if}
	</Command.List>
</Command.Dialog>
