# 多 skill 语义(G10,内核 §10 开放问题)

> 依据:`docs/ai-write-kernel.html` §10 末尾的开放问题——"多个 skill 同时激活时,是简单拼接还是有优先级与覆盖?中途改 skill 影响当前步还是仅后续步?"
> 本文给出定稿语义并对接实现。状态:已实现(skill / engine / webui)。

## 1. 决定:有序栈 + 后者覆盖前者

多 skill **不是**无序拼接,而是一个**有序栈(ordered stack)**:

- skill 按选择顺序排列,**栈底在前、栈顶在后**(数组下标 0 = 最先,末尾 = 最后)。
- **冲突解析规则:指令冲突时,栈中靠后的 skill 覆盖靠前的。**

### 为什么是"后者覆盖前者"而不是别的规则

1. **唯一且可读的心智模型**:栈读作"先一个基底人格,后面每一层在其上细化/覆盖"。这与工程界既有的层叠语义同构——CSS 规则、mixin 顺序、分层 `git config` 都是 last-wins。选用户已经内化的规则,无需额外学习。
2. **自由文本无法机械 diff**:skill body 是自然语言人格描述,系统层**无法**可靠地逐条比对两个 skill 哪里冲突、谁更具体。既然机器判不了,就把规则**显式写进 system prompt**,交给模型按确定方向解析——栈顶优先。
3. **单 skill 是退化特例**:一个元素的栈 = 那个 skill 的 body 本身(无标题、无指令前缀,字节级等价于改造前的单 skill 行为)。多 skill 路径因此是单 skill 的严格超集,零回归风险。

### 拼接结构

`compose_stack(bodies)`(`src/skill/mod.rs`)产出 voice 块:

- 0 个非空 body → 空串(上层回退到引擎默认 writer prompt)。
- 1 个非空 body → 就是该 body,别无装饰。
- ≥2 个 body → 前缀一条**优先级指令**(明文说明"栈中靠后的 skill 在冲突时覆盖靠前的"),随后每个 body 落在自己的 `## Skill N` 标题下,**按顺序**铺开。

voice 块之后,engine 再追加固定的 `SLAVE_OPERATIONAL_RULES`(锁纪律 + `report` 义务)。**操作规则永远压过 skill 栈**——这条不变量沿用 G8/G9,栈语义不触碰它。

## 2. 中途改 skill:只影响其后步骤(对齐 §4 / G8)

skill 栈是**磁盘上的状态**,不钉进 context。slave 的 system prompt 由
`compose_slave_prompt_from_skill` 在**每一轮**从磁盘重读整个栈(`load_skills_ordered`)后重新拼接:

- 中途编辑栈里**任何**一个 skill 文件,或(经上层)增删栈成员,都只影响其后从磁盘重读的轮次。
- 任一 id 读取失败 → 整轮回退到静态 fallback prompt,**绝不**产出半截/错乱的 prompt(失败原子化)。

这与 §4(事实源=磁盘、每步重读)和 G8 完全一致;多 skill 只是把"读一个文件"扩成"按序读一组文件再 last-wins 拼接"。

## 3. 实现接线

| 层 | 改动 |
|---|---|
| `skill` | 新增 `load_skills_ordered(dir, ids)`(保序加载、首个坏 id 即 fail-fast)、`compose_stack(bodies)`(有序栈拼接 + 优先级指令)。 |
| `engine` | `SlaveSkill.id: String` → `ids: Vec<String>`(有序栈);新增 `compose_slave_prompt_multi(bodies)`;`compose_slave_prompt_from_skill` 改为读整栈;新增 `Master::run_goal_with_skills`(多 body),`run_goal_with_skill` 退化为其单元素特例。 |
| `webui` | `ChatRequest` 新增 `skill_ids: Vec<String>`(多选,优先于单值 `skill_id`,后者保留向后兼容);chat 走 `run_goal_with_skills` 并把同一组 ids 作为磁盘源安装(每轮重读)。前端 chat 的 skill 选择器改成 `<select multiple>`,文档序即栈序。 |

## 4. 测试

- `compose_stack`:空/单/多;多栈保序 + 含优先级指令 + 各自标题。
- `load_skills_ordered`:请求序被保留(不重排)、首个坏 id fail-fast、空选择=空栈。
- engine:两 skill 栈在 system prompt 里**按序**出现(base 在 refine 前)、含 last-wins 指令、操作规则仍追加;中途改**栈顶** skill 文件,次轮 prompt 反映新内容(磁盘重读)。
- webui:多 skill 栈里任一坏 id → 404(即便前面的 id 合法)。

## 5. 未覆盖 / 留给后续

- **去重**:`load_skills_ordered` 不对 ids 去重(重复 id 按给定顺序加载)。若将来要"同一 skill 不重复生效",在调用方按需去重即可,语义层不强制。
- **per-skill 权重 / 显式 override 标记**:当前只有"位置即优先级"。若某天需要"这条指令是硬覆盖"之类细粒度控制,可在 skill front matter 加字段——但那超出 §10 开放问题的范围,暂不做。
