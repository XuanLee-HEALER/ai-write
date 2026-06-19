# Coordinator 设计(对齐内核 §6)

> 依据:`docs/ai-write-kernel.html` §6(并发控制)。本文是该节四条机制(6.A 声明锁 / 6.B 无死锁 / 6.C 队首优先 / 6.D 提交进临界区)的可实现化设计,并给出与现状代码的接线/迁移。
> 状态:设计稿,供 review 后拆分实现。

---

## 1. 定位:它是三件事合一

coordinator 不是"又一个锁管理器",它是**操作级事务的唯一权威**,同时承担:

1. **锁管理器**:把锁从「单文件」升到「操作」,锁集**预先声明**、**全有全无**授予。
2. **公平调度器**:人/AI 同走它(公理 Ⅱ);人的请求**插队首但不抢占**。
3. **提交者**:每个事务在临界区**内**做**单次** git commit —— 一个认知单元 = 一个 commit(§5 推论 5.1)。

它**独占** workspace 的锁状态与那个非 `Sync` 的 `Vcs` 句柄,因此所有写与提交都从这一个门进出。"无人能绕过 coordinator 直接抢文件锁"(机制 6.B 的铁律)在类型层面落实为:`Workspace` 的加锁方法与 `Vcs` 的提交方法**只对 coordinator 可见**,不再暴露给工具层。

## 2. 与公理/不变量的对应

| 内核条目 | coordinator 落点 |
|---|---|
| 不变量·恒成品 | 队列里下一个拿到的永远是已 commit 的成品态;commit 在临界区内,放锁前文件已自洽 |
| 公理 Ⅱ·同等地位 | `Priority::{Human, Agent}`,人插队首、不抢占 |
| §5·认知单元=一次 commit | 一个事务 = 一次 commit(含正文 + manifest,合并为单 commit) |
| 6.A 声明锁 | `TxnRequest.locks` 必填,操作影响集开始前已知 |
| 6.B 无死锁 | 全有全无获取(不持部分等待)→ 无 hold-and-wait;锁集按路径全序 |
| 6.C 队首优先 | 人 ticket 入队首,等当前持锁者自然 commit 完 |
| 6.D 提交进临界区 | commit 是 submit 内部步骤,在放锁前执行 |

---

## 3. 核心类型

```rust
/// 调度优先级。人插等待队列队首(但不抢占运行中的事务);AI 走 FIFO。
pub enum Priority { Human, Agent }

/// 一个事务声明要触碰的、workspace 相对路径集合。
/// 用有序集合存储 —— 排序后即「按路径的全序」,是 6.B 防死锁的规范获取顺序。
pub struct LockSet(BTreeSet<PathBuf>);

/// 事务请求:谁、什么优先级、声明影响集、给 commit message/可观测用的标签。
pub struct TxnRequest {
    pub writer: WriterId,
    pub priority: Priority,
    pub locks: LockSet,   // 机制 6.A:开始前一次性声明全部
    pub label: String,
}

/// 临界区内交给事务体的上下文:暴露受沙箱约束的文件读写,
/// 但**不**暴露加锁/提交控制(那是 coordinator 的职责)。
pub struct TxnCtx<'a> {
    ws: &'a mut Workspace,
    writer: &'a WriterId,
    declared: &'a LockSet,            // 事务体写入的路径必须 ⊆ declared
    touched: &'a mut BTreeSet<PathBuf>,
}

/// 事务结果:本次单 commit 的短 SHA 与实际改动路径。
pub struct TxnOutcome { pub sha: Option<String>, pub paths: Vec<PathBuf> }

pub enum CoordError {
    Undeclared(PathBuf),  // 事务体写了未声明的路径(违反 6.A)
    Workspace(ToolError),
    Vcs(VcsError),
    Aborted(String),
}
```

## 4. 唯一入口与生命周期

```rust
impl Coordinator {
    /// 提交一个事务。body 在临界区内运行,写文件并返回 commit message。
    pub fn submit<F>(&self, req: TxnRequest, body: F) -> Result<TxnOutcome, CoordError>
    where F: FnOnce(&mut TxnCtx) -> Result<String, CoordError>;
}
```

`submit` 七步(每步对应一条机制,缺一不可):

1. **入队**:human ticket 插到等待队列**队首**(排在所有在等的 agent 之前),agent ticket 入队尾(6.C)。
2. **等待**:直到本 ticket 的声明锁集**全部空闲**且轮到它(按队列顺序就该资源而言)。获取是**全有全无**——拿不全就一个不占、退避重试,**绝不持部分锁等待**(6.B,杜绝 hold-and-wait)。
3. **获取**:把声明锁集整体标记为本 writer 持有。
4. **进临界区**:用 workspace + 独占的 `Vcs` 组装 `TxnCtx`。
5. **运行 body**:写受影响文件、返回 commit message;校验 `touched ⊆ declared`,越界即 `Undeclared` 中止。
6. **单次提交**(6.D):把本事务实际改动的全部路径(正文 + manifest)合并为**一个** commit,在放锁**前**执行。需要 `Vcs::commit_paths`(见 §6)。
7. **放锁 / 出队 / 唤醒**等待者。

返回 `TxnOutcome`。

## 5. 并发模型(slave 是真并发线程,必须正确)

现状:多个 slave 各开**自己的** `Vcs` 句柄、共用同一仓库 —— 真并发提交会竞争 index(目前靠"派发串行 + 单文件内存锁"侥幸不炸)。coordinator 收口:

- coordinator **独占**单个 `Vcs`(`Vcs` 非 `Sync`),所有 commit 串行经过它。
- **不相交锁集**的事务体可并发执行(两个 agent 改不同文章);**提交步骤**经 `Vcs` 自己的 mutex 串行。
- **相交锁集**的事务由声明锁互斥,天然串行。
- 死锁不可能:全有全无获取 ⇒ 无 hold-and-wait ⇒ 无环等待。

实现基座:`Coordinator { state: Mutex<CoordState>, cv: Condvar, vcs: Mutex<Vcs>, ws_root }`,`CoordState { held: BTreeMap<PathBuf,WriterId>, queue: VecDeque<Ticket> }`。多个 slave 共享 `Arc<Coordinator>`。

> v1 可先把整段临界区串行(一次一个事务)作为最简正确基线,声明锁/队列仍按上面建模以保前向兼容;不相交并发作为后续优化。两条路线都满足内核四机制。

## 6. 与现状的接线 / 迁移(关键)

| 现状 | 改为 |
|---|---|
| 锁活在 `Workspace`(per 单文件内存 BTreeMap),模型用 `acquire_lock`/`release_lock` 两个工具显式管 | 锁状态移入 coordinator(或 coordinator 成为 `Workspace` 锁方法的**唯一**调用者);**移除** `acquire_lock`/`release_lock` 工具——加锁随事务隐式发生 |
| 每个编辑工具 `write_article`/`edit_article`/`apply_edits` 自己 acquire→write→commit | 每个编辑 = 一个**单文件事务**提交给 coordinator;工具体只写文件,锁与 commit 由 coordinator 包办 |
| `ToolCtx::commit_article` 做**两次** commit(正文 + index) | 一个事务**单次** commit 覆盖正文 + index —— 一个认知单元一个 commit |
| 无多文件原子操作 | 新增 `split_article` / `merge_articles`:声明锁 = {源文件, 新建文件, index.json},一次写完、单 commit |
| manifest(index.json)无锁,被多处直接 `save_index` | manifest 整文件锁:任何结构操作的声明锁集**必含** index.json(机制 6.2,不上行级锁) |
| slave **整篇会话**持锁(开头 acquire、结尾 release),人要改得等整篇写完 | 每个**编辑**是独立事务(acquire→write→commit→release),人最多等**一个 commit**(6.C 的"等当前一步走完") |
| 多 slave 各开自己的 `Vcs`,无跨线程提交串行 | coordinator 独占单 `Vcs`,所有提交串行经过它 |

slave 系统提示词随之简化:**不再**教它 acquire/release;它只调编辑工具,每次调用即一个原子成品提交。

## 7. 新增 / 改动的 API 清单(供拆任务)

- 新增 `src/coordinator/`:`Coordinator`、`TxnRequest`、`LockSet`、`Priority`、`TxnCtx`、`TxnOutcome`、`CoordError`、`submit`。
- 新增 `Vcs::commit_paths(paths: &[&Path], author, message) -> Result<String, VcsError>`:暂存多路径、单 commit(现有 `commit_file` 成为 `commit_paths` 单元素特例或保留)。
- `ToolCtx`:去掉 `commit_article` 的双提交,改由 coordinator 收口;工具不再直接碰 `Vcs`。
- `Workspace`:`acquire_lock`/`release_lock`/`ensure_lock_held` 收敛为 coordinator 内部使用(或标记 `pub(crate)`)。
- `engine`:slave/master 派发改为持 `Arc<Coordinator>`;编辑工具走事务;移除显式锁工具及其在 prompt 里的说明。
- 新增 `split_article` / `merge_articles` 编辑工具(跨文件逻辑操作,coordinator 事务)。

## 8. 测试策略(对齐 req/tool 模块门禁)

- 单元:声明锁全有全无;两个不相交事务可并发;两个相交事务串行;human ticket 插队首但不抢占(用 barrier/channel 编排两线程断言顺序);单 commit 覆盖正文 + index(history 长度 +1 而非 +2)。
- 跨文件:`split_article` 后源/新文件/manifest 三者在**同一个** commit(`git show --stat` 单提交含三路径)。
- 死锁:压力测试 N 线程随机相交锁集,断言无挂死、全部完成。
- 回归:现有 vcs/tool 测试在新提交粒度下更新(双提交→单提交)。

## 9. 开放问题
- v1 串行 vs 不相交并发:先串行落地,再开并发(见 §5 注)。
- 事务体 disk 写已发生时若 commit 失败,如何回滚(git 层失败 → 文件已改)。建议:commit 失败则 `Vcs` 层 checkout 回 HEAD 对应路径,再上抛 `Aborted`。
- split/merge 的影响集是否真"开始前可知":拆分目标文件数由内容决定 —— 需约定"由调用方/模型先声明产出文件清单"才满足 6.A;否则该操作不符合声明锁前提,需单列讨论。

