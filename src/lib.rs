//! ai-write —— DeepSeek V4 辅助写作工具。
//!
//! 当前仅含最底层 `req` module:无状态 DeepSeek API wrapper。
//! 详见 `docs/req-module-design.md`。

pub mod req;

// The shared rich-text content model with character-level authorship. Pure data
// (no IO), always available; the DSL and provenance layers are built on it.
pub mod content;

// The v0 collaborative-writing layers are synchronous (sync + `std::thread`),
// built on the blocking `req` client, so they are gated on the `blocking`
// feature.
#[cfg(feature = "blocking")]
pub mod engine;
#[cfg(feature = "blocking")]
pub mod session;
#[cfg(feature = "blocking")]
pub mod tool;

// Version control for the workspace: each successful edit is committed to a git
// repository via libgit2 (`git2`), giving the workspace history, diff, and undo.
// Synchronous, built on the `blocking` layer alongside the workspace.
#[cfg(feature = "blocking")]
pub mod vcs;

// Observability: a push-based event stream (`Event` + `EventSink`) that lets the
// session and engine narrate the AI writing process to a UI. Default sink is a
// no-op, so it is transparent to code that does not opt in.
#[cfg(feature = "blocking")]
pub mod observe;

// The presentation WebUI backend: an `axum` + SSE server that visualizes the AI
// writing process. Strictly feature-gated (`webui`) and built on the synchronous
// `blocking` layer, bridged to async via `spawn_blocking` + a broadcast channel.
#[cfg(feature = "webui")]
pub mod webui;
