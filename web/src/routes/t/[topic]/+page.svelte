<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { workspace } from '$lib/stores/workspace.svelte';
	import { layout } from '$lib/stores/layout.svelte';
	import { connection } from '$lib/stores/connection.svelte';
	import { toast } from '$lib/stores/toast';
	import { createApi, ApiRequestError } from '$lib/api/client';
	import type { ChatResponse, ThemeConfig } from '$lib/api/types';
	import {
		turnFromChat,
		opFromFeed,
		writerModelLabel,
		WRITER_MODELS,
		type DialogueTurn,
		type OpRow
	} from '$lib/topic';
	import * as Select from '$lib/components/ui/select/index.js';
	import * as DropdownMenu from '$lib/components/ui/dropdown-menu/index.js';
	import { ScrollArea } from '$lib/components/ui/scroll-area/index.js';
	import type { PageData } from './$types';

	// Topic / Orchestration view (TOPIC section of AI-Write.dc.html, ui-ux-design
	// §6.2). Two regions: the master dialogue work surface (human goal bubble +
	// master plan bubble embedding the structured product) and the Topic Inspector
	// (theme config form + the live AI-operation feed off the SSE stream). The
	// planning layer never shows article body — only structure, writers, reports.
	let { data }: { data: PageData } = $props();

	const api = createApi();

	// Sync the loaded topic into the Navigator store. The store mutations read +
	// write the store's own `topics` state, so they are untracked — this effect
	// depends only on the load data (`data.theme` / `data.articles`), not on the
	// state it writes (which would loop). The root layout already seeds every
	// theme, so ensuring this one is present is a defensive no-op in practice.
	$effect(() => {
		const theme = data.theme;
		const articles = data.articles;
		untrack(() => {
			if (!workspace.topics.some((t) => t.theme === theme)) {
				workspace.setThemes([theme, ...workspace.topics.map((t) => t.theme)]);
			}
			workspace.setArticles(theme, articles);
			workspace.syncSelection({ kind: 'topic', theme });
		});
	});

	/* ---- master dialogue ---------------------------------------------------- */

	// The dialogue is seeded from the theme's persisted goal (the standing topic
	// goal) and grows one turn per `chat` round dispatched from the input below.
	let turns = $state<DialogueTurn[]>([]);
	$effect(() => {
		// Re-seed the opening turn whenever the loaded topic changes.
		const goal = data.config?.description?.trim() || `为「${data.theme}」规划章节结构`;
		turns = [turnFromChat(goal, data.articles, null)];
	});

	/* ---- input row: goal + skills + writer model ---------------------------- */

	let goal = $state('');
	let skillIds = $state<string[]>([]);
	let writerModel = $state<string>('');
	let dispatching = $state(false);

	const skillLabel = $derived.by(() => {
		if (skillIds.length === 0) return '不指定 skill';
		if (skillIds.length === 1) {
			return data.skills.find((s) => s.id === skillIds[0])?.name ?? skillIds[0];
		}
		return `${skillIds.length} 个 skill`;
	});

	function toggleSkill(id: string, on: boolean) {
		skillIds = on ? [...skillIds.filter((s) => s !== id), id] : skillIds.filter((s) => s !== id);
	}

	async function dispatch() {
		const text = goal.trim();
		if (!text || dispatching) return;
		dispatching = true;
		// Optimistically append the human turn so the dialogue feels live; the
		// master plan rows fill in from the result (or fall back to current
		// articles on failure).
		turns = [...turns, turnFromChat(text, data.articles, null)];
		try {
			const res: ChatResponse = await api.chat(data.theme, {
				goal: text,
				skill_ids: skillIds.length ? skillIds : undefined,
				slave_model: writerModel || undefined
			});
			turns = [...turns.slice(0, -1), turnFromChat(text, data.articles, res)];
			toast.show(res.message ? '已下达 · master 规划完成' : '已下达');
			goal = '';
		} catch (e) {
			const msg = e instanceof ApiRequestError ? e.message : '下达失败';
			toast.error(msg);
		} finally {
			dispatching = false;
		}
	}

	/* ---- topic inspector: theme config form --------------------------------- */

	let description = $state('');
	let configSkillIds = $state<string[]>([]);
	let configModel = $state<string>('');
	let savingConfig = $state(false);

	// Seed both the input defaults and the config form from the loaded theme
	// config, re-running only when the topic changes (a fresh `load`). Reading
	// `data.theme` registers the dependency without capturing edits mid-topic.
	$effect(() => {
		data.theme; // dependency: reset on topic switch
		skillIds = data.config?.default_skill_ids ?? [];
		writerModel = data.config?.slave_model ?? '';
		description = data.config?.description ?? '';
		configSkillIds = data.config?.default_skill_ids ?? [];
		configModel = data.config?.slave_model ?? '';
	});

	const configSkillLabel = $derived.by(() => {
		if (configSkillIds.length === 0) return '不指定';
		if (configSkillIds.length === 1) {
			return data.skills.find((s) => s.id === configSkillIds[0])?.name ?? configSkillIds[0];
		}
		return `${configSkillIds.length} 个 skill`;
	});

	function toggleConfigSkill(id: string, on: boolean) {
		configSkillIds = on
			? [...configSkillIds.filter((s) => s !== id), id]
			: configSkillIds.filter((s) => s !== id);
	}

	const configDirty = $derived(
		description !== (data.config?.description ?? '') ||
			configModel !== (data.config?.slave_model ?? '') ||
			JSON.stringify(configSkillIds) !== JSON.stringify(data.config?.default_skill_ids ?? [])
	);

	async function saveConfig() {
		if (savingConfig) return;
		savingConfig = true;
		const next: ThemeConfig = {
			description,
			default_skill: configSkillIds[configSkillIds.length - 1] ?? null,
			default_skill_ids: configSkillIds,
			slave_model: configModel || null
		};
		try {
			const saved = await api.putThemeConfig(data.theme, next);
			data.config = saved;
			toast.show('主题配置已保存');
		} catch (e) {
			toast.error(e instanceof ApiRequestError ? e.message : '保存失败');
		} finally {
			savingConfig = false;
		}
	}

	/* ---- AI operation stream (SSE) ------------------------------------------ */

	let ops = $state<OpRow[]>([]);
	onMount(() => {
		connection.connect();
		// Backfill from whatever is already buffered, then subscribe live.
		ops = connection.feed
			.map(opFromFeed)
			.filter((o): o is OpRow => o !== null)
			.slice(0, 60);
		return connection.on((item) => {
			const op = opFromFeed(item);
			if (op) ops = [op, ...ops].slice(0, 60);
		});
	});

	/* ---- responsive inspector toggle (mobile sheet) ------------------------- */

	let showInspector = $state(true);
	onMount(() => {
		showInspector = !layout.inspectorSegmented;
	});
</script>

<div class="flex min-h-0 min-w-0 flex-1">
	<!-- work surface: master dialogue -->
	<section class="bg-paper flex min-h-0 min-w-0 flex-1 flex-col">
		<div class="border-rule-soft flex-none border-b px-[20px] py-[16px] sm:px-[28px] sm:py-[20px]">
			<div class="font-disp text-ink-ghost mb-[8px] text-[11px] tracking-[.24em] uppercase">
				编排 · Orchestration
			</div>
			<h1 class="font-disp m-0 text-[22px] font-semibold sm:text-[26px]">{data.theme}</h1>
			<p class="text-ink-faint mt-[8px] max-w-[60ch] text-[14px]">
				与 master 对话下达主题级目标;master 规划、派发 writer、汇总回报。Topic 面是规划层,不显示正文。
			</p>
			{#if layout.inspectorSegmented}
				<button
					type="button"
					class="border-rule text-ink-soft hover:bg-cream2 mt-[12px] h-[28px] rounded-[4px] border px-[11px] text-[12px]"
					onclick={() => (showInspector = !showInspector)}
				>
					{showInspector ? '隐藏配置' : '主题配置'}
				</button>
			{/if}
		</div>

		<div data-scroll class="min-h-0 flex-1 overflow-auto px-[20px] py-[24px] sm:px-[28px]">
			<div class="mx-auto flex max-w-[720px] flex-col gap-[20px]">
				{#each turns as turn, i (i)}
					{@render goalBubble(turn.goal)}
					{@render planBubble(turn)}
				{/each}

				<!-- input row -->
				<div
					class="border-rule bg-card mt-[6px] flex flex-col gap-[10px] rounded-[8px] border p-[10px_12px]"
				>
					<textarea
						bind:value={goal}
						rows="2"
						placeholder="下达下一个主题级目标…"
						class="text-ink resize-none border-none bg-transparent font-serif text-[14px] leading-[1.6] outline-none"
						onkeydown={(e) => {
							if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') dispatch();
						}}
					></textarea>
					<div class="flex flex-wrap items-center gap-[8px]">
						<!-- skill multi-select -->
						<DropdownMenu.Root>
							<DropdownMenu.Trigger
								class="border-rule bg-paper text-ink-soft hover:bg-cream2 inline-flex h-[30px] items-center gap-[6px] rounded-[5px] border px-[10px] text-[12px]"
							>
								<span class="text-ink-ghost">＃</span>{skillLabel}<span class="text-ink-ghost">▾</span>
							</DropdownMenu.Trigger>
							<DropdownMenu.Content class="w-[260px]">
								<DropdownMenu.Label class="text-ink-faint text-[11px]">
									skill 栈 · 多选(后选优先)
								</DropdownMenu.Label>
								<DropdownMenu.Separator />
								{#if data.skills.length === 0}
									<div class="text-ink-ghost px-[8px] py-[6px] text-[12px] italic">无可用 skill</div>
								{:else}
									{#each data.skills as s (s.id)}
										<DropdownMenu.CheckboxItem
											checked={skillIds.includes(s.id)}
											onCheckedChange={(v) => toggleSkill(s.id, v)}
											closeOnSelect={false}
										>
											<div class="flex flex-col">
												<span class="text-[13px]">{s.name}</span>
												{#if s.description}
													<span class="text-ink-faint text-[11px]">{s.description}</span>
												{/if}
											</div>
										</DropdownMenu.CheckboxItem>
									{/each}
								{/if}
							</DropdownMenu.Content>
						</DropdownMenu.Root>

						<!-- writer model select -->
						<Select.Root type="single" bind:value={writerModel}>
							<Select.Trigger
								class="border-rule bg-paper text-ink-soft hover:bg-cream2 h-[30px] gap-[6px] rounded-[5px] px-[10px] font-mono text-[11.5px]"
							>
								{writerModel ? writerModelLabel(writerModel) : 'writer model'}
							</Select.Trigger>
							<Select.Content>
								{#each WRITER_MODELS as m (m.id)}
									<Select.Item value={m.id} label={m.label}>{m.label}</Select.Item>
								{/each}
							</Select.Content>
						</Select.Root>

						<div class="flex-1"></div>

						<button
							type="button"
							onclick={dispatch}
							disabled={!goal.trim() || dispatching}
							class="bg-accent h-[32px] rounded-[5px] px-[16px] font-serif text-[13px] text-white transition-opacity disabled:cursor-not-allowed disabled:opacity-45"
						>
							{dispatching ? '下达中…' : '下达'}
						</button>
					</div>
				</div>
			</div>
		</div>
	</section>

	<!-- topic inspector: theme config + AI operation stream -->
	{#if showInspector}
		<aside
			data-scroll
			class="border-rule bg-cream min-h-0 overflow-auto border-l max-sm:absolute max-sm:inset-x-0 max-sm:bottom-0 max-sm:top-[var(--sheet-top,40%)] max-sm:z-30 max-sm:border-t sm:w-[300px] sm:flex-none lg:w-[340px]"
			aria-label="主题配置"
		>
			<div class="flex items-center justify-between px-[16px] pt-[14px] pb-[4px]">
				<span class="font-disp text-ink-faint text-[11px] tracking-[.22em] uppercase">主题配置</span
				>
				<button
					type="button"
					onclick={saveConfig}
					disabled={!configDirty || savingConfig}
					class="text-accent text-[11px] disabled:cursor-not-allowed disabled:opacity-40"
				>
					{savingConfig ? '保存中…' : '保存'}
				</button>
			</div>
			<div class="border-rule flex flex-col gap-[12px] border-b px-[16px] pt-[8px] pb-[16px]">
				<label class="flex flex-col gap-[5px]">
					<span class="text-ink-faint text-[12px]">描述 / 目标</span>
					<textarea
						bind:value={description}
						rows="3"
						class="border-rule bg-card text-ink resize-none rounded-[4px] border p-[8px] font-serif text-[13px] outline-none"
						placeholder="主题描述 / 目标"
					></textarea>
				</label>

				<div class="flex flex-col gap-[5px]">
					<span class="text-ink-faint text-[12px]">默认 skill</span>
					<DropdownMenu.Root>
						<DropdownMenu.Trigger
							class="border-rule bg-card text-ink hover:bg-cream2 flex h-[32px] items-center justify-between rounded-[4px] border px-[10px] text-[13px]"
						>
							{configSkillLabel}<span class="text-ink-ghost">▾</span>
						</DropdownMenu.Trigger>
						<DropdownMenu.Content class="w-[260px]">
							<DropdownMenu.Label class="text-ink-faint text-[11px]">
								默认 skill 栈 · 多选
							</DropdownMenu.Label>
							<DropdownMenu.Separator />
							{#if data.skills.length === 0}
								<div class="text-ink-ghost px-[8px] py-[6px] text-[12px] italic">无可用 skill</div>
							{:else}
								{#each data.skills as s (s.id)}
									<DropdownMenu.CheckboxItem
										checked={configSkillIds.includes(s.id)}
										onCheckedChange={(v) => toggleConfigSkill(s.id, v)}
										closeOnSelect={false}
									>
										<div class="flex flex-col">
											<span class="text-[13px]">{s.name}</span>
											{#if s.description}
												<span class="text-ink-faint text-[11px]">{s.description}</span>
											{/if}
										</div>
									</DropdownMenu.CheckboxItem>
								{/each}
							{/if}
						</DropdownMenu.Content>
					</DropdownMenu.Root>
				</div>

				<div class="flex flex-col gap-[5px]">
					<span class="text-ink-faint text-[12px]">默认 writer model</span>
					<Select.Root type="single" bind:value={configModel}>
						<Select.Trigger
							class="border-rule bg-card text-ink hover:bg-cream2 flex h-[32px] w-full items-center justify-between rounded-[4px] px-[10px] font-mono text-[12px]"
						>
							{configModel ? writerModelLabel(configModel) : '未设置'}
						</Select.Trigger>
						<Select.Content>
							{#each WRITER_MODELS as m (m.id)}
								<Select.Item value={m.id} label={m.label}>{m.label}</Select.Item>
							{/each}
						</Select.Content>
					</Select.Root>
				</div>
			</div>

			<div class="px-[16px] pt-[14px] pb-[4px]">
				<span class="font-disp text-ink-faint text-[11px] tracking-[.22em] uppercase">AI 操作流</span
				>
			</div>
			<div class="px-[16px] pt-[4px] pb-[24px]">
				{#if ops.length === 0}
					<p class="text-ink-ghost text-[12px] italic">
						暂无操作。master 派发 writer 后,round / tool / commit / slave 事件会实时出现在这里。
					</p>
				{:else}
					{#each ops as op (op.id)}
						<div class="border-rule-soft flex gap-[10px] border-b py-[7px]">
							<span
								class="h-fit rounded-[3px] border px-[5px] py-[1px] font-mono text-[9.5px] tracking-[.04em] whitespace-nowrap uppercase"
								style:color={op.color}
								style:border-color={op.color}>{op.tag}</span
							>
							<span class="text-ink-soft flex-1 text-[12.5px] leading-[1.5]">{op.text}</span>
							<span class="text-ink-ghost font-mono text-[10px] whitespace-nowrap">{op.time}</span>
						</div>
					{/each}
				{/if}
			</div>
		</aside>
	{/if}
</div>

<!-- ---- snippets ---------------------------------------------------------- -->

{#snippet goalBubble(text: string)}
	<div
		class="border-author-you/25 bg-author-you/[0.06] max-w-[78%] self-end rounded-[10px_10px_2px_10px] border p-[12px_15px]"
	>
		<div class="text-author-you mb-[5px] font-mono text-[10px] tracking-[.06em]">你 · 主题级目标</div>
		<div class="text-ink text-[14.5px] leading-[1.7]">{text}</div>
	</div>
{/snippet}

{#snippet planBubble(turn: DialogueTurn)}
	<div
		class="border-rule bg-card max-w-[88%] self-start rounded-[10px_10px_10px_2px] border p-[14px_16px]"
	>
		<div class="mb-[9px] flex items-center gap-[8px]">
			<span class="bg-accent h-[8px] w-[8px] rounded-full"></span>
			<span class="text-accent font-mono text-[10px] tracking-[.06em]">master · 规划 + 派发</span>
		</div>
		{#if turn.masterMessage}
			<div class="text-ink mb-[12px] text-[14.5px] leading-[1.7]">{turn.masterMessage}</div>
		{:else}
			<div class="text-ink-faint mb-[12px] text-[14px] leading-[1.7] italic">
				下达目标后,master 在此给出规划说明并派发 writer。当前展示主题既有结构。
			</div>
		{/if}

		<!-- structured product: the chat plan -->
		<div class="border-rule-soft overflow-hidden rounded-[6px] border">
			<div class="bg-cream2 text-ink-faint px-[12px] py-[7px] font-mono text-[10px] tracking-[.08em]">
				{#if turn.rows.length}
					{turn.rows.length} 篇文章 · 派发 report
				{:else}
					暂无文章 · 下达目标以创建结构
				{/if}
			</div>
			{#each turn.rows as row (row.file)}
				<div class="border-rule-soft flex items-center gap-[10px] border-t px-[12px] py-[9px]">
					<span
						class="h-[8px] w-[8px] flex-none rounded-full"
						style:background={row.style.color}
						style:margin-left="{row.depth * 12}px"
					></span>
					<span class="text-ink flex-1 truncate text-[13.5px]">{row.title}</span>
					<span class="text-ink-faint font-mono text-[10px] whitespace-nowrap">{row.writer}</span>
					{@render badge(row.badge, row.tone)}
				</div>
			{/each}
		</div>

		{#if turn.outcome}
			<div class="text-ink-ghost mt-[10px] font-mono text-[10px]">outcome · {turn.outcome}</div>
		{/if}
	</div>
{/snippet}

{#snippet badge(text: string, tone: 'done' | 'editing' | 'pending')}
	<span
		class="rounded-[10px] border px-[7px] py-[2px] font-mono text-[10px] whitespace-nowrap"
		class:border-rule={tone === 'pending'}
		class:bg-cream2={tone === 'pending'}
		class:text-ink-faint={tone === 'pending'}
		style:background={tone === 'done'
			? 'oklch(0.96 0.025 150)'
			: tone === 'editing'
				? 'oklch(0.96 0.025 62)'
				: undefined}
		style:color={tone === 'done'
			? 'oklch(0.38 0.06 150)'
			: tone === 'editing'
				? 'oklch(0.42 0.07 50)'
				: undefined}
		style:border-color={tone === 'done'
			? 'oklch(0.86 0.04 150)'
			: tone === 'editing'
				? 'oklch(0.86 0.04 62)'
				: undefined}>{text}</span
	>
{/snippet}
