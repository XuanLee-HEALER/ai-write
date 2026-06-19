# 内核实现结果(G1–G14)—— workflow 交付 + 主控独立 review

> 来源:`docs/kernel-gap-analysis.md` 的 G1–G14,经一条 10 阶段顺序 opus workflow 实现(coordinator spine → prompt/skill 链 → periphery)。
> 设计依据:`docs/ai-write-kernel.html`、`docs/coordinator-design.md`、`docs/multi-skill-design.md`、`docs/sidecar-resources.md`。
> **主控独立校验**(非只信 workflow 自报):`just ci`(fmt/clippy/check)/ `just test`(**287 单测 + 6 集成[2 live ignored] + 64 doctest**)/ `just doc` 三者**全绿**;coordinator 核心逐行读过;webui 端到端真机冒烟(含新 blame 端点)通过。

---

## 1. 各阶段落地一览

| 阶段 | 落地 | 自报门禁 |
|---|---|---|
| **G2** | `Vcs::commit_paths`(多路径单提交);`commit_file` 委托给它 | 绿 |
| **G1** | 新模块 `src/coordinator/`:§6 四机制;独占单 `Vcs`+`Workspace`;编辑工具走单事务(正文+index 合一单 commit);移除 acquire/release 工具;slave 锁粒度降到 per-edit;master/slave 共享 `Arc<Coordinator>` | 绿 |
| **G5** | `Coordinator::split_article`/`merge_articles` + 原生工具:声明锁含源/新建/index.json,单 commit | 绿 |
| **G8** | slave 系统提示词**每轮从磁盘重读**(`SlaveSkill{dir,ids}` + `load_skills_ordered`);中途改 skill 仅影响其后轮 | 绿 |
| **G9** | Session 运行期动态系统提示词段(插入/覆盖) | 绿 |
| **G10** | 多 skill = **有序栈 + last-wins**(`compose_stack`);单 skill 为退化特例;详见 `multi-skill-design.md` | 绿 |
| **G12** | `Model::Pinned(String)` 逃生舱 + `Model::pinned()`;贯穿 WriterId/git author/contributors | 绿 |
| **G11** | 新模块 `src/search/`:`SearchProvider` trait + `StubProvider` + `SearchTool`;`writing_tools_with_search()` seam 可换真 MCP provider;正交于本地 `find` | 绿 |
| **G13** | `Vcs::blame` + `article_blame` 工具 + `GET /api/articles/{theme}/{file}/blame` | 绿 |
| **G14** | sidecar 资源约定(`<name>.assets/`)+ Workspace 沙箱内解析/列举;详见 `sidecar-resources.md` | 绿 |

10/10 自报 green,无 WARNING(无非绿阶段)。

## 2. 主控独立 review 结论

- **门禁**:我这边独立重跑三门禁全绿(测试数 211→287 单测、42→64 doctest,与 10 块新功能一致)。
- **coordinator 核心(读码确认)**:`acquire` 全有全无获取(`!running && is_front && 所有声明路径空闲` 才进,绝不持部分锁)→ 无 hold-and-wait → 无死锁;human 插队首、agent 入队尾、不抢占;`submit` 成功/失败**都释放锁**(不漏锁);临界区内 `touched ⊆ declared` 校验 + 空改动不提交 + 单 `commit_paths` 提交。`state` 锁与 `ws` 锁不嵌套持有,无锁序死锁。v1 用 `running` 标志整段串行(设计 §5 的最简正确基线)。
- **真机冒烟**:webui themes/articles/history/skills/**blame** 全 200;blame 返回逐行作者 + sha,正确。

## 3. 诚实缺口 / 取巧点(需后续收口)

按影响排序:

1. **webui 未走 coordinator(最该跟进)**:webui 的人工编辑/undo 路径仍各开自己的 per-request `Vcs`/`Workspace`,**不经 coordinator**。一旦 master 运行(用 coordinator 的 `Vcs`)与人在 webui 触发写/undo(另一个 `Vcs`)并发,就是 coordinator 本要消除的双句柄竞争。当前单用户、undo 罕见,实际风险低;但架构上是**未完成的迁移**,应把 webui 写路径接进 coordinator。
2. **undo 非完整事务**:`undo_last` 经 `with_vcs` 串行于提交,但**未包进 `Coordinator::submit`** 完整事务(本轮范围限定在 write/edit/apply_edits);锁已隐式,旧「undo 需显式锁」测试改为「无需显式锁」。
3. **v1 临界区整段串行**:`running` 标志使一次只跑一个事务(最简正确基线);声明锁/队列已按未来「不相交锁集并发」建模,但并发本身未开。
4. **G11 搜索是 stub**:`StubProvider` 默认返回「未配置」;真 MCP provider 的接线是**文档化的集成点**(`writing_tools_with_search()` 注入),未真连(MCP 环境相关)。
5. **G2 空 slice 错误**:复用了 `VcsError::NoHistory` 而非新增专用变体(避免 enum 扰动)。
6. **G12**:`Model::as_str` 由 `&'static str` 改为 `&str`(为承载 `Pinned` 的运行期 id);`V4Flash/V4Pro` 现为 family 别名,`Pinned` 才是带日期快照。
7. **读路径双 Workspace 句柄**:session 仍保留各线程自己的 `Workspace` 句柄做只读/结构工具(read/list/create/find),只有**写+提交**走 coordinator 单句柄。文件是 SSOT,只读并发安全,但「create_* 结构操作」未纳入 coordinator 事务(manifest 写竞争在多 master 并发下理论存在,当前单 master 不触发)。

## 4. 新增文件
- `src/coordinator/mod.rs`(G1/G5)、`src/search/mod.rs`(G11)。
- `docs/multi-skill-design.md`(G10)、`docs/sidecar-resources.md`(G14)、本文件。
- `skills/`(预置写作 skill,v3 起)。

## 5. 我的判断
核心交付(coordinator 四机制 + 单认知单元单 commit + per-edit 锁粒度 + split/merge + 多 skill 栈 + blame + model 钉死)**落地扎实、测试充分、读码与冒烟均通过**,可进入 review。**唯一建议优先收口的是 §3.1**(webui 写路径接 coordinator),否则「单一权威」在 UI 这条边上有个缺口。其余缺口都是有意的范围边界或前向兼容取舍,已逐条记录。**未提交**(按约定等你发话)。
