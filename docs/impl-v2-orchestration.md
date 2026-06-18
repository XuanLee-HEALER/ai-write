# 实现文档 v2:LLM 编排(`feat/llm-orchestration`)

> worktree:`feat/llm-orchestration`,只动 `src/engine`(新增 `src/engine/orchestration.rs` + 扩展 `src/engine/mod.rs`)。
> 目标:把 `Master` 从 v0/v1 的**确定性** `run_one` 升级为**真正 LLM 驱动**的编排 session,同时**保留** `run_one` 与全部既有测试。

---

## 0. 一句话

`Master::run_goal(goal)` 把一个 master `Session` 配上一组**编排工具**和编排 system prompt,跑 `run_until_done`,让模型自己**规划文章集 → 建结构 → 一文一 slave 派发 → 收结构化 report → 决定是否收尾**;全程发既有 `Event`,离线 loopback 测试零真机断言「规划/派发/汇总」。

---

## 1. 编排工具(`src/engine/orchestration.rs`)

五个工具,各实现 `crate::tool::Tool`(带 JSON schema),装进 master session 的 `ToolRegistry`:

| 工具 | 作用 | 状态来源 |
|---|---|---|
| `create_theme` | 建主题目录 | `ctx.ws`(沙箱工作区) |
| `create_article` | 在主题里建一篇空文章并记入 index | `ctx.ws` |
| `list_articles` | 列主题下已有文章(阅读序) | `ctx.ws` |
| `dispatch_writer` | 派一个 slave 写**一篇**文章,阻塞到它 report,记录 `SlaveReport` | `OrchestratorState` |
| `list_reports` | 把目前收集到的所有 `SlaveReport` 回给模型 | `OrchestratorState` |

### 1.1 为什么需要 `OrchestratorState`(共享状态)

`create_theme` / `create_article` / `list_articles` 只需要 session 已经在每次 tool 调用里塞进来的沙箱 `Workspace` 句柄(`ToolCtx::ws`),够用。

但 `dispatch_writer` 需要三样 `ToolCtx` **不携带**的东西:
- `Client` —— 驱动 slave 的 round;
- 工作区**根路径** `workspace_root` —— slave 自己开沙箱句柄;
- `EventSink` —— slave 的生命周期/内层步骤汇进同一 feed。

再加上「不断累加的 report 列表」。这四样放进一个 `pub(crate) struct OrchestratorState`,用 `Arc` 在 `DispatchWriter` / `ListReports` 之间共享。`reports` 用 `Mutex<Vec<SlaveReport>>` 守护(v2 master 单线程驱动,锁基本无竞争)。**这个 state 就是 `run_goal` 把模型攒下的 report 读回来的缝。**

### 1.2 幂等设计

`create_theme` / `create_article` 把工作区的「已存在」信号(`ToolError::Lock`)翻译成良性的 `created: false`,**不报错**。这样模型可以「声明完整的文章集」而不必跟踪哪些已存在,重规划同一主题也不会翻车。其它真实错误(`SandboxViolation` / `NotFound` / `Io`)照常作为 `ToolError` 回给模型,让它自适应——不放松任何 guard rail。

### 1.3 安全 / 沙箱

- 主题/文章创建走的是写作工具同一套沙箱工作区 API:`..` 穿越、绝对路径、系统路径一律被拒(`SandboxViolation`)。
- `dispatch_writer` 派出的 slave,其 `Session` 完全沙箱化,**writer 身份由 master 配置确定性派生**(`Agent{ model: slave_model, label }`)——模型只能选**任务文本**,选不了工作区外的路径,也注入不了任意 writer 身份。

---

## 2. Master 循环(`src/engine/mod.rs`)

### 2.1 新入口 `Master::run_goal`

```rust
pub fn run_goal(
    &mut self,
    goal: &str,
    options: SessionOptions, // master 自己 round 的 model / 轮数预算 / 重试
    slave_model: &str,       // slave 写作时的 model 身份(记进 contributor provenance)
) -> Result<GoalOutcome, req::Error>
```

流程:

1. 复用 master 现有 session 的 `Client` 与 `EventSink`(`client_clone()` / `event_sink()`)。
2. 建 `Arc<OrchestratorState>`(client / workspace_root / events / slave_model)。
3. 新建一个 master `Session`:`MASTER_SYSTEM_PROMPT` + `orchestration_tools(state)`,`set_workspace(root, WriterId::Human)`(master 以 human 身份在它治理的工作区上跑 `create_*`),`set_event_sink("master", events)`,`push_user(goal)`。
4. `session.run_until_done()` —— 模型自己规划、建结构、一文一 slave 派发、读 report、收尾。
5. 终态:
   - `Step::Failed(e)` → 把 session 吸收进 `self.session`(让 `usage()` 仍反映部分运行)后返回 `Err(e)`;
   - `Step::Done(text)` → `outcome="done"`,`message=text`;
   - `Step::NeedHuman`(预算耗尽等)→ `outcome="need_human"`;
   - 从 `state.reports()` 取回全部 `SlaveReport`。
6. 把编排 session 吸收进 `self.session`,于是 `Master::usage()` 此后反映 master 自己这次 goal 的 token 用量。

### 2.2 返回类型 `GoalOutcome`

```rust
pub struct GoalOutcome {
    pub outcome: String,         // "done" | "need_human"
    pub message: String,         // master 收尾的最后一句 assistant 文本
    pub reports: Vec<SlaveReport>, // 按派发序的逐篇结果
}
```

把「master 自己怎么收的尾」和「具体产物(逐篇 report)」打包,调用方不必重读 master transcript。

### 2.3 与 `run_one` 的关系

- `run_one`(确定性 v0 路径)**逐行未改**——diff 里删掉的全是 doc 注释重写,没动任何可执行代码。它仍是「建主题/一篇文章 → 派一个 slave → 收 report」的确定性 Rust,session 只用来共享 client/sink。
- 两条路共用同一套 slave 机制(`spawn_slave_with_sink`)与同一个可观测 feed。

---

## 3. 可观测性(复用既有 `Event`,**未加新变体**)

不需要新 `Event` 变体即可拿到完整观测:

- master session 自身发 `SessionStarted{role:"master"}` / `RoundStarted` / `ModelMessage` / `ToolCalled` / `ToolResult` / `Finished`;
- `dispatch_writer` 内部走 `spawn_slave_with_sink`,slave 发 `SlaveSpawned` / (内层 round 事件) / `SlaveReported`。

于是一个 UI 在**同一条流**里看到:master 的每次工具调用 + 每个 slave 的完整生命周期。任务说明允许「genuinely needed 时」加 `#[non_exhaustive]` 的 `Event` 变体,但这里复用已足够,**故不加**(更小的改动面、零跨文件耦合)。

---

## 4. 决策 + 取舍

| # | 决策 | 取舍 / 理由 |
|---|---|---|
| O1 | 编排工具放 `src/engine/orchestration.rs`,**不放 `src/tool`** | 遵守 worktree 隔离,与 provenance worktree 零撞文件 |
| O2 | `dispatch_writer` **同步阻塞**(派一个、join、记 report、回模型) | 让 master 一次只推进一篇、读到 report 再决策,符合 supervisor 模式;也使 loopback 测试的请求顺序**确定**,单条有序响应列表即可喂完 master+slave |
| O3 | 结构工具走 `ctx.ws`,派发/汇报工具走 `Arc<OrchestratorState>` | 各取所需:前者 session 已注入沙箱句柄;后者要 client/root/sink/reports,`ToolCtx` 不带 |
| O4 | `create_theme/article` 把「已存在」当良性 no-op | 模型可声明完整文章集而不跟踪存在性;真错仍回模型自适应 |
| O5 | `run_goal` **内部新建** orchestration session(不复用传入 session 的 tools) | 工具一定接线正确;复用传入 session 的 client/sink 保持一个后端、一条 feed |
| O6 | **复用既有 `Event`,不加新变体** | master+slave 事件已给全链路观测;改动面最小、纯增量 |
| O7 | slave 失败 = report 里的 `Failed`,**不是** `run_goal` 的 `Err` | 只有 master 自己 round 的致命 `req::Error` 才 `Err`;slave 失败让模型(和调用方)能反应 |
| O8 | 保留 `run_one` 逐行不变 | 不破坏 v0/v1 路径与全部既有测试 |

---

## 5. 公共 API(本 worktree 新增 / 改动)

新增(`ai_write::engine`):
- `Master::run_goal(&mut self, goal: &str, options: SessionOptions, slave_model: &str) -> Result<GoalOutcome, req::Error>`
- `struct GoalOutcome { outcome: String, message: String, reports: Vec<SlaveReport> }`
- `pub mod engine::orchestration`,内含 `pub struct CreateTheme / CreateArticle / ListArticles / DispatchWriter / ListReports`(均 impl `Tool`)。
  - 注:`DispatchWriter::new` / `ListReports::new` 取 `Arc<OrchestratorState>`(`pub(crate)`),故构造器为 `pub(crate)`;外部不能自行构造这些工具,它们由 `run_goal` 内部接线。`OrchestratorState` 本身 `pub(crate)`,不暴露。

改动(纯增量、无破坏):
- `Master` 结构体 doc 更新(描述两条入口);`Master::usage` doc 更新(`run_goal` 后反映真实用量)。
- 新增私有 `MASTER_SYSTEM_PROMPT`、`fn orchestration_tools(...)`。

**无公共 API 回归**:既有 `pub` 项签名一律未变。

---

## 6. 测试覆盖(全离线,零真机)

手法:loopback TCP 假 DeepSeek server(`127.0.0.1:0`)+ `Client::builder().base_url(...)`,喂 canned JSON 响应。

`src/engine/orchestration.rs::tests`:
- `create_theme_is_idempotent` / `create_article_is_idempotent_and_listed` —— 幂等 + index 维护。
- `create_theme_rejects_sandbox_escape` —— `../evil` 被 `SandboxViolation` 拒。
- `create_article_missing_theme_errors` —— 缺主题 `NotFound`。
- `dispatch_writer_rejects_empty_task` —— 空任务 `InvalidArgs`。
- `list_reports_starts_empty` —— 初始空。
- `dispatch_writer_runs_a_slave_and_records_the_report` —— **真跑一个 slave round**(loopback `stop` 响应),断言 report 被记录、`list_reports` 能读到。

`src/engine/mod.rs::tests`:
- `run_goal_plans_dispatches_and_aggregates_over_a_fake` —— **端到端**:有序假 server 喂 `create_theme → create_article → dispatch_writer →(slave stop)→ master stop`;断言 `outcome="done"`、收尾 message、`reports.len()==1` 且 `Done`、文章落盘可 list、master `usage().rounds>=1`(真驱动了模型,区别于确定性 `run_one`)。
- `run_goal_surfaces_a_collected_slave_failure_without_erroring` —— slave round 返回 **HTTP 400(非瞬时,不重试)**,断言 `run_goal` **不** `Err`,而是 `reports[0].status==Failed`。

既有引擎/session/tool/observe 测试**全绿**(120 lib unit + 27 doctest + 6 integration)。

---

## 7. 已知缺口 / TODO

- **slave token 用量不回流**:slave 的 `UsageTotals` 活在它自己线程的 session 里,`run_goal` 只通过 `SlaveReport` 回结果,不把 slave 的 `UsageTotals` 跨 join 汇进 master(与 v0 一致)。如需全局用量,需在 report 旁加一个 usage 通道。
- **串行派发**:`dispatch_writer` 一次派一篇并阻塞。多篇文章是**顺序**写的,没用上「不同文章各持自己锁、可并行」的能力。并行派发(模型一轮发多个 `dispatch_writer`,或 master 侧并发 join)是后续优化。
- **无自动重试/重派**:slave `failed`/`needs_human` 只如实回模型,由 prompt 约束「别死循环重派」;没有引擎层的自动重启策略(与 `run_one` 的 TODO 同源)。
- **`slave_model` 与 master `options.model` 解耦**:slave 固定用 `SessionOptions::default()` 跑,只有 model **身份字符串**由 `slave_model` 指定,slave 的轮数预算/thinking 不可单独配置。若要 per-slave 调参,需把 `SessionOptions` 也下放进 `OrchestratorState`。
- **集成层留合并期**:与 content/dsl/provenance 无关(本 worktree 设计如此),真正把文章内容接进字级 provenance / DSL 渲染是合并后的连接工作。
