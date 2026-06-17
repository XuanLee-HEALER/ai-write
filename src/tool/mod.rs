//! Tool system, dispatch registry, and the writing workspace model.
//!
//! This module is one of the two pillars of v0 (the other being
//! [`session`](crate::session)). It provides:
//!
//! - The [`Tool`] trait — a named, JSON-schema-described callable that operates
//!   on a [`ToolCtx`] and returns a [`ToolResult`].
//! - The [`ToolRegistry`] — holds a set of tools, exports their
//!   [`req::Tool`](crate::req::Tool) definitions for a [`Session`](crate::session::Session),
//!   dispatches a call by name, and turns a [`ToolResult`] into the JSON payload
//!   of a `tool` reply message.
//! - The [`workspace`] submodule — the on-disk workspace model
//!   ([`Workspace`] / [`Theme`] / [`Article`] / [`Index`]) plus the
//!   single-writer article lock and the path sandbox.
//!
//! - The [`tools`] submodule — the concrete v0 native writing tools (theme and
//!   article lifecycle, read/find, the lock-guarded editors, locks, and the
//!   slave report), assembled into a registry by
//!   [`tools::writing_tools`].
//!
//! [`Workspace`]: workspace::Workspace
//! [`Theme`]: workspace::Theme
//! [`Article`]: workspace::Article
//! [`Index`]: workspace::Index
//!
//! # Safety boundaries
//!
//! Deterministic guard rails live in the tools: every path is resolved inside
//! the workspace root, `..` escapes / absolute paths / system paths are
//! rejected, and oversized or binary files are refused. When a tool refuses, the
//! refusal is returned as a [`ToolError`] and surfaced back to the model as a
//! `tool` message, letting the model adapt its strategy. The tools never relax a
//! guard rail because the model asked them to.

pub mod tools;
pub mod workspace;

use serde::{Deserialize, Serialize};

use crate::req::FunctionCall;
use workspace::{Workspace, WriterId};

/// The result of a tool invocation.
///
/// `Ok` carries the JSON value that becomes the content of the `tool` reply
/// message; `Err` carries a [`ToolError`] that is *also* fed back to the model
/// (as an error payload) so it can recover, rather than aborting the session.
pub type ToolResult = std::result::Result<serde_json::Value, ToolError>;

/// An error produced by a tool, designed to be surfaced back to the model.
///
/// A tool error is **not** a fatal session error: the registry serializes it
/// into the `tool` reply so the model can read what went wrong and change
/// strategy (re-read the article, chunk a large file, pick a different tool).
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ToolError {
    /// The arguments did not match the tool's schema or were otherwise invalid.
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    /// A requested path escaped the workspace sandbox (absolute, `..` traversal,
    /// or a system path).
    #[error("path is outside the workspace sandbox: {0}")]
    SandboxViolation(String),
    /// The named theme, article, or other resource does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The article lock could not be acquired, or an edit was attempted without
    /// holding it.
    #[error("lock error: {0}")]
    Lock(String),
    /// The target file is too large or is binary, and was refused. The model is
    /// expected to adapt (chunked reads, summaries, a different tool).
    #[error("unsupported content: {0}")]
    Unsupported(String),
    /// An underlying I/O failure (the message is the rendered error).
    #[error("io error: {0}")]
    Io(String),
    /// No tool with the requested name is registered.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    /// Any other tool-level failure.
    #[error("tool failed: {0}")]
    Other(String),
}

/// The mutable context handed to a [`Tool`] for the duration of one call.
///
/// It bundles the workspace the tool operates on with the identity of the writer
/// performing the call, so lock-guarded tools can check and record ownership.
pub struct ToolCtx<'a> {
    /// The workspace rooted at the theme directory tree.
    pub ws: &'a mut Workspace,
    /// The identity of the writer issuing this call (used by lock-guarded tools).
    pub writer: WriterId,
}

impl<'a> ToolCtx<'a> {
    /// Creates a tool context binding a workspace to a writer identity.
    pub fn new(ws: &'a mut Workspace, writer: WriterId) -> Self {
        ToolCtx { ws, writer }
    }
}

/// A named, schema-described callable the model may invoke.
///
/// Each implementor advertises a [`req::Tool`](crate::req::Tool) definition via
/// [`Tool::schema`] and performs its work in [`Tool::call`], reading and writing
/// through the [`ToolCtx`].
pub trait Tool {
    /// The unique function name advertised to the model (`[a-zA-Z0-9_-]`, ≤ 64
    /// chars). Must match the `name` inside [`Tool::schema`].
    fn name(&self) -> &str;

    /// The tool's wire definition (name, description, JSON-schema parameters),
    /// ready to be advertised in a request's `tools` array.
    fn schema(&self) -> crate::req::Tool;

    /// Executes the tool against `ctx` with the model-supplied `args`.
    ///
    /// # Errors
    ///
    /// Returns a [`ToolError`] for any failure (bad arguments, sandbox violation,
    /// missing resource, lock contention, unsupported content). The error is
    /// reported back to the model rather than aborting the session.
    fn call(&self, args: serde_json::Value, ctx: &mut ToolCtx<'_>) -> ToolResult;
}

/// A set of [`Tool`]s, with name-based dispatch and definition export.
///
/// A [`Session`](crate::session::Session) holds one registry: it exports all
/// tool definitions into each request and routes the model's `tool_calls` back
/// to the matching tool by name.
#[derive(Default)]
pub struct ToolRegistry {
    /// The registered tools, in registration order.
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        ToolRegistry::default()
    }

    /// Registers a tool, returning the registry for chaining.
    ///
    /// Later registrations of the same name shadow earlier ones at dispatch time;
    /// callers should keep names unique.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> &mut Self {
        self.tools.push(tool);
        self
    }

    /// Returns `true` if no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Exports every registered tool's [`req::Tool`](crate::req::Tool) definition,
    /// suitable for a request's `tools` array. Returns an empty vector when no
    /// tools are registered.
    pub fn definitions(&self) -> Vec<crate::req::Tool> {
        self.tools.iter().map(|t| t.schema()).collect()
    }

    /// Dispatches a single model-issued [`FunctionCall`] to the matching tool.
    ///
    /// The call's JSON-encoded `arguments` are parsed before being handed to the
    /// tool's [`Tool::call`]. Empty arguments are treated as an empty JSON object.
    /// When more than one tool shares a name the **last** registered wins.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::UnknownTool`] if no tool matches the call's name,
    /// [`ToolError::InvalidArgs`] if the arguments are not valid JSON, or whatever
    /// [`ToolError`] the tool itself produced.
    pub fn dispatch(&self, call: &FunctionCall, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let Some(tool) = self.tools.iter().rev().find(|t| t.name() == call.name) else {
            return Err(ToolError::UnknownTool(call.name.clone()));
        };
        let trimmed = call.arguments.trim();
        let args: serde_json::Value = if trimmed.is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(trimmed)
                .map_err(|e| ToolError::InvalidArgs(format!("arguments are not valid JSON: {e}")))?
        };
        tool.call(args, ctx)
    }
}
