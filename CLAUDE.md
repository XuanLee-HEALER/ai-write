# ai-write

Rust 实现的 AI 辅助写作工具,模型用 DeepSeek V4(`deepseek-v4-flash` + `deepseek-v4-pro` 组合)。

## 工具链 / 任务运行

- **toolchain:cargo**(stable,当前 rustc/cargo 1.96)。
- **task runner:just**,根目录 `justfile`,常用命令收进 recipe(`just build` / `just test` / `just fmt` / `just lint` / `just check` / `just ci`)。
- **高频任务先写 recipe 再调**:format / lint / check / test / build 这类反复跑的任务,先在 `justfile` 落成 recipe,再用 `just <recipe>` 执行;**不直接敲 `cargo fmt` / `cargo clippy` 等裸命令**。一次性排查(`cargo tree`、`cargo add` 等)可直接用。

## 操作约定(重要)

**能用工具完成的,绝不自己手编辑文件:**

- 加 / 删依赖 → `cargo add` / `cargo remove`,**不手改 `Cargo.toml` 的 `[dependencies]`**。
- 建项目 / crate → `cargo init` / `cargo new`,不手搓骨架。
- 依赖版本 → `cargo add` 自动锁当时最新 stable,不手写版本号(要旧版才显式 `cargo add foo@x.y`)。
- 跑构建 / 测试 / 格式化 / lint → 走 `just` recipe 或 `cargo` 子命令,不手动拼。
- `Cargo.lock`、`target/` 等生成物不手动碰。
- 只有没有对应工具能生成的文件(`justfile`、`CLAUDE.md`、`docs/*.md`、源码逻辑)才手写。

## 代码注释 / 文档规范

- **公共接口(`pub` 项)的文档注释、module doc(`//!`)一律用英文**,写全:用途、参数、返回值、错误(`# Errors`)、`# Examples`(会发网络请求的示例用 ` ```no_run `)、必要时 `# Panics`。目标是 `cargo doc` 直接产出 production-ready 文档。
- 内部实现注释、`docs/` 设计文档、本文件可用中文。

## 文档

`docs/` 下的设计与实现文档,按主题分组:

**产品与核心设计**

- `docs/product-discussion.md` —— AI–人协作写作的上层业务讨论稿(产品定位与协作机制)。
- `docs/design.md` —— 最初的设计思考(req 13 点 + session 流程 + 上层业务愿景)。
- `docs/deepseek-api-research.md` —— DeepSeek V4(flash / pro)API 实测调研。
- `docs/req-module-design.md` —— req module(无状态 wrapper)设计定稿。
- `docs/coordinator-design.md` —— Coordinator 事务并发控制设计(对齐内核 §6)。
- `docs/api-contract.md` —— 前后端共建的 JSON/SSE API 契约基线。
- `docs/multi-skill-design.md` —— 多 skill 栈语义(内核 §10 开放问题)。
- `docs/sidecar-resources.md` —— sidecar 资源(如 `.prov.json`)约定(对齐内核 §7)。
- `docs/ui-ux-design-draft.md` —— UI/UX 设计稿草稿。

**内核对齐**

- `docs/kernel-gap-analysis.md` —— 内核抽象 vs 当前实现的 diff 清单(待 assign 的 gap)。
- `docs/kernel-impl-results.md` —— 内核 gap(G1–G14)的实现结果。

**分阶段实现(计划 + 结果)**

- `docs/impl-v0.md` —— v0:session + tool 两大模块 + engine 薄层 prototype。
- `docs/impl-v1.md` / `docs/impl-v1-results.md` —— v1:版本管理 + AI 操作可观测 + 展示用 WebUI(计划 / 结果)。
- `docs/impl-v2.md` —— v2 总览:content / DSL / provenance / LLM 编排(三 worktree 并行)。
- `docs/impl-v2-dsl.md` —— v2·DSL 层实现。
- `docs/impl-v2-provenance.md` —— v2·provenance(字级作者层)实现。
- `docs/impl-v2-orchestration.md` —— v2·LLM 编排实现。
- `docs/impl-v2-results.md` —— v2 实现结果。
- `docs/impl-v3.md` / `docs/impl-v3-results.md` —— v3:webui 重构 + 文章层级 + skill 系统 + master 对话接入(计划 / 结果)。

**WebUI 重构**

- `docs/webui-refactor-assessment.md` —— webui 重构评估(前后端分离:Svelte + shadcn + Tailwind)。
- `docs/webui-rebuild-results.md` —— webui 重构结果(双轨 workflow + 主控 review)。

## 代码结构

后端 Rust crate(`src/`,feature:`blocking`(默认)/ `async` / `webui`):

```
src/
├── lib.rs                 crate 根:模块声明 + feature gate 注释
├── req/                   L1·无状态 DeepSeek client wrapper(始终可用)
│   ├── mod.rs             门面:Client 入口 + 重导出
│   ├── blocking.rs        同步后端(ureq,默认 feature)
│   ├── client_async.rs    异步后端(reqwest,async feature)
│   ├── protocol.rs        后端无关编解码:请求编码 + chat/models/balance 解析 + SSE 行解码
│   ├── model.rs           Model 枚举(V4Flash / V4Pro + Pinned 快照逃逸口)
│   ├── error.rs           错误归一:Error / ApiError / TransportError
│   └── types/             请求 / 响应数据类型(Message/Role/Tool/ChatRequest/ChatResponse/Usage…)
├── content/mod.rs         L2·富文本纯数据契约(Document/Run/Block/AuthorId,无 IO)
├── dsl/mod.rs             L2·行式富文本 DSL(parse / serialize / render_html)
├── provenance/mod.rs      L2·字级作者编辑原语(apply_edit + contributors/diff 查询)
├── skill/mod.rs           L2·写作 skill 系统(从 `.md` 加载,多 skill 栈 last-wins)
├── session/mod.rs         L2·通用 agentic 回合引擎(历史 / tool dispatch / finish_reason)[blocking]
├── tool/                  L2·工具系统 + 工作区模型 [blocking]
│   ├── mod.rs             Tool trait + ToolRegistry + ToolCtx
│   ├── tools.rs           17+ 写作工具(主题/文章 lifecycle、读写编辑、history/diff/blame/undo)
│   └── workspace.rs       工作区模型(主题=目录 / 文章=文件、单写锁、路径沙箱、Index manifest)
├── coordinator/mod.rs     L2·事务并发控制(全有全无锁、无死锁、人队首、临界区内单次 commit、独占 Vcs)[blocking]
├── vcs/mod.rs             L2·libgit2 版本化(逐编辑 commit、署名、history/diff/restore/undo)[blocking]
├── observe/mod.rs         L2·推送式事件流(Event + EventSink,默认 NullSink)[blocking]
├── search/mod.rs          L2·可插拔搜索(SearchProvider trait + StubProvider,正交于本地 find)[blocking]
├── engine/                L3·Master/Slave 编排 [blocking]
│   ├── mod.rs             Master 派 Slave 写单篇、SlaveTask/SlaveReport、SLAVE_OPERATIONAL_RULES
│   └── orchestration.rs   LLM 编排(Master 用工具 run goal、计划 / 汇报)
├── webui/mod.rs           L4·axum 纯 JSON/SSE API 后端(AppState + 路由,sync 引擎经 spawn_blocking 桥接)[webui]
└── bin/
    ├── demo.rs            CLI 冒烟:demo <主题> <任务>,派一个 Slave 写文章 [blocking]
    └── webui.rs           后端 API 服务器入口(读 env + AppState::from_env)[webui]
```

前端 `web/`(SvelteKit + Svelte 5 + Tailwind 4 + shadcn-svelte,Bun 跑):

```
web/
├── package.json           前端依赖与脚本(bun run dev/build/check/start)
├── svelte.config.js       SvelteKit 配置(adapter-node)
├── vite.config.ts         Vite 配置(dev :5173,/api[含 SSE]代理到后端 :8080)
├── tsconfig.json          TypeScript 配置
├── components.json        shadcn-svelte 组件配置
├── server.ts              生产服务器(Hono on Bun:挂 adapter-node 产物 + 反代 /api)
├── static/robots.txt      静态资源
└── src/
    ├── app.html           HTML 外壳
    ├── app.css            全局样式(Tailwind + 主题 token)
    ├── app.d.ts           全局类型声明
    ├── hooks.server.ts    SvelteKit 服务端 hooks(/api 转发到后端)
    ├── routes/            页面路由
    │   ├── +layout.svelte / +layout.ts        全局布局(AppShell)+ 布局 load
    │   ├── +page.svelte                        首页(落地 / 主题入口)
    │   ├── t/[topic]/+page.(svelte|ts)         TOPIC 视图:master 对话编排 + 文章树产物
    │   └── a/[theme]/[file]/+page.(svelte|ts)  ARTICLE 视图:单篇工作面(编辑 + 活动 / 版本)
    └── lib/
        ├── index.ts                  lib 桶导出
        ├── utils.ts                  通用工具(cn 等)
        ├── api/client.ts             后端 API 客户端(fetch + SSE 订阅,对齐 api-contract)
        ├── api/types.ts              API wire 类型(前端视角的契约)
        ├── article.ts                ARTICLE 视图 view-model 派生(纯函数,F2)
        ├── topic.ts                  TOPIC 视图 view-model 派生(纯函数,F1)
        ├── author.ts                 作者身份 → 视觉编码(色 / 纹理 / 标签,非纯色彩 a11y,F3)
        ├── txn.ts                    coordinator 事务态 reducer(idle/ai-busy/queued/your-turn,F4)
        ├── stores/                   Svelte 5 runes 全局状态
        │   ├── workspace.svelte.ts   工作区状态(主题 / 文章 / 选中)
        │   ├── connection.svelte.ts  SSE 连接状态
        │   ├── layout.svelte.ts      布局态(分栏)
        │   ├── palette.svelte.ts     命令面板态
        │   └── toast.ts              toast 通知
        └── components/
            ├── shell/                应用骨架(AppShell / CommandPalette / ConnectionIndicator)
            ├── navigator/            左侧文章树导航(Navigator / TreeRow)
            └── ui/                   shadcn-svelte 基础组件脚手架(button/dialog/select/tabs/…)
```
