//! Synchronous DeepSeek client backed by `ureq` (no async runtime).
//!
//! Enabled by the default `blocking` feature. The entry point is [`Client`],
//! constructed with [`Client::from_env`] or [`Client::builder`].

use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use crate::req::error::{Error, Result, TransportError};
use crate::req::protocol::{self, Sse};
use crate::req::types::common::{Balance, ModelInfo};
use crate::req::types::request::{ChatRequest, StreamOptions};
use crate::req::types::response::{ChatResponse, Chunk};

/// Default API base URL.
const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

/// A synchronous DeepSeek API client.
///
/// Cloning is cheap: the underlying `ureq` agent shares its connection pool.
///
/// # Examples
///
/// ```no_run
/// use ai_write::req::blocking::Client;
/// use ai_write::req::{ChatRequest, Model, Thinking};
///
/// let client = Client::from_env()?;
/// let req = ChatRequest::builder(Model::V4Flash)
///     .user("Say hi in three words.")
///     .thinking(Thinking::Disabled)
///     .max_tokens(32)
///     .build()?;
/// let resp = client.chat(&req)?;
/// println!("{:?}", resp.content());
/// # Ok::<(), ai_write::req::Error>(())
/// ```
#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
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
    pub fn chat(&self, req: &ChatRequest) -> Result<ChatResponse> {
        let body = protocol::encode_request(req)?;
        let mut resp = self
            .agent
            .post(self.url(protocol::CHAT_PATH))
            .header("Authorization", self.auth())
            .header("Content-Type", "application/json")
            .send(body.as_slice())
            .map_err(map_ureq)?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(map_ureq)?;
        if !(200..300).contains(&status) {
            return Err(protocol::api_error(status, &text));
        }
        protocol::parse_chat(&text)
    }

    /// Performs a streaming chat completion, returning an iterator over decoded
    /// [`Chunk`]s. Keep-alive comments and blank lines are filtered out and the
    /// stream ends at `[DONE]`. `stream_options.include_usage` is requested, so
    /// the final chunk carries `usage`.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial request fails or the server replies with
    /// a non-2xx status. Per-chunk decode/transport errors surface as `Err`
    /// items from the iterator.
    pub fn chat_stream(&self, req: &ChatRequest) -> Result<ChatStream> {
        let mut req = req.clone();
        req.stream = Some(true);
        req.stream_options = Some(StreamOptions {
            include_usage: true,
        });
        let body = protocol::encode_request(&req)?;
        let resp = self
            .agent
            .post(self.url(protocol::CHAT_PATH))
            .header("Authorization", self.auth())
            .header("Content-Type", "application/json")
            .send(body.as_slice())
            .map_err(map_ureq)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let mut resp = resp;
            let text = resp.body_mut().read_to_string().map_err(map_ureq)?;
            return Err(protocol::api_error(status, &text));
        }
        let reader: Box<dyn Read> = Box::new(resp.into_body().into_reader());
        Ok(ChatStream {
            lines: BufReader::new(reader).lines(),
            done: false,
        })
    }

    /// Lists the available models (`GET /models`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Api`], [`Error::Transport`], or [`Error::Decode`].
    pub fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let (status, text) = self.get(protocol::MODELS_PATH)?;
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
    pub fn balance(&self) -> Result<Balance> {
        let (status, text) = self.get(protocol::BALANCE_PATH)?;
        if !(200..300).contains(&status) {
            return Err(protocol::api_error(status, &text));
        }
        protocol::parse_balance(&text)
    }

    fn get(&self, path: &str) -> Result<(u16, String)> {
        let mut resp = self
            .agent
            .get(self.url(path))
            .header("Authorization", self.auth())
            .call()
            .map_err(map_ureq)?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(map_ureq)?;
        Ok((status, text))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_key)
    }
}

/// A synchronous iterator over streaming [`Chunk`]s, returned by
/// [`Client::chat_stream`].
pub struct ChatStream {
    lines: std::io::Lines<BufReader<Box<dyn Read>>>,
    done: bool,
}

impl Iterator for ChatStream {
    type Item = Result<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            match self.lines.next() {
                None => {
                    self.done = true;
                    return None;
                }
                Some(Err(e)) => {
                    self.done = true;
                    return Some(Err(Error::Transport(TransportError::Other(Box::new(e)))));
                }
                Some(Ok(line)) => match protocol::decode_line(&line) {
                    Sse::Skip => continue,
                    Sse::Done => {
                        self.done = true;
                        return None;
                    }
                    Sse::Data(data) => return Some(protocol::parse_chunk(data)),
                },
            }
        }
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

    /// Sets a global timeout covering the whole request/response. By default no
    /// timeout is set; the server may hold a connection open for up to 10
    /// minutes before inference starts.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Builds the [`Client`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if no API key was provided.
    pub fn build(self) -> Result<Client> {
        let api_key = self
            .api_key
            .ok_or_else(|| Error::Config("api_key is required".into()))?;
        let base_url = self.base_url.trim_end_matches('/').to_string();
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(self.timeout)
            .build();
        Ok(Client {
            agent: ureq::Agent::new_with_config(config),
            api_key,
            base_url,
        })
    }
}

/// Translates a `ureq` error into a backend-agnostic [`Error`].
fn map_ureq(e: ureq::Error) -> Error {
    use ureq::Error as U;
    let kind = match e {
        U::Timeout(_) => TransportError::Timeout,
        U::HostNotFound | U::ConnectionFailed => TransportError::Connect,
        U::Io(_) => TransportError::Closed,
        other => TransportError::Other(Box::new(other)),
    };
    Error::Transport(kind)
}
