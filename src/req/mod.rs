//! Stateless DeepSeek API client (`req` module).
//!
//! This module is a thin, stateless wrapper over the DeepSeek HTTP API: one call
//! performs one HTTP round trip. It owns request/response types, a
//! backend-agnostic [`error`] model, SSE hygiene, and two interchangeable IO
//! backends selected by Cargo features:
//!
//! - **`blocking`** (default) — synchronous client backed by `ureq`, no async
//!   runtime.
//! - **`async`** — asynchronous client backed by `reqwest`.
//!
//! Higher-level concerns (sessions, context/token management, retries, rate
//! limiting) live above this layer. See `docs/req-module-design.md` for the full
//! design.

pub mod error;
pub mod model;
pub mod types;

// Pure core shared by both backends: URL/header/body assembly and SSE line
// decoding. Consumed by the client backends.
mod protocol;

#[cfg(feature = "blocking")]
pub mod blocking;

// The async client is re-exported as `req::Client`.
#[cfg(feature = "async")]
mod client_async;
#[cfg(feature = "async")]
pub use client_async::Client;

pub use error::{ApiError, ApiErrorKind, Error, Result, TransportError};
pub use model::Model;
pub use types::common::{Balance, BalanceInfo, ModelInfo};
pub use types::request::{
    ChatRequest, ChatRequestBuilder, Effort, FunctionCall, FunctionDef, Message, ResponseFormat,
    Role, StreamOptions, Thinking, Tool, ToolCall, ToolChoice,
};
pub use types::response::{
    ChatResponse, Choice, Chunk, ChunkChoice, CompletionTokensDetails, Delta, FinishReason,
    PromptTokensDetails, RespMessage, Usage,
};
