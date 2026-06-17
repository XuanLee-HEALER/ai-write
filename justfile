# ai-write 任务运行器(just）

# 默认:类型检查全 feature
default: check

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
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

# 真机集成测试(读 .env 的 DEEPSEEK_API_KEY,跑 #[ignore] 的 live_* 用例)
test-live:
    set -a; . ./.env; set +a; cargo test --all-features -- --ignored --nocapture

# 真机 demo(读 .env 的 DEEPSEEK_API_KEY):Master 派一个 Slave 写一篇文章到 workspace/<主题>/
# 用法:just demo "<主题>" "<写作任务>"
demo theme task:
    set -a; . ./.env; set +a; cargo run --bin demo -- "{{theme}}" "{{task}}"

# 展示用 WebUI(读 .env 的 DEEPSEEK_API_KEY):axum + SSE 服务,浏览器看 AI 写作过程
# 默认监听 127.0.0.1:8080,工作区 ./workspace(可用 AI_WRITE_BIND / AI_WRITE_WORKSPACE 覆盖)
webui:
    set -a; . ./.env; set +a; cargo run --bin webui --features webui
