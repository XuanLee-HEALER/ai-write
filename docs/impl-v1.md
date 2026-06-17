# 实现文档 v1:版本管理 + AI 操作可观测 + 展示用 WebUI

> 状态:**设计 + 实现规格**(自主推进;供次日 review)。
> 依据:`product-discussion.md`(§2 协作模型、§3 工作区 libgit2/provenance、可观测)、`impl-v0.md`(已落地的 session/tool/engine)。
> 目标:在 v0 基础上,补齐 ① 版本管理(libgit2)② AI 操作可观测(事件流)③ 展示用 WebUI(把 AI 写作过程可视化)。
> 决策原则:**不再问人,取我能想到的最优解**;所有关键决策汇总在 §7,供 review。

---

## 0. 总体架构决策

```
L1 req      (done)        无状态 client
L2 session  (done)        通用 agentic 引擎  ── 新增:可挂 EventSink 发事件
L2 tool     (done)        工具 + 工作区      ── 新增:写操作经 vcs 提交,带 author
L2 vcs      (新)          libgit2:工作区即 git 仓库,逐次编辑成 commit,history/diff/undo
L2 observe  (新)          Event 枚举 + EventSink:把 AI 的每一步操作流出来
L3 engine   (done)        Master/Slave      ── 新增:发 master/slave 事件
L4 webui    (新, feature) axum + SSE + 内嵌前端:列工作区、触发写作、实时看 AI 操作、看版本/diff
```

**三条主决策**
1. **版本管理 = 工作区即 git 仓库,每次成功编辑 = 一个 commit**(author = 写者 model id / human)。撤销 = 把文章恢复到上一个 commit(文章编辑级 undo);history/diff 走 libgit2。
2. **可观测 = 推送式事件**:session/engine 在关键点向一个 `EventSink` 发结构化 `Event`;默认 no-op,WebUI 注入一个 channel sink 扇出给浏览器。引擎不认识 HTTP。
3. **WebUI = `axum` + SSE + 单页内嵌前端**,gated 在 `webui` feature + 独立 bin。SSE 做实时操作流(比 WebSocket 简单),前端用原生 HTML/JS(无 npm 构建,自包含)。

**依赖与 feature**
- `git2`(libgit2 绑定)→ 归 `blocking` feature(`blocking = ["dep:ureq", "dep:git2"]`),工作区是同步的。
- `webui = ["blocking", "dep:axum", "dep:tokio", ...]`;`src/bin/webui.rs` 用 `#[cfg(feature = "webui")]`。
- `default` 仍 `["blocking"]`(不含 webui,保持精简)。
- 全部 `cargo add` 加,不手改 `[dependencies]`。

---

## 1. 版本管理(`vcs` 模块,libgit2 / git2）

**模型**:工作区根 = 一个 git 仓库。`Workspace::open` 时若无 `.git` 则 `git init`。每篇文章是被版本管理的文件。

**逐次编辑成 commit**:`write_article` / `edit_article` / `apply_edits` 成功落盘后,`vcs` 提交该文章:
- author / committer = 写者身份(`WriterId` → `name <synthetic-email>`,如 `deepseek-v4-flash/slave-1 <agent@ai-write.local>` / `human <human@ai-write.local>`)。
- message = 简短描述(工具名 + 主题/文件,如 `edit(rust/article.md): apply_edits 2 ops`)。
- 只 stage + commit该文件(不 `add -A`),index 文件(`index.json`)也随主题结构变化提交。

**对外 API(草案)**
```rust
pub struct Vcs { repo: git2::Repository }
pub struct CommitInfo { pub id: String, pub author: String, pub message: String, pub time: i64 }
impl Vcs {
    pub fn open_or_init(root: &Path) -> Result<Self, VcsError>;
    pub fn commit_file(&self, rel: &Path, author: &WriterId, message: &str) -> Result<String, VcsError>; // -> short sha
    pub fn history(&self, rel: &Path) -> Result<Vec<CommitInfo>, VcsError>;
    pub fn diff(&self, rel: &Path, from: Option<&str>, to: Option<&str>) -> Result<String, VcsError>; // unified patch
    pub fn restore(&self, rel: &Path, commit: &str, author: &WriterId) -> Result<String, VcsError>;   // 撤销/回退:把文件恢复到某 commit,再提交一次(保留历史)
    pub fn undo_last(&self, rel: &Path, author: &WriterId) -> Result<Option<String>, VcsError>;        // 回退到上一版
}
```

**决策**
- **提交粒度 = 每次编辑工具调用一次**(支撑"文章编辑级 undo")。
- **撤销用"恢复+再提交"而非 `reset`**:历史不被改写(可 archeology),回退本身也是一条记录。
- **provenance**:git author 即文件级溯源(对齐 v0 的文件级 provenance + 字级 provenance 留待 DSL 阶段)。
- git2 默认 vendored libgit2(`libgit2-sys` 自编译);若环境缺 cmake/编译器导致失败,fallback:`cargo add git2 --no-default-features --features ...`(实现阶段按报错处理)。

集成点:`vcs` 由 `tool` 层在写操作成功后调用(`ToolCtx` 持有/可访问 `Vcs`)。同一篇文章的并发由 v0 的内存锁保证单写者,故提交无竞争。

---

## 2. AI 操作可观测(`observe` 模块)

把 AI 的每一步"流"出来,供 UI 实时渲染(对齐 §2 的"人可观察 master/slave 的操作")。

**Event(草案)**
```rust
#[non_exhaustive]
pub enum Event {
    SessionStarted { role: String, system_excerpt: String },
    RoundStarted   { round: u32 },
    ModelMessage   { text: String },                       // 中间/最终 assistant 文本
    ToolCalled     { name: String, args: serde_json::Value },
    ToolResult     { name: String, ok: bool, summary: String },
    EditCommitted  { article: String, author: String, sha: String, diff_summary: String },
    SlaveSpawned   { theme: String, file: String, writer: String },
    SlaveReported  { status: String, summary: String },
    Finished       { outcome: String },                    // Done / NeedHuman / Failed
}
// derive: Debug, Clone, Serialize  (前端按 JSON 渲染)
```

**EventSink**
```rust
pub trait EventSink: Send + Sync { fn emit(&self, event: Event); }
pub struct NullSink;                       // 默认:丢弃
// webui 提供 channel-backed sink:emit → broadcast → 各 SSE 订阅者
```

**接线**:`Session` 持有 `Arc<dyn EventSink>`(默认 `NullSink`),在 build/round/tool/finish 处 `emit`;`engine` 在派生 slave / 收 report 处 emit。**引擎只认 `EventSink`,不认 HTTP/SSE**(解耦)。这同时就是 §2 的"命令/操作日志"的运行时来源。

**决策**:推送式(sink)而非轮询;事件 `Serialize` 成 JSON;session 的 `run_*` 新增可选 sink(不破坏 v0 的纯离线测试——`NullSink` 默认)。

---

## 3. 展示用 WebUI(`webui` feature + `src/bin/webui.rs`)

**栈**:`axum`(async)+ `tokio`(rt-multi-thread)+ SSE;前端 = **单页内嵌 HTML/JS**(`include_str!`,无构建步骤)。

**同步引擎 × 异步服务的桥**:写作任务在 `tokio::task::spawn_blocking`(或 `std::thread`)里跑同步的 Master/Slave;事件经 `tokio::sync::broadcast` 从阻塞侧推到各 SSE 订阅者。引擎侧的 `EventSink` 实现就是"往 broadcast sender 发"。

**HTTP 接口(草案)**
| 方法 路径 | 作用 |
|---|---|
| `GET /` | 内嵌单页(列工作区 + 触发写作 + 实时操作流 + 历史/diff) |
| `GET /api/themes` | 列主题 |
| `GET /api/themes/:theme/articles` | 列文章(读索引) |
| `GET /api/articles/:theme/:file` | 文章当前内容 |
| `POST /api/tasks` | `{theme,file,task}` → 起一篇写作,返回 task_id;后台 spawn_blocking 跑 Master→Slave |
| `GET /api/events` | **SSE**:实时推 `Event` JSON(AI 操作可视化的数据源) |
| `GET /api/articles/:theme/:file/history` | git 历史(版本列表) |
| `GET /api/articles/:theme/:file/diff?from=&to=` | 两版本间 unified diff |
| `POST /api/articles/:theme/:file/undo` | 撤销到上一版(`vcs.undo_last`) |

**前端(单页)要素**:① 左侧主题/文章树;② "新建写作"表单(主题+文件+任务)→ POST /api/tasks;③ 中间"AI 操作流"面板:`EventSource('/api/events')` 实时渲染 RoundStarted / ToolCalled / EditCommitted… + 文章内容随之刷新;④ 右侧"版本"面板:history 列表 + 选两版看 diff + undo 按钮。样式从简(内联 CSS),重在把过程**讲清楚、可视化**。

**决策**:SSE(非 WebSocket,单向够用且简单);前端零依赖内嵌(契合 Rust 项目、无 npm);webui 严格 feature-gated,默认构建不含。

---

## 4. 模块 / feature / 依赖布局

```
src/
  req/ session/ tool/ engine/      (已存在;session/tool 小幅扩展)
  vcs/        (新)  git2 封装
  observe/    (新)  Event + EventSink
  bin/
    demo.rs                         (已存在)
    webui.rs   #[cfg(feature="webui")]  axum 服务 + 内嵌前端
  webui/                            (新, 可选) 前端静态文件(若不内联则放这,优先 include_str! 内嵌)
```
- `blocking = ["dep:ureq", "dep:git2"]`
- `async = [...]`(不变)
- `webui = ["blocking", "dep:axum", "dep:tokio", "dep:tower-http"(可选,静态/trace)]`,tokio 开 `rt-multi-thread,macros,sync`
- `default = ["blocking"]`

---

## 5. 落地顺序(workflow 阶段,顺序流水线)

1. **vcs**:git2 封装(open_or_init / commit_file / history / diff / undo)+ 完整测试(用 tempdir git 仓库)。
2. **tool×vcs 集成**:写/编辑工具成功后提交,带 author;`ToolCtx` 暴露 `Vcs`;补测试(编辑→产生 commit、author 正确、undo 生效)。
3. **observe**:Event + EventSink(+ NullSink);接进 session 与 engine 的关键点;测试(用一个记录型 sink 断言事件序列)。
4. **webui 后端**:`webui` feature + axum 服务 + 全部 API + SSE + spawn_blocking 桥 + broadcast sink。
5. **webui 前端**:内嵌单页(工作区树 / 起任务 / 实时操作流 / 历史 + diff + undo)。
6. **集成 + 验收 + 文档**:`just ci`/`just test`/`just doc`(含 `--features webui`)全绿;写 `docs/impl-v1-results.md`(各阶段决策 + 结果 + 已知缺口)。

---

## 6. 验收标准

- `just ci`、`just test`、`just doc` 全绿(含 webui feature);无 `todo!()` 在可达路径。
- doc 标准对齐 req 模块(英文 rustdoc、`# Errors`/`# Examples`(IO 用 `no_run`)、production-ready)。
- vcs / observe / tool×vcs 有充分单测;编辑真的产生 git commit 且 author 正确、undo 可回退。
- webui:`just webui` 起服务,浏览器能列工作区、起一篇写作、**实时看到 AI 操作流**、看版本历史与 diff、undo。(真机写作冒烟由我做。)
- 留下 `docs/impl-v1.md`(本文)+ `docs/impl-v1-results.md`(实现期决策与结果)。

---

## 7. 关键决策清单(供 review)

| # | 决策 | 取舍 |
|---|---|---|
| V1 | 工作区即 git 仓库,**逐编辑提交**,author=写者 | 细粒度历史 + 文件级 provenance;commit 数会多 |
| V2 | 撤销 = **恢复+再提交**(不 reset) | 历史不改写、可 archeology;代价是多一条回退记录 |
| V3 | git2 默认 vendored libgit2 | 自包含;依赖系统有编译器/cmake |
| V4 | 可观测 = **推送式 EventSink**,引擎不认 HTTP | 解耦;UI 注入 channel sink |
| V5 | WebUI = **axum + SSE + 内嵌单页** | 简单、自包含、无 npm;非 WebSocket |
| V6 | webui **feature-gated** + 独立 bin | 默认构建精简;webui 才拉 axum/tokio |
| V7 | 字级 provenance / DSL **仍 deferred** | v1 仍文件级(git author);DSL 单独阶段 |
| V8 | 同步引擎 × 异步服务用 **spawn_blocking + broadcast** 桥 | 不改引擎为 async;隔离清晰 |


