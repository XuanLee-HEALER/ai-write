# webui 重构结果(双轨 workflow + 主控 review)

> 来源:`docs/webui-refactor-assessment.md` 的 P0–P4,经一条**双轨并行** opus workflow 实现(后端 `src/` B1→B2→B3 ‖ 前端 `web/` F0→F1→F2→F3→F4),两路对齐 `docs/api-contract.md`。
> **主控独立校验**:后端 `just ci/test/doc` 全绿(**330 单测 + 6 集成[2 live ignored] + 72 doctest**);前端 `bun run build` + `bun run check`(svelte-check **0 error / 0 warning / 953 文件**);**真机视觉 review**(`just webui` + `just web-dev`,Chrome 比对设计稿)。

---

## 1. 交付一览(8/8 阶段 green)

**后端(`src/`,纯 API)**
- **B1**:`PUT /api/articles/..`(人工写,经 Coordinator 单事务)、`POST /chat` 收 `skill_ids[]` + 返回结构化 `plan{created,dispatched}`、`GET .../contributions`(blame 聚合,largest-remainder 取整和=100)、**删内嵌 HTML(`GET /` + index.html)→ 纯 JSON/SSE API**、`ThemeConfig.default_skill_ids`。
- **B2**(最大块):文章字级作者集成。`From<WriterId> for AuthorId`;所有写路径(三编辑工具 + 人工 PUT + coordinator TxnCtx)经 `provenance::reauthor` 记录字级作者,持久化到**每文 sidecar `<file>.prov.json`**(纯文本文件仍是 SSOT、git/blame 不变);`GET ...?format=rich` 返回 `{blocks:[{kind,runs:[{text,author}]}],authors}`。
- **B3**:`observe::Event` 加 `TxnAcquired/TxnQueued/TxnReleased/HandoffToHuman`,Coordinator 持 EventSink 发事件、走 SSE;`POST/DELETE .../request-edit`(人插队首/取消);webui 写路径接 Coordinator(收口 kernel-impl §3.1)。

**前端(`web/`,SvelteKit5+Vite+Tailwind4+shadcn-svelte,bun)**
- **F0**:shadcn-svelte(纸色主题)、**Hono on Bun 生产服务器 `web/server.ts`**(挂 adapter-node + 反代 /api 含 SSE)、API client + stores(workspace/connection[SSE]/layout[响应式]/palette/toast)、路由 `/ ` `/t/[topic]` `/a/[theme]/[file]`、Navigator(真数据 + 层级 + 拖拽重排/改父 + 检索)、⌘K 命令面板、sonner toasts、响应式坍缩。
- **F1** Topic 编排面、**F2** Article 单篇面(view/edit、工具栏、diff、版本时间线、blame、署名卡)、**F3** 作者着色(色/纹理/标签,默认关仅 view)、**F4** 实时事务态(busy/排队可取消/轮到你了 + COORDINATOR 块)。

## 2. 主控 review:发现并修复的 2 个真 bug

绿构建/类型检查抓不到的**运行时**问题,真机 review 抓出:
1. **致命:`$effect` 死循环**(`effect_update_depth_exceeded`)—— `+layout`、topic、article 三处「load 数据→store」同步 effect 调 `setThemes`/`setArticles`,而这些方法内部 `this.topics.map` 读 + 赋值写**同一 `$state`**,在 effect 里读写同源 → 无限重跑,**Topic/Article 视图根本渲染不出来**(只剩欢迎态)。**修**:三处 store 写入用 `untrack()` 包裹,effect 只依赖 `data.*`。
2. **SSE 卡 `connecting`**:后端 SSE keepalive 默认 15s,空闲 15s 无字节 → vite 代理缓冲、头不透传 → `EventSource` 不 `onopen`。**修**:后端 keepalive 降到 2s(字节即时流,代理立即转发)→ 指示灯转 **● live**。

修后两路门禁仍全绿;真机 welcome→topic→article 全程**零控制台报错**。

## 3. 真机视觉结论

- Topic 编排面、Article 单篇面**均忠实于 `AI-Write.dc.html`**:亮暖纸感、衬线、accent #7a2e2e、三区布局、工具栏(阅读/编辑、作者着色、格式组、三栏/合并)、COORDINATOR 事务块、版本时间线、blame、**共有署名卡(贡献占比 + 诚实边界「不提供已验证人类原创徽章」)**。
- rich 端点出 runs(B2 在线);旧文章无 sidecar 时优雅降级为单作者游程。

## 4. 已知缺口 / 契约对齐 / 待续

- **F3/F4 仅渲染态已验**:作者着色多色、busy/排队/轮到你了 的**动态行为**需一次**真机写作 run**(会调 DeepSeek)才能完整触发;当前 demo 文章单作者、无活跃事务,故只验证了静态渲染 + 无报错。建议后续 `just demo` 跑一篇多作者协作再过一遍。
- **契约对齐(B1 flag)**:`plan.dispatched[i].writer` 是后端按派发顺序重构的 `slave_model/slave-<n>`(SlaveReport 不带 writer/file);如要精确每篇 writer,需让 `SlaveReport` 携带其 file/writer——记一笔。
- **B2 sidecar 决策**:字级作者存 sidecar、**不入 commit**(避免破坏现有声明锁/单 commit 断言);纯文本文件 + git 仍是 SSOT。若要字级作者也版本化,是后续单独一块。
- **生产 Hono 服务器**:已产出 `web/server.ts` + `just web-serve`,但我本轮视觉 review 走的是 `just web-dev`(vite);Hono 生产路径需单独冒烟一次。
- model id 钉日期快照(G12)已在后端;前端 writer model 选择器列表可对齐到 dated snapshot。

## 5. 运行
- 后端域 API:`just webui`(:8080,纯 API)。前端 dev:`just web-dev`(:5173,代理 /api)。生产:`just web-build` + `just web-serve`(Hono/Bun)。
- **未提交**(按约定等你发话)。
