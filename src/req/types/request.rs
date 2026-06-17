//! Request types for `POST /chat/completions`, plus the [`ChatRequest`] builder.
//!
//! Messages follow the flat OpenAI-style shape: a single [`Message`] struct with
//! a [`Role`] discriminator and optional fields. Combinations that are invalid
//! on the wire (for example a `system` message carrying `tool_calls`) are
//! representable by the type but are avoided by using the role constructors
//! ([`Message::system`], [`Message::user`], [`Message::assistant`],
//! [`Message::tool`]).

use serde::{Deserialize, Serialize};

use crate::req::error::{Error, Result};
use crate::req::model::Model;

/// The author of a [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System / developer instruction.
    System,
    /// End-user input.
    User,
    /// Model output fed back as conversation history.
    Assistant,
    /// The result of a tool call, replying to a prior `tool_calls` entry.
    Tool,
}

/// A single conversation message.
///
/// Prefer the role constructors over building this struct by hand. All optional
/// fields are omitted from the wire form when `None`.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    /// Who authored the message.
    pub role: Role,
    /// Text content. May be `None` on an assistant message that only carries
    /// `tool_calls`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Optional participant name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Thinking-mode chain, only used when back-filling a prefix completion
    /// (Beta). Must be stripped from ordinary multi-turn history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Tool calls emitted by a previous assistant turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// The id of the tool call this `tool` message responds to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Beta prefix completion: when `Some(true)` on the final assistant message,
    /// the model continues from `content` instead of starting fresh.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<bool>,
}

impl Message {
    fn bare(role: Role) -> Self {
        Message {
            role,
            content: None,
            name: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            prefix: None,
        }
    }

    /// Creates a `system` message.
    pub fn system(content: impl Into<String>) -> Self {
        Message {
            content: Some(content.into()),
            ..Message::bare(Role::System)
        }
    }

    /// Creates a `user` message.
    pub fn user(content: impl Into<String>) -> Self {
        Message {
            content: Some(content.into()),
            ..Message::bare(Role::User)
        }
    }

    /// Creates an `assistant` message (e.g. when replaying history).
    pub fn assistant(content: impl Into<String>) -> Self {
        Message {
            content: Some(content.into()),
            ..Message::bare(Role::Assistant)
        }
    }

    /// Creates a `tool` message replying to the tool call `tool_call_id`.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Message {
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            ..Message::bare(Role::Tool)
        }
    }
}

/// Thinking-mode control (a DeepSeek V4 extension).
///
/// When omitted from a request (`None`), the server default applies, which is
/// **thinking enabled**. Thinking tokens count toward `completion_tokens` and
/// consume the `max_tokens` budget, so leave generous headroom when enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    /// Disable the thinking chain; the model answers directly.
    Disabled,
    /// Enable the thinking chain at the given [`Effort`].
    Enabled {
        /// How hard the model should think.
        effort: Effort,
    },
}

/// Reasoning effort for [`Thinking::Enabled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Standard reasoning depth.
    High,
    /// Maximum reasoning depth (slower, pricier).
    Max,
}

impl Serialize for Thinking {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            Thinking::Disabled => {
                let mut m = serializer.serialize_map(Some(1))?;
                m.serialize_entry("type", "disabled")?;
                m.end()
            }
            Thinking::Enabled { effort } => {
                let mut m = serializer.serialize_map(Some(2))?;
                m.serialize_entry("type", "enabled")?;
                m.serialize_entry("reasoning_effort", effort)?;
                m.end()
            }
        }
    }
}

/// Output format for the completion (`response_format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Plain text (the default).
    Text,
    /// JSON object mode. The prompt must contain the word "json" and should show
    /// an example structure; see `docs/deepseek-api-research.md` §7.
    JsonObject,
}

/// A tool the model may call (currently always a function).
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    #[serde(rename = "type")]
    kind: &'static str,
    /// The function definition.
    pub function: FunctionDef,
}

impl Tool {
    /// Wraps a [`FunctionDef`] as a callable tool.
    pub fn function(function: FunctionDef) -> Self {
        Tool {
            kind: "function",
            function,
        }
    }
}

/// A function definition advertised to the model.
#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    /// Function name (`[a-zA-Z0-9_-]`, ≤ 64 chars).
    pub name: String,
    /// When and how the model should call this function.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the function's parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// A tool call. Emitted by the model in a response, and replayed verbatim inside
/// an assistant history [`Message`], hence both `Serialize` and `Deserialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique id of this call, echoed back in the matching `tool` message.
    pub id: String,
    /// Call type, always `"function"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The function name and (JSON-encoded) arguments.
    pub function: FunctionCall,
}

/// The function name and arguments of a [`ToolCall`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    /// The function name.
    pub name: String,
    /// Arguments as a JSON-encoded string (parse before use).
    pub arguments: String,
}

/// Controls whether and which tool the model may call (`tool_choice`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// Never call a tool.
    None,
    /// Let the model decide (the default when tools are present).
    Auto,
    /// Force the model to call some tool.
    Required,
    /// Force the model to call the named function.
    Function {
        /// The function to force.
        name: String,
    },
}

impl Serialize for ToolChoice {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Named<'a> {
            name: &'a str,
        }
        match self {
            ToolChoice::None => serializer.serialize_str("none"),
            ToolChoice::Auto => serializer.serialize_str("auto"),
            ToolChoice::Required => serializer.serialize_str("required"),
            ToolChoice::Function { name } => {
                use serde::ser::SerializeMap;
                let mut m = serializer.serialize_map(Some(2))?;
                m.serialize_entry("type", "function")?;
                m.serialize_entry("function", &Named { name })?;
                m.end()
            }
        }
    }
}

/// Streaming options (`stream_options`), only meaningful when streaming.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct StreamOptions {
    /// When `true`, the final chunk before `[DONE]` carries the `usage` object.
    pub include_usage: bool,
}

/// A chat completion request.
///
/// Build one with [`ChatRequest::builder`]. The `stream` and `stream_options`
/// fields are managed by the client methods (`chat` vs `chat_stream`) and are
/// not set through the builder.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    /// The model to use.
    pub model: Model,
    /// The conversation so far (at least one message).
    pub messages: Vec<Message>,
    /// Thinking-mode control. `None` uses the server default (enabled).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    /// Maximum number of tokens to generate (thinking tokens included).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Sampling temperature (≤ 2). Ignored in thinking mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Nucleus sampling (≤ 1). Ignored in thinking mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Up to 16 stop sequences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    /// Output format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// Tools the model may call (up to 128).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Tool-call policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Whether to return token log-probabilities. Errors in thinking mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    /// Number of top log-probabilities per position (≤ 20, needs `logprobs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u8>,
    /// Custom user id for content-safety attribution and concurrency isolation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Set by the client per call; not exposed on the builder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream: Option<bool>,
    /// Set by the client per call; not exposed on the builder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream_options: Option<StreamOptions>,
}

impl ChatRequest {
    /// Starts building a request for `model`.
    pub fn builder(model: Model) -> ChatRequestBuilder {
        ChatRequestBuilder::new(model)
    }

    /// Validates the entry-level set of locally-detectable illegal combinations,
    /// returning [`Error::InvalidRequest`] on the first problem found.
    ///
    /// Checks performed:
    /// - `messages` is non-empty;
    /// - thinking enabled together with `logprobs`/`top_logprobs` (the server
    ///   errors on this combination);
    /// - `top_logprobs` set without `logprobs == Some(true)`.
    ///
    /// Note: `temperature`/`top_p` are *silently* ignored in thinking mode, not
    /// errors, so they are not rejected here.
    fn validate(&self) -> Result<()> {
        if self.messages.is_empty() {
            return Err(Error::InvalidRequest("messages must not be empty".into()));
        }
        let thinking_on = matches!(self.thinking, Some(Thinking::Enabled { .. }));
        if thinking_on && (self.logprobs == Some(true) || self.top_logprobs.is_some()) {
            return Err(Error::InvalidRequest(
                "logprobs/top_logprobs are not supported in thinking mode".into(),
            ));
        }
        if self.top_logprobs.is_some() && self.logprobs != Some(true) {
            return Err(Error::InvalidRequest(
                "top_logprobs requires logprobs = true".into(),
            ));
        }
        Ok(())
    }
}

/// Builder for [`ChatRequest`], returned by [`ChatRequest::builder`].
///
/// # Examples
///
/// ```
/// use ai_write::req::{ChatRequest, Model, Thinking};
///
/// let req = ChatRequest::builder(Model::V4Flash)
///     .system("You are a careful writing assistant.")
///     .user("Draft an opening line.")
///     .thinking(Thinking::Disabled)
///     .max_tokens(512)
///     .build()
///     .expect("valid request");
///
/// assert_eq!(req.messages.len(), 2);
/// ```
#[derive(Debug, Clone)]
pub struct ChatRequestBuilder {
    req: ChatRequest,
}

impl ChatRequestBuilder {
    fn new(model: Model) -> Self {
        ChatRequestBuilder {
            req: ChatRequest {
                model,
                messages: Vec::new(),
                thinking: None,
                max_tokens: None,
                temperature: None,
                top_p: None,
                stop: None,
                response_format: None,
                tools: None,
                tool_choice: None,
                logprobs: None,
                top_logprobs: None,
                user_id: None,
                stream: None,
                stream_options: None,
            },
        }
    }

    /// Appends an arbitrary [`Message`].
    pub fn message(mut self, message: Message) -> Self {
        self.req.messages.push(message);
        self
    }

    /// Appends a `system` message.
    pub fn system(self, content: impl Into<String>) -> Self {
        self.message(Message::system(content))
    }

    /// Appends a `user` message.
    pub fn user(self, content: impl Into<String>) -> Self {
        self.message(Message::user(content))
    }

    /// Appends an `assistant` message.
    pub fn assistant(self, content: impl Into<String>) -> Self {
        self.message(Message::assistant(content))
    }

    /// Sets thinking-mode control.
    pub fn thinking(mut self, thinking: Thinking) -> Self {
        self.req.thinking = Some(thinking);
        self
    }

    /// Sets the maximum number of tokens to generate.
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.req.max_tokens = Some(max_tokens);
        self
    }

    /// Sets the sampling temperature.
    pub fn temperature(mut self, temperature: f32) -> Self {
        self.req.temperature = Some(temperature);
        self
    }

    /// Sets nucleus sampling `top_p`.
    pub fn top_p(mut self, top_p: f32) -> Self {
        self.req.top_p = Some(top_p);
        self
    }

    /// Sets the stop sequences.
    pub fn stop<I, S>(mut self, stop: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.req.stop = Some(stop.into_iter().map(Into::into).collect());
        self
    }

    /// Sets the response format.
    pub fn response_format(mut self, format: ResponseFormat) -> Self {
        self.req.response_format = Some(format);
        self
    }

    /// Sets the available tools.
    pub fn tools(mut self, tools: Vec<Tool>) -> Self {
        self.req.tools = Some(tools);
        self
    }

    /// Sets the tool-call policy.
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.req.tool_choice = Some(choice);
        self
    }

    /// Enables or disables log-probabilities.
    pub fn logprobs(mut self, logprobs: bool) -> Self {
        self.req.logprobs = Some(logprobs);
        self
    }

    /// Sets the number of top log-probabilities per position.
    pub fn top_logprobs(mut self, n: u8) -> Self {
        self.req.top_logprobs = Some(n);
        self
    }

    /// Sets the custom user id.
    pub fn user_id(mut self, user_id: impl Into<String>) -> Self {
        self.req.user_id = Some(user_id.into());
        self
    }

    /// Finalizes the request after validating it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if validation fails (empty messages, or
    /// an illegal parameter combination).
    pub fn build(self) -> Result<ChatRequest> {
        self.req.validate()?;
        Ok(self.req)
    }
}
