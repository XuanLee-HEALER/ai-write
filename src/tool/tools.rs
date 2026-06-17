//! The v0 native writing tools (everything except search).
//!
//! Each tool implements the [`Tool`] trait: it advertises a
//! JSON-schema [`req::Tool`](crate::req::Tool) definition and performs its work
//! against a [`ToolCtx`], reading and writing the workspace through the sandboxed
//! [`Workspace`](crate::tool::workspace::Workspace) API. Failures are returned as
//! [`ToolError`] values and surfaced back to the model as the content of a `tool`
//! reply, never aborting the session.
//!
//! The set mirrors `impl-v0.md` §3:
//!
//! | Tool | Holds lock | Purpose |
//! |---|---|---|
//! | [`CreateTheme`] / [`DeleteTheme`] | — | theme directories |
//! | [`CreateArticle`] / [`DeleteArticle`] / [`ListArticles`] | — | article files + index |
//! | [`ReadArticle`] / [`Find`] | — | read full text / search the workspace |
//! | [`WriteArticle`] | ✅ | full overwrite |
//! | [`EditArticle`] | ✅ | exact unique `old` → `new` replace |
//! | [`ApplyEdits`] | ✅ | fine-grained, atomic batch of offset/anchor ops |
//! | [`AcquireLock`] / [`ReleaseLock`] | — | single-writer article lock |
//! | [`Report`] | — | slave → master structured report |
//!
//! [`ToolCtx`]: crate::tool::ToolCtx
//! [`ToolError`]: crate::tool::ToolError

use serde::Deserialize;
use serde_json::{Value, json};

use crate::req::{FunctionDef, Tool as ReqTool};
use crate::tool::{Tool, ToolCtx, ToolError, ToolResult};

/// Deserializes a tool's typed argument struct from the model-supplied JSON,
/// mapping any parse failure to [`ToolError::InvalidArgs`].
fn parse_args<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))
}

/// Builds a [`req::Tool`](crate::req::Tool) from a name, description, and a JSON
/// Schema parameter object.
fn def(name: &str, description: &str, parameters: Value) -> ReqTool {
    ReqTool::function(FunctionDef {
        name: name.to_string(),
        description: Some(description.to_string()),
        parameters: Some(parameters),
    })
}

/// A JSON Schema `string` property with a description.
fn string_prop(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

/// Registers all v0 native writing tools (everything except search) into a fresh
/// [`ToolRegistry`](crate::tool::ToolRegistry).
///
/// This is the tool set a slave session is configured with.
///
/// # Examples
///
/// ```
/// use ai_write::tool::tools::writing_tools;
///
/// let registry = writing_tools();
/// assert!(registry.len() >= 12);
/// ```
pub fn writing_tools() -> crate::tool::ToolRegistry {
    let mut r = crate::tool::ToolRegistry::new();
    r.register(Box::new(CreateTheme))
        .register(Box::new(DeleteTheme))
        .register(Box::new(CreateArticle))
        .register(Box::new(DeleteArticle))
        .register(Box::new(ListArticles))
        .register(Box::new(ReadArticle))
        .register(Box::new(Find))
        .register(Box::new(WriteArticle))
        .register(Box::new(EditArticle))
        .register(Box::new(ApplyEdits))
        .register(Box::new(AcquireLock))
        .register(Box::new(ReleaseLock))
        .register(Box::new(Report));
    r
}

// ===========================================================================
// Theme lifecycle
// ===========================================================================

/// Creates a new theme directory (with an empty index).
pub struct CreateTheme;

#[derive(Deserialize)]
struct ThemeArgs {
    theme: String,
}

impl Tool for CreateTheme {
    fn name(&self) -> &str {
        "create_theme"
    }

    fn schema(&self) -> ReqTool {
        def(
            "create_theme",
            "Create a new theme (a top-level directory grouping related articles). \
             Fails if the theme already exists.",
            json!({
                "type": "object",
                "properties": { "theme": string_prop("The theme name (a single path segment).") },
                "required": ["theme"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ThemeArgs = parse_args(args)?;
        let theme = ctx.ws.create_theme(&a.theme)?;
        Ok(json!({ "created": theme.name }))
    }
}

/// Deletes a theme directory and everything in it.
pub struct DeleteTheme;

impl Tool for DeleteTheme {
    fn name(&self) -> &str {
        "delete_theme"
    }

    fn schema(&self) -> ReqTool {
        def(
            "delete_theme",
            "Delete a theme and all of its articles. This is irreversible.",
            json!({
                "type": "object",
                "properties": { "theme": string_prop("The theme name to delete.") },
                "required": ["theme"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ThemeArgs = parse_args(args)?;
        ctx.ws.delete_theme(&a.theme)?;
        Ok(json!({ "deleted": a.theme }))
    }
}

// ===========================================================================
// Article lifecycle
// ===========================================================================

/// Creates a new, empty article file inside a theme and records it in the index.
pub struct CreateArticle;

#[derive(Deserialize)]
struct CreateArticleArgs {
    theme: String,
    file_name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    notes: Option<String>,
}

impl Tool for CreateArticle {
    fn name(&self) -> &str {
        "create_article"
    }

    fn schema(&self) -> ReqTool {
        def(
            "create_article",
            "Create a new, empty article file inside a theme and add it to the \
             reading-order index. Fails if the article already exists.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name (a single path segment)."),
                    "title": string_prop("A human-readable title for the article."),
                    "notes": string_prop("Optional notes, e.g. the originating task."),
                },
                "required": ["theme", "file_name"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: CreateArticleArgs = parse_args(args)?;
        ctx.ws
            .create_article(&a.theme, &a.file_name, &a.title, a.notes)?;
        Ok(json!({ "created": format!("{}/{}", a.theme, a.file_name) }))
    }
}

/// Deletes an article file and removes it from the index.
pub struct DeleteArticle;

#[derive(Deserialize)]
struct ArticleRef {
    theme: String,
    file_name: String,
}

impl Tool for DeleteArticle {
    fn name(&self) -> &str {
        "delete_article"
    }

    fn schema(&self) -> ReqTool {
        def(
            "delete_article",
            "Delete an article file and remove it from the index. Fails if the \
             article is locked; release the lock first.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name to delete."),
                },
                "required": ["theme", "file_name"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ArticleRef = parse_args(args)?;
        ctx.ws.delete_article(&a.theme, &a.file_name)?;
        Ok(json!({ "deleted": format!("{}/{}", a.theme, a.file_name) }))
    }
}

/// Lists the article file names in a theme, in reading order.
pub struct ListArticles;

impl Tool for ListArticles {
    fn name(&self) -> &str {
        "list_articles"
    }

    fn schema(&self) -> ReqTool {
        def(
            "list_articles",
            "List the article file names in a theme, in reading order.",
            json!({
                "type": "object",
                "properties": { "theme": string_prop("The theme to list.") },
                "required": ["theme"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ThemeArgs = parse_args(args)?;
        let articles = ctx.ws.list_articles(&a.theme)?;
        Ok(json!({ "articles": articles }))
    }
}

// ===========================================================================
// Read / search
// ===========================================================================

/// Reads an article's full text.
pub struct ReadArticle;

impl Tool for ReadArticle {
    fn name(&self) -> &str {
        "read_article"
    }

    fn schema(&self) -> ReqTool {
        def(
            "read_article",
            "Read an article's full plain-text body. Refuses oversized or binary \
             files (adapt your strategy if so).",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name to read."),
                },
                "required": ["theme", "file_name"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ArticleRef = parse_args(args)?;
        let text = ctx.ws.read_article(&a.theme, &a.file_name)?;
        Ok(json!({ "text": text }))
    }
}

/// Searches the workspace for a substring, returning matching articles and lines.
pub struct Find;

#[derive(Deserialize)]
struct FindArgs {
    query: String,
    #[serde(default)]
    theme: Option<String>,
}

/// A single search hit returned by [`Find`].
#[derive(serde::Serialize)]
struct FindHit {
    theme: String,
    file_name: String,
    line: usize,
    text: String,
}

impl Tool for Find {
    fn name(&self) -> &str {
        "find"
    }

    fn schema(&self) -> ReqTool {
        def(
            "find",
            "Search the workspace (or a single theme) for a plain-text substring, \
             returning matching articles with line numbers. This is a local \
             workspace search, not a web search.",
            json!({
                "type": "object",
                "properties": {
                    "query": string_prop("The substring to search for (case-sensitive)."),
                    "theme": string_prop("Optional: restrict the search to one theme."),
                },
                "required": ["query"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: FindArgs = parse_args(args)?;
        if a.query.is_empty() {
            return Err(ToolError::InvalidArgs("query must not be empty".into()));
        }

        let themes: Vec<String> = match a.theme {
            Some(t) => vec![t],
            None => list_theme_names(ctx)?,
        };

        let mut hits = Vec::new();
        for theme in themes {
            // Skip a theme that does not exist (e.g. a stale name) rather than
            // failing the whole search.
            let Ok(articles) = ctx.ws.list_articles(&theme) else {
                continue;
            };
            for file_name in articles {
                // Skip unreadable (binary/oversized/missing) articles silently.
                let Ok(text) = ctx.ws.read_article(&theme, &file_name) else {
                    continue;
                };
                for (i, line) in text.lines().enumerate() {
                    if line.contains(&a.query) {
                        hits.push(FindHit {
                            theme: theme.clone(),
                            file_name: file_name.clone(),
                            line: i + 1,
                            text: line.to_string(),
                        });
                    }
                }
            }
        }

        Ok(json!({ "hits": hits }))
    }
}

/// Lists the theme directory names directly under the workspace root.
fn list_theme_names(ctx: &ToolCtx<'_>) -> Result<Vec<String>, ToolError> {
    let mut names = Vec::new();
    let entries = std::fs::read_dir(ctx.ws.root())
        .map_err(|e| ToolError::Io(format!("cannot list workspace root: {e}")))?;
    for entry in entries {
        let entry = entry.map_err(|e| ToolError::Io(e.to_string()))?;
        if entry.path().is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

// ===========================================================================
// Lock-guarded edits
// ===========================================================================

/// Overwrites an article's full text (the caller must hold the lock).
pub struct WriteArticle;

#[derive(Deserialize)]
struct WriteArticleArgs {
    theme: String,
    file_name: String,
    text: String,
}

impl Tool for WriteArticle {
    fn name(&self) -> &str {
        "write_article"
    }

    fn schema(&self) -> ReqTool {
        def(
            "write_article",
            "Replace an article's entire body with new text. You must hold the \
             article lock (call acquire_lock first).",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name to overwrite."),
                    "text": string_prop("The full new body of the article."),
                },
                "required": ["theme", "file_name", "text"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: WriteArticleArgs = parse_args(args)?;
        let writer = ctx.writer.clone();
        ctx.ws
            .write_article(&a.theme, &a.file_name, &a.text, &writer)?;
        Ok(json!({ "written": format!("{}/{}", a.theme, a.file_name), "bytes": a.text.len() }))
    }
}

/// Replaces a single, **unique** occurrence of `old` with `new` (caller holds
/// the lock).
pub struct EditArticle;

#[derive(Deserialize)]
struct EditArticleArgs {
    theme: String,
    file_name: String,
    old: String,
    new: String,
}

impl Tool for EditArticle {
    fn name(&self) -> &str {
        "edit_article"
    }

    fn schema(&self) -> ReqTool {
        def(
            "edit_article",
            "Replace one exact, unique occurrence of `old` with `new` in an \
             article. Fails if `old` is absent or occurs more than once — include \
             enough surrounding context to make it unique. You must hold the \
             article lock.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name to edit."),
                    "old": string_prop("The exact text to find. Must occur exactly once."),
                    "new": string_prop("The replacement text."),
                },
                "required": ["theme", "file_name", "old", "new"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: EditArticleArgs = parse_args(args)?;
        if a.old.is_empty() {
            return Err(ToolError::InvalidArgs("`old` must not be empty".into()));
        }
        let writer = ctx.writer.clone();
        let current = ctx.ws.read_article(&a.theme, &a.file_name)?;

        let count = current.matches(&a.old).count();
        match count {
            0 => {
                return Err(ToolError::InvalidArgs(format!(
                    "`old` not found in `{}/{}`",
                    a.theme, a.file_name
                )));
            }
            1 => {}
            n => {
                return Err(ToolError::InvalidArgs(format!(
                    "`old` is not unique in `{}/{}` ({n} occurrences); add more context",
                    a.theme, a.file_name
                )));
            }
        }

        let updated = current.replacen(&a.old, &a.new, 1);
        ctx.ws
            .write_article(&a.theme, &a.file_name, &updated, &writer)?;
        Ok(json!({ "edited": format!("{}/{}", a.theme, a.file_name) }))
    }
}

/// A single fine-grained edit operation in an [`ApplyEdits`] batch.
///
/// Operations are resolved against the article's text **as it was before the
/// batch began**, then applied together. Offset-based variants address bytes;
/// anchor-based variants locate a unique substring. Anchors must match exactly
/// once, offsets must lie on UTF-8 character boundaries, and spans must not
/// overlap — any violation rolls back the entire batch.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum EditOp {
    /// Insert `text` at byte offset `at`.
    Insert {
        /// Byte offset (on a char boundary, `0..=len`) to insert at.
        at: usize,
        /// The text to insert.
        text: String,
    },
    /// Insert `text` immediately before the unique anchor substring.
    InsertBefore {
        /// A substring occurring exactly once in the original text.
        anchor: String,
        /// The text to insert before the anchor.
        text: String,
    },
    /// Insert `text` immediately after the unique anchor substring.
    InsertAfter {
        /// A substring occurring exactly once in the original text.
        anchor: String,
        /// The text to insert after the anchor.
        text: String,
    },
    /// Delete the byte range `start..end`.
    Delete {
        /// Inclusive start byte offset (on a char boundary).
        start: usize,
        /// Exclusive end byte offset (on a char boundary, `>= start`).
        end: usize,
    },
    /// Delete the unique anchor substring.
    DeleteText {
        /// A substring occurring exactly once in the original text.
        anchor: String,
    },
    /// Replace the byte range `start..end` with `text`.
    Replace {
        /// Inclusive start byte offset (on a char boundary).
        start: usize,
        /// Exclusive end byte offset (on a char boundary, `>= start`).
        end: usize,
        /// The replacement text.
        text: String,
    },
    /// Replace the unique anchor substring with `text`.
    ReplaceText {
        /// A substring occurring exactly once in the original text.
        anchor: String,
        /// The replacement text.
        text: String,
    },
}

/// A normalized span edit: replace `start..end` (byte offsets into the original
/// text) with `replacement`. Every [`EditOp`] lowers to one of these.
struct Span {
    start: usize,
    end: usize,
    replacement: String,
}

/// Locates the unique byte range of `anchor` in `text`, erroring if it is absent
/// or occurs more than once.
fn unique_anchor(text: &str, anchor: &str) -> Result<(usize, usize), ToolError> {
    if anchor.is_empty() {
        return Err(ToolError::InvalidArgs("anchor must not be empty".into()));
    }
    let count = text.matches(anchor).count();
    match count {
        0 => Err(ToolError::InvalidArgs(format!(
            "anchor `{anchor}` not found"
        ))),
        1 => {
            let start = text.find(anchor).expect("counted one match");
            Ok((start, start + anchor.len()))
        }
        n => Err(ToolError::InvalidArgs(format!(
            "anchor `{anchor}` is not unique ({n} occurrences)"
        ))),
    }
}

/// Validates that `offset` is within `0..=len` and lands on a char boundary.
fn check_boundary(text: &str, offset: usize, what: &str) -> Result<(), ToolError> {
    if offset > text.len() {
        return Err(ToolError::InvalidArgs(format!(
            "{what} offset {offset} is past end of text ({} bytes)",
            text.len()
        )));
    }
    if !text.is_char_boundary(offset) {
        return Err(ToolError::InvalidArgs(format!(
            "{what} offset {offset} is not on a UTF-8 character boundary"
        )));
    }
    Ok(())
}

/// Lowers a single [`EditOp`] to a normalized [`Span`] against `text`.
fn lower_op(text: &str, op: EditOp) -> Result<Span, ToolError> {
    let span = match op {
        EditOp::Insert { at, text: t } => {
            check_boundary(text, at, "insert")?;
            Span {
                start: at,
                end: at,
                replacement: t,
            }
        }
        EditOp::InsertBefore { anchor, text: t } => {
            let (start, _) = unique_anchor(text, &anchor)?;
            Span {
                start,
                end: start,
                replacement: t,
            }
        }
        EditOp::InsertAfter { anchor, text: t } => {
            let (_, end) = unique_anchor(text, &anchor)?;
            Span {
                start: end,
                end,
                replacement: t,
            }
        }
        EditOp::Delete { start, end } => {
            check_boundary(text, start, "delete start")?;
            check_boundary(text, end, "delete end")?;
            if end < start {
                return Err(ToolError::InvalidArgs(format!(
                    "delete end {end} is before start {start}"
                )));
            }
            Span {
                start,
                end,
                replacement: String::new(),
            }
        }
        EditOp::DeleteText { anchor } => {
            let (start, end) = unique_anchor(text, &anchor)?;
            Span {
                start,
                end,
                replacement: String::new(),
            }
        }
        EditOp::Replace {
            start,
            end,
            text: t,
        } => {
            check_boundary(text, start, "replace start")?;
            check_boundary(text, end, "replace end")?;
            if end < start {
                return Err(ToolError::InvalidArgs(format!(
                    "replace end {end} is before start {start}"
                )));
            }
            Span {
                start,
                end,
                replacement: t,
            }
        }
        EditOp::ReplaceText { anchor, text: t } => {
            let (start, end) = unique_anchor(text, &anchor)?;
            Span {
                start,
                end,
                replacement: t,
            }
        }
    };
    Ok(span)
}

/// Applies a batch of [`EditOp`]s to `text` atomically.
///
/// Every op is lowered to a [`Span`] against the *original* `text`, the spans are
/// checked for overlap, then applied right-to-left so earlier offsets stay valid.
/// If any op fails to lower, or any two spans overlap, the whole batch is rejected
/// and `text` is left untouched (the caller never writes a partial result).
fn apply_edits_atomic(text: &str, ops: Vec<EditOp>) -> Result<String, ToolError> {
    if ops.is_empty() {
        return Err(ToolError::InvalidArgs("no edit operations supplied".into()));
    }

    // Lower every op first; a failure here aborts before any mutation.
    let mut spans: Vec<Span> = Vec::with_capacity(ops.len());
    for op in ops {
        spans.push(lower_op(text, op)?);
    }

    // Reject overlapping spans. Sort by start; a pure insertion (start == end)
    // sharing an endpoint with another span is allowed, but any range overlap is
    // not. Sorting also gives us right-to-left application order.
    spans.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
    for pair in spans.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        // Overlap when the next span starts strictly before the previous ends.
        if b.start < a.end {
            return Err(ToolError::InvalidArgs(format!(
                "edit operations overlap (span {}..{} and {}..{})",
                a.start, a.end, b.start, b.end
            )));
        }
    }

    // Apply right-to-left so offsets into the original text remain valid.
    let mut out = text.to_string();
    for span in spans.into_iter().rev() {
        out.replace_range(span.start..span.end, &span.replacement);
    }
    Ok(out)
}

/// Applies a batch of fine-grained edit operations to an article atomically.
pub struct ApplyEdits;

#[derive(Deserialize)]
struct ApplyEditsArgs {
    theme: String,
    file_name: String,
    edits: Vec<EditOp>,
}

impl Tool for ApplyEdits {
    fn name(&self) -> &str {
        "apply_edits"
    }

    fn schema(&self) -> ReqTool {
        def(
            "apply_edits",
            "Apply a batch of fine-grained edits to an article atomically. Each \
             edit is one of: insert (at a byte offset, or before/after a unique \
             anchor substring), delete (a byte range, or a unique anchor), or \
             replace (a byte range, or a unique anchor). All edits resolve against \
             the text as it was before the batch; if any edit fails (anchor not \
             unique, offset off a character boundary, spans overlap) the whole \
             batch is rolled back and nothing is written. You must hold the \
             article lock.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name to edit."),
                    "edits": {
                        "type": "array",
                        "description": "The edit operations to apply atomically.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "op": {
                                    "type": "string",
                                    "enum": [
                                        "insert", "insert_before", "insert_after",
                                        "delete", "delete_text", "replace", "replace_text"
                                    ],
                                    "description": "The operation kind."
                                },
                                "at": { "type": "integer", "description": "Byte offset for `insert`." },
                                "start": { "type": "integer", "description": "Start byte offset for `delete`/`replace`." },
                                "end": { "type": "integer", "description": "End byte offset (exclusive) for `delete`/`replace`." },
                                "anchor": string_prop("Unique substring for anchor-based ops."),
                                "text": string_prop("Inserted/replacement text where applicable."),
                            },
                            "required": ["op"],
                        },
                    },
                },
                "required": ["theme", "file_name", "edits"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ApplyEditsArgs = parse_args(args)?;
        let writer = ctx.writer.clone();
        let current = ctx.ws.read_article(&a.theme, &a.file_name)?;
        let updated = apply_edits_atomic(&current, a.edits)?;
        ctx.ws
            .write_article(&a.theme, &a.file_name, &updated, &writer)?;
        Ok(json!({ "edited": format!("{}/{}", a.theme, a.file_name) }))
    }
}

// ===========================================================================
// Locks
// ===========================================================================

/// Acquires the single-writer lock on an article for the calling writer.
pub struct AcquireLock;

impl Tool for AcquireLock {
    fn name(&self) -> &str {
        "acquire_lock"
    }

    fn schema(&self) -> ReqTool {
        def(
            "acquire_lock",
            "Acquire the single-writer lock on an article before editing it. \
             Fails if another writer holds it.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name to lock."),
                },
                "required": ["theme", "file_name"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ArticleRef = parse_args(args)?;
        let writer = ctx.writer.clone();
        ctx.ws.acquire_lock(&a.theme, &a.file_name, &writer)?;
        Ok(json!({ "locked": format!("{}/{}", a.theme, a.file_name) }))
    }
}

/// Releases the article lock held by the calling writer.
pub struct ReleaseLock;

impl Tool for ReleaseLock {
    fn name(&self) -> &str {
        "release_lock"
    }

    fn schema(&self) -> ReqTool {
        def(
            "release_lock",
            "Release the single-writer lock you hold on an article.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name to unlock."),
                },
                "required": ["theme", "file_name"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ArticleRef = parse_args(args)?;
        let writer = ctx.writer.clone();
        ctx.ws.release_lock(&a.theme, &a.file_name, &writer)?;
        Ok(json!({ "released": format!("{}/{}", a.theme, a.file_name) }))
    }
}

// ===========================================================================
// Reporting
// ===========================================================================

/// The slave's structured report tool: it terminates the writing loop with a
/// summary the master consumes.
///
/// The tool does not touch the workspace; it simply echoes the structured report
/// back as its result so the orchestration layer can read it off the final `tool`
/// message. The report fields mirror
/// [`SlaveReport`](crate::engine::SlaveReport).
pub struct Report;

#[derive(Deserialize)]
struct ReportArgs {
    status: String,
    summary: String,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    needs: Option<String>,
}

impl Tool for Report {
    fn name(&self) -> &str {
        "report"
    }

    fn schema(&self) -> ReqTool {
        def(
            "report",
            "Report your final result to the master and finish. Call this once the \
             article is done (or when you are blocked and need a human).",
            json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["done", "needs_human", "failed"],
                        "description": "The terminal status of your work.",
                    },
                    "summary": string_prop("A short summary of what you did."),
                    "result": string_prop("The concrete result (e.g. the article path or text)."),
                    "needs": string_prop("What you need next, if blocked."),
                },
                "required": ["status", "summary"],
            }),
        )
    }

    fn call(&self, args: Value, _ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ReportArgs = parse_args(args)?;
        match a.status.as_str() {
            "done" | "needs_human" | "failed" => {}
            other => {
                return Err(ToolError::InvalidArgs(format!(
                    "unknown report status `{other}` (expected done/needs_human/failed)"
                )));
            }
        }
        Ok(json!({
            "status": a.status,
            "summary": a.summary,
            "result": a.result,
            "needs": a.needs,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::workspace::{Workspace, WriterId};

    fn agent() -> WriterId {
        WriterId::Agent {
            model: "deepseek-v4-pro".to_string(),
            label: "s1".to_string(),
        }
    }

    /// Sets up a workspace with one theme and one article, returning the temp dir
    /// (kept alive) and an opened [`Workspace`].
    fn fixture() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        (dir, ws)
    }

    /// Dispatches one tool call by name against a context, returning its result.
    fn call(ws: &mut Workspace, tool: &dyn Tool, args: Value) -> ToolResult {
        let mut ctx = ToolCtx::new(ws, agent());
        tool.call(args, &mut ctx)
    }

    // ----- Schema generation ------------------------------------------------

    #[test]
    fn writing_tools_registers_full_set() {
        let r = writing_tools();
        let names: Vec<String> = r
            .definitions()
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        for expected in [
            "create_theme",
            "delete_theme",
            "create_article",
            "delete_article",
            "list_articles",
            "read_article",
            "find",
            "write_article",
            "edit_article",
            "apply_edits",
            "acquire_lock",
            "release_lock",
            "report",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        assert_eq!(r.len(), 13);
    }

    #[test]
    fn each_tool_schema_name_matches_tool_name() {
        let r = writing_tools();
        for d in r.definitions() {
            // Every schema must carry a parameters object.
            assert!(d.function.parameters.is_some());
        }
        // Name/schema agreement on a representative sample.
        assert_eq!(WriteArticle.name(), WriteArticle.schema().function.name);
        assert_eq!(ApplyEdits.name(), ApplyEdits.schema().function.name);
    }

    // ----- create/delete theme + article, index maintenance ----------------

    #[test]
    fn create_and_delete_theme_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mut ws = Workspace::open(dir.path()).unwrap();
        call(&mut ws, &CreateTheme, json!({ "theme": "x" })).unwrap();
        assert!(ws.load_index("x").is_ok());
        call(&mut ws, &DeleteTheme, json!({ "theme": "x" })).unwrap();
        assert!(matches!(ws.load_index("x"), Err(ToolError::NotFound(_))));
    }

    #[test]
    fn create_delete_list_articles_tools_maintain_index() {
        let (_d, mut ws) = fixture();
        call(
            &mut ws,
            &CreateArticle,
            json!({ "theme": "t", "file_name": "b.md", "title": "B" }),
        )
        .unwrap();
        let listed = call(&mut ws, &ListArticles, json!({ "theme": "t" })).unwrap();
        assert_eq!(listed["articles"], json!(["a.md", "b.md"]));

        call(
            &mut ws,
            &DeleteArticle,
            json!({ "theme": "t", "file_name": "a.md" }),
        )
        .unwrap();
        let listed = call(&mut ws, &ListArticles, json!({ "theme": "t" })).unwrap();
        assert_eq!(listed["articles"], json!(["b.md"]));
    }

    // ----- read / find ------------------------------------------------------

    #[test]
    fn read_article_tool_returns_text() {
        let (_d, mut ws) = fixture();
        ws.acquire_lock("t", "a.md", &agent()).unwrap();
        ws.write_article("t", "a.md", "body", &agent()).unwrap();
        let out = call(
            &mut ws,
            &ReadArticle,
            json!({ "theme": "t", "file_name": "a.md" }),
        )
        .unwrap();
        assert_eq!(out["text"], "body");
    }

    #[test]
    fn read_missing_article_errors() {
        let (_d, mut ws) = fixture();
        let err = call(
            &mut ws,
            &ReadArticle,
            json!({ "theme": "t", "file_name": "ghost.md" }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[test]
    fn find_tool_locates_matches_across_workspace() {
        let (_d, mut ws) = fixture();
        ws.acquire_lock("t", "a.md", &agent()).unwrap();
        ws.write_article("t", "a.md", "line one\nneedle here\nlast", &agent())
            .unwrap();
        ws.release_lock("t", "a.md", &agent()).unwrap();

        let out = call(&mut ws, &Find, json!({ "query": "needle" })).unwrap();
        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["line"], 2);
        assert_eq!(hits[0]["file_name"], "a.md");

        // No matches => empty.
        let out = call(&mut ws, &Find, json!({ "query": "absent" })).unwrap();
        assert!(out["hits"].as_array().unwrap().is_empty());

        // Empty query rejected.
        let err = call(&mut ws, &Find, json!({ "query": "" })).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    // ----- write / edit, lock enforcement -----------------------------------

    #[test]
    fn write_article_tool_requires_lock() {
        let (_d, mut ws) = fixture();
        let err = call(
            &mut ws,
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "x" }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Lock(_)));

        // With lock, succeeds.
        ws.acquire_lock("t", "a.md", &agent()).unwrap();
        call(
            &mut ws,
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "hi" }),
        )
        .unwrap();
        assert_eq!(ws.read_article("t", "a.md").unwrap(), "hi");
    }

    #[test]
    fn acquire_and_release_lock_tools() {
        let (_d, mut ws) = fixture();
        call(
            &mut ws,
            &AcquireLock,
            json!({ "theme": "t", "file_name": "a.md" }),
        )
        .unwrap();
        // Now writable.
        ws.write_article("t", "a.md", "x", &agent()).unwrap();
        call(
            &mut ws,
            &ReleaseLock,
            json!({ "theme": "t", "file_name": "a.md" }),
        )
        .unwrap();
        // After release, write fails.
        assert!(matches!(
            ws.write_article("t", "a.md", "y", &agent()),
            Err(ToolError::Lock(_))
        ));
    }

    #[test]
    fn edit_article_unique_match() {
        let (_d, mut ws) = fixture();
        ws.acquire_lock("t", "a.md", &agent()).unwrap();
        ws.write_article("t", "a.md", "the quick brown fox", &agent())
            .unwrap();
        call(
            &mut ws,
            &EditArticle,
            json!({ "theme": "t", "file_name": "a.md", "old": "quick", "new": "slow" }),
        )
        .unwrap();
        assert_eq!(ws.read_article("t", "a.md").unwrap(), "the slow brown fox");
    }

    #[test]
    fn edit_article_rejects_non_unique_and_absent() {
        let (_d, mut ws) = fixture();
        ws.acquire_lock("t", "a.md", &agent()).unwrap();
        ws.write_article("t", "a.md", "ab ab ab", &agent()).unwrap();
        // Non-unique.
        let err = call(
            &mut ws,
            &EditArticle,
            json!({ "theme": "t", "file_name": "a.md", "old": "ab", "new": "x" }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        // Absent.
        let err = call(
            &mut ws,
            &EditArticle,
            json!({ "theme": "t", "file_name": "a.md", "old": "zzz", "new": "x" }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        // Article unchanged after both failures.
        assert_eq!(ws.read_article("t", "a.md").unwrap(), "ab ab ab");
    }

    #[test]
    fn edit_article_requires_lock() {
        let (_d, mut ws) = fixture();
        // Seed content under a lock, then drop the lock so the match succeeds but
        // the write-back lock check is what fails.
        ws.acquire_lock("t", "a.md", &agent()).unwrap();
        ws.write_article("t", "a.md", "hello x world", &agent())
            .unwrap();
        ws.release_lock("t", "a.md", &agent()).unwrap();
        let err = call(
            &mut ws,
            &EditArticle,
            json!({ "theme": "t", "file_name": "a.md", "old": "x", "new": "y" }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Lock(_)));
    }

    // ----- apply_edits: correctness + atomic rollback -----------------------

    #[test]
    fn apply_edits_offset_ops() {
        // "hello world" -> insert, delete, replace by offset in one batch.
        let out = apply_edits_atomic(
            "hello world",
            vec![
                EditOp::Insert {
                    at: 0,
                    text: ">> ".into(),
                },
                EditOp::Replace {
                    start: 6,
                    end: 11,
                    text: "WORLD".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(out, ">> hello WORLD");
    }

    #[test]
    fn apply_edits_anchor_ops() {
        let out = apply_edits_atomic(
            "alpha beta gamma",
            vec![
                EditOp::InsertBefore {
                    anchor: "beta".into(),
                    text: "[".into(),
                },
                EditOp::InsertAfter {
                    anchor: "beta".into(),
                    text: "]".into(),
                },
                EditOp::ReplaceText {
                    anchor: "gamma".into(),
                    text: "G".into(),
                },
                EditOp::DeleteText {
                    anchor: "alpha ".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(out, "[beta] G");
    }

    #[test]
    fn apply_edits_rolls_back_on_failing_op() {
        let (_d, mut ws) = fixture();
        ws.acquire_lock("t", "a.md", &agent()).unwrap();
        ws.write_article("t", "a.md", "alpha beta", &agent())
            .unwrap();
        // Second op has a non-existent anchor -> whole batch must fail, file
        // unchanged.
        let err = call(
            &mut ws,
            &ApplyEdits,
            json!({
                "theme": "t", "file_name": "a.md",
                "edits": [
                    { "op": "replace_text", "anchor": "alpha", "text": "ALPHA" },
                    { "op": "delete_text", "anchor": "nonexistent" }
                ]
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        assert_eq!(ws.read_article("t", "a.md").unwrap(), "alpha beta");
    }

    #[test]
    fn apply_edits_rejects_overlapping_spans() {
        let err = apply_edits_atomic(
            "hello world",
            vec![
                EditOp::Replace {
                    start: 0,
                    end: 5,
                    text: "X".into(),
                },
                EditOp::Delete { start: 3, end: 8 },
            ],
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn apply_edits_rejects_non_unique_anchor() {
        let err = apply_edits_atomic(
            "ab ab",
            vec![EditOp::ReplaceText {
                anchor: "ab".into(),
                text: "x".into(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn apply_edits_rejects_offset_off_char_boundary() {
        // "é" is two bytes; offset 1 is mid-character.
        let err = apply_edits_atomic(
            "é",
            vec![EditOp::Insert {
                at: 1,
                text: "x".into(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn apply_edits_rejects_out_of_bounds_offset() {
        let err = apply_edits_atomic(
            "hi",
            vec![EditOp::Insert {
                at: 99,
                text: "x".into(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn apply_edits_rejects_empty_batch() {
        let err = apply_edits_atomic("hi", vec![]).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn apply_edits_requires_lock() {
        let (_d, mut ws) = fixture();
        ws.acquire_lock("t", "a.md", &agent()).unwrap();
        ws.write_article("t", "a.md", "alpha", &agent()).unwrap();
        ws.release_lock("t", "a.md", &agent()).unwrap();
        let err = call(
            &mut ws,
            &ApplyEdits,
            json!({
                "theme": "t", "file_name": "a.md",
                "edits": [ { "op": "replace_text", "anchor": "alpha", "text": "X" } ]
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Lock(_)));
    }

    // ----- report -----------------------------------------------------------

    #[test]
    fn report_tool_echoes_structured_report() {
        let (_d, mut ws) = fixture();
        let out = call(
            &mut ws,
            &Report,
            json!({ "status": "done", "summary": "wrote it", "result": "t/a.md" }),
        )
        .unwrap();
        assert_eq!(out["status"], "done");
        assert_eq!(out["summary"], "wrote it");
        assert_eq!(out["result"], "t/a.md");

        let err = call(
            &mut ws,
            &Report,
            json!({ "status": "bogus", "summary": "x" }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    // ----- argument validation + sandbox via tools --------------------------

    #[test]
    fn tools_reject_malformed_args() {
        let (_d, mut ws) = fixture();
        // Missing required field.
        let err = call(&mut ws, &ReadArticle, json!({ "theme": "t" })).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn tools_reject_sandbox_escape_in_name() {
        let (_d, mut ws) = fixture();
        let err = call(
            &mut ws,
            &ReadArticle,
            json!({ "theme": "t", "file_name": "../../etc/passwd" }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::SandboxViolation(_)));

        let err = call(&mut ws, &CreateTheme, json!({ "theme": "../evil" })).unwrap_err();
        assert!(matches!(err, ToolError::SandboxViolation(_)));
    }
}
