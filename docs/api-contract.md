# AI-Write API 契约(前后端共建基线)

> 用途:webui 重构期,**前端(web/ SvelteKit)与后端(src/ axum)两路并行实现到这同一份契约**。
> 现有端点的精确形状以 `src/webui/mod.rs` 为准;本文重点钉**新增/变更**端点(B1/B2/B3)+ SSE 事件形状,让两路对齐。
> 全部走 `/api` 前缀;dev 期 Vite 代理、prod 期 Hono/Bun 反代到 axum。

## 0. 现有端点(保持,前端直接用)

- `GET /api/themes` → `{themes: string[]}`
- `GET /api/themes/{theme}/articles` → `{theme, articles: [{file, title, parent: string|null, depth: number}]}`(阅读顺序)
- `GET|PUT /api/themes/{theme}/config` → `ThemeConfig`(见 §B1 扩展)
- `POST /api/themes/{theme}/reorder` `{order: string[]}`
- `POST /api/themes/{theme}/articles/{file}/parent` `{parent: string|null}`
- `GET /api/skills` → `{skills: [{id, name, description}]}`
- `GET /api/articles/{theme}/{file}` → `{theme, file, content}`(**B2 扩展为 runs**)
- `GET /api/articles/{theme}/{file}/history` → `{history: [{id, author, message, time}]}`(newest-first)
- `GET /api/articles/{theme}/{file}/diff?from&to` → `{diff: string}`(unified patch)
- `GET /api/articles/{theme}/{file}/blame` → `{blame: [{line_no, author, short_sha}]}`
- `POST /api/articles/{theme}/{file}/undo` → `{undone, committed}` 或 `{undone:false, reason}`
- `POST /api/themes/{theme}/chat` `{goal, skill_id?, slave_model?}` → `{outcome, message, reports}`(**B1 扩展**)
- `GET /api/events`(SSE)→ 每行 `data:` 一个 `Event` JSON(externally-tagged enum,如 `{"RoundStarted":{...}}`)
- `author` 字段统一形如 `"<name> <email>"`,name 为 `"human"` 或 `"<model-id>/<label>"`;前端按 name 解析作者身份与颜色。

## B1 小端点(后端先行,前端按此建类型)

1. **人工写正文**(经 coordinator 单事务,人 writer):
   `PUT /api/articles/{theme}/{file}` body `{text: string}` → `{theme, file, committed: string|null}`(committed=短 sha)。
   行为:走 `Coordinator::submit`(人优先级,声明锁={article,index.json},单 commit)。
2. **多 skill + 结构化编排返回**:
   `POST /api/themes/{theme}/chat` body 新增 `skill_ids?: string[]`(优先于 `skill_id`;有序栈,last-wins,见 `multi-skill-design.md`)。
   返回新增 `plan`:`{ created: [{theme, file, title, parent: string|null}], dispatched: [{file, writer, status, summary}] }`(从 run_goal 的产物/reports 整理)。完整返回 `{outcome, message, reports, plan}`。
3. **贡献占比聚合**(署名卡用):
   `GET /api/articles/{theme}/{file}/contributions` → `{contributions: [{author, label, pct, lines}]}`(基于 git blame 统计;pct 取整、和=100)。
4. **去内嵌 HTML**:删 `GET /`(`index_page`)与 `index.html` 嵌入;axum 退化为纯 API。`POST /api/tasks`(单篇 run_one)可保留或删,前端不依赖它。

`ThemeConfig` 扩展:`{description, default_skill: string|null, default_skill_ids?: string[], slave_model: string|null}`(`default_skill_ids` 为多 skill 栈;保留 `default_skill` 兼容)。

## B2 作者着色(provenance→content::Document 文章存储集成)

文章存储从纯文本迁到 `content::Document`(字级作者游程);编辑工具走 `provenance::apply_edit`;落盘用 DSL serialize。文章 GET 扩展为可返回 runs:
- `GET /api/articles/{theme}/{file}` 默认仍回 `{theme, file, content}`(向后兼容);
- 新增 `?format=rich` → `{theme, file, blocks: [{kind, runs: [{text, author}]}], authors: [{id, label}]}`,其中每个 `run.author` 是作者 tag(`"human"` / `"<model-id>/<label>"`)。前端按 author tag 映射颜色(色板见 §颜色)。
- 人工写 `PUT`(B1)与 AI 编辑都经 `apply_edit`,字级作者随之持久化。

## B3 实时事务态(coordinator 可观测)

coordinator 新增锁/队列事件,经 `observe::EventSink` 进 `GET /api/events` SSE。新增 `Event` 变体(externally-tagged):
- `TxnAcquired { writer, paths: string[] }` —— 某 writer 取得事务(进临界区)。
- `TxnQueued { writer, ahead: number }` —— 某 writer 入队(human 插队首)。
- `TxnReleased { writer }` —— 事务提交完成、释放。
- `HandoffToHuman { theme, file }` —— 轮到人(队首人请求在 AI 提交后获权)。
前端据此驱动:busy 横幅(AI 持事务)/ 排队中(可取消)/ 轮到你了 横幅 + COORDINATOR 事务块。
另:`POST /api/articles/{theme}/{file}/request-edit`(人请求编辑、插队首)→ `{queued: true, ahead}`;取消 `DELETE .../request-edit`。
webui 写路径(PUT/undo)接 coordinator(收口 kernel-impl-results §3.1)。

## 颜色(作者色板,前后端一致)

author tag → oklch 色(与设计稿 `AI-Write.dc.html` 一致):
- `human`(你)= `oklch(0.44 0.075 255)`(蓝),deco solid
- `deepseek-chat*` = `oklch(0.45 0.08 152)`(绿),deco dotted
- `deepseek-reasoner*` = `oklch(0.47 0.09 62)`(琥珀),deco dashed
- 兜底(未知 model)= 取 model-id hash 落到一组预留色;前端实现一个 `authorColor(tag)`。

## 错误约定
- 失败回非 2xx + `{error: string}`;前端就地展示、可重试。
- SSE 尽力而为,断线前端自动重连、不清已渲染。
