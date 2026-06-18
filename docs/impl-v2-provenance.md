# 实现文档 v2 · provenance(字级作者层)

> 状态:`feat/provenance` worktree 实现完成,全门禁绿(`just ci` / `just test` / `just doc`)。
> 模块:`src/provenance`(`mod.rs` + `tests.rs`)。只读 `src/content`(冻结契约),不改 `src/tool`。
> 对应规格:`docs/impl-v2.md` §3。

---

## 1. 职责与边界

在冻结的内容契约 `content::RichText` / `content::Document` 之上,提供**字级作者语义**:

- **单一编辑原语** `apply_edit`:插入 / 删除 / 替换,按作者切分 / 合并游程(Run)。
- **作者查询**:某字节位置的作者、某字节区间的作者切片、单段 `RichText` 的 contributor 集合。
- **文件级聚合** `contributors(&Document)`:全文 contributor 标签(喂 `ArticleMeta::contributors` / git author)。
- **作者归属 diff** `diff(&old, &new)`:谁加了 / 删了什么。

**不做**:任何 IO;不碰 `src/content`、`src/tool`(与 tool 写作工具的接线留合并期);inline 标记(v2 不做)。

---

## 2. 编辑原语语义(核心)

### 2.1 偏移量是「字节」

本层所有偏移量都是对**游程拼接后的纯文本**(`RichText::plain_string()`)的**字节**下标。理由:

- `content` 契约本身就以字节计长(`RichText::len()` 是字节数);
- 上层编辑工具天然说字节偏移;
- 与 `str` 切片 / `str::is_char_boundary` 同语义,验证逻辑直接复用,无歧义。

不在 UTF-8 字符边界上、或越界的偏移 → 返回 `ProvenanceError`,**不静默截断**。

### 2.2 `Edit` 三型

| 变体 | 语义 |
|---|---|
| `Insert{ at, text }` | 在 `at` 处插入 `text`(新字标 `author`) |
| `Delete{ range }` | 删除 `range`,两侧文本及其作者保留 |
| `Replace{ range, new_text }` | 删 `range` 再在原点插 `new_text`(= 一次编辑、一次归一化) |

`Replace` 独立成型(而非「删+插」两步),保证「重写某段」是**一个**原子编辑、只归一化一次,语义更干净。

### 2.3 切分 / 合并 / 归一化

`apply_edit` 的流程:

1. **校验**先行(`check_boundary` / `check_range`)——校验失败时 `text` **原样不动**,绝不留半改状态。
2. **splice**:遍历游程,对跨越 `range` 边界的游程切两刀,保留侧**沿用原作者**;被删区间丢弃。
3. **插入替换文本**:作为单个 `author` 游程,splice 进 `range.start` 处(该点已被第 2 步切成游程边界)。空替换 = 纯删除,不加游程。
4. **normalize**:丢空游程 + 合并相邻同作者游程 → 规范形(契约文档定义的 canonical form:无空游程、无相邻同作者)。

**关键不变式**:未被编辑触碰的字符,作者身份恒不变;新写入字符恒标 `author`;输出永远是规范形。

---

## 3. 查询与聚合

- `author_at(text, at) -> Option<&AuthorId>`:`at == len`(文末)返回 `None`,否则返回该字节所在字符的作者。
- `authors_in_range(text, range) -> Vec<(Range, &AuthorId)>`:把 `range` **精确平铺**成连续、非空、相邻作者相异的切片;空区间 → 空 Vec。
- `contributors_of(text) -> Vec<&AuthorId>`:单段去重,**首见顺序**(reading order)。
- `contributors(doc) -> Vec<String>`:全文档去重,首见顺序,经 `AuthorId::tag()` 渲染成标签。`CodeBlock` 不带字级作者(契约里它的 body 是裸 `String`),故不贡献作者——代码块归属属于块级 meta,在本「纯文本」层之外。返回 owned `String` 而非引用,直接喂文件级 provenance。

---

## 4. 作者归属 diff

### 4.1 表示

`Vec<DiffOp>`,`DiffOp ∈ {Equal, Delete, Insert}`,每个 op 带 `text` 与 `author`:

- `Equal{text, author}`:两版都有的不变文本,带其(新==旧)作者。
- `Delete{text, author}`:仅旧版有(被删),带**原作者**。
- `Insert{text, author}`:仅新版有(新增),带**新作者**。

「谁加了 / 删了什么」直接读 op 类型 + `author`。可重建性:`Equal+Insert` 拼回 `new`;`Equal+Delete` 拼回 `old`(已测)。

### 4.2 算法

字符级 LCS(最长公共子序列)对齐:公共子序列 → `Equal`,仅旧 → `Delete`,仅新 → `Insert`;回溯时逐字符带上作者,再把**同类型同作者**的连续字符合并成一个 op。

**复杂度**:LCS 表 `O(n·m)` 时间与空间(n、m 为两侧字符数)。对段落级 / 文章级输入完全够用,符合「n 不确定很大就别上花算法」(Rob Pike #1)。**已知缺口**见 §7。

---

## 5. 错误类型

`ProvenanceError`(`thiserror`,`#[non_exhaustive]`,`PartialEq` 便于测试断言):

| 变体 | 触发 | 携带 |
|---|---|---|
| `OffsetOutOfBounds` | 偏移 > len | `offset`, `len` |
| `NotCharBoundary` | 偏移落在多字节字符内部 | `offset` |
| `InvalidRange` | `start > end` | `start`, `end` |

无 IO,故全是输入错误;每个都带具体偏移,便于上层定位。

---

## 6. 公共 API 一览

```rust
// 错误
pub enum ProvenanceError { OffsetOutOfBounds{offset,len}, NotCharBoundary{offset}, InvalidRange{start,end} }
pub type Result<T> = std::result::Result<T, ProvenanceError>;

// 编辑原语
pub enum Edit { Insert{at,text}, Delete{range}, Replace{range,new_text} }
impl Edit { fn insert(at, text) -> Edit; fn delete(range) -> Edit; fn replace(range, new_text) -> Edit; }
pub fn apply_edit(text: &mut RichText, edit: Edit, author: &AuthorId) -> Result<()>;
pub fn normalize(text: &mut RichText);

// 查询 / 聚合
pub fn author_at(text: &RichText, at: usize) -> Result<Option<&AuthorId>>;
pub fn authors_in_range(text: &RichText, range: Range<usize>) -> Result<Vec<(Range<usize>, &AuthorId)>>;
pub fn contributors_of(text: &RichText) -> Vec<&AuthorId>;
pub fn contributors(doc: &Document) -> Vec<String>;

// diff
pub enum DiffOp { Equal{text,author}, Delete{text,author}, Insert{text,author} }
pub fn diff(old: &RichText, new: &RichText) -> Vec<DiffOp>;
```

英文 rustdoc 齐全(`# Errors` / `# Examples`),6 个 doctest。

---

## 7. 关键决策与取舍

| # | 决策 | 取舍 |
|---|---|---|
| P1 | 偏移量用**字节**,非 char index | 与 `content::len` / `str` 切片 / 上层工具一致;代价是调用方需保证落在字符边界(本层做校验兜底) |
| P2 | `Replace` 独立成型 | 「重写某段」一次原子编辑、一次归一化;比「删+插」两步更干净 |
| P3 | 校验先行、失败不改 `text` | 编辑要么全成功要么不动,无半改态 |
| P4 | diff 用 `O(n·m)` 字符级 LCS | 段落/文章级足够,简单可证;大输入是已知缺口(§8) |
| P5 | diff 的 `Equal`/`Insert` 归属用**新作者**,`Delete` 用**旧作者** | 「谁在场写的就是谁的」;不变文本新旧作者本就相等 |
| P6 | `contributors` 跳过 `CodeBlock` | 契约里代码块无字级作者;块级归属留 meta,不混进文本层 |
| P7 | `is_char_boundary` 不物化整串 | 直接在游程上定位 + `str::is_char_boundary`,省一次全量拷贝 |

---

## 8. 测试覆盖

`src/provenance/tests.rs`,46 个单测,分组:

- **归一化**:丢空游程、合并相邻同作者、保留相异作者、全空→空。
- **插入**:中间(切分)/ 行首 / 行尾 / 空文本 / 同作者合并 / 空串 no-op。
- **删除**:跨游程中段 / 整段删后合并 / 行首 / 行尾 / 删空全文 / 空区间 no-op。
- **替换**:中段标新作者 / 跨游程(第三作者)/ 替换空=删除 / 替换全文。
- **UTF-8 边界**:插/删落在多字节字符内部被拒、越界被拒、逆序区间被拒、跨游程边界判定、合法多字节边界编辑成功(`café`→`cafe`)。
- **查询**:`author_at`(各位置 + 文末 `None` + 空文本)、`authors_in_range`(精确平铺 / 单游程 / 空区间 / 全范围)。
- **聚合**:`contributors_of` 首见顺序 + 空;`contributors` 跨块首见顺序(含 Heading/List/Quote/CodeBlock)+ 空 + 仅代码块为空。
- **diff**:中插 / 删除 / 替换(新旧作者各归各)/ 全等 / 空→全插 / 全删→空 / Unicode;可重建性断言。
- **端到端**:人写句子 + 两 agent 改不同段,逐游程作者 + contributor 顺序全断言。

---

## 9. 已知缺口 / 留给合并期

1. **diff 复杂度**:`O(n·m)` LCS 在超大输入(整本书级)上内存吃紧。若日后需要,可换 Myers diff(`O(nd)`)或先做块级粗对齐再段内细对齐——当前规格无此需求,故不预先复杂化。
2. **与 `tool` 的接线**:`From<WriterId> for AuthorId` 适配、把 `apply_edit` 接进 tool 的写 / 编辑工具,**有意留给合并期**(避免本 worktree 动共享文件)。`tool::workspace::WriterId::provenance_tag` 与 `AuthorId::tag` 已同构,合并期对接零摩擦。
3. **diff 的 `Equal` 作者取新版**:当一段文本内容不变但作者在两版间「换了人」(理论上 `apply_edit` 不会产生这种情况,但手工构造 `RichText` 可以),`Equal` 会报新作者。这是有意选择(见 P5),非 bug;若上层需要「保留旧作者」语义,合并期可加一个 diff 变体参数。
4. **块级 diff**:当前 `diff` 作用于 `RichText`。`Document` 级 diff(块增删移动)未做——v2 规格只要求 `RichText`/`Document` 间的作者归属差异,文本级 diff 已满足;块结构 diff 留 DSL/合并期按需补。
