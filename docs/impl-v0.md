# 实现文档 v0:协作引擎 + 工具(prototype）

> 状态:实现规格(供 workflow 落地一版"看看效果")。
> 依据:`product-discussion.md` §2(协作模型,已定)/ §4(工具,已定)/ §5(skill,已定)。
> 目标:**端到端跑通** —— Master 派生 Slave,用**真实 DeepSeek** 写出一篇文章到工作区。

---

## 0. v0 范围(明确 in / out)

**IN(本版要实现)**
- `workspace`:主题(目录)/ 文章(**纯文本 / markdown 文件**)/ 索引(manifest)/ 每文章**内存锁**(空闲 / 编辑中)。
- **原生工具(除搜索)**:fs(建/删/列 主题与文章)、文本(读/写/编辑/find)、锁(acquire/release)、Slave 汇报。**全部沙箱在工作区根下,拒绝逃逸/系统路径**。
- `engine`:**Slave 写作循环**(复用 `req::blocking::Client`,(查资料 v0 跳过)→ 写 → 改),**Master**(派生 Slave + 收汇报 + 最小监督)。
- `bin/demo`:给定主题 + 任务,Master 派一个 Slave 写一篇文章,真机跑通,落盘 + 打印结果与命令日志。

**OUT(明确 deferred,后续单独设计/实现)**
- **DSL / HTML / 字级 provenance** —— v0 用纯文本;provenance **只到文件级**(frontmatter 记参与的 model id)。**← 最大的简化。**
- **搜索(MCP)** —— 你指定不做;v0 "查资料"阶段先空跑(用模型自身知识)。
- **libgit2 历史 / undo** —— v0 不接;命令日志只记录、不回放。
- **async / tokio** —— v0 = **同步 + 线程**(blocking req,Slave = thread)。
- **人介入 UI / 抢锁演示**、**skill**、**Master 状态机细化** —— v0 最小或不做(锁机制实现,但 demo 不演示抢锁)。

---

## 1. 模块布局(在现有 `ai-write` crate 内)

```
src/
  req/            # 已完成,直接复用
  workspace/      # theme / article / index / lock
  tools/          # 原生工具 + dispatch(→ req::Tool 定义 + 执行)
  engine/         # slave(写作循环) / master(监督)
  bin/demo.rs     # "看看效果" 入口
```

> v0 先放同一个 crate 的模块里迭代(快);稳定后再考虑拆 crate。

---

## 2. 核心类型(接口先定死,workflow 按此实现)

```rust
// workspace
pub struct Workspace { root: PathBuf }                 // 沙箱根
pub struct Theme { dir: PathBuf, index: Index }        // 主题=目录
pub struct Article { path: PathBuf }                   // 文章=文件
pub enum LockState { Idle, Editing { holder: WriterId } }
pub enum WriterId { Slave(SlaveId), Human }
pub struct Index { articles: Vec<ArticleMeta> }        // 顺序 + 元数据(manifest)

// 命令 / 日志(v0 只记录)
pub enum Command { Edit { article, .. }, Talk { text }, /* … 人的原子操作 */ }
pub struct CommandLog { entries: Vec<Command> }

// 工具:统一抽象,dispatch 到原生实现
pub trait Tool { fn name(&self)->&str; fn schema(&self)->serde_json::Value;
                 fn call(&self, args: serde_json::Value, ctx: &mut ToolCtx) -> ToolResult; }
// ToolResult 回灌成 req 的 tool 消息;越界/过大 → Err 抛回 LLM

// engine
pub struct Slave { client: req::blocking::Client, /* 文章, 工具集, 轮次上限 */ }
pub struct SlaveReport { status, summary, result, needs }   // 结构化摘要,上报给 Master
pub struct Master { client, workspace, /* 派生的 slave 线程句柄 */ }
```

> 这些签名是 workflow 各 agent 的契约;实现时可补字段,但**对外形状**以此为准。

---

## 3. Slave 写作循环

复用 req 的回合,套在文章锁里:

1. (v0 跳过查资料) → 组装上下文(系统提示词 + 任务 + 文章当前内容)。
2. `acquire` 文章锁 → 进入"编辑中"。
3. 回合循环:`ChatRequest`(带工具) → `chat` → 按 `finish_reason` 分支:
   - `tool_calls`:执行工具(读/写/编辑/find)→ 追加 `tool` 结果 → 继续;
   - `stop`:LLM 自评写完 → 跳出;
   - `length`:自动续写(有界);其余瞬时错误走 req 的 `is_transient` 重试。
4. `release` 锁 → 文章回"空闲";向 Master 发 `SlaveReport`。
5. 轮次上限兜底(沿用 `MAX`)。

> "够不够 / 好不好"由 **LLM 自评**;人介入(v0 暂不在 demo 演示)。

## 4. Master(最小监督)

- 接收任务 → 在 workspace 建主题(若无)→ **派生 Slave 线程**写文章。
- 收 `SlaveReport`(结构化摘要),不读 Slave 全量日志。
- Slave panic / 失败 → Master 可读其操作日志,决定 重启 / 终止(v0 实现"终止 + 上报",重启可留 TODO)。
- v0 demo 先单 Slave;多 Slave 并行留接口。

## 5. 工具清单(v0,除搜索)

| 工具 | 参数 | 行为 / 安全 |
|---|---|---|
| `create_theme` / `delete_theme` | name | 在 workspace 下建/删主题目录;删=软删(移回收)或留 TODO |
| `create_article` / `delete_article` | theme, name | 建/删文章文件 + 更新索引 |
| `list_articles` | theme | 读索引 |
| `read_article` | article | 读全文 |
| `write_article` | article, content | 覆写全文(**需持锁**) |
| `edit_article` | article, range/find, replace | 局部编辑(**需持锁**) |
| `find` | theme, query | 工作区内文本检索 |
| `acquire_lock` / `release_lock` | article | 持/放文章锁 |
| `report` | summary, result, needs | Slave → Master 结构化汇报 |

**统一安全**:所有路径**必须解析在 workspace 根内**(防 `..` 逃逸 / 绝对路径 / 系统文件);文件过大 / 二进制 → 工具返回 `Err` 抛回 LLM,由它换策略(§4 已定)。

## 6. demo(看看效果)

```
just demo "<主题>" "<写作任务>"   # 读 .env 的 DEEPSEEK_API_KEY
# → workspace/<主题>/<文章>.md 生成;stdout 打印文章 + usage + 命令日志
```

## 7. 验收标准

- `just ci` 绿(fmt + clippy `-D warnings` + check)、`just test` 绿。
- `just demo` 真机能写出一篇文章到 `workspace/`,锁正确 acquire/release,Slave 向 Master 汇报。
- 工具沙箱:尝试越界路径被拒(加一个 pure 测试)。

---

## 8. 给 workflow 的落地顺序(脊柱优先)

1. **scaffold**:Cargo 依赖(libgit2 暂不加;加 `serde`/`thiserror` 已有)、模块骨架、§2 的类型签名 + Tool trait。
2. **workspace**:Theme/Article/Index/Lock + 沙箱路径解析(+ 越界测试)。
3. **tools**:按 §5 实现,依赖 workspace;每个工具产出 `req::Tool` 定义 + 执行。
4. **engine**:Slave 循环(§3,复用 req)+ Master(§4)。
5. **demo + 验收**:`bin/demo` 串起来,`just ci`/`just test` 必须绿,真机冒烟。

> 强耦合、必须一起编译:workflow 末段必须有**集成 + `just ci` 修复闭环**,不能各 agent 写完就算。

