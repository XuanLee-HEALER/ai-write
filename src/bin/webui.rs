//! `webui` — the presentation WebUI server (`docs/impl-v1.md` §3).
//!
//! Serves an `axum` HTTP + Server-Sent Events API over a workspace so the AI
//! writing process can be visualized in a browser: list themes / articles, start
//! a writing run, watch the operation stream live, and inspect version history /
//! diffs / undo.
//!
//! Usage:
//!
//! ```text
//! just webui            # or: cargo run --bin webui --features webui
//! ```
//!
//! It reads `DEEPSEEK_API_KEY` from the environment (load `.env` first, as the
//! `just webui` recipe does) only so a started writing task can drive the engine;
//! no live API call is made until a task is started. The workspace root defaults
//! to `./workspace` and the bind address to `127.0.0.1:8080`, both overridable by
//! the `AI_WRITE_WORKSPACE` and `AI_WRITE_BIND` environment variables.
//!
//! This binary requires the `webui` feature; without it, `main` prints a hint and
//! exits non-zero.

#[cfg(feature = "webui")]
#[tokio::main]
async fn main() {
    use std::process::exit;

    use ai_write::webui::{AppState, app};

    // Workspace root and bind address, with sensible defaults.
    let workspace_root =
        std::env::var("AI_WRITE_WORKSPACE").unwrap_or_else(|_| "workspace".to_string());
    let bind = std::env::var("AI_WRITE_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    // The DeepSeek client is read from the environment once, up front, so a
    // misconfigured key fails fast (before any browser connects) rather than on
    // the first started task. The key itself is never echoed back over the wire.
    let state = match AppState::from_env(&workspace_root) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("failed to build DeepSeek client from environment: {e}");
            eprintln!("set DEEPSEEK_API_KEY (the `just webui` recipe sources ./.env for you)");
            exit(1);
        }
    };

    let app = app(state);

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("failed to bind {bind}: {e}");
            exit(1);
        }
    };

    match listener.local_addr() {
        Ok(addr) => {
            eprintln!("ai-write webui listening on http://{addr} (workspace: {workspace_root})")
        }
        Err(_) => eprintln!("ai-write webui listening on {bind} (workspace: {workspace_root})"),
    }

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("server error: {e}");
        exit(1);
    }
}

#[cfg(not(feature = "webui"))]
fn main() {
    eprintln!("the `webui` binary requires the `webui` feature");
    eprintln!("build with: cargo run --bin webui --features webui");
    std::process::exit(1);
}
