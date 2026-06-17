# 实现文档 v0:session + tool 两大模块,engine 薄层(prototype）

> 状态:实现规格(**待你 review 固定**;固定后再 orchestrate workflow)。
> 依据:`product-discussion.md` §2(协作模型)/ §4(工具)/ §5(skill)。(早期的 `session-design.md` 已删,其内容由本文 §2 取代;后续 spec 只看 discussion + impl 系列。)
> 目标:端到端跑通 —— 用真实 DeepSeek 写出一篇文章到工作区。

---

## 0. 层次纠正(关键)

`Master / Slave` 是**逻辑角色 / 上层概念**,不是基础模块。lib 里要**先实现的是两个大模块**,engine 只是它们之上的薄薄一层:

```
L1  req      (已完成)   无状态 client
L2  session  (大模块·先做)  req 之上的通用有状态会话 = agentic 回合引擎
L2  tool     (大模块·先做)  工具系统 + 工作区模型 + 沙箱
L3  engine   (薄层·后做)   Master/Slave = 配了不同工具+提示词的 session 组合
demo                       串起来"看看效果"
```

**落地顺序:`session` → `tool` → `engine` → `demo`。**

> 关键观念:**"查资料→写→改"不是写死的状态机**,而是"通用 agentic 循环(session) + 写作工具集 + 写作系统提示词"驱动出来的**涌现行为**。Slave 就是一个被这样配置的 session。

---

## 1. v0 范围(in / out)

**IN**
- `session` 模块:通用 agentic 回合引擎(见 §2)。
- `tool` 模块:Tool trait + 注册表 + dispatch + 原生工具(除搜索)+ 工作区模型(主题/文章/索引)+ 文章锁 + 沙箱(见 §3)。
- `engine` 薄层:Master/Slave 角色组合(见 §4)。
- `bin/demo`:Master 派一个 Slave,用真机 DeepSeek 写一篇文章落盘(见 §5)。

**OUT(明确 deferred)**
- **DSL / HTML / 字级 provenance** —— v0 纯文本;provenance 只到文件级(frontmatter 记 model id)。
- **搜索(MCP)** —— 你指定不做;v0 不含查资料工具(靠模型自身知识写)。
- **libgit2 / undo**、**async/tokio**(v0 = 同步 + 线程)、**人介入 UI / 抢锁演示**、**skill**、**Master 状态机细化** —— 后续。

---

## 2. `session` 模块(大模块之一)

req 之上的**通用、与写作业务无关**的有状态会话。**Master/Slave 都用它**,只是配置不同。

**职责**
- 持有:固定**系统提示词** + **消息历史** + **usage 累计** + 一个 **tool 注册表句柄**。
- 跑**通用 agentic 回合**:组装 messages → `req` chat(带 tools) → 按 `finish_reason` 分支 →(必要时)分发工具 → 回灌 → 循环,`MAX` 轮兜底。
- 复用 req:`blocking::Client`、`ChatRequest::builder`、`FinishReason`、`Usage`、`Error::is_transient`(瞬时重试)、`RespMessage::to_history`(回填剥 `reasoning_content`)。
- **可序列化**(持久化 / 恢复现场)。
- 思考模式开关在 session 级(系统提示词固定,见 §5/skill 结论)。

**finish_reason 分支**(沿用 product-discussion 结论)
- `tool_calls` → 分发工具 → 追加 `tool` 结果 → 继续
- `stop` → 本轮"自评完成",交回上层
- `length` → 自动续写(有界)
- `insufficient_system_resource` / 瞬时错误 → `is_transient` 重试;致命错误 → 上抛

**接口草图**
```rust
pub struct Session {
    client: req::blocking::Client,
    system: String,                 // 固定系统提示词
    history: Vec<req::Message>,      // 回填时剥 reasoning_content
    tools: ToolRegistry,             // 来自 tool 模块
    usage: UsageTotals,
    max_rounds: u32,
}
pub enum Step { Tool(/*调了哪些工具*/), Message(String), Done(String), NeedHuman, Failed(req::Error) }
impl Session {
    pub fn new(client, system, tools, opts) -> Self;
    pub fn push_user(&mut self, text: impl Into<String>);
    pub fn run_round(&mut self) -> Step;   // 跑一轮(对外可观测的最小步)
    pub fn run_until_done(&mut self) -> Step;  // 跑到 Done/NeedHuman/Failed/MAX
    // 序列化:#[derive(Serialize, Deserialize)] on a serializable snapshot
}
```

> 这是 session 抽象的落地规格(取代了早期已删的 `session-design.md`)。

---

## 3. `tool` 模块(大模块之二)

工具系统 + 工具操作的**工作区模型** + 沙箱。session 通过它拿到可调用的工具集。

**结构**
- `Tool` trait:`name` / `schema`(→ `req::Tool` 定义)/ `call(args, ctx) -> ToolResult`。
- `ToolRegistry`:持有一组工具;能导出全部 `req::Tool` 定义给 session;按名分发执行;`ToolResult` 回灌成 req 的 `tool` 消息。
- **工作区模型**(`tool::workspace`):`Workspace{root}` / `Theme`(目录) / `Article`(纯文本文件) / `Index`(manifest,serde 落盘) / 文章锁 `LockState{Idle, Editing{holder}}`。
- **沙箱**:所有路径必须解析在 `workspace` 根内,拒绝 `..` 逃逸 / 绝对路径 / 系统路径。
- 大文件 / 二进制 → 工具返回 `Err` 抛回 LLM(由它换策略)。

**v0 工具清单(除搜索)**
| 工具 | 需持锁 | 说明 |
|---|---|---|
| `create_theme` / `delete_theme` | — | 主题目录(删=软删 / TODO) |
| `create_article` / `delete_article` / `list_articles` | — | 文章文件 + 维护索引 |
| `read_article` / `find` | — | 读全文 / 工作区内检索 |
| `write_article` | ✅ | 覆写全文(粗) |
| `edit_article` | ✅ | 精确替换:`old` → `new`,唯一匹配(中) |
| `apply_edits` | ✅ | **细粒度**:一组精确操作(按 offset / anchor 的 insert / delete / replace),**原子**应用;失败整批回滚 |
| `acquire_lock` / `release_lock` | — | 持 / 放文章锁(单写者) |
| `report` | — | Slave → Master 结构化汇报 |

**接口草图**
```rust
pub trait Tool { fn name(&self) -> &str; fn schema(&self) -> req::Tool;
                 fn call(&self, args: serde_json::Value, ctx: &mut ToolCtx) -> ToolResult; }
pub struct ToolRegistry { tools: Vec<Box<dyn Tool>> }
pub struct ToolCtx<'a> { ws: &'a mut Workspace, /* 当前 writer 身份等 */ }
pub type ToolResult = Result<serde_json::Value, ToolError>;  // Err → 回灌成 tool 消息抛回 LLM
```

---

## 4. `engine`(薄层:Master / Slave 角色组合)

**不是新机器,是 session 的两种配置 + 一点编排。**

- **Slave** = `Session`(写作工具集 + 写作系统提示词 + 目标文章)。跑 `run_until_done`:它自己 `acquire_lock` → 写 → `release_lock`;"查→写→改"是涌现的。结束发 `SlaveReport`。**Slave 跑在一个 thread**(同步语境)。
- **Master** = `Session`(编排工具:建主题/文章、派生 Slave、收汇报)+ 监督。v0:建主题 → 派一个 Slave 线程 → 收 `SlaveReport`(结构化摘要,不读 Slave 全量历史);Slave 失败 → 终止 + 上报(重启留 TODO)。
- 多 Slave 并行写**不同**文章天然无冲突(各持各的文章锁);同一文章单写者由锁保证。

```rust
pub struct SlaveReport { status, summary, result, needs }
pub fn spawn_slave(client, ws, article, task) -> JoinHandle<SlaveReport>;  // thread
pub struct Master { session: Session, ws: Workspace /*…*/ }
```

---

## 5. demo(看看效果)

```
just demo "<主题>" "<写作任务>"     # 读 .env 的 DEEPSEEK_API_KEY(用 req::*::from_env)
# → workspace/<主题>/<文章>.md 生成;stdout 打印文章 + usage(+ 命令日志,若有)
```

---

## 6. 验收标准

- `just ci` 绿(fmt + clippy `-D warnings` + check)、`just test` 绿。
- `just demo` 真机能写出一篇文章到 `workspace/`,锁正确 acquire/release,Slave 向 Master 汇报。
- 沙箱:越界路径被拒(加 pure 测试)。

---

## 7. 落地顺序(给后续 workflow)

1. **`session`**:通用 agentic 回合引擎(§2),复用 req;可独立 `cargo check` + 跑通(可先用一个 echo/dummy tool registry 测一轮)。
2. **`tool`**:Tool trait + Registry + 工作区模型 + 沙箱 + 原生工具(§3),加沙箱/持锁的 pure 测试。
3. **`engine`**:Slave/Master 组合(§4)。
4. **`demo` + 验收**:串起来,`just ci`/`just test` 修到全绿;真机冒烟由主控做。

> 强耦合、必须一起编译:每步跑 `cargo check`,末段闭合 `just ci`/`just test`,不能留 `todo!()` 在被调用路径上。

