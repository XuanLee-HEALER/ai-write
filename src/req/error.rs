//! Error types for the DeepSeek API client.
//!
//! Every fallible operation in this crate returns [`Result`], whose error arm is
//! the single [`Error`] enum. The design goal is a **backend-agnostic** error
//! surface: whether the HTTP call is performed by `ureq` (the `blocking`
//! backend) or `reqwest` (the `async` backend), callers match on the same
//! variants and never observe the underlying library's error type. Swapping the
//! HTTP backend is therefore an internal change that does not break downstream
//! code.
//!
//! # Taxonomy
//!
//! | Variant | Meaning |
//! |---------|---------|
//! | [`Error::Api`] | The server replied with a non-2xx status, classified by [`ApiErrorKind`]. |
//! | [`Error::Transport`] | A network-level failure (timeout, connection, TLS, dropped connection). |
//! | [`Error::Decode`] | A 2xx response body could not be deserialized. |
//! | [`Error::InvalidRequest`] | The request was rejected locally, before being sent. |
//! | [`Error::Config`] | The client was built with invalid configuration. |

use std::fmt;

/// Convenience alias for `Result<T, `[`Error`]`>`.
pub type Result<T> = std::result::Result<T, Error>;

/// The single error type returned by all fallible operations in this crate.
///
/// Marked `#[non_exhaustive]`: match arms must include a wildcard, so new
/// variants can be added in a backward-compatible release.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The server returned a non-2xx HTTP status. The inner [`ApiError`] carries
    /// the raw status code, a semantic [`ApiErrorKind`], and the server's
    /// message (when one could be parsed out of the body).
    #[error("DeepSeek API error {} ({:?}): {}", .0.status, .0.kind, .0.message)]
    Api(ApiError),

    /// A transport-level failure that happened before a complete HTTP response
    /// was received. Backend errors from `ureq`/`reqwest` are normalized into
    /// [`TransportError`].
    #[error(transparent)]
    Transport(#[from] TransportError),

    /// A successful (2xx) response carried a body that could not be parsed into
    /// the expected type.
    #[error("failed to decode {context} response: {source}")]
    Decode {
        /// Which response was being parsed: `"chat"`, `"chunk"`, `"models"`,
        /// `"balance"`, or `"request"` (when encoding the outgoing body).
        context: &'static str,
        /// The underlying serialization error.
        #[source]
        source: serde_json::Error,
    },

    /// The request was rejected locally by validation before any network call
    /// was made (for example, an illegal parameter combination). The string
    /// describes what was wrong.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The client could not be configured (for example, a missing API key in
    /// the environment, or an unparseable base URL).
    #[error("configuration error: {0}")]
    Config(String),
}

impl Error {
    /// Returns `true` when retrying the same request later might plausibly
    /// succeed: server faults ([`ApiErrorKind::ServerError`],
    /// [`ApiErrorKind::Overloaded`]) and recoverable transport failures
    /// (timeout, connection, dropped connection).
    ///
    /// This is **classification only** — the client never retries on its own.
    /// Rate limiting ([`ApiErrorKind::RateLimited`], HTTP 429) is deliberately
    /// **not** treated as transient here, because back-off policy is the
    /// caller's responsibility.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Api(e) => {
                matches!(e.kind, ApiErrorKind::ServerError | ApiErrorKind::Overloaded)
            }
            Error::Transport(t) => matches!(
                t,
                TransportError::Timeout | TransportError::Connect | TransportError::Closed
            ),
            Error::Decode { .. } | Error::InvalidRequest(_) | Error::Config(_) => false,
        }
    }
}

/// A non-2xx response from the DeepSeek API.
#[derive(Debug, Clone)]
pub struct ApiError {
    /// The raw HTTP status code, preserved verbatim for logging and for codes
    /// not covered by [`ApiErrorKind`].
    pub status: u16,
    /// The status code mapped to a semantic kind for ergonomic matching.
    pub kind: ApiErrorKind,
    /// The human-readable message extracted from the response body, or the
    /// (possibly truncated) raw body when no structured message was present.
    pub message: String,
}

impl ApiError {
    /// Builds an [`ApiError`] from a status code and the raw response body,
    /// mapping the code to an [`ApiErrorKind`] and extracting a message.
    ///
    /// This is the single status-to-error mapping shared by both backends.
    pub fn from_status(status: u16, body: &str) -> Self {
        let kind = ApiErrorKind::from_status(status);
        ApiError {
            status,
            kind,
            message: extract_message(body),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:?}): {}", self.status, self.kind, self.message)
    }
}

/// Semantic classification of a non-2xx status returned by the DeepSeek API.
///
/// The codes mirror the documented error table; see
/// `docs/deepseek-api-research.md` §13.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApiErrorKind {
    /// 400 — malformed request body.
    BadRequest,
    /// 401 — invalid API key / authentication failed.
    Unauthorized,
    /// 402 — account balance exhausted.
    InsufficientBalance,
    /// 422 — request parameters are invalid.
    InvalidParams,
    /// 429 — TPM/RPM or concurrency limit reached.
    RateLimited,
    /// 500 — internal server fault.
    ServerError,
    /// 503 — server overloaded.
    Overloaded,
    /// Any other non-2xx status not covered above (see [`ApiError::status`]).
    Other,
}

impl ApiErrorKind {
    /// Maps an HTTP status code to its semantic kind.
    pub fn from_status(status: u16) -> Self {
        match status {
            400 => ApiErrorKind::BadRequest,
            401 => ApiErrorKind::Unauthorized,
            402 => ApiErrorKind::InsufficientBalance,
            422 => ApiErrorKind::InvalidParams,
            429 => ApiErrorKind::RateLimited,
            500 => ApiErrorKind::ServerError,
            503 => ApiErrorKind::Overloaded,
            _ => ApiErrorKind::Other,
        }
    }
}

/// A normalized transport-level failure, independent of the HTTP backend.
///
/// Each backend adapter (`ureq` / `reqwest`) translates its library-specific
/// error into one of these variants; anything that cannot be classified lands
/// in [`TransportError::Other`], which boxes the original error without exposing
/// its concrete type in this crate's public API.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// The request timed out while connecting or while reading the response.
    #[error("request timed out")]
    Timeout,
    /// A connection could not be established (DNS failure, connection refused,
    /// network unreachable).
    #[error("failed to establish connection")]
    Connect,
    /// A TLS handshake or certificate error occurred.
    #[error("TLS error")]
    Tls,
    /// The connection was closed unexpectedly, or failed while reading the body.
    #[error("connection closed unexpectedly")]
    Closed,
    /// Any other transport failure. The backend's original error is preserved
    /// as the boxed source without leaking its concrete type.
    #[error("transport error: {0}")]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

/// Best-effort extraction of a human-readable message from an error body.
///
/// Tries `{"error":{"message":...}}`, then a top-level `{"message":...}`, and
/// finally falls back to the trimmed (and length-capped) raw body.
fn extract_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(msg) = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return msg.to_owned();
        }
        if let Some(msg) = value.get("message").and_then(|m| m.as_str()) {
            return msg.to_owned();
        }
    }
    const MAX: usize = 512;
    let trimmed = body.trim();
    if trimmed.len() > MAX {
        // Truncate on a char boundary to keep the message valid UTF-8.
        let end = trimmed
            .char_indices()
            .take_while(|(i, _)| *i <= MAX)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        format!("{}…", &trimmed[..end])
    } else {
        trimmed.to_owned()
    }
}
