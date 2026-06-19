//! ai-write — a DeepSeek-driven framework for human–AI collaborative writing.
//!
//! The crate is layered so each concern is usable on its own:
//!
//! - [`req`] — a stateless DeepSeek API client wrapper (sync via `ureq`, or
//!   async via `reqwest` under the `async` feature). Always available.
//! - [`content`], [`dsl`], [`provenance`] — the shared rich-text data model
//!   (runs carrying character-level authorship), a line-oriented rich-text DSL
//!   over it, and the single edit primitive that preserves authorship. Pure,
//!   always available.
//! - [`skill`] — writing skills: named system-prompt presets loaded from disk.
//!   Pure, always available.
//! - [`session`], [`tool`], [`coordinator`], [`vcs`], [`observe`], [`search`] —
//!   the synchronous collaborative-writing substrate (turn engine, tool +
//!   workspace model, transaction coordinator, git versioning, event stream,
//!   pluggable search), gated on the `blocking` feature.
//! - [`engine`] — Master/Slave orchestration built on that substrate (`blocking`).
//! - [`webui`] — an `axum` + SSE JSON/SSE API backend (`webui` feature),
//!   consumed by the SvelteKit frontend under `web/`.
//!
//! See `CLAUDE.md` (code-structure section) and `docs/` for design and
//! implementation notes.

pub mod req;

// The shared rich-text content model with character-level authorship. Pure data
// (no IO), always available; the DSL and provenance layers are built on it.
pub mod content;

// v2 layers on the content model (pure, always available). Pre-declared so each
// is implemented in its own worktree without touching this file; see
// `docs/impl-v2.md`.
pub mod dsl;
pub mod provenance;

// Writing skills: named system-prompt presets loaded from `./skills/*.md`. Pure
// (std fs + string parsing), always available; the engine composes a chosen
// skill with its fixed operational preamble, and the WebUI lets a user pick one
// when talking to the master. See `docs/impl-v3.md`.
pub mod skill;

// The v0 collaborative-writing layers are synchronous (sync + `std::thread`),
// built on the blocking `req` client, so they are gated on the `blocking`
// feature.
#[cfg(feature = "blocking")]
pub mod engine;

// The operation-level transaction coordinator (kernel §6): the single authority
// for every mutating workspace operation. It owns the workspace lock state and
// the one non-`Sync` `Vcs` handle, grants declared locks all-or-nothing
// (deadlock-free), schedules humans queue-head, and commits one cognitive unit
// per transaction inside the critical section. Synchronous, built on the
// `blocking` workspace + `vcs` layers.
#[cfg(feature = "blocking")]
pub mod coordinator;

// Web / reference search as a pluggable capability (kernel §10: 搜索经第三方
// MCP). A `SearchProvider` trait + a native `search` tool + a no-network stub
// provider, orthogonal to the local substring `find`. Built on the `tool` layer
// (`Tool`/`ToolCtx`), so gated on `blocking`. The crate ships only the contract
// and the stub; a session-connected MCP backend is plugged in as a provider.
#[cfg(feature = "blocking")]
pub mod search;
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
