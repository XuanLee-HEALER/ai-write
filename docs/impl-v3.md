# 实现文档 v3:webui 重构 + 文章层级 + skill 系统 + master 对话接入

> 状态:**设计 + 实现规格**(经 chat review 确认;供次日 review)。
> 目标:把展示用 webui 从「单篇运行 + 平铺文章」升级为「主题级对话编排 + 文章层级 + 三栏文章视图」,并引入可选的写作 skill。
> 对应前序:`docs/impl-v2-results.md §5`(本轮做其中「master 对话接入 webui」一项,**不做** 纯文本→`content::Document` 的字级作者迁移)。

---

## 0. 范围边界(重要)

- **本轮做**:① 文章层级 manifest(父子 + 阅读顺序)② skill 系统(从 `./skills/*.md` 加载写作提示词,master 对话时选)③ `Master::run_goal` 接进 webui(主题级对话)④ 前端重构:左树层级 + 中间区双模式(theme 对话 / article 三栏)。
- **本轮不做**(仍是独立「集成层」):文章从纯文本迁到 `content::Document`、provenance `apply_edit` 落盘、DSL `render_html` 的 `data-author` 字级作者染色。所以文章三栏里:**左=纯文本正文、中=事件流、右=git 版本/diff**,**不是**字级染色。

---

## 1. 文章层级 manifest(P1,`src/tool/workspace.rs`)

数据模型(最简,Rob Pike:简单数据结构):
- `ArticleMeta` 增 `#[serde(default)] parent: Option<String>`(父文章的 file_name;`None`=顶层)。
- `Index.order: Vec<String>` **不变**,仍是全局**阅读顺序**(线性化)。
- 层级树由 `parent` 指针推导,同级按它们在 `order` 中的相对位置排。**一个阅读顺序源 + 一组父指针**,不引入嵌套 children 向量。
- `Index` 增 `#[serde(default)] config: ThemeConfig`(见 §4)。

新增 workspace 方法:
- `set_parent(theme, file, parent: Option<&str>)`:校验父存在、非自身、**无环**(从拟定父沿 parent 上溯,遇到 file 即环 → 拒绝),写回 index。
- `reorder(theme, new_order: Vec<String>)`:校验 new_order 与现有文章集合**同集**(不增不漏),替换 `order`。
- `load_config / save_config`(经 `Index.config`)。

向后兼容:旧 `index.json` 无 `parent` / `config` 字段 → serde default → 全文章顶层、空配置。

## 2. AI 能组织层级(P2,`src/engine/orchestration.rs`)

新增 master 工具 `organize_articles`:一次设若干文章的父子关系 + 可选整体阅读顺序,经 `ctx.ws.set_parent` / `reorder`。注册进 `orchestration_tools`,master 规划时即可写结构(人也能通过 webui 端点写,见 §4)。

## 3. skill 系统(P3,新增 `src/skill/` + engine 接线)

- `Skill { id, name, description, body }`,`id` = 文件名 stem。从 dev 目录 `./skills/*.md` 加载:可选 frontmatter(`---` 包裹的 `key: value`,认 `name` / `description`)+ 正文 = 提示词 body。
- `load_skills(dir) -> Result<Vec<Skill>, SkillError>` / `load_skill(dir, id)`。纯 std(fs + 字符串解析),**不 feature-gate**(engine 与 webui 都用)。
- **engine 拼接**:把现有 `SLAVE_SYSTEM_PROMPT` 拆成 `SLAVE_OPERATIONAL_RULES`(锁/工具/report 机制,固定)+ `DEFAULT_SLAVE_ROLE`(角色/风格,默认)。
  `compose_slave_prompt(skill_body) = skill_body + "\n\n" + SLAVE_OPERATIONAL_RULES`;无 skill 时 `skill_body = DEFAULT_SLAVE_ROLE`(行为与现状等价)。
  **skill 只管角色/文风/拒绝,不碰工具机制**——锁纪律和 report 由固定段保证。
- 透传:`SlaveTask` 增 `#[serde(default)] system_prompt: Option<String>`(None→默认拼接);`OrchestratorState` 持所选 `slave_prompt`,`dispatch` 据此建 slave;`Master::run_goal` 增 `slave_skill_body: Option<&str>` 参数。

## 4. theme 全局 config + webui 后端(P4,`src/webui/mod.rs`)

`ThemeConfig`(存进 `Index.config`):
- `description: String` —— 主题目标/描述。
- `default_skill: Option<String>` —— 默认写作 skill id。
- `slave_model: Option<String>` —— 派发 slave 用的模型(`deepseek-v4-flash` / `deepseek-v4-pro`)。

新增 / 改动端点:
- `GET /api/themes/{theme}/articles` —— **改返回** outline:`[{file, title, parent, depth}]`,按阅读顺序。前端据 depth 缩进画树。
- `GET /api/themes/{theme}/config` / `PUT /api/themes/{theme}/config` —— 读写 ThemeConfig。
- `POST /api/themes/{theme}/articles/{file}/parent` —— 人工设父子(body `{parent: string|null}`)。
- `POST /api/themes/{theme}/reorder` —— 人工设阅读顺序(body `{order: [..]}`)。
- `GET /api/skills` —— 列 dev skill(`[{id, name, description}]`)。
- `POST /api/themes/{theme}/chat` —— body `{goal, skill_id?, slave_model?}`,跑 `Master::run_goal`(spawn_blocking),事件走**现有 SSE**,返回 `GoalOutcome`。
- 保留 `POST /api/tasks`(单篇 run_one)兼容,不删。
- `AppState` 增 `skills_dir: PathBuf`(dev 默认 `./skills`,可 `AI_WRITE_SKILLS` 覆盖)。

master「对话」语义(决策 D1):**每次提交 = 一次 `run_goal`**(单次目标→规划/派发/收束),前端把多次目标 + master 回复累积成对话 log。真·有状态多轮(master session 跨轮记忆)较重,**延后**。

## 5. 前端重构(P5,`src/webui/index.html`)

- **左栏**:Workspace + theme(不变);theme 下文章改**层级树**(按 outline 的 depth 缩进)。点 theme → theme 模式;点文章 → article 模式。
- **中间区 theme 模式**:master 对话框(goal 输入 + skill 选择 + model 选择)+ 对话 log + AI operations 时间线 + theme config 表单。**不显示正文**。
- **中间区 article 模式**:三栏 **3 : 0.5 : 0.5** —— 左=正文(上方工具栏:刷新/撤销/diff)、中=按本文过滤的协作状态/命令流(事件流)、右=git 版本/diff/undo(复用现有)。

---

## 6. 关键决策(供 review)

| # | 决策 | 取舍 |
|---|---|---|
| D1 | master 对话 = 每次提交跑一次 `run_goal`,多轮持久化延后 | 不改 Session/Master 生命周期即可上线;有状态多轮单独做 |
| D2 | skill 只管角色/文风/拒绝,操作规则(锁/report)固定拼前缀 | 防止 skill 覆盖导致 slave 不加锁/不报告 |
| D3 | 文章层级 = `parent` 指针 + 全局 `order` 阅读顺序 | 单一阅读顺序源 + 父指针;不引入嵌套结构 |
| D4 | skill 文件 dev 放 `./skills/*.md`,frontmatter + body | 贴合「dev 放当前目录、做成 skill」;无需新依赖,手写极简解析 |
| D5 | 本轮不做字级作者染色(纯文本→Document 迁移仍独立) | 控制行为变更面;中/右栏先用事件流 + git diff |
| D6 | theme config 存进 `Index.config`(同 index.json) | 不新增文件;随 index 一起被 git 版本化 |

## 7. 缺口 / 留待后续
- 字级作者可视化(DSL `render_html` + provenance `diff`)= 独立集成层。
- master 有状态多轮对话。
- article 视图「从正文直接发起 AI 编辑」(本轮工具栏只放查看/版本类)。
- slave token 用量未折回 master(沿用 v2 缺口)。
