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

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::coordinator::Coordinator;
use crate::req::FunctionCall;
use crate::vcs::Vcs;
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
    /// A version-control operation (commit / history / diff / undo) failed after
    /// the edit itself succeeded, or a history/diff/undo tool could not be served.
    /// The on-disk article is already up to date; only the git side failed.
    #[error("version control error: {0}")]
    Vcs(String),
    /// Any other tool-level failure.
    #[error("tool failed: {0}")]
    Other(String),
}

/// The mutable context handed to a [`Tool`] for the duration of one call.
///
/// It bundles the workspace the tool operates on with the identity of the writer
/// performing the call, and optionally a [`Vcs`] handle and a shared
/// [`Coordinator`].
///
/// # Coordinator-routed edits (kernel §6)
///
/// The [`coord`](ToolCtx::coord) handle is **optional**. When present (the
/// [`engine`](crate::engine) layer attaches one shared by the master and every
/// slave), the article editors ([`WriteArticle`](tools::WriteArticle),
/// [`EditArticle`](tools::EditArticle), [`ApplyEdits`](tools::ApplyEdits)) route
/// each edit through [`Coordinator::submit`] as a single-file transaction whose
/// declared lock set is `{ <theme>/<file>, <theme>/index.json }`. The coordinator
/// owns the operation-level lock and the one [`Vcs`], so locking is implicit per
/// edit and the body **and** the theme manifest land in **one** commit (one
/// cognitive unit = one commit). The model never acquires or releases locks.
///
/// # Version control without a coordinator
///
/// The [`vcs`](ToolCtx::vcs) handle is also optional. When a coordinator is
/// **absent** but a `Vcs` is present, the editors fall back to writing through the
/// workspace and committing via [`ToolCtx::commit_article`] (body + index in one
/// commit). When **both** are absent (the workspace-only unit tests) the editors
/// write to disk and skip the commit, so the tool layer is usable without a git
/// repository. The history / diff tools read through whichever of the coordinator
/// or the `Vcs` is attached.
pub struct ToolCtx<'a> {
    /// The workspace rooted at the theme directory tree. Used by read / structure
    /// tools; with a coordinator attached, mutating edits go through the
    /// coordinator's own workspace instead.
    pub ws: &'a mut Workspace,
    /// The identity of the writer issuing this call (the scheduling priority and
    /// the git author of any commit produced by the call).
    pub writer: WriterId,
    /// The workspace's version-control handle, when version control is enabled
    /// without a coordinator. `None` disables direct committing.
    pub vcs: Option<&'a Vcs>,
    /// The shared transaction coordinator, when the engine attached one. When
    /// present, article edits are submitted as coordinator transactions and the
    /// history / diff tools read through the coordinator's exclusive [`Vcs`].
    pub coord: Option<Arc<Coordinator>>,
}

impl<'a> ToolCtx<'a> {
    /// Creates a tool context binding a workspace to a writer identity, with no
    /// version control or coordinator attached.
    ///
    /// Use [`ToolCtx::with_vcs`] to attach a [`Vcs`] so successful edits are
    /// committed directly, or [`ToolCtx::with_coordinator`] to route edits through
    /// the shared transaction coordinator.
    pub fn new(ws: &'a mut Workspace, writer: WriterId) -> Self {
        ToolCtx {
            ws,
            writer,
            vcs: None,
            coord: None,
        }
    }

    /// Attaches a shared [`Coordinator`] to this context, so article edits are
    /// submitted as coordinator transactions and the version-control tools read
    /// through the coordinator's exclusive [`Vcs`]. Returns the context for
    /// chaining.
    ///
    /// When a coordinator is attached it takes precedence over any [`Vcs`] set
    /// with [`ToolCtx::with_vcs`]: the coordinator owns the canonical lock state
    /// and the single commit path.
    pub fn with_coordinator(mut self, coord: Arc<Coordinator>) -> Self {
        self.coord = Some(coord);
        self
    }

    /// Attaches a [`Vcs`] handle to this context, enabling per-edit commits and
    /// the history / diff / undo tools. Returns the context for chaining.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use ai_write::tool::ToolCtx;
    /// # use ai_write::tool::workspace::{Workspace, WriterId};
    /// # use ai_write::vcs::Vcs;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut ws = Workspace::open("workspace")?;
    /// let vcs = Vcs::open_or_init(Path::new("workspace"))?;
    /// let ctx = ToolCtx::new(&mut ws, WriterId::Human).with_vcs(&vcs);
    /// # let _ = ctx;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_vcs(mut self, vcs: &'a Vcs) -> Self {
        self.vcs = Some(vcs);
        self
    }

    /// Commits an article **and** its theme index to version control as **one**
    /// commit after a successful edit, attributing it to [`writer`](ToolCtx::writer).
    ///
    /// This is the no-coordinator fallback path: a no-op (returning `Ok(None)`)
    /// when no [`Vcs`] is attached, so the editors can call it unconditionally.
    /// When a `Vcs` is present it folds the article file (`<theme>/<file_name>`)
    /// and the theme index (`<theme>/index.json`) into a **single**
    /// [`Vcs::commit_paths`] commit — one cognitive unit is one commit
    /// (`docs/coordinator-design.md` §5), replacing the earlier two-commit
    /// granularity. The index is included only when it exists on disk.
    ///
    /// The edit has already been written to disk by the time this is called, so a
    /// version-control failure does not lose content; it is surfaced as
    /// [`ToolError::Vcs`] so the model can see that the commit did not land.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Vcs`] if staging or committing the article (plus the
    /// index) fails.
    pub fn commit_article(
        &self,
        theme: &str,
        file_name: &str,
        message: &str,
    ) -> Result<Option<String>, ToolError> {
        let Some(vcs) = self.vcs else {
            return Ok(None);
        };
        let article_rel = std::path::Path::new(theme).join(file_name);
        let index_rel = std::path::Path::new(theme).join("index.json");

        // One commit covering the body and (when present) the manifest.
        let mut paths: Vec<&std::path::Path> = vec![article_rel.as_path()];
        if self.ws.root().join(&index_rel).exists() {
            paths.push(index_rel.as_path());
        }
        let sha = vcs
            .commit_paths(&paths, &self.writer, message)
            .map_err(|e| ToolError::Vcs(e.to_string()))?;
        Ok(Some(sha))
    }

    /// Persists `text` as the full new body of `<theme>/<file_name>` and records
    /// the edit as exactly one commit, returning the commit's short SHA (or `None`
    /// when version control is disabled).
    ///
    /// This is the single seam the article editors
    /// ([`WriteArticle`](tools::WriteArticle), [`EditArticle`](tools::EditArticle),
    /// [`ApplyEdits`](tools::ApplyEdits)) write through, so locking and committing
    /// are uniform regardless of how the new text was computed:
    ///
    /// - **With a [`Coordinator`] attached**, the write is a coordinator
    ///   transaction (kernel §6): the declared lock set is
    ///   `{ <theme>/<file>, <theme>/index.json }`, acquired all-or-nothing, and the
    ///   body plus manifest are committed once inside the critical section. Locking
    ///   is implicit — the model never touches a lock.
    /// - **Without a coordinator**, it writes through the workspace (taking the
    ///   in-memory per-article lock the caller is expected to hold) and commits via
    ///   [`ToolCtx::commit_article`] (body + index, one commit). This preserves the
    ///   pre-coordinator behaviour for the workspace-only tests.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Lock`] if the no-coordinator path is used and the
    /// caller does not hold the article lock, [`ToolError::NotFound`] /
    /// [`ToolError::Unsupported`] for a workspace failure, or [`ToolError::Vcs`] if
    /// the commit (coordinator or direct) fails.
    pub fn commit_full_text(
        &mut self,
        theme: &str,
        file_name: &str,
        text: &str,
        message: &str,
    ) -> Result<Option<String>, ToolError> {
        if let Some(coord) = self.coord.clone() {
            use crate::coordinator::{LockSet, TxnRequest};
            let article_rel = std::path::Path::new(theme).join(file_name);
            let index_rel = std::path::Path::new(theme).join("index.json");
            let locks = LockSet::new().with(&article_rel).with(&index_rel);
            let req = TxnRequest::new(self.writer.clone(), locks, message.to_string());
            let text = text.to_string();
            let theme = theme.to_string();
            let file_name = file_name.to_string();
            let message = message.to_string();
            let outcome = coord
                .submit(req, move |ctx| {
                    ctx.write_article(&theme, &file_name, &text)?;
                    Ok(message)
                })
                .map_err(coord_error_to_tool)?;
            return Ok(outcome.sha);
        }

        // No coordinator: write through the workspace (lock must be held) and
        // commit body + index in one commit.
        let writer = self.writer.clone();
        self.ws.write_article(theme, file_name, text, &writer)?;
        self.commit_article(theme, file_name, message)
    }
}

/// Maps a [`CoordError`](crate::coordinator::CoordError) to the [`ToolError`] the
/// model sees, so a coordinator-routed edit reports failures in the same shape as
/// the direct path.
fn coord_error_to_tool(err: crate::coordinator::CoordError) -> ToolError {
    use crate::coordinator::CoordError;
    match err {
        CoordError::Undeclared(path) => ToolError::Other(format!(
            "edit touched an undeclared path: {}",
            path.display()
        )),
        CoordError::Workspace(e) => e,
        CoordError::Vcs(e) => ToolError::Vcs(e.to_string()),
        CoordError::Aborted(msg) => ToolError::Other(msg),
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
