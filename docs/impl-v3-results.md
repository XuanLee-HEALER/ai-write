# 实现结果 v3:webui 重构 + 文章层级 + skill 系统 + master 对话接入

> 状态:**全部实现并通过门禁**(供 review)。对应设计:`docs/impl-v3.md`。
> 校验基线:`just ci`(fmt-check + clippy + check)/ `just test`(**211 单测 + 6 集成[2 live ignored] + 42 doctest**)/ `just doc` 三者全绿;webui 端点经真机冒烟(零 LLM 调用)。

---

## 1. 文章层级 manifest(P1,`src/tool/workspace.rs`)

- `ArticleMeta` 加 `#[serde(default)] parent: Option<String>`;`Index` 加 `#[serde(default)] config: ThemeConfig`;新增 `ThemeConfig{description, default_skill, slave_model}`。旧 `index.json`(无 parent / config)经 serde default 正常加载 —— 有测试 `old_index_without_parent_or_config_deserializes` 守。
- 新增方法:`set_parent`(校验存在 / 非自身 / **环检测**)、`reorder`(校验为现有文章的排列)、`load_config` / `save_config`、`article_outline`(返回 `[{file,title,parent,depth}]`,深度由 parent 上溯,带防御性上限)。
- `delete_article` 现在把被删节点的子节点**上提到祖父**,避免悬空 parent 指针。
- 测试:层级/深度、自环/环/缺失拒绝、删除再挂、reorder 排列校验、config 往返、旧索引兼容(workspace 模块 28 测)。

## 2. organize_articles 工具(P2,`src/engine/orchestration.rs`)

- master 新工具 `organize_articles`:一次设 `relations`(父子)+ 可选 `order`(阅读顺序),经 `ctx.ws.set_parent` / `reorder`。注册进 `orchestration_tools`,master 系统提示词新增对应步骤。
- 测试:设层级+顺序、workspace 错误(缺父 / 非排列)上抛。

## 3. skill 系统 + engine 接线(P3,新增 `src/skill/`)

- `Skill{id,name,description,body}`;`load_skills(dir)` / `load_skill(dir,id)`,frontmatter(`---` 包裹 `name`/`description`)+ body 解析。**缺目录 → 空列表(非错)**;`load_skill` 拒绝路径穿越 id。纯 std,`lib.rs` 无条件导出。
- engine:`SLAVE_SYSTEM_PROMPT` 拆成 `SLAVE_OPERATIONAL_RULES`(锁/工具/report,固定)+ `DEFAULT_SLAVE_ROLE`;新 `pub fn compose_slave_prompt(skill_body)` = skill + 操作规则。无 skill 时 = `compose(DEFAULT_SLAVE_ROLE)`,**行为与改造前等价**。
- 透传:`SlaveTask` 加 `#[serde(default)] system_prompt: Option<String>`;`build_slave_session` 用它或默认;`OrchestratorState` 持 `slave_prompt`,`dispatch` 用之;`Master::run_goal` 加 `slave_skill_body: Option<&str>`。
- **决策 D2 落地**:skill 只决定角色/文风,锁与 report 机制由固定段保证,skill 无法绕过。

## 4. webui 后端(P4,`src/webui/mod.rs`)

新增端点(均有 handler 测试):
- `GET /api/themes/{theme}/articles` —— 改返回 outline(file/title/parent/depth)。
- `GET` / `PUT /api/themes/{theme}/config`。
- `POST /api/themes/{theme}/articles/{file}/parent`(人工设父子,body `{parent}`)。
- `POST /api/themes/{theme}/reorder`(人工设顺序,body `{order}`)。
- `GET /api/skills`。
- `POST /api/themes/{theme}/chat` —— **阻塞**跑 `Master::run_goal`(spawn_blocking),先确保 theme 存在并把目标限定在该 theme,带所选 skill + slave_model,事件走现有 SSE,返回 `{outcome,message,reports}`。
- `AppState` 加 `skills_dir`(默认 `./skills`,`AI_WRITE_SKILLS` 覆盖)+ `with_skills_dir`。保留 `POST /api/tasks`(单篇 run_one)兼容。
- 冒烟(seed 一个 theme 直接 curl):`/`、themes、skills(中文正确)、articles outline、config、set parent、reorder 全部按预期。

## 5. 前端(P5,`src/webui/index.html`,纯 vanilla,UI 文案英文)

- 左栏:theme 树,文章按 outline 的 `depth` 缩进成层级。点 theme name → theme 模式;点文章 → article 模式。
- theme 模式:master 对话(goal + skill 选择 + model 选择 + 对话 log)、theme config 表单(描述/默认 skill/默认 model + Save)、AI operations 时间线。**不显示正文**。
- article 模式:三栏 **3 : 0.5 : 0.5** —— 正文(顶部工具栏:刷新/撤销)、协作命令流(SSE 事件镜像)、版本(history/diff/undo)。
- 对话每次提交 = 一次 `POST .../chat`(决策 D1),live 进度走 SSE。

---

## 6. 决策回顾(对照 impl-v3 §6)

D1~D6 全部按设计落地,无偏离。补充实现细节:
- master 对话**按 theme 限定**:`run_master_chat` 先 `create_theme`(幂等)并在目标里要求 master 只在该 theme 内创建/派发。
- article 模式的「协作命令流」v1 是**全量事件镜像**(不按文章精确过滤)—— 单文章派发时就是「AI 正在编辑本文」;多文章并发时会混入其它文章的事件(已知简化,见 §7)。

## 7. 已知缺口 / 留待后续

- **字级作者染色未做**(纯文本→`content::Document` 迁移仍是独立集成层)—— 中/右栏目前是事件流 + git diff,不是 `dsl::render_html` 的 `data-author`。
- master **无状态多轮**:每次提交独立 `run_goal`,跨轮无记忆。
- article 协作流**未按文章精确过滤**(全量镜像)。
- article 视图**不能直接发起 AI 编辑**(编辑统一从 master 对话发起);工具栏只放查看/版本类。
- master 对话阻塞返回(run_goal 同步跑完才回 HTTP),长目标会让该请求长时间挂起 —— dev 可接受,live 进度有 SSE。
- slave token 用量未折回 master(沿用 v2 缺口)。

## 8. 新增文件
- `src/skill/mod.rs` —— skill 加载/解析。
- `skills/functional-writing.md` —— 预置「功能型写作」提示词(可 commit,待你 refine)。
- `docs/impl-v3.md` / `docs/impl-v3-results.md`。
