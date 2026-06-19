# ai-write 任务运行器(just）

# Windows 走 PowerShell 7;macOS / Linux 仍用默认 shell(sh)。
set windows-shell := ['pwsh.exe', '-NoLogo', '-Command']
# 自动加载根目录 .env(替代各 recipe 里的 `set -a; . ./.env; set +a`),
# 文件不存在时静默跳过;DEEPSEEK_API_KEY 等密钥即由它注入。
set dotenv-load := true

# 默认:列出所有 recipe
default:
    @just --list

# 交付前全套校验:格式检查 + lint + 全 feature check
ci: fmt-check lint check

# 构建(默认 feature = blocking/sync)
build:
    cargo build

# 仅 async feature(sync 关闭)
build-async:
    cargo build --no-default-features --features async

# 全 feature
build-all:
    cargo build --all-features

# 类型检查(全 feature)
check:
    cargo check --all-features

# 测试(全 feature)
test:
    cargo test --all-features

# 格式化
fmt:
    cargo fmt

# 格式校验(不改文件,CI 用)
fmt-check:
    cargo fmt --check

# clippy(全 feature,警告即错)
lint:
    cargo clippy --all-features -- -D warnings

# 生成文档(intra-doc 链接断裂即报错,验证 rustdoc 干净)
[unix]
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

[windows]
doc:
    $env:RUSTDOCFLAGS = "-D warnings"; cargo doc --no-deps --all-features

# 真机集成测试(读 .env 的 DEEPSEEK_API_KEY,跑 #[ignore] 的 live_* 用例)
test-live:
    cargo test --all-features -- --ignored --nocapture

# 真机 demo(读 .env 的 DEEPSEEK_API_KEY):Master 派一个 Slave 写一篇文章到 workspace/<主题>/
# 用法:just demo "<主题>" "<写作任务>"
demo theme task:
    cargo run --bin demo -- "{{theme}}" "{{task}}"

# 后端域 API(读 .env 的 DEEPSEEK_API_KEY):axum 纯 JSON/SSE API,**不再内嵌 UI**。
# UI 是 web/ 的 SvelteKit 前端(见 web-dev / dev)。默认监听 127.0.0.1:8080,
# 工作区 ./workspace(可用 AI_WRITE_BIND / AI_WRITE_WORKSPACE / AI_WRITE_SKILLS 覆盖)。
webui:
    cargo run --bin webui --features webui

# `api` 是 `webui` 的别名(语义更准:它现在是纯后端 API)。
api: webui

# ---- 前端 web/(SvelteKit + bun + Tailwind + shadcn-svelte) ----

# 安装前端依赖(首次必跑一次)
web-install:
    cd web && bun install

# 前端 dev server(Vite,:5173;/api 含 SSE 代理到 :8080 的 axum)。
# 需先 `just web-install` 一次,且后端在跑(`just webui`)。浏览器开 http://localhost:5173
# (注意:vite 绑 localhost/IPv6,用 localhost 而非 127.0.0.1 访问)
web-dev:
    cd web && bun run dev

# 构建前端(adapter-node 产物供 Hono/Bun 生产服务器挂载)
web-build:
    cd web && bun run build

# 前端类型检查(svelte-check)
web-check:
    cd web && bun run check

# 前端生产服务器(Hono on Bun:挂 adapter-node 产物 + 反代 /api[含 SSE] 到 axum)。
# 先 `just web-build`;AI_WRITE_API 默认 http://127.0.0.1:8080,PORT 默认 3000。
web-serve:
    cd web && bun run start

# 一键全栈本地开发:后端 API(:8080,后台)+ 前端 dev(:5173,前台);Ctrl-C 一起停。
# 首次先 `just web-install`。起来后浏览器开 http://localhost:5173
[unix]
dev:
    #!/usr/bin/env bash
    set -uo pipefail
    echo "▸ 启动后端 API (axum) on http://127.0.0.1:8080 …"
    cargo run --bin webui --features webui &
    api_pid=$!
    trap 'echo; echo "▸ 停止后端 API ($api_pid)"; kill $api_pid 2>/dev/null || true' EXIT INT TERM
    echo "▸ 启动前端 dev (vite) on http://localhost:5173  (Ctrl-C 停止全部)"
    cd web && bun run dev

[windows]
dev:
    $api = Start-Process cargo -ArgumentList 'run','--bin','webui','--features','webui' -NoNewWindow -PassThru; \
        Write-Host "▸ 后端 API (axum) on http://127.0.0.1:8080  |  前端 vite on http://localhost:5173  (Ctrl-C 一起停)"; \
        try { Set-Location web; bun run dev } finally { Write-Host "▸ 停止后端 API ($($api.Id))"; Stop-Process -Id $api.Id -ErrorAction SilentlyContinue }