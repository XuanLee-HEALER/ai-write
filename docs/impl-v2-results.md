# 实现结果 v2:content / DSL / provenance / LLM 编排(三 worktree 并行)

> 状态:**三块已实现并合并进 `main`,全绿**(供 review)。
> 对应设计:`docs/impl-v2.md`。各块决策详见 `docs/impl-v2-dsl.md` / `docs/impl-v2-provenance.md` / `docs/impl-v2-orchestration.md`。
> 校验基线(主控独立重跑,merged main):`just ci` / `just test`(**189 单测 + 6 集成[2 live ignored] + 39 doctest**)/ `just doc` 三者全绿;全程零真机调用。

---

## 0. 并行执行情况

| worktree / 分支 | 提交 | 产物 | 门禁(各自) |
|---|---|---|---|
| `feat/dsl` | `3eba781` | `src/dsl/mod.rs` + 决策文档 | ci/test/doc 绿 |
| `feat/provenance` | `7e18641` | `src/provenance/{mod,tests}.rs` + 决策文档 | ci/test/doc 绿 |
| `feat/llm-orchestration` | `51d3450` | `src/engine/{mod,orchestration}.rs` + 决策文档 | ci/test/doc 绿 |

- 三个 opus agent 各钉一个**真实 git worktree** 并行实现(harness 原生 worktree 隔离本会话不可用,故用 `git worktree add` 手动建)。
- **改动域完全不相交**(dsl→`src/dsl`、provenance→`src/provenance`、orchestration→`src/engine`;`src/content`/`src/lib.rs` 三方都没碰)→ 三路合并**零冲突**(`--no-ff`,见 `git log`)。
- 共享契约 `src/content`(main 预置、冻结)是并行可行的关键。

---

## 1. content(main 预置契约)

`src/content/mod.rs`:`AuthorId` / `Run{text,author}` / `RichText{runs}` / `Block`(Paragraph/Heading/CodeBlock/ListItem/Quote)/ `Document`。纯数据 + serde,无 IO,无 feature gate。字级作者 = **作者游程**表示。7 个测试。

## 2. DSL(`feat/dsl`,详见 `docs/impl-v2-dsl.md`)

- `parse(&str, AuthorId) -> Result<Document, DslError>` / `serialize(&Document) -> String`(值级互逆)/ `render_html(&Document) -> String`(每 run 渲染为转义 `<span data-author>`,连续 list item 合并 `<ul>`)。
- 自研行式语法:`: ` 段落、`#1`–`#6` 标题、`- ` 列表、`> ` 引用、` ``` ` 代码块。`DslError`(non_exhaustive,带 1-based 行号)。24 单测。
- 缺口:无 inline 标记(粗斜体/链接,v3)、列表扁平、serialize 不编码字级作者(扁平成单作者——字级作者归 provenance + `data-author`)。

## 3. provenance(`feat/provenance`,详见 `docs/impl-v2-provenance.md`)

- **单一编辑原语** `apply_edit(&mut RichText, Edit, &AuthorId)`(Insert/Delete/Replace,字节偏移;**先校验后改,出错不动原文**;切分/合并游程、新文标作者、归一化)。
- `author_at` / `authors_in_range`(精确铺砌)/ `contributors_of` / `contributors(&Document)->Vec<String>` / `diff(&RichText,&RichText)->Vec<DiffOp>`(字级 LCS,Delete 带旧作者、Insert/Equal 带新作者)。`ProvenanceError`(越界/非字符边界/非法区间)。46 单测。
- 缺口:`diff` 为 O(n·m) LCS(段落/文章级够用);`Document` 级块结构 diff 未做;`tool` 接线留合并期。

## 4. LLM 编排(`feat/llm-orchestration`,详见 `docs/impl-v2-orchestration.md`)

- `Master::run_goal(&mut self, goal, options, slave_model) -> Result<GoalOutcome, req::Error>`:给 master Session 配**编排工具** + 编排系统提示词,LLM 规划→派发→汇总。`GoalOutcome{outcome,message,reports}`。
- `engine::orchestration` 工具:`CreateTheme` / `CreateArticle` / `ListArticles` / `DispatchWriter`(经现有 `spawn_slave_with_sink` 派 slave 并记 report)/ `ListReports`。复用现有 `Event`(master role + slave 的 SlaveSpawned/SlaveReported,单 feed 可观测),未新增变体。
- **`run_one` 可执行代码字节级未变**,v0/v1 全部测试保留。离线 loopback 测试断言 plan→dispatch→aggregate 零真机。
- 缺口:slave token 用量未折回 master;`dispatch_writer` 串行阻塞(暂无并行多文派发);无引擎级自动重试/改派;slave 用 `SessionOptions::default()`(仅 model 身份可配)。

---

## 5. 主控 review 与下一步

**review**:三模块代码独立读过 + 合并后独立重跑三门禁全绿;改动域不相交、合并零冲突;`run_one` 未回归。三块作为**独立能力**已落地、测试充分、文档齐全。

**有意留给下一块的「集成层」**(本轮**未做**,需单独一块谨慎推进,因为它会改 v0/v1 现有文章存储与工具行为):
1. `From<WriterId> for AuthorId`(平凡适配;`WriterId::provenance_tag` 与 `AuthorId::tag` 已 1:1)。
2. 把文章从**纯文本**迁到**内容模型**:编辑工具(`write_article`/`edit_article`/`apply_edits`)走 `provenance::apply_edit` + DSL 序列化,落盘 `content::Document`,字级作者随之持久化(配合 git 文件级 + 字级两层 provenance)。
3. WebUI 用 `dsl::render_html`(带 `data-author`)做正文渲染、用 `provenance::diff` 做作者归属可视化。
4. 编排:`Master::run_goal` 接进 webui 的 `POST /api/tasks`(让人能下"主题级目标"而非单篇)。

> 集成是一次**行为变更**(纯文本→富文本模型),应作为独立 block + 自己的测试,故未塞进本轮的并行实现里。

**已知缺口汇总**:见各 §2–§4 末尾及三份决策文档的 §7/末节。
