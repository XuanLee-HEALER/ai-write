# 实现文档 v2:内容模型 / DSL / provenance / LLM 编排(三 worktree 并行）

> 状态:**设计 + 并行实现规格**(自主推进;供 review)。
> 三块:① DSL(富文本结构 + 语法 + HTML 渲染)② provenance(字级作者 + 编辑原语 + diff/聚合)③ LLM 编排(Master 真正用 LLM 驱动)。
> 并行策略:**三个 worktree 各一块**,共享一个 main 上预置的"内容模型"纯数据契约(`src/content`),互不改对方文件。

---

## 0. 并行可行性的关键:共享内容模型契约

provenance 与 DSL 在**数据上耦合**——字级作者就活在富文本的文本节点里。要让它们在隔离 worktree 里并行且各自能过门禁,采用:

**在 main 上先 scaffold `src/content`(只放纯数据类型,无重逻辑、可编译可测),作为冻结契约。** 然后:
- **DSL worktree** 在 `src/dsl` 加逻辑(语法 parse/serialize、HTML render),操作 `content::Document`。
- **provenance worktree** 在 `src/provenance` 加逻辑(编辑原语、作者查询、diff、聚合),操作 `content::RichText`。
- **编排 worktree** 改 `src/engine`(+ 编排工具),与内容模型无关。
- **三方都不改 `src/content`**(把它当固定 API);各自只新增自己的模块 → 合并零冲突。

字级 provenance 的数据表示(最优解):**文本 = 一串「作者游程(Run)」**,每段连续同作者文本是一个 `Run{text, author}`;编辑时按作者切分/合并游程。这是 CRDT/OT 系世界里 char-level attribution 的标准高效表示。

---

## 1. 共享契约:`src/content`(main 预置,冻结)

```rust
// 纯数据,serde 可序列化;无 IO、无重逻辑
pub enum AuthorId { Human, Agent { model: String, label: String } }   // 镜像 WriterId,但 content 自包含
pub struct Run { pub text: String, pub author: AuthorId }             // 连续同作者文本
pub struct RichText { pub runs: Vec<Run> }                            // 携带字级作者的文本
pub enum Block {
    Paragraph(RichText),
    Heading { level: u8, text: RichText },
    CodeBlock { lang: Option<String>, code: String },                 // 代码块不做字级作者(整块一作者,记在 meta)
    ListItem(RichText),
    Quote(RichText),
}
pub struct Document { pub blocks: Vec<Block> }
// 平凡构造器:Run::new / RichText::from_plain(text, author) / RichText::plain_str / Document::new …
```
- v2 **不做 inline 标记**(粗体/斜体/链接):core 只到"块结构 + 字级作者游程",标记留 v3(DSL 语法可预留)。
- `AuthorId` 与 `tool::workspace::WriterId` 同构;合并期我提供一个 `From<WriterId> for AuthorId` 适配(放 content 或 tool,届时定)。

---

## 2. DSL worktree(`src/dsl`,branch `feat/dsl`)

**职责**:自研一套**富文本 DSL 语法** ⇄ `content::Document` 的双向转换,以及 `Document → HTML` 渲染(所见即所得的底层)。

- **DSL 语法**(最优解,自定):一套比 markdown 更明确、可携带块类型的轻量标记;`parse(&str) -> Result<Document>` 与 `serialize(&Document) -> String` 互逆(round-trip 稳定)。
- **HTML 渲染**:`render_html(&Document) -> String`,块→语义 HTML(`<p>/<h1>/<pre>/<li>/<blockquote>`),文本节点把 `RichText` 的游程渲染为带 `data-author` 的 `<span>`(为字级 provenance 可视化预留)。
- 自包含、可独立过门禁;只读 `content` 类型,**不改 `src/content`**。
- 测试:parse/serialize round-trip、各块类型、HTML 转义与 data-author、错误语法路径。
- 留 `docs/impl-v2-dsl.md`(语法定义 + 决策 + API)。

---

## 3. provenance worktree(`src/provenance`,branch `feat/provenance`)

**职责**:在 `content::RichText` 上的**字级作者语义**。

- **单一编辑原语**(对齐 product-discussion §9 的自洽性):`apply_edit(&mut RichText, edit, author)` —— 插入/删除/替换,**按作者切分/合并游程**;新写入的字标 `author`,未动的保留原作者。AI 与人最终都走这一条。
- **作者查询**:某字符区间的作者、整段 contributor 集合。
- **provenance diff**:两版 `RichText`/`Document` 间按作者归属的差异(谁加了/删了什么)。
- **文件级聚合**:Document 的全部 contributor(喂 `ArticleMeta::contributors` / git author)。
- 自包含、可独立过门禁;只读 `content` 类型,**不改 `src/content`、不改 `src/tool`**(与 tool 的集成留合并期,避免动共享文件)。
- 测试:游程切分/合并、插入/删除/替换的作者正确性、区间查询、聚合、diff、UTF-8 边界。
- 留 `docs/impl-v2-provenance.md`(编辑原语语义 + 决策 + API)。

---

## 4. LLM 编排 worktree(`src/engine` 扩展,branch `feat/llm-orchestration`)

**职责**:把 Master 从 v0/v1 的**确定性** `run_one` 升级为**真正 LLM 驱动的编排 session**。

- Master = 一个配了**编排工具**的 `Session`:`create_theme` / `create_article` / `dispatch_writer`(派 slave 写某文章)/ `collect_reports` / `list_articles` 等;LLM 自己规划(可一题多文)、派发、看 report 决定下一步。
- `dispatch_writer` 工具内部走现有 `spawn_slave`(thread);Master 收结构化 `SlaveReport`(不读 slave 全量历史,沿用 supervisor 模式)。
- 保留确定性 `run_one` 作为兼容入口/fallback;新增 `run_llm(goal)` 之类的 LLM 编排入口。
- 编排工具放 `src/engine`(**不放 `src/tool`**,避免与 provenance worktree 撞文件)。
- 与 content/dsl/provenance **无关**,可完全独立并行。
- 测试:离线 loopback client 喂 canned 编排 tool-call 序列,断言 Master 规划/派发/汇总路径(零真机)。
- 留 `docs/impl-v2-orchestration.md`(编排工具 + Master 循环 + 决策 + API)。

---

## 5. 合并 / 集成计划(我做)

三 worktree 跑完 → 我逐分支 review + 顺序合进 main:
1. `feat/dsl` → main(只加 `src/dsl`,零冲突)。
2. `feat/provenance` → main(只加 `src/provenance`,零冲突)。
3. `feat/llm-orchestration` → main(只动 `src/engine`)。
4. **集成层(我写)**:`From<WriterId> for AuthorId`;把 provenance 的编辑原语接进 tool 的写/编辑工具(让文章真正走内容模型 + 字级作者),webui 用 dsl 的 HTML render + provenance diff 做可视化。这一步是**有意留给合并后做**的连接工作,不塞进任一 worktree。
5. 全量 `just ci/test/doc` 绿,真机冒烟,写 `docs/impl-v2-results.md`(合并结果 + 集成决策 + 缺口)。

---

## 6. 关键决策(供 review)

| # | 决策 | 取舍 |
|---|---|---|
| C1 | 字级 provenance = **作者游程(Run)** 表示 | 高效、编辑友好;比逐字符标注省 |
| C2 | **content 纯数据契约预置 main 并冻结**,三 worktree 只加不改 | 隔离并行零冲突;代价是契约要一次想全 |
| C3 | v2 **不做 inline 标记**(粗斜体/链接) | 先把"块 + 字级作者 + DSL + HTML"打通;标记留 v3 |
| C4 | 自研 DSL 语法(非直接复用 markdown) | 对齐你"自研 DSL"原话;可携带块类型与 data-author |
| C5 | provenance↔tool、dsl↔webui 的**集成留合并期**,不塞进 worktree | worktree 不动共享文件 → 合并零冲突;集成由我统一做 |
| C6 | Master LLM 编排**新增入口、保留确定性 `run_one`** | 不破坏 v0/v1 现有路径与测试 |
| C7 | 三块均 **opus worktree 并行**,各自过门禁后再合 | 并行效率;耦合处用冻结契约 + 合并期集成化解 |

