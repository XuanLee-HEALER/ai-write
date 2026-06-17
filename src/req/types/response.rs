//! Response types for `POST /chat/completions`, for both the non-streaming
//! response ([`ChatResponse`]) and streaming chunks ([`Chunk`]).
//!
//! Token counts are taken verbatim from the server's `usage` object; this crate
//! never estimates them.

use serde::Deserialize;

use crate::req::types::request::{Message, Role, ToolCall};

/// A complete (non-streaming) chat completion response.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    /// Unique response id.
    pub id: String,
    /// Object type, e.g. `"chat.completion"`.
    pub object: String,
    /// Unix timestamp (seconds) of creation.
    pub created: i64,
    /// The model that produced the response, as reported by the server (kept as
    /// a raw string; see [`Model`](crate::req::model::Model)).
    pub model: String,
    /// Backend configuration fingerprint, when present.
    #[serde(default)]
    pub system_fingerprint: Option<String>,
    /// The generated choices (typically one).
    pub choices: Vec<Choice>,
    /// Token usage, when reported.
    #[serde(default)]
    pub usage: Option<Usage>,
}

impl ChatResponse {
    /// The first choice's text content, if any.
    pub fn content(&self) -> Option<&str> {
        self.choices.first()?.message.content.as_deref()
    }

    /// The first choice's thinking chain, if the model was in thinking mode.
    pub fn reasoning(&self) -> Option<&str> {
        self.choices.first()?.message.reasoning_content.as_deref()
    }

    /// The first choice's finish reason, if present.
    pub fn finish_reason(&self) -> Option<&FinishReason> {
        self.choices.first()?.finish_reason.as_ref()
    }
}

/// One generated choice within a [`ChatResponse`].
#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    /// Index of this choice.
    pub index: u32,
    /// The assistant message produced.
    pub message: RespMessage,
    /// Why generation stopped (absent in some streaming frames).
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
}

/// The assistant message inside a [`Choice`].
#[derive(Debug, Clone, Deserialize)]
pub struct RespMessage {
    /// The role, always `"assistant"` here.
    pub role: String,
    /// The answer text. May be empty if thinking consumed the token budget.
    #[serde(default)]
    pub content: Option<String>,
    /// The thinking chain (thinking mode only).
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Tool calls requested by the model.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
}

impl RespMessage {
    /// Converts this response message into a request [`Message`] suitable for
    /// the next turn's history.
    ///
    /// `reasoning_content` is intentionally **dropped**: re-sending it in
    /// ordinary multi-turn history is rejected by the API (HTTP 400).
    pub fn to_history(&self) -> Message {
        Message {
            role: Role::Assistant,
            content: self.content.clone(),
            name: None,
            reasoning_content: None,
            tool_calls: self.tool_calls.clone(),
            tool_call_id: None,
            prefix: None,
        }
    }
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// Natural stop or a stop sequence was hit.
    Stop,
    /// `max_tokens` or the context limit was reached.
    Length,
    /// Output was filtered by the safety policy.
    ContentFilter,
    /// The model invoked one or more tools.
    ToolCalls,
    /// Generation was interrupted by a backend resource shortage.
    InsufficientSystemResource,
    /// An unrecognized reason; the raw string is preserved.
    Unknown(String),
}

impl<'de> Deserialize<'de> for FinishReason {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "stop" => FinishReason::Stop,
            "length" => FinishReason::Length,
            "content_filter" => FinishReason::ContentFilter,
            "tool_calls" => FinishReason::ToolCalls,
            "insufficient_system_resource" => FinishReason::InsufficientSystemResource,
            _ => FinishReason::Unknown(s),
        })
    }
}

/// Token usage for a request, reported verbatim by the server.
#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    /// Input tokens.
    pub prompt_tokens: u32,
    /// Output tokens (includes thinking tokens).
    pub completion_tokens: u32,
    /// `prompt_tokens + completion_tokens`.
    pub total_tokens: u32,
    /// Input tokens served from the on-disk prefix cache (billed at the cache
    /// hit rate).
    #[serde(default)]
    pub prompt_cache_hit_tokens: u32,
    /// Input tokens not served from cache (billed at the miss rate).
    #[serde(default)]
    pub prompt_cache_miss_tokens: u32,
    /// Breakdown of completion tokens (e.g. reasoning tokens).
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    /// Breakdown of prompt tokens (OpenAI-compatible cached count).
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

impl Usage {
    /// The number of thinking tokens, if reported.
    pub fn reasoning_tokens(&self) -> Option<u32> {
        self.completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
    }
}

/// Completion-token breakdown.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct CompletionTokensDetails {
    /// Thinking-chain tokens (thinking mode only).
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

/// Prompt-token breakdown (OpenAI-compatible).
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PromptTokensDetails {
    /// Cached prompt tokens (equivalent to `prompt_cache_hit_tokens`).
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

/// A single streaming chunk (`object = "chat.completion.chunk"`).
#[derive(Debug, Clone, Deserialize)]
pub struct Chunk {
    /// Unique response id (stable across the stream).
    pub id: String,
    /// Object type, e.g. `"chat.completion.chunk"`.
    pub object: String,
    /// Unix timestamp (seconds).
    pub created: i64,
    /// The model name.
    pub model: String,
    /// Backend configuration fingerprint, when present.
    #[serde(default)]
    pub system_fingerprint: Option<String>,
    /// Incremental choices.
    pub choices: Vec<ChunkChoice>,
    /// Usage, populated only on the final chunk when `include_usage` was set.
    #[serde(default)]
    pub usage: Option<Usage>,
}

impl Chunk {
    /// The first choice's incremental text content, if any.
    pub fn delta_content(&self) -> Option<&str> {
        self.choices.first()?.delta.content.as_deref()
    }

    /// The first choice's incremental thinking content, if any.
    pub fn delta_reasoning(&self) -> Option<&str> {
        self.choices.first()?.delta.reasoning_content.as_deref()
    }

    /// The first choice's finish reason, if this chunk carries one.
    pub fn finish_reason(&self) -> Option<&FinishReason> {
        self.choices.first()?.finish_reason.as_ref()
    }
}

/// One incremental choice within a [`Chunk`].
#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    /// Index of this choice.
    pub index: u32,
    /// The incremental delta.
    pub delta: Delta,
    /// Finish reason, present on the terminating chunk for this choice.
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
}

/// The incremental payload of a [`ChunkChoice`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Delta {
    /// Role, present on the first chunk only.
    #[serde(default)]
    pub role: Option<String>,
    /// Incremental text content.
    #[serde(default)]
    pub content: Option<String>,
    /// Incremental thinking content.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Incremental tool calls.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
}
