//! Asynchronous DeepSeek client backed by `reqwest`.
//!
//! Enabled by the `async` feature and re-exported as [`crate::req::Client`]. The
//! method surface mirrors the blocking client; only `async`/`.await` and the
//! streaming return type differ.

use std::pin::Pin;
use std::time::Duration;

use futures_core::Stream;
use futures_util::StreamExt;

use crate::req::error::{Error, Result, TransportError};
use crate::req::protocol::{self, Sse};
use crate::req::types::common::{Balance, ModelInfo};
use crate::req::types::request::{ChatRequest, StreamOptions};
use crate::req::types::response::{ChatResponse, Chunk};

/// Default API base URL.
const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

/// A boxed stream of decoded chat [`Chunk`]s, returned by
/// [`Client::chat_stream`].
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<Chunk>> + Send>>;

/// An asynchronous DeepSeek API client.
///
/// Cloning is cheap: the underlying `reqwest` client shares its connection pool.
///
/// # Examples
///
/// ```no_run
/// # async fn run() -> Result<(), ai_write::req::Error> {
/// use ai_write::req::Client;
/// use ai_write::req::{ChatRequest, Model, Thinking};
///
/// let client = Client::from_env()?;
/// let req = ChatRequest::builder(Model::V4Flash)
///     .user("Say hi in three words.")
///     .thinking(Thinking::Disabled)
///     .max_tokens(32)
///     .build()?;
/// let resp = client.chat(&req).await?;
/// println!("{:?}", resp.content());
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl Client {
    /// Starts building a client.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Builds a client from the environment.
    ///
    /// Reads `DEEPSEEK_API_KEY` (required) and `DEEPSEEK_BASE_URL` (optional,
    /// defaults to `https://api.deepseek.com`). Loading a `.env` file is the
    /// caller's responsibility.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if `DEEPSEEK_API_KEY` is not set.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .map_err(|_| Error::Config("DEEPSEEK_API_KEY is not set".into()))?;
        let mut builder = Client::builder().api_key(api_key);
        if let Ok(base_url) = std::env::var("DEEPSEEK_BASE_URL") {
            builder = builder.base_url(base_url);
        }
        builder.build()
    }

    /// Performs a non-streaming chat completion (`POST /chat/completions`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Api`] for a non-2xx response, [`Error::Transport`] for a
    /// network failure, or [`Error::Decode`] if the body cannot be parsed.
    pub async fn chat(&self, req: &ChatRequest) -> Result<ChatResponse> {
        let body = protocol::encode_request(req)?;
        let resp = self
            .http
            .post(self.url(protocol::CHAT_PATH))
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(map_reqwest)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(map_reqwest)?;
        if !(200..300).contains(&status) {
            return Err(protocol::api_error(status, &text));
        }
        protocol::parse_chat(&text)
    }

    /// Performs a streaming chat completion, returning a [`Stream`] of decoded
    /// [`Chunk`]s. Keep-alive comments and blank lines are filtered out and the
    /// stream ends at `[DONE]`. `stream_options.include_usage` is requested, so
    /// the final chunk carries `usage`.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial request fails or the server replies with
    /// a non-2xx status. Per-chunk decode/transport errors surface as `Err`
    /// items from the stream.
    pub async fn chat_stream(&self, req: &ChatRequest) -> Result<ChatStream> {
        let mut req = req.clone();
        req.stream = Some(true);
        req.stream_options = Some(StreamOptions {
            include_usage: true,
        });
        let body = protocol::encode_request(&req)?;
        let resp = self
            .http
            .post(self.url(protocol::CHAT_PATH))
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(map_reqwest)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.text().await.map_err(map_reqwest)?;
            return Err(protocol::api_error(status, &text));
        }

        let byte_stream = resp.bytes_stream();
        let stream = async_stream::stream! {
            let mut byte_stream = std::pin::pin!(byte_stream);
            let mut buf: Vec<u8> = Vec::new();
            let mut done = false;
            while let Some(item) = byte_stream.next().await {
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(map_reqwest(e));
                        return;
                    }
                };
                buf.extend_from_slice(&bytes);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&raw);
                    match protocol::decode_line(&line) {
                        Sse::Skip => {}
                        Sse::Done => {
                            done = true;
                            break;
                        }
                        Sse::Data(data) => yield protocol::parse_chunk(data),
                    }
                }
                if done {
                    break;
                }
            }
            // Flush a trailing line that arrived without a final newline.
            if !done && !buf.is_empty() {
                let line = String::from_utf8_lossy(&buf);
                if let Sse::Data(data) = protocol::decode_line(&line) {
                    yield protocol::parse_chunk(data);
                }
            }
        };
        Ok(Box::pin(stream))
    }

    /// Lists the available models (`GET /models`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Api`], [`Error::Transport`], or [`Error::Decode`].
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let (status, text) = self.get(protocol::MODELS_PATH).await?;
        if !(200..300).contains(&status) {
            return Err(protocol::api_error(status, &text));
        }
        protocol::parse_models(&text)
    }

    /// Fetches the account balance (`GET /user/balance`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Api`], [`Error::Transport`], or [`Error::Decode`].
    pub async fn balance(&self) -> Result<Balance> {
        let (status, text) = self.get(protocol::BALANCE_PATH).await?;
        if !(200..300).contains(&status) {
            return Err(protocol::api_error(status, &text));
        }
        protocol::parse_balance(&text)
    }

    async fn get(&self, path: &str) -> Result<(u16, String)> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(map_reqwest)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(map_reqwest)?;
        Ok((status, text))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

/// Builder for [`Client`].
pub struct ClientBuilder {
    api_key: Option<String>,
    base_url: String,
    timeout: Option<Duration>,
}

impl ClientBuilder {
    fn new() -> Self {
        ClientBuilder {
            api_key: None,
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: None,
        }
    }

    /// Sets the API key (required).
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Overrides the base URL (default `https://api.deepseek.com`).
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Sets a request timeout. By default no timeout is set; the server may hold
    /// a connection open for up to 10 minutes before inference starts.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Builds the [`Client`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if no API key was provided or the underlying
    /// HTTP client could not be constructed.
    pub fn build(self) -> Result<Client> {
        let api_key = self
            .api_key
            .ok_or_else(|| Error::Config("api_key is required".into()))?;
        let base_url = self.base_url.trim_end_matches('/').to_string();
        let mut http = reqwest::Client::builder();
        if let Some(timeout) = self.timeout {
            http = http.timeout(timeout);
        }
        let http = http
            .build()
            .map_err(|e| Error::Config(format!("failed to build HTTP client: {e}")))?;
        Ok(Client {
            http,
            api_key,
            base_url,
        })
    }
}

/// Translates a `reqwest` error into a backend-agnostic [`Error`].
fn map_reqwest(e: reqwest::Error) -> Error {
    let kind = if e.is_timeout() {
        TransportError::Timeout
    } else if e.is_connect() {
        TransportError::Connect
    } else if e.is_body() || e.is_decode() {
        TransportError::Closed
    } else {
        TransportError::Other(Box::new(e))
    };
    Error::Transport(kind)
}
