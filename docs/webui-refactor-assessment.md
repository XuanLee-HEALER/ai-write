# webui 重构评估:按 AI-Write.dc.html 设计稿,前后端分离(Svelte + shadcn + Tailwind)

> 性质:**实施前评估**,供 review 决策。设计源:Claude Design 项目 `AI-Write.dc.html`(基于 `docs/ui-ux-design-draft.md` + `docs/ai-write-kernel.html` 生成)。
> 结论先行:**这不是一次「前端换皮」**——设计稿的两个核心特性(作者着色、实时事务态)会把此前**有意推迟的后端工作**(provenance→文章存储集成、coordinator 可观测、人工写端点)一并拉进来。是否做这些后端工作,决定了本次的体量,需你先拍板。

---

## 1. 设计稿读后小结

- 一个 **1440×900 桌面单屏**高保真交互原型(`DCLogic`/类 React 运行时 + `support.js`)。**非可移植代码**,是视觉 + 交互的事实规格;实现 = 用 Svelte 重建。
- **视觉基调**:亮、暖、纸感;衬线(Georgia / Noto Serif SC / Palatino)+ 等宽;accent `#7a2e2e`;作者色用 `oklch`(你=蓝 255 / chat=绿 152 / reasoner=琥珀 62)。token 已在根节点以 CSS 变量给全(`--paper/--cream/--ink/--accent/...`)。
- **两个工作面**(状态切换,非多 frame):
  - **Article 单篇**:编辑画布(**阅读/编辑**双模式、**作者着色**默认关且仅阅读可开、**色/纹理/标签**三种表达、格式组仅编辑可用、`请 AI 编辑本段`/`diff`、三栏/合并布局切换、**AI 持有事务 busy 横幅 + 轮到你了 横幅**)+ Inspector(活动流含 **COORDINATOR 事务块**[持锁人 / 排队中可**取消** / 轮到你了] + COMMAND STREAM;版本含**时间线[轨/列] + blame + 共有署名卡[贡献占比条 + 带日期快照 id + 诚实边界]**)。
  - **Topic 编排**:master 对话(人目标气泡 + master 规划气泡内嵌**结构化产物**:创建 4 篇 + writer + report 徽章)+ Topic Inspector(主题配置:描述/默认 skill/默认 writer model[`deepseek-reasoner-2026-05-28`]+ AI 操作流)。
  - **全局**:命令面板(⌘K)、toasts。
- **忠实兑现** §11 决策与内核:view/edit only、着色默认关仅 view、队首可取消、亮暖纸感、**model id 带日期快照(对上 G12)**、blame/署名/诚实边界(无「已验证人类原创」徽章)。
- **未覆盖**:响应式/移动端(设计稿是桌面单屏,§4 的坍缩阶梯没出像素稿)、light 之外无 dark、settings/skill 库页未画。

---

## 2. 设计 ↔ 现有后端 API 的差距(最关键)

设计稿要的数据,现有 axum 后端**部分拿不出来**。逐项:

| 设计特性 | 需要的数据/能力 | 现状 | 差距 |
|---|---|---|---|
| **作者着色**(逐 run 上色) | 文章按**作者游程**(`RichText.runs` + `data-author`)返回 | `GET /api/articles/..` 只回**纯文本** `content`;`provenance` 模块有 `RichText`,但**文章仍以纯文本落盘**(impl-v2 §5 推迟的「集成层」从未做) | **大**:需把文章存储迁到 `content::Document` + 编辑走 `provenance::apply_edit`,字级作者才存得下、取得出 |
| **实时事务态**(持锁人/排队/轮到你了) | coordinator 的锁/队列状态**可观测**(事件流) | `observe::Event` 无 coordinator 事件;SSE 只有写作 run 事件;且 webui 写路径**未走 coordinator**(kernel-impl-results §3.1) | **中**:加 coordinator 事件变体 + 接 observe + webui 写路径接 coordinator |
| **人直接编辑正文**(edit 模式→人 commit) | 人工写文章正文的**写端点**(经 coordinator 单事务) | webui 无人工写正文端点(只有 undo/chat/parent/reorder/config) | **中**:新增 `PUT /api/articles/..`(人 writer,走 coordinator) |
| **共有署名 · 贡献占比** | 按作者的贡献百分比(blame/provenance 聚合) | `Vcs::blame` 有(G13),但无聚合端点 | **小**:加聚合端点(从 blame 统计) |
| **side-by-side diff / 行级 blame** | diff + blame 端点 | `/diff`、`/blame` 都有 | 无差距 |
| **编排结构化产物**(创建 N 篇 + report 徽章) | `run_goal` 返回**结构化计划**(建了哪些文章/层级/各 report) | `run_goal` 回 `{outcome,message,reports}`,无「计划」结构 | **小**:扩 chat 返回结构 |
| **多 skill 多选** | chat 带 skill **栈** | 后端支持栈(G10),但 `POST /chat` 只收单 `skill_id` | **小**:`skill_ids: []` |
| **命令面板 / toasts / 树拖拽重排** | 纯前端 + 已有端点(reorder/parent) | 端点已有 | 前端实现即可 |

> 一句话:**作者着色**(设计稿的灵魂)绑死了那块一直推迟的 **provenance→文章存储集成**;**实时事务态**绑死了 **coordinator 可观测 + webui 写路径接 coordinator**(正好是 kernel-impl-results §3.1 我标的头号收口项)。

---

## 3. 前后端分离架构方案

```
┌─ frontend (新)  SvelteKit / Svelte SPA ─┐        ┌─ backend (现, 扩展) Rust axum ─┐
│  Vite + Tailwind + shadcn-svelte        │  HTTP  │  REST JSON + SSE              │
│  组件 + stores;SSE 订阅 /api/events    │ ─────▶ │  /api/* + coordinator + engine │
│  设计 token → Tailwind theme            │  /api  │  (provenance/blame/...)        │
└─────────────────────────────────────────┘        └────────────────────────────────┘
```

- **前端栈**:Svelte 5(runes)+ Vite + Tailwind v4 + **shadcn-svelte**(bits-ui 之上的无样式原语)。状态用 Svelte stores(替代 `.dc.html` 的 `DCLogic` state)。设计稿的 CSS 变量 → Tailwind theme token(亮暖纸感一套)。
- **API 契约**:沿用现有 REST(themes/articles/config/chat/skills/history/diff/blame/undo/reorder/parent)+ SSE(`/api/events`),按 §2 补齐缺口端点。前端只认 JSON,不再内嵌 HTML。
- **构建/部署(建议)**:`vite build` 出静态;**axum 用 `rust-embed` 把 dist 打进二进制**(延续现在单二进制、`include_str!` 的形态)`/` 服务 SPA、`/api/*` 服务接口。dev 时 Vite proxy `/api`→axum。
  - 备选:前端独立托管(Vercel/静态)+ axum 仅 API + CORS。对「展示/自用工具」不必要,**默认单二进制内嵌**。
- **`.dc.html` 的定位**:视觉 + 交互**规格**,不是代码来源(它依赖 Claude Design 私有运行时)。我按它重建。

## 4. shadcn-svelte 适配评估

设计稿的纸感是**高度定制**的,shadcn 是**脚手架不是外观**——它给可访问的底层原语,皮按设计 token 重铺。映射:

| 设计元素 | shadcn-svelte 原语 |
|---|---|
| ⌘K 命令面板 | `command`(cmdk)+ `dialog` |
| 三栏/合并、可拖拽分隔 | `resizable`(paneforge) |
| 合并 Inspector 的 tab | `tabs` |
| toasts | `sonner` |
| 树行右键/更多、skill 多选 | `dropdown-menu` / `select` / `popover` |
| 提示(着色 tip 等) | `tooltip` |
| 滚动区 | `scroll-area` |
| 按钮/输入/分段 | `button` / `input` / 自绘 segmented |

结论:**契合**,作为可访问性 + 行为基座;视觉全部走 Tailwind token 覆盖。作者着色、coordinator 事务块、署名卡、编排气泡这些**业务组件是自绘**(shadcn 无对应)。

## 5. 响应式(设计稿缺,需补)

设计稿只有桌面单屏。移动/平板坍缩按我草稿 `ui-ux-design-draft.md §4`(三区坍缩阶梯 + segmented + bottom tab + drawer)在**代码里实现**;高保真移动稿可让设计师后补,但工程上我先按 §4 落地,不阻塞。

## 6. 工作量与分期(建议)

- **P0 前端骨架**:Svelte+Vite+Tailwind+shadcn 工程、token、App Shell、Navigator、路由/状态、SSE 订阅、axum 内嵌 dist。
- **P1 Topic 编排面**:对话 + 结构化产物 + 主题配置 + 操作流(对现有 chat/config/events,补 chat 返回结构 + skill 多选)。
- **P2 Article 单篇面(不含着色)**:view/edit、人工写端点(经 coordinator)、diff、版本时间线、blame、署名聚合、命令面板、toasts、响应式坍缩。
- **P3 作者着色(重)**:provenance→`content::Document` 文章存储集成 + 编辑走 `apply_edit` + 文章按 runs 返回 + 前端三表达着色。**这是 P3 也是最大后端块**。
- **P4 实时事务态**:coordinator 事件 + observe 接入 + busy/排队/轮到你了 微交互闭环。

P0–P2 可在**现有 API + 少量小端点**上完成,先拿到「能用的新前端」;P3/P4 是把设计灵魂补全的后端重活。

## 7. 需要你拍板的决策
1. **保真范围**:做到哪一步?(见 §6)——是否本次就做 P3(作者着色=provenance 集成)与 P4(实时事务态)。这决定体量。
2. **前端工程形态**:Svelte+Vite **SPA**(最简,axum 内嵌)还是 **SvelteKit**(结构化、未来可 SSR)。
3. **部署形态**:axum **内嵌 dist 单二进制**(默认)还是前端独立托管 + CORS。
4. **旧 webui 去留**:重构期保留旧 `src/webui/index.html` 并行,还是直接替换。

## 8. 我的建议
- **分期落地**:P0→P2 先把 Svelte 新前端跑起来(现有 API 够用),P3/P4 作为后续两块(它们各自是真后端工作,值得单独 review)。这样你很快能看到新 UI,而把「作者着色 + 实时事务态」这两块重的留给后端补齐时再上。
- **工程形态**:Svelte+Vite **SPA** + axum `rust-embed` 内嵌(延续单二进制),**SvelteKit 暂不需要**(无 SSR/SEO 诉求)。
- **旧 webui**:重构期**保留并行**(新前端在新路径或新端口),P2 完成、冒烟通过后再切换、删旧。
- 先不动 P3/P4 的后端,等你定范围;P0–P2 我可以直接开。

