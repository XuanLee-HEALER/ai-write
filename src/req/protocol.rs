//! Backend-independent protocol helpers shared by both client backends:
//! endpoint paths, request/response (de)serialization, status-to-error mapping,
//! and Server-Sent Events line decoding.
//!
//! Nothing here performs IO; the `blocking` and `async` backends call into these
//! functions after obtaining bytes from their respective HTTP libraries.

use serde::Deserialize;

use crate::req::error::{ApiError, Error, Result};
use crate::req::types::common::{Balance, ModelInfo};
use crate::req::types::request::ChatRequest;
use crate::req::types::response::{ChatResponse, Chunk};

/// Path of the chat completions endpoint.
pub(crate) const CHAT_PATH: &str = "/chat/completions";
/// Path of the list-models endpoint.
pub(crate) const MODELS_PATH: &str = "/models";
/// Path of the account-balance endpoint.
pub(crate) const BALANCE_PATH: &str = "/user/balance";

/// Envelope of `GET /models` (`{"object":"list","data":[...]}`).
#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelInfo>,
}

/// Serializes a chat request body to JSON bytes.
pub(crate) fn encode_request(req: &ChatRequest) -> Result<Vec<u8>> {
    serde_json::to_vec(req).map_err(|source| Error::Decode {
        context: "request",
        source,
    })
}

/// Parses a non-streaming chat completion response body.
pub(crate) fn parse_chat(body: &str) -> Result<ChatResponse> {
    serde_json::from_str(body).map_err(|source| Error::Decode {
        context: "chat",
        source,
    })
}

/// Parses a `GET /models` body into the model list.
pub(crate) fn parse_models(body: &str) -> Result<Vec<ModelInfo>> {
    serde_json::from_str::<ModelList>(body)
        .map(|m| m.data)
        .map_err(|source| Error::Decode {
            context: "models",
            source,
        })
}

/// Parses a `GET /user/balance` body.
pub(crate) fn parse_balance(body: &str) -> Result<Balance> {
    serde_json::from_str(body).map_err(|source| Error::Decode {
        context: "balance",
        source,
    })
}

/// Parses a single streaming `data:` payload into a [`Chunk`].
pub(crate) fn parse_chunk(data: &str) -> Result<Chunk> {
    serde_json::from_str(data).map_err(|source| Error::Decode {
        context: "chunk",
        source,
    })
}

/// Builds an [`Error::Api`] from a non-2xx status code and the raw body.
pub(crate) fn api_error(status: u16, body: &str) -> Error {
    Error::Api(ApiError::from_status(status, body))
}

/// The outcome of decoding one line of an SSE stream.
pub(crate) enum Sse<'a> {
    /// A `data:` payload to be parsed as a [`Chunk`].
    Data(&'a str),
    /// The `[DONE]` terminator.
    Done,
    /// A blank line or `:`-comment (e.g. `: keep-alive`) to ignore.
    Skip,
}

/// Decodes a single SSE line per the wire format and DeepSeek's keep-alive
/// behavior: blank lines and `:`-comments are skipped, `data: [DONE]`
/// terminates, and any other `data:` line is a JSON payload.
pub(crate) fn decode_line(line: &str) -> Sse<'_> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() || line.starts_with(':') {
        return Sse::Skip;
    }
    match line.strip_prefix("data:") {
        Some(rest) => {
            let data = rest.trim_start();
            if data == "[DONE]" {
                Sse::Done
            } else {
                Sse::Data(data)
            }
        }
        // `event:` / `id:` / unknown field lines are ignored.
        None => Sse::Skip,
    }
}
