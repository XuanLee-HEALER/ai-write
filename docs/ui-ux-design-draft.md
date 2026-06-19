# AI-Write · UI/UX 设计稿(草稿 v0.1)

> 受众:设计师。性质:**讨论用草稿**,覆盖主要页面与交互方式,不锁视觉细节。
> 基线:当前 webui(深色、三栏布局)。目标:**全平台**——桌面 / 笔记本 / 平板 / 手机 / 原生壳,响应式从设计之初就是约束而非补丁。
> 配套:产品的底层机制见 `docs/ai-write-kernel.html`(本稿的交互模型必须忠于其中的内核约束)。

---

## 1. 设计前提:内核约束如何落到界面

AI-Write 是**工具**而非内容产品,几条内核公理直接决定交互边界,设计需内化:

- **恒成品(every committed state is a deliverable)**:界面里**不存在「草稿 / 成品」状态切换**。文章在任意时刻都是可读可交付的;唯一的状态轴是**历史(version timeline)**,不是「完成度」。设计上禁止出现进度条式的「完成百分比」「待定稿」标记。
- **人机同等地位(symmetric agency)**:人与 AI 是对称的协作者。二者的贡献用**同一种视觉语言**表示(作者色 + 身份标签),不给 AI 任何「特殊态」(没有「AI 草稿待人确认」这类不对称框架)。
- **diff 即信道(diff as the channel)**:协作过程的可见性来自**变更流(commit / diff timeline)**,而非「观看 AI 落笔」。版本、归属、历史是**一等公民界面**,常驻可达。
- **署名是证据(authorship as evidence)**:署名由 diff/blame 的作者归属支撑,**诚实呈现**。设计上**不得**伪造「已验证人类作者」徽章(内核 §11 明确这条边界不可由系统弥合)。
- **coordinator 队首优先、不抢占**:人请求编辑时若 AI 正持有事务,人**插队首但不打断当前提交**。这要求一个专门的交互态:「**轮到你了(下一个提交后生效)**」,而非「立即抢占」。

> 这些不是装饰性原则,而是会直接产出/否决具体控件的硬约束。任何控件若与上述任一条冲突,应被否决。

---

## 2. 信息架构(IA)

三层对象 + 两类工作面:

```
Workspace(工作区)
└── Topic 主题(= 目录)
    └── Article 文章(= 文件;有父子层级 + 阅读顺序,mdBook SUMMARY 心智)
```

- **两类工作面**:
  - **Orchestration / 编排面**(选中 Topic):与 master 对话下达主题级目标 + 主题配置 + 实时操作流。
  - **Authoring / 单篇面**(选中 Article):读写正文 + 协作活动流 + 版本/署名。
- **导航语义**:Topic↔Article 是 **master–detail**;Article 的父子层级在导航区以缩进 + disclosure 表达,**阅读顺序是显式可拖拽的**(manifest 为唯一事实源,**绝不用文件名前缀编码顺序**)。

---

## 3. 三区画布模型(全平台布局的基元)

整个产品围绕**三个语义区**组织;所有 form factor 都是这三区的**坍缩/重组**,而非各端各画一套:

| 区 | 角色 | 当前 webui 对应 |
|---|---|---|
| **Navigator 导航区** | Workspace 树:Topic / Article 层级、检索、新建、重排/改父 | 左侧 Workspace 卡 |
| **Work Surface 工作面** | 主操作区:编排对话 *或* 正文画布(含工具栏) | 中间区 |
| **Inspector 检视区** | 上下文检视:版本 / diff / 署名 / 活动流 | 右侧 Versions(+ 中栏 Collaboration) |

> 关键决策:Article 单篇面在桌面是 **正文 : 活动 : 版本 = 3 : 0.5 : 0.5** 的三栏(沿用现状),但「活动流」和「版本」在语义上同属 **Inspector**,只是桌面把它拆成两条窄栏。这个归类让小屏坍缩有统一规则(见 §4)。

---

## 4. 响应式与跨端策略(全尺寸,核心)

### 4.1 断点 token(建议初值,待 design token 化)

| token | 区间(px) | 典型 form factor |
|---|---|---|
| `xs` | < 480 | 手机竖屏 |
| `sm` | 480–768 | 手机横屏 / 小屏 |
| `md` | 768–1024 | 平板竖屏 |
| `lg` | 1024–1280 | 平板横屏 / 小笔记本 |
| `xl` | 1280–1680 | 桌面 |
| `2xl` | ≥ 1680 | 大屏桌面 |

- **流式栅格**:12 列 fluid grid;正文画布有**最佳阅读宽度(measure ≈ 66–75ch)**上限,超宽时正文居中、两侧留白,**绝不把正文拉满**。
- **组件级自适应用 container query**(不只靠全局断点):工具栏在窄容器里溢出折叠为「更多」菜单,卡片在窄容器里由横排转纵排。

### 4.2 三区坍缩阶梯(同一套区,不同坍缩)

| 断点 | Navigator | Work Surface | Inspector |
|---|---|---|---|
| `2xl` / `xl` | 常驻(可手动折叠) | 居中,三栏(单篇面) | 常驻,版本+活动两窄栏 |
| `lg` | 常驻但收窄 | 主区 | 合并为**单条可折叠 Inspector**(活动/版本用内部 tab) |
| `md`(平板竖) | **overlay drawer**(汉堡唤出) | 全宽 | **可切换的右侧 overlay / 底部 sheet** |
| `sm` / `xs`(手机) | **全屏 drawer** | 单区全屏 | 并入工作面:用 **segmented control** 切「正文 / 活动 / 版本」 |

- **手机端单篇面**:顶部 segmented control `[正文 · 活动 · 版本]`,横扫(swipe)切段;`3:0.5:0.5` 的桌面比例在此退化为**分段视图**,不做三栏挤压。
- **底部 tab bar(手机主导航)**:`工作区 / 编排 / 当前文章`,符合拇指热区;次级动作进 overflow 或上下文菜单。

### 4.3 输入模态与触控

- **多模态**:pointer + keyboard(桌面)、touch(平板/手机)。同一动作要给**等价的非拖拽路径**(如重排:桌面 drag-and-drop;触控给显式**拖拽手柄** + 「上移/下移/改父级」上下文菜单 fallback)。
- **触控规范**:hit target ≥ 44×44pt;手势:横扫切 segment、长按唤上下文菜单、下拉刷新(谨慎,避免与滚动冲突)。
- **键盘优先**:桌面提供 **command palette(⌘K / Ctrl-K)**与可达的快捷键(切文章、发起编排、打开 diff、撤销)。

### 4.4 原生壳与大屏

- **safe-area insets**:`env(safe-area-inset-*)` 适配刘海/灵动岛/底部 home indicator;原生壳遵循**平台导航约定**(iOS 边缘返回手势、Android 返回键)。
- **窗口可变**:桌面窗口缩放、iPad split view、Stage Manager 一律走 container query,不假定固定宽。
- **密度模式**:`comfortable / compact` 两档(桌面默认 comfortable,信息密集场景可切 compact)。
- **主题**:深色(现状)为默认,light 同时设计;全部走 **design token**(颜色/间距/圆角/排版/阴影/层级),禁止硬编码色值。

---

## 5. 全局框架(App Shell)

- **顶栏(App Bar)**:品牌标识 + 全局连接/实时状态(`live` 指示灯,源自 SSE)+ 全局动作(命令面板入口、主题切换、工作区切换)。窄屏退化为单行:汉堡(唤 Navigator)+ 标题 + overflow。
- **实时状态指示器(connection/presence)**:`live / reconnecting / offline` 三态,语义色 + 文案;它是协作可信度的锚点,常驻可见。
- **命令面板(command palette)**:键盘优先的全局动作入口(跳转文章、发起编排、打开 diff、撤销、切 skill)。
- **全局 toast / 通知**:瞬时反馈(保存成功、撤销、错误);与「活动流」区分——toast 是瞬时确认,活动流是**可回溯的过程记录**。

---

## 6. 主要页面 / 视图

### 6.1 Workspace Navigator(导航区)

- **Topic 列表**:可展开;Topic 行支持新建文章、重命名、主题设置入口。
- **Article 层级树**:按 `depth` 缩进 + disclosure 三角;**阅读顺序**由排序决定且**可拖拽重排 / 拖拽改父级**(写回 manifest)。当前 webui 已有缩进层级,本稿把它升级为**可编辑结构**。
- **行内元数据(可选、密度自适应)**:作者归属指示(人/AI 主导色点)、是否有未读 AI 活动的 badge。
- **检索/过滤**:按标题/正文(对应后端 `find`)。
- **空态**:无 Topic 时的引导(新建主题 / 与 master 对话生成结构)。

### 6.2 Topic View · Orchestration(编排面)

选中 Topic 进入。**不显示正文**(内核:Topic 面是规划层)。三块:

1. **Master Dialogue(编排对话)**:目标输入 + skill 选择 + writer model 选择 + 对话 log。语义不是普通聊天,而是「**下达主题级目标 → master 规划/派发 → 汇总回报**」;每次提交产出一段**可追溯的编排记录**(plan / dispatch / reports)。
   - 设计点:对话气泡里嵌**结构化产物**(创建了哪些文章、层级、各 writer 的 report 状态徽章),而非纯文本。
2. **Theme Config(主题配置)**:描述/目标、默认 skill、默认 writer model。inspector 式表单,保存即生效。
3. **AI Operations Timeline(操作流)**:SSE 实时事件(round / tool call / commit / slave 生命周期),是编排过程的**可见性载体**。

### 6.3 Article View · Authoring(单篇面,三区工作面)

选中 Article 进入。桌面三栏(`正文 : 活动 : 版本 ≈ 3 : 0.5 : 0.5`),小屏按 §4.2 坍缩为 segmented。

- **A. 正文画布(Editor/Reader Canvas)+ 工具栏(toolbar)**
  - 这是产品化的关键升级:从「只读 + 刷新」升级为**可编辑的富文本/Markdown 画布**,人可直接改正文(人的编辑 = 人的 commit)。
  - **作者着色(authorship coloring)**:正文按**作者游程(run)**渲染 `data-author` 着色(provenance 提供),可切换显隐;图例(legend)标注人/各 model id 的色。这是「同等地位 + diff 即信道」在正文层的落点。
  - **工具栏**:结构化格式动作、撤销/重做、发起「请 AI 编辑本段」、打开 diff、版本跳转。窄容器溢出折叠。
  - **恒成品**:画布不显示「未保存草稿」态——编辑落定即一个 commit(经 coordinator 单事务),画布始终呈现成品。
- **B. 协作活动流(Collaboration / Command Stream)**
  - **本文范围**的实时操作流:「AI 正在编辑…」「提交了一处改动」等(SSE,按本文过滤——现状是全量镜像,产品化需精确过滤)。
  - **coordinator 事务态可视化**:谁持有当前编辑事务、人请求是否在队首等待(「**轮到你了 · 下一个提交后生效**」),锁/临界区状态。这是「队首优先、不抢占」的专属界面。
- **C. 版本 / 署名检视区(Versions / Provenance Inspector)**
  - **commit 时间线**:每个版本含作者(人/model id)、message、时间;newest-first。
  - **diff 查看器**:选两个版本对比(inline 或 side-by-side),按作者归属着色。
  - **blame(行级归属)**:逐行作者(人/model id)——「署名是证据」的最细粒度呈现。
  - **撤销/还原(undo/revert)**:文章级、commit 粒度,**永不重写历史**(revert 自身也是一次 commit)。

### 6.4 Diff / Review

- 独立可放大的 diff 面(桌面可抽出为侧栏或模态,移动端为全屏)。
- **作者归属 diff**:增删按作者着色,头部标注「人 vs deepseek-<dated-id>」。
- 评审动作:接受/还原到某版本(都落为新 commit)。

### 6.5 Provenance & Signature(署名/版权)

- **文件签名**:每篇文章携带共有署名(model id + 人工签名);model id **钉死到带日期快照**(复现/归属硬约束,见内核 §9)。
- **贡献构成**:基于 diff/blame 的作者占比(诚实呈现,不美化)。
- **诚实边界提示**:不提供「已验证人类原创」徽章(§11 边界);如需,只能呈现「commit 作者分布」这一事实,不做价值判断。

### 6.6 Skills & Settings

- **Skill 库**:写作人格(角色/文风/拒绝项)的列表/编辑;支持**多 skill 叠加**(优先级/覆盖语义,待定,见内核 §10 与实现 G10)。
- **工作区设置**:模型、密度、主题、快捷键。

---

## 7. 关键交互流(key flows)

1. **编排派发流(orchestration)**:Topic 面下达目标 → master 规划(建主题/文章、组织层级)→ 逐篇派发 writer → 操作流实时滚动 → 对话回汇总(各篇 report 状态)。设计:派发中给**可中断的进度感**(但中断遵循「队首不抢占」语义,见下)。
2. **直接编辑流(human authoring)**:在正文画布直接改 → 落定为人的 commit → 活动流出现该提交 → 版本区+1。无「保存草稿」中间态。
3. **人机协作 · 队首优先(non-preemptive handoff)**:人发起编辑而 AI 正持事务 → 人请求**插队首**并显示「**排队中 · 当前 AI 提交后轮到你**」→ 当前 commit 完成 → 自动切到人,画布进入可编辑态。**不打断、不丢 AI 当前一步**(代价是一个 commit 的等待,revert 廉价)。这是本产品独有、必须专门设计的协作微交互。
4. **流式/实时态(streaming)**:SSE 驱动;乐观 UI;活动流逐条 slide-in;编辑落定后正文/版本区按事件刷新。断线 → `reconnecting` 态 + 自动重连,不清空已渲染内容。
5. **作者归属可视化(provenance toggle)**:正文着色 / blame / diff 三处共用同一套作者色与图例;可一键显隐着色,降噪。
6. **结构编辑(reorder/reparent)**:导航区拖拽改阅读顺序/父级 → 写回 manifest → 树即时重排;触控走手柄 + 菜单 fallback。

---

## 8. 状态与反馈(states)

每个数据视图都需四态 + 实时态:

- **empty**:有引导性 CTA(新建/对话生成),非空白。
- **loading**:骨架屏(skeleton),非整屏 spinner;保持布局稳定避免 layout shift。
- **error**:就地错误 + 可重试,文案具体(对应后端 `{error}`),不吞错。
- **partial / live**:流式增量(活动流、编排对话)用增量插入 + 自动滚到底(可锁定)。
- **busy(事务持有)**:正文画布在 AI 持事务时进入只读 + 「AI 编辑中」标识,人触发编辑即转入「队首排队」态(见 §7.3)。

---

## 9. 组件清单(design system primitives,供搭建)

- **基础**:design token(color/space/radius/type scale/elevation/z-index)、深浅主题、密度档。
- **导航**:tree item(带 disclosure / 缩进 / drag handle)、drawer、bottom tab bar、breadcrumb、command palette。
- **工作面**:editor canvas(富文本/Markdown,带作者着色)、toolbar(可溢出折叠)、segmented control、split pane(可拖拽分隔 + 可折叠)。
- **检视**:commit timeline item、diff viewer(inline/side-by-side)、blame gutter(行级作者)、author chip / legend、signature card。
- **对话**:message bubble(可嵌结构化产物)、report status badge、event/timeline item(类型化:round/tool/commit/slave/finished)。
- **反馈**:toast、skeleton、inline error、connection indicator、queue/transaction status pill(「轮到你了」)。
- **表单**:select、textarea、switch、model/skill picker(支持多选叠加)。

---

## 10. 可访问性(a11y)

- 键盘可达:全部交互有键盘路径;焦点环可见;命令面板兜底。
- 语义:正确 ARIA role(tree / tablist / log[活动流用 `aria-live=polite`] / dialog)。
- 对比度:作者着色不可仅靠色相区分(叠加纹理/标签),满足 WCAG AA;深浅主题均达标。
- 触控:≥44pt;手势均有非手势等价路径。
- 动效:尊重 `prefers-reduced-motion`,流式插入可降级为无动画。

---

## 11. 决策(原待讨论项,已定)

| # | 议题 | 决策 |
|---|---|---|
| 1 | 正文编辑器形态 | **WYSIWYG**;编辑器只有 **view / edit** 两模式切换(无第三态)。 |
| 2 | 作者着色默认态 | **默认关**,手动开启,且**仅在 view 模式可开**(edit 模式不着色)。 |
| 3 | 队首排队微交互强度 | 常规功能,**与工具同等视觉重量**(不抢眼);**允许反悔**(人可取消本次排队/编辑请求)。 |
| 4 | 多 skill 叠加 UI | **简单多选**(不做可拖拽优先级排序器)。 |
| 5 | 移动端编排 | 单独做一个**精简编排视图**(不复用桌面编排面)。 |
| 6 | 原生范围 | 需考虑原生;澄清见下。基线先 web/PWA(含 webview 壳),真原生范围待拍板。 |
| 7 | 视觉基调 | 向写作工具靠拢:**亮色、偏暖、纸感**;弱化当前深色技术感。 |

**关于 #6(Tauri 算不算原生)**——你的理解基本对:
- **Tauri / Electron**:UI 跑在 **webview**(Tauri 用系统 webview,Electron 自带 Chromium)。本质是「原生壳 + web 渲染」的混合体——**设计上就是同一套响应式 web UI,零额外成本**。
- **SwiftUI / Jetpack Compose**:真·平台原生 UI(系统控件渲染)。
- **Flutter**:也**不是** webview——它有自绘引擎(Skia/Impeller),画自己的控件;属「原生编译 + 自绘」,既非 webview 也非平台原生控件。
- **建议**:基线做 **响应式 web + PWA**,它顺带零成本覆盖 Tauri/Electron 这类 webview 壳;**只有真原生(SwiftUI/Compose/Flutter)才需要另一条设计轨**(design token → 原生组件映射 + 平台导航约定),作为**条件交付**(见 §12 · G)。是否纳入真原生由你拍板。

---

## 12. 设计师交付物清单(Designer Deliverables)

> 基于本稿(含 §11 已定决策)需产出的内容。建议优先级:**A → B → C → D**(基础先行),E/F 并行细化,G 视范围,H 贯穿。

### A. 设计基础(Design System / Foundations)
- [ ] **Design token 全集**:color / spacing scale / radius / type scale(含中英文 + 等宽字体用于 diff/code)/ elevation / z-index / motion(duration·easing)。以变量管理,dev-ready(可导出 JSON / Tailwind 映射)。
- [ ] **视觉基调**:**亮色、偏暖、纸感**主题为默认与主交付(§11·7);深色作为可选次级主题(降优先级,可后置)。
- [ ] **作者色板(authorship palette)**:人 + 多 model id 的可区分色;**不可仅靠色相**(叠加纹理/标签),满足 WCAG AA。
- [ ] **图标库**:统一线性图标集。
- [ ] **密度档**:comfortable / compact 两套间距规格。

### B. 响应式与布局规范(Responsive / Layout)
- [ ] **断点 token + 栅格规范**:6 档(xs–2xl)的 12 列 fluid grid、容器宽度、正文 measure 上限。
- [ ] **三区坍缩阶梯逐断点高保真**(§4.2):2xl/xl/lg/md/sm/xs 各关键屏;Navigator 在各档的 persistent / drawer 形态;Inspector 的栏/sheet/segmented 形态。
- [ ] **App Shell 规范**(§5):顶栏 + 实时状态指示(live/reconnecting/offline)+ command palette,各断点形态。

### C. 主要页面高保真(Key Screens)
- [ ] **Workspace Navigator**:层级树 + disclosure + 缩进;**拖拽重排/改父**的视觉态 + **触控拖拽手柄 + 上移/下移/改父菜单 fallback**。
- [ ] **Topic 编排面**:master 对话(含**结构化产物气泡** + report 状态徽章)+ 主题配置表单 + AI 操作流。
- [ ] **移动端精简编排视图**(§11·5,独立设计,非桌面复用)。
- [ ] **Article 单篇工作面**:桌面三栏(3:0.5:0.5)+ 手机 segmented(正文/活动/版本)。
  - [ ] **正文编辑器**:**WYSIWYG**,**view / edit 两模式切换**(§11·1)两套态。
  - [ ] **作者着色**:**默认关、仅 view 模式可开**(§11·2)的开/关两态 + 图例。
  - [ ] **协作活动流** + **coordinator 事务态 / 队首排队 pill**:常规视觉重量、**可反悔/取消**(§11·3)。
  - [ ] **版本检视**:commit timeline / diff viewer(inline + side-by-side)/ blame gutter(行级作者)/ signature card。
- [ ] **Diff / Review 放大态**:桌面侧栏或模态、移动端全屏;增删按作者着色。
- [ ] **Provenance / 署名面**:贡献构成(基于 diff/blame)+ **诚实边界**(无「已验证人类原创」徽章,§6.5/内核 §11)。
- [ ] **Skills 库 + 设置**:skill 列表/编辑、**多 skill 简单多选**(§11·4)、工作区设置。

### D. 组件库(Component Library / UI Kit)
- [ ] §9 组件清单逐个的**多态规格**:default / hover / focus / active / disabled / loading / error / selected,含**触控态**与各断点变体(auto-layout)。
- [ ] **统一状态规格**(§8):empty / loading-skeleton / error / live / busy(事务持有)。

### E. 交互与动效(Interaction / Motion)
- [ ] **关键流交互原型**(prototype):编排派发、直接编辑、**队首非抢占切换**、流式插入、拖拽重排、作者着色切换。
- [ ] **动效规范**:slide-in / skeleton / 转场;含 `prefers-reduced-motion` 降级。
- [ ] **手势规范**:swipe 切 segment、长按上下文菜单,均配**非手势等价路径**。

### F. 可访问性(a11y)
- [ ] 对比度报告(主/次主题均达 AA)、键盘焦点流与快捷键地图、ARIA role 标注(tree / tablist / `aria-live` 活动流 / dialog)。

### G. 平台范围相关(条件交付,取决于原生决策 §11·6)
- [ ] **Web / PWA + webview 壳(Tauri/Electron)**:响应式 web 设计**即全覆盖**,无需额外原生设计。
- [ ] **(若纳入真原生:SwiftUI / Compose / Flutter)**:token → 平台原生组件映射 + 平台导航/交互约定规范 + safe-area 适配标注。

### H. 交付格式(Handoff)
- [ ] **Figma**:component library + variants + auto-layout + 各断点响应式 frames;token 以 variables 管理;measure/redline 标注;dev-ready 导出。
- [ ] 交互原型 + spec/redline 文档。

---

> 草稿到此。建议下一步:先就 §4(坍缩阶梯)、§6.3(单篇三区工作面)、§11 决策对齐,再进 §12·A/B/C 的高保真。需要的话我可把本稿排成与 `ai-write-kernel.html` 同款的可读 HTML 版供评审分发。
