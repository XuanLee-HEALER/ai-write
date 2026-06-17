# ai-write

Rust 实现的 AI 辅助写作工具,模型用 DeepSeek V4(`deepseek-v4-flash` + `deepseek-v4-pro` 组合)。

## 工具链 / 任务运行

- **toolchain:cargo**(stable,当前 rustc/cargo 1.96)。
- **task runner:just**,根目录 `justfile`,常用命令收进 recipe(`just build` / `just test` / `just fmt` / `just lint` / `just check` / `just ci`)。
- **高频任务先写 recipe 再调**:format / lint / check / test / build 这类反复跑的任务,先在 `justfile` 落成 recipe,再用 `just <recipe>` 执行;**不直接敲 `cargo fmt` / `cargo clippy` 等裸命令**。一次性排查(`cargo tree`、`cargo add` 等)可直接用。

## 操作约定(重要)

**能用工具完成的,绝不自己手编辑文件:**

- 加 / 删依赖 → `cargo add` / `cargo remove`,**不手改 `Cargo.toml` 的 `[dependencies]`**。
- 建项目 / crate → `cargo init` / `cargo new`,不手搓骨架。
- 依赖版本 → `cargo add` 自动锁当时最新 stable,不手写版本号(要旧版才显式 `cargo add foo@x.y`)。
- 跑构建 / 测试 / 格式化 / lint → 走 `just` recipe 或 `cargo` 子命令,不手动拼。
- `Cargo.lock`、`target/` 等生成物不手动碰。
- 只有没有对应工具能生成的文件(`justfile`、`CLAUDE.md`、`docs/*.md`、源码逻辑)才手写。

## 代码注释 / 文档规范

- **公共接口(`pub` 项)的文档注释、module doc(`//!`)一律用英文**,写全:用途、参数、返回值、错误(`# Errors`)、`# Examples`(会发网络请求的示例用 ` ```no_run `)、必要时 `# Panics`。目标是 `cargo doc` 直接产出 production-ready 文档。
- 内部实现注释、`docs/` 设计文档、本文件可用中文。

## 文档

- `docs/deepseek-api-research.md` —— DeepSeek API 实测调研。
- `docs/req-module-design.md` —— req module(无状态 wrapper)设计定稿。
- `docs/design.md` —— 原始设计思考 + review 回复。

## 当前阶段

只做 lib 的 `req` module:无状态 `Client` wrapper,sync 基底 + async(feature)。Session/worker、harness 在外层,后续再做。详见 `docs/req-module-design.md`。
