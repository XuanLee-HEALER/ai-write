# 实现结果 v1:版本管理 + AI 操作可观测 + 展示用 WebUI

> 状态:**已落地,全绿**(供次日 review)。
> 对应规格:`docs/impl-v1.md`(§1 vcs / §2 observe / §3 webui)。
> 本文记录**实际做出的决策**(含理由)、最终对外 API / 路由面、新增测试覆盖、运行方式、以及已知缺口 / TODO / 取巧点。
> 校验基线(本机,2026-06-18):`just ci`、`just test`、`just doc` **三者全绿**;无 `todo!()`/`unimplemented!()` 在可达路径;**全程未发起任何 DeepSeek 真机调用**。

---

## 0. 验收快照

| 校验 | 结果 |
|---|---|
| `just ci`(fmt-check + clippy `--all-features -D warnings` + check `--all-features`) | ✅ 绿 |
| `just test`(`cargo test --all-features`) | ✅ 107 lib + 6 integration(2 个 live `#[ignore]`,未动)+ 26 doctests,全过 |
| `just doc`(`RUSTDOCFLAGS=-D warnings cargo doc --no-deps --all-features`) | ✅ 绿,rustdoc 干净 |
| 可达路径 `todo!`/`unimplemented!`/`unreachable!` | ✅ 无 |
| 真机 DeepSeek 调用 | ✅ 全程零(测试用 loopback TCP 假服务 / 离线 client) |

**新增测试净增量**(相对 v0 的 64 lib):vcs +15、tool×vcs +10、observe +8、webui +10 = +43,合计 107 lib。doctests 26(v0 17 + 9 新)。

**feature 布局(最终,均经 `cargo add` 加,未手改 `[dependencies]`)**
```toml
default = ["blocking"]
blocking = ["dep:ureq", "dep:git2"]
async   = ["dep:reqwest", "dep:futures-core", "dep:async-stream", "dep:futures-util"]
webui   = ["blocking", "dep:axum", "dep:tokio", "dep:tokio-stream"]
```
`git2` 归 `blocking`(工作区同步);`webui` 拉 `axum`/`tokio`/`tokio-stream`。`default` 不含 `webui`,保持精简。**未引入 `tower-http`**(规格标"可选"):SSE 用 axum 内置 `response::sse`,`/` 用 `include_str!` 内嵌单页,无需静态文件 / trace 层。

---

## 1. 版本管理(`vcs` 模块,libgit2 / git2)

**文件:** `src/vcs/mod.rs`,gated `#[cfg(feature = "blocking")] pub mod vcs;` in `src/lib.rs`。

### 对外 API(最终签名)
```rust
pub struct Vcs { /* wraps git2::Repository */ }

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VcsError {
    Git(#[from] git2::Error),   // "git error: {0}"
    NonUtf8Path(PathBuf),       // "path is not valid UTF-8: {0}"
    NoHistory(String),          // "no such history: {0}"
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommitInfo {
    pub id: String,       // short SHA(10 hex)
    pub author: String,   // "<name> <email>"
    pub message: String,
    pub time: i64,        // author Unix 时间戳(秒)
}

impl Vcs {
    pub fn open_or_init(root: &Path) -> Result<Self, VcsError>;
    pub fn commit_file(&self, rel: &Path, author: &WriterId, message: &str) -> Result<String, VcsError>;
    pub fn history(&self, rel: &Path) -> Result<Vec<CommitInfo>, VcsError>;
    pub fn diff(&self, rel: &Path, from: Option<&str>, to: Option<&str>) -> Result<String, VcsError>;
    pub fn restore(&self, rel: &Path, commit: &str, author: &WriterId) -> Result<String, VcsError>;
    pub fn undo_last(&self, rel: &Path, author: &WriterId) -> Result<Option<String>, VcsError>;
}
```
签名与 §1 草案一致。`CommitInfo` 额外 derive `Deserialize`(规格只要 `Serialize`,加 `Deserialize` 便于 WebUI round-trip,纯附加)。

### 决策
- **V1 — 逐编辑提交,author = 写者。** `commit_file` 只 `index.add_path(rel)`(绝不 `add_all`/`-A`),`index.json` 是独立的一次 `commit_file`("一次调用 = 一个 commit")。
- **WriterId → git 签名映射**:`name` 即文件级 provenance tag——`Human`→`"human"`、`Agent{model,label}`→`"<model>/<label>"`(如 `deepseek-v4-flash/slave-1`),与 `ArticleMeta::contributors` 1:1 对齐;`email` 合成且稳定(`human@ai-write.local` / `agent@ai-write.local`),仅为 libgit2 语法合法,永不投递。
- **V2 — 撤销 = 恢复 + 再提交,绝不 `reset`。** `restore` 把历史 blob 写回工作树并新提交一次;`undo_last` 目标是 `history(rel)[1]`(上一版),不足 2 个 commit 返回 `Ok(None)`。历史不改写、可 archeology。
- **首个提交是 root commit**(无 parent,经 unborn-HEAD 分支);后续 parent off HEAD。
- **`diff` 语义**:`from=None`→空树(整文件当新增);`to=None`→工作树(未提交改动);两者 `Some`→tree-to-tree;pathspec 限定 `rel`;无法解析的 revision 映射为 `NoHistory`(非裸 git 错误)。
- **`history`** topo+time 遍历 HEAD,只保留该文件 blob 相对 parent 有变化(或 root 首现)的 commit;未跟踪文件 → 空 vec(非错误)。
- **V3 — git2 vendored,无需 fallback。** 环境有 cmake/clang/pkg-config,`git2 v0.21`(libgit2-sys 1.9.4 vendored)直接 build 通过。`cargo add --optional` 产生的多余 `git2 = [...]` 子 feature 已在 `[features]` 表(非 `[dependencies]`)折进 `blocking`。

### 测试(15)
覆盖:只 stage 命名文件(`commit_only_stages_the_named_file`)、首提交为 root、undo 内容回退且历史 +1、diff 三种端点语义、history 顺序与去重、非 UTF-8/无历史错误路径等。均用 tempdir git 仓库。

---

## 2. tool × vcs 集成(`docs/impl-v1.md` §1 "集成点")

**文件:** `src/tool/mod.rs`、`src/tool/tools.rs`、`src/tool/workspace.rs`。

### 接线
- **`ToolCtx` 新增 `vcs: Option<&'a Vcs>`。** `ToolCtx::new(ws, writer)` 签名**不变**(`vcs: None`),所有 v0 调用点 / 测试逐字编译;新建造器 `with_vcs(self, &Vcs) -> Self` 挂句柄。无 Vcs 时行为 = v0(写盘不提交)。
- **`Session` 持有惰性 `Vcs`**:新增 `vcs: Option<Vcs>`,`new`/`restore` 置 `None`,`set_workspace` 重置 `None`(换 root 重开仓库)。`dispatch_tool_calls` 里工作区打开后 `Vcs::open_or_init(ws.root())` 跑一次复用;每轮 `ToolCtx` 用 `.with_vcs(&vcs)`。`workspace: &mut` 与 `vcs: &` 是 `Session` 的不相交字段,借用干净。**Vcs 打开失败非致命**(留禁用,编辑仍落盘)。
- **三个编辑器(`WriteArticle`/`EditArticle`/`ApplyEdits`)写盘成功后调 `ctx.commit_article(theme, file_name, message)`。** 无 Vcs 时是返回 `Ok(None)` 的 no-op,故无条件调用。有 Vcs 时产生**两个 commit**:文章文件,再 `<theme>/index.json`(若存在)。message 形如 `edit(t/a.md): apply_edits 2 ops`、`index(t): record edit to a.md`。成功 payload 增 `"committed": "<short-sha>"`(Vcs 关时该字段缺省,非 null)。

### 新增 model-facing 工具(注册数 13→16)
- **`article_history`** `{theme, file_name}` → `{history:[CommitInfo…]}`(最新在前)。
- **`article_diff`** `{theme, file_name, from?, to?}` → `{diff:"<unified patch>"}`。`from`/`to` 是 commit id 或 `HEAD~n`。
- **`undo_last`** `{theme, file_name}` → `{undone, committed}` 或 `{undone:false, reason}`。**持锁**——它绕过 `write_article` 改文件,故经新公开 `Workspace::ensure_lock_held`(私有 `require_lock` 的薄公开包装)守单写者不变量。
- 三者在无 Vcs 时返回新增的 `ToolError::Vcs(String)`(`#[non_exhaustive]` 枚举,纯附加,不破坏 match)。

### 决策
- **T1** — Vcs 在 `ToolCtx` 上 opt-in(`Option<&Vcs>`),不强制:保 v0 签名 / 测试,工具层无 git 也可用。(否决"Vcs owned on Workspace"——会让 `Workspace` 非 `Sync` 并把沙箱模型缠进 libgit2。)
- **T2** — 每次编辑两个 commit(文章、index),各自独立:守 "一次调用 = 一个 commit"。**后果**:index commit 会插在文章 commit 之间,故 `HEAD~1` **不是**上一文章版——model 应拿 `article_history` 的 id 喂 `article_diff`。
- **T3** — 提交失败经 `ToolError::Vcs` 上抛,但只在写盘已落地之后:编辑对磁盘恒久,model 看到 git 侧失败可反应。
- **T4** — Vcs 打开失败在 session 层静默非致命。
- **T5** — `undo_last` 持文章锁(它走 Vcs 侧信道改文件);`article_history`/`article_diff` 只读、无锁。
- **T6** — 每 session / 每线程独占 `Vcs`:slave 各自在自己 root 上开句柄,非 `Sync` 的 repo 不跨线程共享,内存锁已串行化同文件编辑。

### 测试(10)
作者正确的单提交、index 单独提交、apply_edits op 计数 message、无 Vcs 仍写盘不提交(v0 行为)、history 顺序、diff 跨版本、undo 回退且历史 +1、undo 需持锁、单版本 undo 报无可撤销、Vcs 关时 history 工具报错。

---

## 3. AI 操作可观测(`observe` 模块,`docs/impl-v1.md` §2)

**文件:** `src/observe/mod.rs`,gated `#[cfg(feature = "blocking")]`;接进 `src/session/mod.rs`、`src/engine/mod.rs`。

### 对外 API(最终)
```rust
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub enum Event {
    SessionStarted { role: String, system_excerpt: String },
    RoundStarted   { round: u32 },
    ModelMessage   { text: String },
    ToolCalled     { name: String, args: serde_json::Value },
    ToolResult     { name: String, ok: bool, summary: String },
    EditCommitted  { article: String, author: String, sha: String },
    SlaveSpawned   { theme: String, file: String, writer: String },
    SlaveReported  { status: String, summary: String },
    Finished       { outcome: String },   // "done" | "need_human" | "failed"
}
pub trait EventSink: Send + Sync { fn emit(&self, event: Event); }
#[derive(Debug, Clone, Copy, Default)] pub struct NullSink;  // no-op
```
`Event` 只 derive `Serialize`(前端按 JSON 渲染);`args` 用 `serde_json::Value`(已解析),UI 直接拿结构化参数。

> **与规格的偏差(已记):** §2 草案的 `EditCommitted` 含 `diff_summary` 字段,**实现去掉了它**——commit 事件由工具结果 payload 的 `{edited, committed}` 派生(见 O4),避免在事件发射点再算一次 diff;UI 需 diff 走 `/api/articles/.../diff` 端点。这是有意取舍,非遗漏。

### 发射点
- **Session**:新字段 `events: Arc<dyn EventSink>`(默认 `NullSink`)、`role: String`、`round: u32`;公开 `set_event_sink(role, sink) -> &mut Self`、`event_sink() -> Arc<dyn EventSink>`。`SessionStarted`(round 0→1 一次)、`RoundStarted`(每轮)、`ModelMessage`(非空 assistant 文本)、`ToolCalled`/`ToolResult`(每次 dispatch 前后,summary ≤200 字)、`EditCommitted`(结果含 `edited`+`committed` 时,author = `provenance_tag()`)、`Finished`(经私有 `finish(step)` 漏斗,每终态轮恰一次)。
- **Engine**:新公开 `spawn_slave_with_sink(client, root, task, sink)`;`spawn_slave` 变薄包装传 `NullSink`(签名不变)。slave 线程首行 `SlaveSpawned`、末行 `SlaveReported`;`Master::run_one` 把 master 的 `event_sink()` 传给 slave,master+slave 事件汇入一条 feed。

### 决策
- **O1** 默认 `NullSink`、setter opt-in:可观测透明,v0 离线路径逐字不变,snapshot 不受影响。
- **O2** sink 为 `Arc<dyn EventSink>`,master→slaves 共享:一个 sink 多路复用;`Send+Sync` 必需(slave 跨线程发)。
- **O3** 单 `Finished` 漏斗:每 `run_round` 出口经 `finish(step)`,终态恰一次,非终态不发。
- **O4** `EditCommitted` 由工具结果 payload 派生,**不**新增工具层 hook:编辑器已回显 `{edited, committed}`,session 读这两字段——工具层公开 API 不动,事件 sha 与真正落地的一致。
- **O5** `WriterId::provenance_tag` 提为 `pub`(附加):事件 author 即写者 provenance,且 = git author = index contributor,提为公开避免第三处重实现。
- **O6** `spawn_slave_with_sink` 新增、`spawn_slave` 保留:生命周期事件在 slave 线程发,无论何时 join 都能 bracket。
- **O7** 无网络全流程测试:用 `127.0.0.1:0` `TcpListener` 喂 canned DeepSeek JSON,经真 `run_until_done` 断言事件序列(走真实发射路径,零真机出网,用 `Client::builder().base_url(...)` 接缝)。

### 测试(8)
含 helper 单测 + 两个 loopback 全流程(session 事件序列、engine 生命周期),断言完整事件链且零真机调用。

---

## 4. WebUI 后端(`webui` feature + `src/bin/webui.rs`,`docs/impl-v1.md` §3)

**文件:** `src/webui/mod.rs`、`src/bin/webui.rs`、`src/lib.rs`(gate)、`Cargo.toml`、`justfile`。

### 路由表(axum 0.8 `{param}` 语法,实际构建)
| 方法 路径 | handler | 返回 |
|---|---|---|
| `GET /` | `index_page` | 内嵌单页 HTML(`include_str!("index.html")`) |
| `GET /api/themes` | `list_themes` | `{themes:[..]}`(root 子目录排序,`.` 前缀跳过) |
| `GET /api/themes/{theme}/articles` | `list_articles` | `{theme, articles:[..]}`(索引阅读序) |
| `GET /api/articles/{theme}/{file}` | `get_article` | `{theme, file, content}` |
| `POST /api/tasks` | `start_task` | `202 Accepted` + `{task_id, theme, file}` |
| `GET /api/events` | `events` | **SSE** `Event` JSON 流 |
| `GET /api/articles/{theme}/{file}/history` | `article_history` | `{history:[CommitInfo..]}` |
| `GET /api/articles/{theme}/{file}/diff?from=&to=` | `article_diff` | `{diff:"<patch>"}` |
| `POST /api/articles/{theme}/{file}/undo` | `article_undo` | `{undone, committed}` 或 `{undone:false, reason}` |

`app(state: AppState) -> Router` 是唯一声明点;bin 只 bind+`axum::serve`,测试在进程内 build。错误统一经 `ApiError{status,message}`→`{"error":"..."}`;`ToolError`/`VcsError` 映 404(NotFound/NoHistory)、400(Sandbox/InvalidArgs)、409(Lock)、否则 500。

### EventSink → broadcast → SSE
`AppState` 持 `events: tokio::sync::broadcast::Sender<Event>`(cap 1024);`BroadcastSink(Sender)` 实现 `EventSink`,`emit` 走 `let _ = self.0.send(event)`——非阻塞、有意 lossy(无订阅/lag 即丢,绝不卡引擎)。`AppState::event_sink()` 返 `Arc::new(BroadcastSink(...))`,即 stage 3 的 `set_event_sink`/`event_sink()` 接缝。`events` handler:`subscribe()`→`BroadcastStream`→`filter_map` 成 `SseEvent::json_data`,丢序列化失败 / lag / 关闭错误(保流存活),带 `KeepAlive::default()`。

### spawn_blocking 任务模型
`start_task` 自增 `AtomicU64` task id,克隆 `{root, client, sink, theme, file}`,`tokio::task::spawn_blocking(run_writing_task)`,**立即返 202**不等运行。`run_writing_task`(同步,阻塞池)开 `Workspace`、建共享 `Client` 的无工具 master `Session`、装 broadcast sink(role `"master"`)、驱 `Master::run_one`——事件全扇进 broadcast→每个 SSE 订阅者。失败以 `Finished{outcome:"failed"}`/`SlaveReported` 事件出现在流上,非 HTTP 错误(响应已发)。slave 写者 `Agent{model:default_model, label:"slave-1"}`。

### 决策
- **W1** feature 恰 `webui = ["blocking","dep:axum","dep:tokio","dep:tokio-stream"]`;`tokio` 开 `rt-multi-thread,macros,sync,net,time`,`tokio-stream` 开 `sync`。`cargo add --optional` 产生的多余子 feature 已在 `[features]` 表折进 `webui`。**弃 `tower-http`**(SSE 用内置、`/` 用内嵌)。
- **W2** 用 `tokio-stream` 而非 `futures-*`(后者归 `async` feature,`webui` 不拉):`tokio-stream` 一并给 `Stream`+`StreamExt`+`BroadcastStream`,`cargo check --features webui` 不带 `async` 即过。
- **W3** `AppState` 不持 `Workspace`/`Vcs`,按请求(读)/按任务(写)现开:锁是进程内状态、`Vcs` 非 `Sync`,现开句柄保 `AppState: Clone+Send+Sync`(只 `Arc`/`Sender`/`Client`)。
- **W4** `POST /api/tasks` fire-and-forget(202),结果走 SSE:同步运行可能数十秒,阻塞响应会毁实时 UX。这是 V8 的具体落地。
- **W5** UI `undo` 端点以 `WriterId::Human` 署名且**不持锁**(异于 model-facing `undo_last` 工具):写作运行的锁只在其 slave 线程(进程内,HTTP 进程的新 `Vcs` 看不到),人工 revert 由操作者权威决定。仍经 `Vcs::undo_last`(restore+recommit,不 reset),历史保留。
- **W6** `DEEPSEEK_API_KEY` 启动时经 `Client::from_env` 读一次,fail-fast;key 不序列化进任何响应。bin 默认 `AI_WRITE_WORKSPACE=workspace`、`AI_WRITE_BIND=127.0.0.1:8080`(env 可覆盖)。

### 测试(10,全离线)
broadcast sink 转发 / 无订阅 lossy / `event_sink` 装配、列主题、列文章阅读序、取内容、缺文件 404、history+diff 反映两次提交(`HEAD~1..HEAD` 含两版正文)、`start_task` 返 202 不跑 live、起任务后 SSE 上落 `SlaveSpawned`(用**离线** client,bounded 500ms drain,证 `spawn_blocking→sink→broadcast` 端到端而 slave 首轮触不到网络)。

---

## 5. WebUI 前端(`docs/impl-v1.md` §3)

**文件:** `src/webui/index.html`(~520 行自包含 HTML/CSS/JS,无框架 / 无构建);`src/webui/mod.rs`(`const INDEX_PAGE: &str = include_str!("index.html")`,`index_page` 返 `Html(INDEX_PAGE)`)。无后端逻辑 / 路由 / 依赖 / 测试改动。

### 页面(三栏暗色)
- **顶栏**:标题 + 连接指示灯(`EventSource` `onopen`/`onerror` 驱动:绿 "live" / 红 "reconnecting…")。
- **左栏 工作区树**:`GET /api/themes` 可折叠主题;展开惰性载 `/articles`(阅读序);点文章 `selectArticle`→载内容+history,高亮当前;`↻` 刷新。
- **中栏 三卡**:① 新建写作(theme/file/task → POST `/api/tasks`,显 task_id,设跟踪文章);② AI 操作流(`EventSource('/api/events')`,逐 `data:` `JSON.parse`,clear 按钮);③ 文章当前内容(等宽可滚)。
- **右栏 版本/Diff**:`/history` 渲染 commit 卡(短 id / message / `stripEmail(author)` / 本地化时间),每卡 checkbox(最多选 2,旧的自动剔);"Undo last" POST `/undo`(<2 commit 时禁用,先确认,处理 `{committed}` 与 `{undone:false,reason}`);"Compare ✓"(恰选 2 时启用)调 `/diff?from=&to=`,`+`/`-`/`@@`/header 行染色。

### 事件渲染(外部标签枚举 `{Kind:{...}}`)
`onmessage` 取 `Object.keys(payload)[0]` 为 kind → `handleEvent`。各变体染色时间线行;`EditCommitted`(若为跟踪文章)触发 `refreshArticle()`+`loadHistory()`;`Finished` 触发 `loadTree()`+`refreshArticle()`+`loadHistory()`。**default arm 渲染未知 kind 的原始 JSON**(`Event` 是 `#[non_exhaustive]`,UI 永不静默丢事件)。

### 决策
- **F1** `include_str!("index.html")` 而非内联字符串字面量:`.html` 文件得编辑器工具与 HTML 校验,后端仍是单内嵌资产、无静态服务器。
- **F2** 原生 JS,无框架 / 构建(§3 "原生 HTML/JS,无 npm");小 `el()`/`$()` helper + `EventSource` + `fetch`。
- **F3** 事件 switch 的 default arm 渲染未知 kind(向前兼容 `#[non_exhaustive]`)。
- **F4** 文章自动刷新事件驱动、非轮询(`EditCommitted`/`Finished` 触发)。
- **F5** diff 需恰选 2 个 commit;undo 先确认且 <2 版本禁用(对齐 V2 restore+recommit、W5 人工署名)。
- **F6** `stripEmail()` 仅显 provenance 名(合成 `@ai-write.local` 对人是噪声)。
- **F7** lossy / best-effort SSE 对齐后端:`onerror` 翻红,浏览器 `EventSource` 自动重连,不重放漏掉的事件(对齐 broadcast 的 lossy 设计)。

---

## 6. 运行方式

```bash
# 全套静态校验(交付门禁)
just ci

# 全 feature 测试(含 webui;不打真机)
just test

# 文档(rustdoc 警告即错)
just doc

# 起 WebUI(读 ./.env 的 DEEPSEEK_API_KEY,默认 127.0.0.1:8080,工作区 ./workspace)
just webui
#   等价:set -a; . ./.env; set +a; cargo run --bin webui --features webui
#   可覆盖:AI_WRITE_BIND=0.0.0.0:9000 AI_WRITE_WORKSPACE=/path/to/ws just webui
```
浏览器开 `http://127.0.0.1:8080`:列工作区 → 起一篇写作 → 实时看 AI 操作流 → 看版本历史 / diff / undo。
`just webui` 的 `.env` 引导镜像现有 `test-live` / `demo` recipe(`set -a; . ./.env; set +a`)。

---

## 7. 已知缺口 / TODO / 取巧点(供 orchestrator review)

1. **真机写作冒烟未做(按任务约束)。** 全程禁止真机 DeepSeek 调用,故 `just webui` 起服务 + 真起一篇写作的端到端冒烟**未跑**;`POST /api/tasks` 的引擎驱动路径仅经离线 client 验证到 `SlaveSpawned`(网络前)。人工冒烟仍待做(规格 §6 注明"真机写作冒烟由我做")。
2. **`EditCommitted` 砍掉了 `diff_summary` 字段**(§2 草案有)。有意取舍:事件由工具结果 payload 派生、不在发射点算 diff;UI 需要 diff 走 `/diff` 端点。若 review 要 inline diff 摘要,需在事件层补算。
3. **`POST /api/tasks` 无任务状态查询 / 取消端点。** task_id 只用于 UI 关联 SSE,无 `GET /api/tasks/{id}` 状态或 cancel;失败只在 SSE 上以 `Finished{failed}` 出现。多任务并发时 UI 靠事件内容区分,无 task_id 标注在事件上(事件流是全局单 feed,未按 task_id 分路)。
4. **SSE 有意 lossy。** broadcast cap 1024,无订阅 / lag / 重连不重放——慢客户端或重连会丢中间事件(对齐 W1/V8)。生产若需可靠投递得换持久化 / 重放游标。
5. **slave 写者固定 `label:"slave-1"`。** WebUI 起的任务总署名 `<model>/slave-1`,未按并发任务区分 label;多任务版本历史的 author 可能撞名(对齐 demo 约定,非 bug,但 review 可留意)。
6. **`HEAD~1` 不是上一文章版**(T2 的后果):因 index.json 提交插在文章提交间。model 与 UI 都应以 `article_history` 的 commit id 做 diff/undo,不要假设 `HEAD~n` 对齐文章版本。`undo_last` 已正确用 `history[1]` 而非 `HEAD~1`。
7. **WebUI undo 不持锁**(W5):若一篇文章正被某写作运行编辑、同时人工点 undo,二者的 `Vcs` 句柄不共享锁,理论上可交错。当前部署是单人演示用,未做跨进程锁;多人 / 并发场景需补。
8. **`git2` vendored libgit2 依赖本机编译链**(cmake/clang)。CI / 他机若缺,需按 V3 fallback(`--no-default-features` 选特性);本机已验证 vendored 直接通过,未配 fallback feature 组。
9. **前端无单测 / 无浏览器自动化验证。** `index.html` 仅经 HTML 良构性目视检查 + `include_str!` 编译;DOM 行为、SSE 渲染、diff 染色未自动化测试。

---

## 8. orchestrator 复核(2026-06-18,主控独立验收)

主控(非 workflow 子 agent)独立复核:

- **独立重跑** `just ci` / `just test`(107 lib + 26 doctest + 6 集成,2 live `#[ignore]`)/ `just doc` —— **全绿**,与 §0 一致。
- **代码 review**:vcs `commit_file` 确为 `index.add_path`(只 stage 命名文件)、`undo_last` = `restore`(写回历史 blob + 再提交,**无 `reset`**);webui 同步×异步桥(`spawn_blocking` + `broadcast` + SSE)接线正确;observe `NullSink` 默认、master→slave 共享 sink 正确。
- **真机 WebUI 冒烟(补做,§7-1 现已关闭)**:`just webui` 起服务 → `POST /api/tasks` 写一篇《Rust 借用简介》:
  - SSE 实时流捕获到完整序列 `SlaveSpawned → SessionStarted → RoundStarted → ToolCalled → ToolResult …(多轮)… → EditCommitted` —— **AI 操作可视化的数据流端到端正常**。
  - 文章落盘 `workspace/demo/intro.md`(298 字节)。
  - **git 历史** `9f886b4 edit(demo/intro.md): write_article`,**author = `deepseek-v4-flash/slave-1 <agent@ai-write.local>`** —— 逐编辑提交 + 文件级 provenance 端到端生效(决策 V1)。
- **结论**:v1 三块(版本管理 / 可观测 / WebUI)功能正常,无需返工。§7 其余 2–9 为合理的 v0/v1 取巧点或 deferred 项,不阻塞。

---

*本文档由各阶段实现期决策汇编而成,内容以实际落地代码为准(非 aspirational)。校验基线见 §0;主控复核见 §8。*
