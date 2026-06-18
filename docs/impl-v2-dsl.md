# impl-v2 DSL 层实现文档(`src/dsl`)

> worktree: `feat/dsl`。职责见 `docs/impl-v2.md` §2:自研富文本 DSL 语法 ⇄ `content::Document` 双向转换 + `Document → HTML` 渲染。只读 `src/content`(冻结契约),不改其它模块。

## 1. 语法定义(grammar)

行导向(line-oriented):一个 `Document` = 一串块(`Block`),**每块占一行**,代码块是唯一的多行例外。每行用一个开头**记号(sigil)**标明块类型,从前 1–2 个字符即可判定,无需 look-ahead、无需计数、不与正文冲突。

| 块 | 语法 | 例 |
|---|---|---|
| `Paragraph` | `: ` + 文本 | `: Hello world.` |
| `Heading` | `#` + 层级数字 `1`–`6` + ` ` + 文本 | `#2 A subsection` |
| `ListItem` | `- ` + 文本 | `- first point` |
| `Quote` | `> ` + 文本 | `> to be or not` |
| `CodeBlock` | ` ``` ` + 可选 lang,正文若干行,闭合 ` ``` ` | 见下 |

代码块:

````text
```rust
fn main() {}
```
````

- 块之间用单个 `\n` 连接;serialize **不产生空分隔行**,末尾无换行。
- parse **忽略块间空行**,所以手写输入可以自由留白(空行只是视觉分隔)。
- **空块**:写成裸记号 —— `:`(空段落)、`-`(空列表项)、`>`(空引用)、`#3`(空三级标题,数字后无空格)。serialize 对空 `RichText` 输出裸记号(不带尾随空格),与之精确互逆。

### 行内转义(inline escaping)

v2 无 inline 标记(粗/斜/链接),正文(`RichText`)在 DSL 形态里**摊平为 plain string**。因为是行导向,只需转义两个会破坏分帧的字符:

- `\` → `\\`
- 换行 `\n` → `\n`(字面反斜杠 + n)

parse 反向还原。出现孤立的 `\`(行尾)或 `\` 后跟 `\`/`n` 以外的字符 → `DslError::BadEscape`。这样**正文里出现记号样字符也不会被误判成块引导**(它跟在本块自己的记号之后,纯属文本)。例:`: > not a quote` 是一个内容为 `> not a quote` 的段落。

## 2. 公共 API

```rust
// src/dsl/mod.rs
pub fn parse(input: &str, author: AuthorId) -> Result<Document, DslError>;
pub fn serialize(doc: &Document) -> String;
pub fn render_html(doc: &Document) -> String;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum DslError {
    UnknownBlock(usize),     // 行号:无可识别记号
    BadHeadingLevel(usize),  // 行号:# 后不是 1-6 数字+空格
    UnterminatedCode(usize), // 开栏行号:代码栏未闭合
    BadEscape(usize),        // 行号:非法 \ 转义
}
```

- `parse`:把 DSL 文本解析成 `Document`,**全部正文归属传入的 `author`**(单作者,每块一个 `Run`)。字级多作者是 provenance 层的事,DSL 不管。
- `serialize`:输出**规范形式**(canonical):每非代码块一行、代码块用 ` ``` ` 围栏、无空分隔行、块间单 `\n`、无尾换行;正文按上面规则转义。
- `render_html`:见下。
- `DslError`:`thiserror`,`#[non_exhaustive]`,带 1-based 行号,实现 `PartialEq`/`Eq` 便于测试断言。

### round-trip 保证

`parse` 与 `serialize` 在**值层面**互逆:

- `parse(&serialize(d), a) == d`,对任何正文为单作者 `a`、且已规范化(每块一个 run)的 `d`。
- `serialize(&parse(s, a)?)` == `s` 的**规范形式**。不被保留的只有两类信息(刻意如此):块间装饰性空行、字级作者区分 —— 二者都不属于 DSL 语法表达的范围。

## 3. HTML 渲染

`render_html(&Document) -> String`,块→语义 HTML:

- `Paragraph` → `<p>…</p>`
- `Heading{level}` → `<h1>`…`<h6>`(level 越界时 clamp 到 1..=6 防御性处理)
- `ListItem` → `<li>`,**连续的 ListItem 合并进单个 `<ul>`**(v2 列表是扁平的)
- `Quote` → `<blockquote>…</blockquote>`
- `CodeBlock{lang, code}` → `<pre><code>` / `<pre><code class="language-…">`

**正文逐 run 渲染为 `<span data-author="…">…</span>`**,属性值是 run 作者的 `AuthorId::tag()`(`"human"` 或 `"<model>/<label>"`)。每个 `Run` 一个 span —— 这是字级 provenance 可视化的承载点(provenance 层切出多作者 run 后,这里自然渲染成多个带不同 `data-author` 的 span)。

**转义**:元素文本、`data-author` 属性值、代码块的 lang 与正文,全部 HTML-escape 五个字符:`&`→`&amp;` `<`→`&lt;` `>`→`&gt;` `"`→`&quot;` `'`→`&#39;`。输出是**片段**(无 `<html>`/`<body>` 外壳),供上层嵌入。

注意:HTML 渲染走的是 `RichText.runs`(保留字级作者),而 DSL serialize 走 `plain_string()`(摊平)—— 二者目的不同,前者给 webui 可视化,后者给可编辑的 DSL 文本。

## 4. 设计决策与理由

| # | 决策 | 理由 / 取舍 |
|---|---|---|
| D1 | **行导向 + 开头记号**,非 markdown | 块类型从前缀即可判定,映射到 `Block` 全(total)且可逆(每块唯一写法、每行唯一块义);对齐 impl-v2 §6 C4「自研 DSL」。 |
| D2 | 标题用**字面数字** `#3 ` 而非计数 `###` | 消除「`####` 是四个还是手抖」的歧义;让 1–6 全程精确 round-trip;层级解析 O(1)。 |
| D3 | 段落记号选 `: `(非裸文本) | 裸文本当段落会让「未知语法」无从报错;显式记号让每行都自描述,malformed 输入能精确定位行号。 |
| D4 | 仅转义 `\` 与 `\n` | 行导向下只有这两个字符破坏分帧;转义集最小 → DSL 文本最贴近原文、最易手写/阅读。 |
| D5 | 空块 = 裸记号(`:` `-` `>` `#3`) | 让空 `RichText` 也有唯一可逆写法,round-trip 不丢空块;serialize 对空正文不加尾随空格,精确对称。 |
| D6 | parse 忽略空行,serialize 不产空行 | 手写友好(可留白)+ 输出规范(无冗余);代价是空行不 round-trip —— 但空行不承载语义,刻意不保留。 |
| D7 | 代码块 lang `trim` 后空则视为 `None` | ` ``` ` 与 ` ```  ` (尾随空格)都映射 `lang=None`,语义一致;非空 lang 也 `trim`,避免不可见空白进 class。 |
| D8 | HTML 逐 run 一个 span | 字级 provenance 的可视化承载;单作者文档退化成单 span,无额外开销。 |
| D9 | `'` 转义为 `&#39;`(非 `&apos;`) | `&apos;` 非 HTML4 命名实体,`&#39;` 全场景安全(含属性值与正文)。 |
| D10 | `DslError` 带行号 + `PartialEq` | 行号利于排错与测试断言;`#[non_exhaustive]` 留扩展余地(对齐 crate 内 `req::Error` 风格)。 |
| D11 | Heading level 越界时 serialize/render **clamp** 而非 panic | parse 产出的 level 必在 1..=6;但手搓的 `Document` 可能越界,clamp 保证仍输出合法语法/标签,不崩。 |

## 5. 测试覆盖(`src/dsl/mod.rs` 内 `#[cfg(test)]` + 5 个 doctest)

单元测试(24 个):

- **round-trip / 各块类型**:段落、全 6 级标题、列表+引用、带 lang 代码块、无 lang 多行代码块、空代码块、混合文档。
- **空文档**:parse/serialize/render 均处理空。
- **空块**:`:` `-` `>` `#3` 裸记号双向。
- **空行容错**:块间多空行被忽略;serialize 规范化(无空行)。
- **行内转义**:`\` 与换行 round-trip;记号样正文存活(`: > text`)。
- **错误路径**:`UnknownBlock`(含多行定位)、`BadHeadingLevel`(`#7`/`#0`/`#x`/`#2no-space` 四种)、`UnterminatedCode`、`BadEscape`(行尾孤立 `\` + 未知 `\t`)。
- **作者归属**:parse 把正文归给传入 author(用 agent 验证)。
- **HTML 渲染**:各块类型标签;连续 ListItem 合并单 `<ul>`(双 ul 计数);多 run 多 span;文本+属性 HTML 转义(含 `< & > " '`);代码块 lang 与正文转义(`<script>` 不泄漏);空段落渲染 `<p></p>`。
- **serialize→parse 方向**:手搓含 tab/unicode/JSON 代码块的 `Document` 还原。

doctest(5 个):module 级总览、`parse`、`serialize`、`render_html`(含多 run + 转义断言)、`DslError`(经各 fn 的 `# Examples`)。

## 6. 门禁

全绿:`just ci`(fmt-check + clippy `-D warnings` + check --all-features)、`just test`(135 全过,其中 dsl 24 单测 + 4 doctest)、`just doc`(rustdoc `-D warnings`)。另验 `cargo build --no-default-features --features async`(`content`/`dsl` 不受 feature gate,async-only 下也须编译)通过。`cargo fmt` 已跑。无新增依赖(`thiserror` 已在)。

## 7. 已知缺口 / 非目标

- **无 inline 标记**(粗/斜/链接/行内 code):对齐 impl-v2 §6 C3,留 v3;语法上记号占用了行首,未来 inline 标记可在正文内用成对定界符扩展,与现转义共存。
- **列表扁平**:无嵌套、无有序列表(`<ol>`)、无列表项内多段。连续 `ListItem` 即一个 `<ul>`,符合 content 契约(§1 注释「v2 keeps lists flat」)。
- **空行不 round-trip**:见 D6,刻意。块间装饰性空行在 parse→serialize 后归一消失。
- **字级作者不进 DSL 文本**:DSL serialize 摊平作者(走 `plain_string`);字级作者只在 `render_html` 的 `data-author` 体现。DSL 文本是「可编辑源」,作者归属由 provenance 层在 `RichText` 上维护。故 `serialize(parse(s, A))` 永远单作者 —— 多作者 `Document` 经 DSL round-trip 会丢作者区分(但 plain 文本与块结构不丢)。这是 DSL 作为「文本源」与 provenance 作为「作者真相」的分工,非 bug。
- **代码块正文不能含恰为 ` ``` ` 的整行**:这会被当成闭合围栏(与 markdown 同款限制)。v2 不引入更长围栏/缩进逃逸;真有此需求留后续。
- **CRLF**:`str::lines()` 会吃掉 `\r\n` 的 `\r`,故 CRLF 输入可 parse,但 serialize 只产 `\n`(LF 规范化)。视为 feature 而非缺口。
```
