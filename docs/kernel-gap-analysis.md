# 内核抽象 vs 当前实现 —— diff 清单(待 assign)

> 依据:`docs/ai-write-kernel.html`。逐条把内核的核心抽象对比当前代码,标出 diff(gap)与可 assign 的工作项 G#。
> coordinator 的完整设计见 `docs/coordinator-design.md`。

## 对照表

| 内核条目 | 当前实现 | 对齐? | gap → 工作项 |
|---|---|---|---|
| §3 文章=文件 / topic=目录 | `Workspace`:theme=目录、article=文件 | ✅ | — |
| §3 恒成品(commit 边界) | 每次编辑产生 commit | ◐ | 提交粒度破坏"一个认知单元一个 commit",见 G2 |
| §4 事实源=磁盘、每步重读 | 工具 `read_article`/`write_article` 走磁盘 | ◐ | skill/系统提示词在会话构造时一次性烤入,不每步从磁盘重读 → **G8** |
| §5 diff 即信道 / git 协作 / libgit2 | `vcs`(git2)+ 单文件锁 + history/diff | ✅ | — |
| §5 认知单元=一次 commit | `ToolCtx::commit_article` 做**两次** commit(正文 + index) | ✗ | 单 commit 原子化 → **G2** |
| §6 coordinator(事务边界) | **不存在**;锁是 `Workspace` 的 per 单文件内存锁 | ✗ | 实现 coordinator → **G1**(含 6.A/6.B/6.C/6.D) |
| §6.A 声明锁(影响集预知) | 无;锁逐个 acquire | ✗ | 随 **G1** |
| §6.B 无死锁(全序/全有全无、不可绕过) | 无全局视图;工具直接 `ws.acquire_lock` | ✗ | 随 **G1** |
| §6.C 队首优先、不抢占 | 无队列;slave **整篇会话**持锁,人要等整篇写完 | ✗ | 随 **G1**(锁粒度降到 per-edit) |
| §6.D 提交进临界区 | commit 散在各工具,非临界区事务 | ✗ | 随 **G1** + **G2** |
| §6 跨文件逻辑操作(拆分/合并) | **不存在** | ✗ | 新增 split/merge 事务 → **G5** |
| §6.2 manifest 整文件锁 | index.json 多处直接 `save_index`,**无锁** | ✗ | 随 **G1**(声明锁集必含 index.json) |
| §7 组织模型:index 是结构事实源、parent+order、不用文件名前缀 | v3 已实现 `Index{order, parent, config}` + `article_outline` | ✅ | — |
| §7 sidecar 资源(富文本/配图→PDF) | 文章=纯文本单文件 | ✗ | 「正文 + sidecar 约定」→ **G14**(低优先) |
| §8 reasoning 不入 git | `reasoning_content` 多轮被剥离,不落盘/不 commit | ✅ | — |
| §9 署名=diff 证据 / WriterId / 文件签名 | `WriterId` + `ArticleMeta.contributors` + git author | ✅ | — |
| §9 model id 钉死到带日期快照 | `Model` = `deepseek-v4-flash`/`pro`(具体但非 dated snapshot) | ◐ | 钉到带日期快照 id → **G12** |
| §9 git blame 行级归属 | 有 history/diff/undo,**无** blame 暴露 | ◐ | blame 工具/端点 → **G13** |
| §10 搜索经第三方 MCP | `find` 仅本地子串搜索 | ✗ | 接 MCP 搜索 → **G11** |
| §10 系统提示词动态插入/修改 | system 固定于 `Session` 构造 | ✗ | 动态系统提示词 → **G9**(与 G8 同源) |
| §10 多 skill 激活 + 优先级/覆盖 | 有序栈 + 后者覆盖前者,每轮从磁盘重读(`compose_stack` / `run_goal_with_skills`) | ✅ | **G10** 已实现,语义见 `multi-skill-design.md` |
| §11 人 commit 真伪边界 | 无法系统层分辨"人写"vs"人点头" | — | 内核明确留未决,**不 assign**(记录为硬边界) |

图例:✅ 已对齐 · ◐ 部分 · ✗ 缺失。

## 可 assign 工作项汇总

- **G1 Coordinator 内核**(最大块):新增 `src/coordinator/`,落地 §6 四机制 + 独占单 `Vcs` + 把 `Workspace` 锁收口、移除显式 `acquire_lock`/`release_lock` 工具、slave 锁粒度降到 per-edit、slave prompt 简化。设计见 `coordinator-design.md`。依赖 G2。
- **G2 单 commit 原子化**:新增 `Vcs::commit_paths`(多路径单提交),把"正文 + index"双提交合并为一个 commit。G1 的前置。
- **G5 跨文件逻辑操作**:`split_article` / `merge_articles`,作为 coordinator 事务(声明锁含源/新建/index.json,单 commit)。依赖 G1。注意 §6.A 前提:产出文件清单需先声明(见 coordinator-design §9)。
- **G8 skill/系统提示词从磁盘每步重读**(§4):改"会话构造一次性烤入"为"每步/每轮从磁盘重读",中途改 skill 仅影响其后步骤。
- **G9 动态系统提示词**(§10):运行中插入/修改系统提示词;与 G8 同源,可合并设计。
- **G10 多 skill 语义**(§10 开放问题):**已实现**。定为有序栈 + 后者覆盖前者(last-wins),栈每轮从磁盘重读(§4),单 skill 为退化特例。详见 `multi-skill-design.md`。
- **G11 MCP 搜索工具**(§10):接第三方 MCP 搜索,补 `find` 之外的 web/资料检索能力。
- **G12 model id 钉死**(§9):`Model` 扩到可指定带日期快照 id,署名/复现用确切版本。
- **G13 git blame 行级归属**(§9):暴露 `git blame` 行级作者(工具 + webui 端点)。
- **G14 sidecar 资源**(§7,低优先):富文本/配图的"正文 + sidecar"约定,为转 PDF 铺路。

## 不 assign(记录)
- **§11 硬边界**:人在未实质产出的 diff 上署人名,任何系统都无法从单个 commit 分辨"人写"与"人点头"。内核明确不收口,这里同样留作已知边界,不立任务。
