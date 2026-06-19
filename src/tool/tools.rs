//! The v0 native writing tools, plus the outward web/reference search tool.
//!
//! Each tool implements the [`Tool`] trait: it advertises a
//! JSON-schema [`req::Tool`](crate::req::Tool) definition and performs its work
//! against a [`ToolCtx`], reading and writing the workspace through the sandboxed
//! [`Workspace`](crate::tool::workspace::Workspace) API. Failures are returned as
//! [`ToolError`] values and surfaced back to the model as the content of a `tool`
//! reply, never aborting the session.
//!
//! The set mirrors `impl-v0.md` §3, with locking made implicit per edit by the
//! [`coordinator`](crate::coordinator) (kernel §6) — there are no longer any
//! model-facing `acquire_lock` / `release_lock` tools:
//!
//! | Tool | Purpose |
//! |---|---|
//! | [`CreateTheme`] / [`DeleteTheme`] | theme directories |
//! | [`CreateArticle`] / [`DeleteArticle`] / [`ListArticles`] | article files + index |
//! | [`ReadArticle`] / [`Find`] | read full text / substring-search the workspace |
//! | [`SearchTool`](crate::search::SearchTool) | outward web/reference search (kernel §10), via a pluggable backend |
//! | [`WriteArticle`] | full overwrite (one atomic commit) |
//! | [`EditArticle`] | exact unique `old` → `new` replace (one atomic commit) |
//! | [`ApplyEdits`] | fine-grained, atomic batch of offset/anchor ops (one commit) |
//! | [`SplitArticle`] / [`MergeArticles`] | cross-file split/merge (one commit, declared lock set) |
//! | [`ArticleHistory`] / [`ArticleDiff`] | git version history / unified diff |
//! | [`ArticleBlame`] | per-line authorship (`git blame`, kernel §9) |
//! | [`UndoLast`] | article-level undo (restore + re-commit) |
//! | [`Report`] | slave → master structured report |
//!
//! The three article editors route each edit through
//! [`Coordinator::submit`](crate::coordinator::Coordinator::submit) when a
//! coordinator is attached to the [`ToolCtx`] (the [`engine`](crate::engine)
//! shares one across the master and every slave): the declared lock set is the
//! article plus its theme `index.json`, acquired all-or-nothing, and the body and
//! manifest are committed together as **one** commit. Without a coordinator they
//! fall back to writing through the workspace and committing directly.
//!
//! The [`ArticleHistory`] / [`ArticleDiff`] / [`ArticleBlame`] / [`UndoLast`]
//! tools are backed by
//! the [`vcs`](crate::vcs) module; they read through the coordinator's exclusive
//! [`Vcs`](crate::vcs::Vcs) when one is attached, or a directly attached `Vcs`
//! otherwise, and return [`ToolError::Vcs`] when neither is enabled.
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

/// Builds the success payload for a lock-guarded editor, folding in `extra`
/// fields and the commit SHA when version control recorded one.
///
/// The `edited` field always names the article; `committed` carries the short
/// SHA when a [`Vcs`](crate::vcs::Vcs) was attached and the edit was committed,
/// and is absent (rather than `null`) when version control is disabled — so a
/// model running without a repository sees a clean, commit-free result.
fn commit_result(theme: &str, file_name: &str, sha: Option<String>, extra: Value) -> Value {
    let mut out = json!({ "edited": format!("{theme}/{file_name}") });
    if let (Some(map), Value::Object(extra)) = (out.as_object_mut(), extra) {
        map.extend(extra);
        if let Some(sha) = sha {
            map.insert("committed".to_string(), Value::String(sha));
        }
    }
    out
}

/// Registers all v0 native writing tools into a fresh
/// [`ToolRegistry`](crate::tool::ToolRegistry).
///
/// This is the tool set a slave session is configured with. It includes the
/// local substring [`Find`] **and** the outward web/reference
/// [`SearchTool`](crate::search::SearchTool) (kernel §10): the two are
/// orthogonal — `find` scans the on-disk workspace, `search` reaches outside it.
/// The search tool is wired with the no-network
/// [`StubProvider`](crate::search::StubProvider) by default (it answers with an
/// explicit "no search backend configured" result); a host that has connected an
/// MCP search server swaps in a real provider via [`writing_tools_with_search`].
///
/// # Examples
///
/// ```
/// use ai_write::tool::tools::writing_tools;
///
/// let registry = writing_tools();
/// assert!(registry.len() >= 17);
/// ```
pub fn writing_tools() -> crate::tool::ToolRegistry {
    writing_tools_with_search(crate::search::SearchTool::with_stub())
}

/// Registers all v0 native writing tools, using the supplied
/// [`SearchTool`](crate::search::SearchTool) for the outward search capability.
///
/// Use this instead of [`writing_tools`] when a real
/// [`SearchProvider`](crate::search::SearchProvider) (e.g. an adapter over a
/// session-connected MCP search server) is available, so the `search` tool is
/// live rather than the no-network stub. Everything else is identical.
///
/// # Examples
///
/// ```
/// use ai_write::search::SearchTool;
/// use ai_write::tool::tools::writing_tools_with_search;
///
/// let registry = writing_tools_with_search(SearchTool::with_stub());
/// assert!(registry.len() >= 17);
/// ```
pub fn writing_tools_with_search(search: crate::search::SearchTool) -> crate::tool::ToolRegistry {
    let mut r = crate::tool::ToolRegistry::new();
    r.register(Box::new(CreateTheme))
        .register(Box::new(DeleteTheme))
        .register(Box::new(CreateArticle))
        .register(Box::new(DeleteArticle))
        .register(Box::new(ListArticles))
        .register(Box::new(ReadArticle))
        .register(Box::new(Find))
        .register(Box::new(search))
        .register(Box::new(WriteArticle))
        .register(Box::new(EditArticle))
        .register(Box::new(ApplyEdits))
        .register(Box::new(SplitArticle))
        .register(Box::new(MergeArticles))
        .register(Box::new(ArticleHistory))
        .register(Box::new(ArticleDiff))
        .register(Box::new(ArticleBlame))
        .register(Box::new(UndoLast))
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
            "Replace an article's entire body with new text. Locking and version \
             control are automatic: each edit is one atomic commit.",
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
        let message = format!(
            "edit({}/{}): write_article ({} bytes)",
            a.theme,
            a.file_name,
            a.text.len()
        );
        let sha = ctx.commit_full_text(&a.theme, &a.file_name, &a.text, &message)?;
        Ok(commit_result(
            &a.theme,
            &a.file_name,
            sha,
            json!({ "bytes": a.text.len() }),
        ))
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
             enough surrounding context to make it unique. Locking and version \
             control are automatic: each edit is one atomic commit.",
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
        let message = format!(
            "edit({}/{}): edit_article (1 replacement)",
            a.theme, a.file_name
        );
        let sha = ctx.commit_full_text(&a.theme, &a.file_name, &updated, &message)?;
        Ok(commit_result(&a.theme, &a.file_name, sha, json!({})))
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
             batch is rolled back and nothing is written. Locking and version \
             control are automatic: each successful batch is one atomic commit.",
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
        let op_count = a.edits.len();
        let current = ctx.ws.read_article(&a.theme, &a.file_name)?;
        let updated = apply_edits_atomic(&current, a.edits)?;
        let message = format!(
            "edit({}/{}): apply_edits {} ops",
            a.theme, a.file_name, op_count
        );
        let sha = ctx.commit_full_text(&a.theme, &a.file_name, &updated, &message)?;
        Ok(commit_result(&a.theme, &a.file_name, sha, json!({})))
    }
}

// ===========================================================================
// Cross-file structural operations (split / merge)
// ===========================================================================

/// Maps a [`CoordError`](crate::coordinator::CoordError) to the [`ToolError`] the
/// model sees for a split/merge transaction.
fn coord_tool_error(err: crate::coordinator::CoordError) -> ToolError {
    use crate::coordinator::CoordError;
    match err {
        CoordError::Undeclared(path) => ToolError::InvalidArgs(format!(
            "operation touched an undeclared output path: {} (declare every output file up front)",
            path.display()
        )),
        CoordError::Workspace(e) => e,
        CoordError::Vcs(e) => ToolError::Vcs(e.to_string()),
        CoordError::Aborted(msg) => ToolError::InvalidArgs(msg),
    }
}

/// A single output article in a [`SplitArticle`] / [`MergeArticles`] request, as
/// the model supplies it.
#[derive(Deserialize)]
struct OutputArticleArgs {
    file_name: String,
    #[serde(default)]
    title: String,
    content: String,
    #[serde(default)]
    parent: Option<String>,
}

impl OutputArticleArgs {
    /// Lowers the model-supplied output into the coordinator's [`NewArticle`].
    fn into_new_article(self) -> crate::coordinator::NewArticle {
        crate::coordinator::NewArticle {
            file_name: self.file_name,
            title: self.title,
            content: self.content,
            parent: self.parent,
        }
    }
}

/// Splits one source article into several new articles in a single atomic commit.
pub struct SplitArticle;

#[derive(Deserialize)]
struct SplitArticleArgs {
    theme: String,
    source_file: String,
    source_content: String,
    outputs: Vec<OutputArticleArgs>,
}

impl Tool for SplitArticle {
    fn name(&self) -> &str {
        "split_article"
    }

    fn schema(&self) -> ReqTool {
        def(
            "split_article",
            "Split one source article into several new articles in a single atomic \
             commit. The source is rewritten to `source_content` (its retained \
             overview portion — pass its current text to keep it unchanged), and \
             each entry in `outputs` becomes a brand-new article with its own body, \
             title, and optional parent. You MUST enumerate every output file up \
             front: the set of files the operation touches (the source, each \
             output, and the theme index) is locked together and committed once. \
             An output that already exists, collides with the source, or is \
             duplicated is rejected.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the source and all outputs live in."),
                    "source_file": string_prop("The source article file name to split."),
                    "source_content": string_prop("The new body the source is rewritten to (its retained portion)."),
                    "outputs": {
                        "type": "array",
                        "description": "The new articles to carve out, in reading order. Declaring the full list up front is required.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "file_name": string_prop("The new article file name (a single path segment)."),
                                "title": string_prop("A human-readable title for the new article."),
                                "content": string_prop("The full body of the new article."),
                                "parent": string_prop("Optional parent article file name in the theme hierarchy."),
                            },
                            "required": ["file_name", "content"],
                        },
                    },
                },
                "required": ["theme", "source_file", "source_content", "outputs"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: SplitArticleArgs = parse_args(args)?;
        let Some(coord) = ctx.coord.clone() else {
            return Err(ToolError::Other(
                "split_article requires the transaction coordinator (it is a cross-file operation)"
                    .to_string(),
            ));
        };
        let outputs: Vec<crate::coordinator::NewArticle> = a
            .outputs
            .into_iter()
            .map(OutputArticleArgs::into_new_article)
            .collect();
        let output_names: Vec<String> = outputs.iter().map(|o| o.file_name.clone()).collect();
        let plan = crate::coordinator::SplitPlan {
            theme: a.theme.clone(),
            source_file: a.source_file.clone(),
            source_content: a.source_content,
            outputs,
        };
        let outcome = coord
            .split_article(ctx.writer.clone(), plan)
            .map_err(coord_tool_error)?;
        let mut out = json!({
            "split": format!("{}/{}", a.theme, a.source_file),
            "outputs": output_names,
        });
        if let (Some(map), Some(sha)) = (out.as_object_mut(), outcome.sha) {
            map.insert("committed".to_string(), Value::String(sha));
        }
        Ok(out)
    }
}

/// Merges several source articles into one target article in a single atomic
/// commit.
pub struct MergeArticles;

#[derive(Deserialize)]
struct MergeArticlesArgs {
    theme: String,
    sources: Vec<String>,
    target: OutputArticleArgs,
}

impl Tool for MergeArticles {
    fn name(&self) -> &str {
        "merge_articles"
    }

    fn schema(&self) -> ReqTool {
        def(
            "merge_articles",
            "Merge several source articles into one target article in a single \
             atomic commit. The `target` article is created with the merged body \
             you supply, then every `sources` article is deleted and removed from \
             the index. You MUST list every source plus the target up front: those \
             files (and the theme index) are locked together and committed once. \
             Requires at least two sources; a target that collides with a surviving \
             article is rejected (the target may reuse a source's name).",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the sources and target live in."),
                    "sources": {
                        "type": "array",
                        "description": "The source article file names to merge and then delete (at least two).",
                        "items": { "type": "string" },
                    },
                    "target": {
                        "type": "object",
                        "description": "The new article the merged content is written into.",
                        "properties": {
                            "file_name": string_prop("The target article file name."),
                            "title": string_prop("A human-readable title for the target article."),
                            "content": string_prop("The full merged body."),
                            "parent": string_prop("Optional parent article file name in the theme hierarchy."),
                        },
                        "required": ["file_name", "content"],
                    },
                },
                "required": ["theme", "sources", "target"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: MergeArticlesArgs = parse_args(args)?;
        let Some(coord) = ctx.coord.clone() else {
            return Err(ToolError::Other(
                "merge_articles requires the transaction coordinator (it is a cross-file operation)"
                    .to_string(),
            ));
        };
        let target_name = a.target.file_name.clone();
        let plan = crate::coordinator::MergePlan {
            theme: a.theme.clone(),
            sources: a.sources.clone(),
            target: a.target.into_new_article(),
        };
        let outcome = coord
            .merge_articles(ctx.writer.clone(), plan)
            .map_err(coord_tool_error)?;
        let mut out = json!({
            "merged": a.sources,
            "into": format!("{}/{}", a.theme, target_name),
        });
        if let (Some(map), Some(sha)) = (out.as_object_mut(), outcome.sha) {
            map.insert("committed".to_string(), Value::String(sha));
        }
        Ok(out)
    }
}

// ===========================================================================
// Version control (history / diff / undo)
// ===========================================================================

/// Maps a [`VcsError`](crate::vcs::VcsError) to the [`ToolError::Vcs`] the model
/// sees.
fn vcs_tool_error(err: crate::vcs::VcsError) -> ToolError {
    ToolError::Vcs(err.to_string())
}

/// Runs `op` against whichever version-control backend is attached: the shared
/// [`Coordinator`](crate::coordinator::Coordinator)'s exclusive
/// [`Vcs`](crate::vcs::Vcs) (preferred, so reads serialize with commits), or a
/// directly attached `Vcs`. Returns [`ToolError::Vcs`] when neither is present.
fn with_vcs<R>(
    ctx: &ToolCtx<'_>,
    op: impl FnOnce(&crate::vcs::Vcs) -> Result<R, crate::vcs::VcsError>,
) -> Result<R, ToolError> {
    if let Some(coord) = ctx.coord.as_ref() {
        return coord.with_vcs(op).map_err(vcs_tool_error);
    }
    if let Some(vcs) = ctx.vcs {
        return op(vcs).map_err(vcs_tool_error);
    }
    Err(ToolError::Vcs(
        "version control is not enabled for this workspace".to_string(),
    ))
}

/// Lists an article's commit history (newest first), backed by libgit2.
pub struct ArticleHistory;

impl Tool for ArticleHistory {
    fn name(&self) -> &str {
        "article_history"
    }

    fn schema(&self) -> ReqTool {
        def(
            "article_history",
            "List the version history of an article, newest first. Each entry has \
             a short commit id, the author (the writer who made the edit), the \
             commit message, and a Unix timestamp. Use the ids with `article_diff` \
             or to understand how the article evolved.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name."),
                },
                "required": ["theme", "file_name"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ArticleRef = parse_args(args)?;
        let rel = std::path::Path::new(&a.theme).join(&a.file_name);
        let history = with_vcs(ctx, |vcs| vcs.history(&rel))?;
        Ok(json!({ "history": history }))
    }
}

/// Renders a unified diff of an article between two versions, backed by libgit2.
pub struct ArticleDiff;

#[derive(Deserialize)]
struct ArticleDiffArgs {
    theme: String,
    file_name: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}

impl Tool for ArticleDiff {
    fn name(&self) -> &str {
        "article_diff"
    }

    fn schema(&self) -> ReqTool {
        def(
            "article_diff",
            "Show a unified diff of an article between two versions. `from` and \
             `to` are commit ids (from `article_history`), or `HEAD`, `HEAD~1`, … \
             Omit `from` to diff against the empty file (the full content as \
             additions); omit `to` to diff a committed version against the current \
             working file. An empty result means no change between the two.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name."),
                    "from": string_prop("The base revision (commit id / HEAD~n). Optional."),
                    "to": string_prop("The target revision (commit id / HEAD). Optional; defaults to the working file."),
                },
                "required": ["theme", "file_name"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ArticleDiffArgs = parse_args(args)?;
        let rel = std::path::Path::new(&a.theme).join(&a.file_name);
        let patch = with_vcs(ctx, |vcs| {
            vcs.diff(&rel, a.from.as_deref(), a.to.as_deref())
        })?;
        Ok(json!({ "diff": patch }))
    }
}

/// Reports per-line authorship of an article (`git blame`), backed by libgit2.
pub struct ArticleBlame;

impl Tool for ArticleBlame {
    fn name(&self) -> &str {
        "article_blame"
    }

    fn schema(&self) -> ReqTool {
        def(
            "article_blame",
            "Show line-by-line authorship of an article (git blame). Returns one \
             entry per line of the article's last committed version, each with a \
             1-based line number, the author (the writer who last touched that \
             line — `human …` or `<model>/<label> …`), and the short commit id. \
             Use it to see who wrote which part. Reflects committed content only; \
             uncommitted edits are not attributed.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name."),
                },
                "required": ["theme", "file_name"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ArticleRef = parse_args(args)?;
        let rel = std::path::Path::new(&a.theme).join(&a.file_name);
        let blame = with_vcs(ctx, |vcs| vcs.blame(&rel))?;
        Ok(json!({ "blame": blame }))
    }
}

/// Reverts an article to its previous committed version, recording the revert as
/// a new commit (article-level undo).
pub struct UndoLast;

impl Tool for UndoLast {
    fn name(&self) -> &str {
        "undo_last"
    }

    fn schema(&self) -> ReqTool {
        def(
            "undo_last",
            "Undo the most recent edit to an article: restore its previous \
             committed version and record that restoration as a new commit \
             (history is preserved, never rewritten). Returns the new commit id, \
             or reports that there was nothing to undo when the article has only \
             one version. Locking is automatic.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name to undo."),
                },
                "required": ["theme", "file_name"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ArticleRef = parse_args(args)?;
        let writer = ctx.writer.clone();
        // Undo writes the article and commits the revert; with a coordinator
        // attached this runs through its exclusive Vcs (serialized with every
        // other commit), so locking is implicit — no explicit lock is needed.
        let rel = std::path::Path::new(&a.theme).join(&a.file_name);
        let reverted = with_vcs(ctx, |vcs| vcs.undo_last(&rel, &writer))?;
        match reverted {
            Some(sha) => Ok(json!({
                "undone": format!("{}/{}", a.theme, a.file_name),
                "committed": sha,
            })),
            None => Ok(json!({
                "undone": false,
                "reason": "nothing to undo (article has only one version)",
            })),
        }
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
            "search",
            "write_article",
            "edit_article",
            "apply_edits",
            "split_article",
            "merge_articles",
            "article_history",
            "article_diff",
            "article_blame",
            "undo_last",
            "report",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        // The explicit lock tools were removed: locking is now implicit per edit
        // through the coordinator (kernel §6).
        assert!(!names.contains(&"acquire_lock".to_string()));
        assert!(!names.contains(&"release_lock".to_string()));
        // 17 local writing tools + the outward `search` tool (kernel §10).
        assert_eq!(r.len(), 18);
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

    // ----- vcs integration: edits commit, history/diff/undo -----------------

    use crate::vcs::Vcs;

    /// Sets up a workspace **and** a [`Vcs`] over the same temp-dir root, with one
    /// theme and one locked article ready to edit. Returns the temp dir (kept
    /// alive), the workspace, and the vcs handle.
    fn vcs_fixture() -> (tempfile::TempDir, Workspace, Vcs) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");
        let vcs = Vcs::open_or_init(ws.root()).expect("open_or_init");
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        ws.acquire_lock("t", "a.md", &agent()).unwrap();
        (dir, ws, vcs)
    }

    /// Dispatches one tool call against a context carrying a [`Vcs`].
    fn call_vcs(ws: &mut Workspace, vcs: &Vcs, tool: &dyn Tool, args: Value) -> ToolResult {
        let mut ctx = ToolCtx::new(ws, agent()).with_vcs(vcs);
        tool.call(args, &mut ctx)
    }

    #[test]
    fn write_article_produces_one_commit_with_the_right_author() {
        let (_d, mut ws, vcs) = vcs_fixture();
        let out = call_vcs(
            &mut ws,
            &vcs,
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "first draft" }),
        )
        .unwrap();
        // The result advertises the commit it produced.
        let sha = out["committed"].as_str().expect("committed sha");
        assert_eq!(sha.len(), 10);

        // Exactly one commit touched the article, authored by the agent identity.
        let hist = vcs.history(std::path::Path::new("t/a.md")).unwrap();
        assert_eq!(hist.len(), 1, "one edit must be one commit");
        assert_eq!(hist[0].id, sha);
        assert_eq!(hist[0].author, "deepseek-v4-pro/s1 <agent@ai-write.local>");
        assert!(hist[0].message.contains("write_article"));
    }

    #[test]
    fn index_json_is_committed_in_the_same_commit_as_the_article() {
        let (_d, mut ws, vcs) = vcs_fixture();
        let out = call_vcs(
            &mut ws,
            &vcs,
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "body" }),
        )
        .unwrap();
        let sha = out["committed"].as_str().expect("committed sha");

        // The theme index (which records the new contributor) is versioned in the
        // SAME single commit as the body — one cognitive unit is one commit, not
        // two (the pre-coordinator behaviour committed body and index separately).
        let article_hist = vcs.history(std::path::Path::new("t/a.md")).unwrap();
        let index_hist = vcs.history(std::path::Path::new("t/index.json")).unwrap();
        assert_eq!(article_hist.len(), 1, "article touched by one commit");
        assert_eq!(index_hist.len(), 1, "index.json touched by one commit");
        assert_eq!(
            article_hist[0].id, index_hist[0].id,
            "body and index share the same single commit"
        );
        assert_eq!(article_hist[0].id, sha);
    }

    #[test]
    fn apply_edits_message_records_op_count() {
        let (_d, mut ws, vcs) = vcs_fixture();
        // Seed initial content (commit 1).
        call_vcs(
            &mut ws,
            &vcs,
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "alpha beta gamma" }),
        )
        .unwrap();
        // Two-op batch (commit 2).
        call_vcs(
            &mut ws,
            &vcs,
            &ApplyEdits,
            json!({
                "theme": "t", "file_name": "a.md",
                "edits": [
                    { "op": "replace_text", "anchor": "alpha", "text": "A" },
                    { "op": "replace_text", "anchor": "gamma", "text": "G" }
                ]
            }),
        )
        .unwrap();
        let hist = vcs.history(std::path::Path::new("t/a.md")).unwrap();
        assert_eq!(hist.len(), 2);
        assert!(
            hist[0].message.contains("apply_edits 2 ops"),
            "message was: {}",
            hist[0].message
        );
        assert_eq!(ws.read_article("t", "a.md").unwrap(), "A beta G");
    }

    #[test]
    fn edits_without_vcs_skip_commit_but_still_write() {
        // The no-vcs `call` helper omits the Vcs, so the result has no `committed`
        // field and the edit still lands on disk (v0 behaviour preserved).
        let (_d, mut ws) = fixture();
        ws.acquire_lock("t", "a.md", &agent()).unwrap();
        let out = call(
            &mut ws,
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "no git here" }),
        )
        .unwrap();
        assert!(out.get("committed").is_none());
        assert_eq!(ws.read_article("t", "a.md").unwrap(), "no git here");
    }

    #[test]
    fn article_history_tool_lists_commits_newest_first() {
        let (_d, mut ws, vcs) = vcs_fixture();
        for text in ["v1", "v2", "v3"] {
            call_vcs(
                &mut ws,
                &vcs,
                &WriteArticle,
                json!({ "theme": "t", "file_name": "a.md", "text": text }),
            )
            .unwrap();
        }
        let out = call_vcs(
            &mut ws,
            &vcs,
            &ArticleHistory,
            json!({ "theme": "t", "file_name": "a.md" }),
        )
        .unwrap();
        let entries = out["history"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        // Newest first; the most recent commit message mentions write_article.
        assert!(
            entries[0]["message"]
                .as_str()
                .unwrap()
                .contains("write_article")
        );
    }

    #[test]
    fn article_diff_tool_shows_change_between_versions() {
        let (_d, mut ws, vcs) = vcs_fixture();
        call_vcs(
            &mut ws,
            &vcs,
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "line one\n" }),
        )
        .unwrap();
        call_vcs(
            &mut ws,
            &vcs,
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "line two\n" }),
        )
        .unwrap();
        // Diff between the two article commits, located via history. Each edit is
        // now exactly one commit (body + index together), so the two entries are
        // the two article versions.
        let hist = vcs.history(std::path::Path::new("t/a.md")).unwrap();
        let (newest, prior) = (&hist[0].id, &hist[1].id);
        let out = call_vcs(
            &mut ws,
            &vcs,
            &ArticleDiff,
            json!({ "theme": "t", "file_name": "a.md", "from": prior, "to": newest }),
        )
        .unwrap();
        let patch = out["diff"].as_str().unwrap();
        assert!(patch.contains("-line one"), "patch: {patch}");
        assert!(patch.contains("+line two"), "patch: {patch}");
    }

    #[test]
    fn article_blame_tool_attributes_each_line_to_its_writer() {
        // Kernel §9: the blame tool must report per-line authorship that
        // distinguishes a human edit from a model edit. Build a two-writer
        // history through the editors, then assert the tool's output.
        let (_d, mut ws, vcs) = vcs_fixture();
        // v1: the agent (vcs_fixture's writer) writes a three-line draft.
        call_vcs(
            &mut ws,
            &vcs,
            &WriteArticle,
            json!({
                "theme": "t",
                "file_name": "a.md",
                "text": "agent first\nagent second\nagent third\n",
            }),
        )
        .unwrap();
        // v2: a human revises only the middle line. Hand the article lock over
        // from the fixture's agent to the human first (the editors require the
        // caller to hold the lock).
        ws.release_lock("t", "a.md", &agent()).unwrap();
        ws.acquire_lock("t", "a.md", &WriterId::Human).unwrap();
        {
            let mut ctx = ToolCtx::new(&mut ws, WriterId::Human).with_vcs(&vcs);
            WriteArticle
                .call(
                    json!({
                        "theme": "t",
                        "file_name": "a.md",
                        "text": "agent first\nhuman revised\nagent third\n",
                    }),
                    &mut ctx,
                )
                .unwrap();
        }
        // Restore the agent's lock so the final `call_vcs` (agent writer) blame
        // read does not trip the single-writer lock.
        ws.release_lock("t", "a.md", &WriterId::Human).unwrap();
        ws.acquire_lock("t", "a.md", &agent()).unwrap();

        let out = call_vcs(
            &mut ws,
            &vcs,
            &ArticleBlame,
            json!({ "theme": "t", "file_name": "a.md" }),
        )
        .unwrap();
        let lines = out["blame"].as_array().unwrap();
        assert_eq!(lines.len(), 3, "one entry per line");

        let author = |i: usize| lines[i]["author"].as_str().unwrap();
        let line_no = |i: usize| lines[i]["line_no"].as_u64().unwrap();

        // Line 1 & 3: untouched by the human → still the agent's commit.
        assert_eq!(line_no(0), 1);
        assert_eq!(author(0), "deepseek-v4-pro/s1 <agent@ai-write.local>");
        // Line 2: the human's edit.
        assert_eq!(line_no(1), 2);
        assert_eq!(author(1), "human <human@ai-write.local>");
        assert_eq!(line_no(2), 3);
        assert_eq!(author(2), "deepseek-v4-pro/s1 <agent@ai-write.local>");
    }

    #[test]
    fn undo_last_tool_reverts_to_previous_version() {
        let (_d, mut ws, vcs) = vcs_fixture();
        call_vcs(
            &mut ws,
            &vcs,
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "original\n" }),
        )
        .unwrap();
        call_vcs(
            &mut ws,
            &vcs,
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "changed\n" }),
        )
        .unwrap();
        assert_eq!(ws.read_article("t", "a.md").unwrap(), "changed\n");

        let out = call_vcs(
            &mut ws,
            &vcs,
            &UndoLast,
            json!({ "theme": "t", "file_name": "a.md" }),
        )
        .unwrap();
        assert!(out["committed"].is_string());
        // Content reverted on disk; history grew (revert is itself a commit).
        assert_eq!(ws.read_article("t", "a.md").unwrap(), "original\n");
        let hist = vcs.history(std::path::Path::new("t/a.md")).unwrap();
        assert_eq!(hist.len(), 3, "undo restores then re-commits");
    }

    #[test]
    fn undo_last_tool_needs_no_explicit_lock() {
        // Locking is now implicit: undo no longer requires the caller to hold an
        // explicit lock. Two committed versions, then undo, succeeds even with the
        // workspace lock released.
        let (_d, mut ws, vcs) = vcs_fixture();
        for text in ["one\n", "two\n"] {
            call_vcs(
                &mut ws,
                &vcs,
                &WriteArticle,
                json!({ "theme": "t", "file_name": "a.md", "text": text }),
            )
            .unwrap();
        }
        ws.release_lock("t", "a.md", &agent()).unwrap();
        let out = call_vcs(
            &mut ws,
            &vcs,
            &UndoLast,
            json!({ "theme": "t", "file_name": "a.md" }),
        )
        .unwrap();
        assert!(out["committed"].is_string());
        assert_eq!(ws.read_article("t", "a.md").unwrap(), "one\n");
    }

    #[test]
    fn undo_last_tool_with_single_version_reports_nothing_to_undo() {
        let (_d, mut ws, vcs) = vcs_fixture();
        call_vcs(
            &mut ws,
            &vcs,
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "only\n" }),
        )
        .unwrap();
        let out = call_vcs(
            &mut ws,
            &vcs,
            &UndoLast,
            json!({ "theme": "t", "file_name": "a.md" }),
        )
        .unwrap();
        assert_eq!(out["undone"], json!(false));
    }

    #[test]
    fn history_tools_error_when_vcs_disabled() {
        // Without a Vcs or coordinator attached, the version-control tools surface
        // ToolError::Vcs.
        let (_d, mut ws) = fixture();
        let err = call(
            &mut ws,
            &ArticleHistory,
            json!({ "theme": "t", "file_name": "a.md" }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Vcs(_)));
    }

    // ----- coordinator-routed edits (implicit locking, single commit) -------

    use std::sync::Arc;

    use crate::coordinator::Coordinator;

    /// Dispatches one tool call against a context carrying a shared
    /// [`Coordinator`], with a fresh per-thread workspace handle for read tools.
    fn call_coord(
        ws: &mut Workspace,
        coord: Arc<Coordinator>,
        tool: &dyn Tool,
        args: Value,
    ) -> ToolResult {
        let mut ctx = ToolCtx::new(ws, agent()).with_coordinator(coord);
        tool.call(args, &mut ctx)
    }

    #[test]
    fn coordinator_routed_write_commits_body_and_index_as_one_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let coord = Arc::new(Coordinator::open(dir.path()).expect("coordinator"));
        coord
            .with_workspace(|ws| {
                ws.create_theme("t")?;
                ws.create_article("t", "a.md", "A", None)
            })
            .unwrap();
        // A second, read-only workspace handle stands in for the session's own.
        let mut ws = Workspace::open(dir.path()).expect("reopen");

        // No explicit lock acquired anywhere: the coordinator locks implicitly.
        let out = call_coord(
            &mut ws,
            Arc::clone(&coord),
            &WriteArticle,
            json!({ "theme": "t", "file_name": "a.md", "text": "coordinated body" }),
        )
        .unwrap();
        let sha = out["committed"]
            .as_str()
            .expect("committed sha")
            .to_string();

        // The body and index landed in exactly ONE commit, via the coordinator's
        // single Vcs.
        coord
            .with_vcs(|vcs| {
                let hist_a = vcs.history(std::path::Path::new("t/a.md"))?;
                let hist_i = vcs.history(std::path::Path::new("t/index.json"))?;
                assert_eq!(hist_a.len(), 1, "article: one commit");
                assert_eq!(hist_i.len(), 1, "index: one commit");
                assert_eq!(hist_a[0].id, hist_i[0].id, "same single commit");
                assert_eq!(hist_a[0].id, sha);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            coord
                .with_workspace(|ws| ws.read_article("t", "a.md"))
                .unwrap(),
            "coordinated body"
        );
    }

    #[test]
    fn coordinator_routed_history_reads_through_the_coordinator() {
        let dir = tempfile::tempdir().expect("tempdir");
        let coord = Arc::new(Coordinator::open(dir.path()).expect("coordinator"));
        coord
            .with_workspace(|ws| {
                ws.create_theme("t")?;
                ws.create_article("t", "a.md", "A", None)
            })
            .unwrap();
        let mut ws = Workspace::open(dir.path()).expect("reopen");

        for text in ["v1", "v2"] {
            call_coord(
                &mut ws,
                Arc::clone(&coord),
                &WriteArticle,
                json!({ "theme": "t", "file_name": "a.md", "text": text }),
            )
            .unwrap();
        }
        // The history tool reads through the coordinator's Vcs (no direct Vcs set).
        let out = call_coord(
            &mut ws,
            Arc::clone(&coord),
            &ArticleHistory,
            json!({ "theme": "t", "file_name": "a.md" }),
        )
        .unwrap();
        assert_eq!(out["history"].as_array().unwrap().len(), 2);
    }

    // ----- split / merge native tools (G5) ----------------------------------

    /// Opens a coordinator over a fresh temp dir, creates theme `t` with the named
    /// articles, and returns the temp dir (kept alive), the shared coordinator, and
    /// a second read-only workspace handle for tool dispatch.
    fn split_merge_fixture(articles: &[&str]) -> (tempfile::TempDir, Arc<Coordinator>, Workspace) {
        let dir = tempfile::tempdir().expect("tempdir");
        let coord = Arc::new(Coordinator::open(dir.path()).expect("coordinator"));
        coord
            .with_workspace(|ws| {
                ws.create_theme("t")?;
                for a in articles {
                    ws.create_article("t", a, a, None)?;
                }
                Ok(())
            })
            .unwrap();
        let ws = Workspace::open(dir.path()).expect("reopen");
        (dir, coord, ws)
    }

    #[test]
    fn split_article_tool_commits_once_and_updates_manifest() {
        let (_d, coord, mut ws) = split_merge_fixture(&["all.md"]);
        let out = call_coord(
            &mut ws,
            Arc::clone(&coord),
            &SplitArticle,
            json!({
                "theme": "t",
                "source_file": "all.md",
                "source_content": "intro\n",
                "outputs": [
                    { "file_name": "a.md", "title": "A", "content": "a\n", "parent": "all.md" },
                    { "file_name": "b.md", "title": "B", "content": "b\n", "parent": "all.md" }
                ]
            }),
        )
        .unwrap();
        let sha = out["committed"]
            .as_str()
            .expect("committed sha")
            .to_string();
        assert_eq!(out["outputs"], json!(["a.md", "b.md"]));

        // One commit; source + both new files + index all share it.
        coord
            .with_vcs(|vcs| {
                for rel in ["t/all.md", "t/a.md", "t/b.md", "t/index.json"] {
                    assert_eq!(vcs.history(std::path::Path::new(rel)).unwrap()[0].id, sha);
                }
                Ok(())
            })
            .unwrap();

        // Manifest reflects the new hierarchy.
        let outline = coord.with_workspace(|w| w.article_outline("t")).unwrap();
        let files: Vec<&str> = outline.iter().map(|o| o.file.as_str()).collect();
        assert_eq!(files, vec!["all.md", "a.md", "b.md"]);
        assert_eq!(outline[1].parent.as_deref(), Some("all.md"));
    }

    #[test]
    fn merge_articles_tool_commits_once_and_updates_manifest() {
        let (_d, coord, mut ws) = split_merge_fixture(&["a.md", "b.md"]);
        let out = call_coord(
            &mut ws,
            Arc::clone(&coord),
            &MergeArticles,
            json!({
                "theme": "t",
                "sources": ["a.md", "b.md"],
                "target": { "file_name": "merged.md", "title": "Merged", "content": "ab\n" }
            }),
        )
        .unwrap();
        let sha = out["committed"]
            .as_str()
            .expect("committed sha")
            .to_string();
        assert_eq!(out["into"], "t/merged.md");

        coord
            .with_vcs(|vcs| {
                assert_eq!(
                    vcs.history(std::path::Path::new("t/merged.md")).unwrap()[0].id,
                    sha
                );
                assert_eq!(
                    vcs.history(std::path::Path::new("t/index.json")).unwrap()[0].id,
                    sha
                );
                Ok(())
            })
            .unwrap();

        let files = coord.with_workspace(|w| w.list_articles("t")).unwrap();
        assert_eq!(files, vec!["merged.md"]);
    }

    #[test]
    fn split_article_tool_rejects_duplicate_output() {
        let (_d, coord, mut ws) = split_merge_fixture(&["all.md"]);
        let err = call_coord(
            &mut ws,
            Arc::clone(&coord),
            &SplitArticle,
            json!({
                "theme": "t",
                "source_file": "all.md",
                "source_content": "x",
                "outputs": [
                    { "file_name": "a.md", "content": "1" },
                    { "file_name": "a.md", "content": "2" }
                ]
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "got {err:?}");
    }

    #[test]
    fn split_merge_tools_require_a_coordinator() {
        // Without a coordinator attached the cross-file tools refuse: they are
        // inherently multi-file transactions that must funnel through one.
        let (_d, mut ws) = fixture();
        let err = call(
            &mut ws,
            &SplitArticle,
            json!({
                "theme": "t",
                "source_file": "a.md",
                "source_content": "x",
                "outputs": [ { "file_name": "b.md", "content": "y" } ]
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Other(_)), "got {err:?}");

        let err = call(
            &mut ws,
            &MergeArticles,
            json!({
                "theme": "t",
                "sources": ["a.md", "x.md"],
                "target": { "file_name": "m.md", "content": "y" }
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Other(_)), "got {err:?}");
    }
}
