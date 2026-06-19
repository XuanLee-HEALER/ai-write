//! Web / reference **search** as a pluggable capability (kernel §10).
//!
//! The kernel places search in the tool layer's *periphery*: it is a capability,
//! "搜索（经第三方 MCP）" — search via a third-party MCP backend — and is
//! deliberately swappable without disturbing any of the kernel's nine sections.
//! This module honours that by drawing a hard line between *the contract* and
//! *the backend*:
//!
//! - [`SearchProvider`] is the contract: a query goes in, [`SearchResults`] come
//!   out. It is the single seam every backend implements.
//! - [`StubProvider`] is the always-available, **no-network** default: it answers
//!   every query with an explicit "no search backend configured" result so the
//!   library — and its test suite — never reaches the network, yet the search
//!   *tool* is still present and well-formed for the model to call.
//! - A real backend (an MCP search server connected to the running session) is
//!   plugged in by implementing [`SearchProvider`] over that connection; see
//!   [the integration guide](#plugging-in-an-mcp-search-backend) below.
//!
//! Search is **orthogonal to the local [`find`] tool**. `find` is a
//! substring scan of the on-disk workspace (the writer's own articles);
//! [`SearchTool`] reaches *outward* for web / reference material. They share no
//! state, never shadow each other, and a session may carry one, both, or neither.
//!
//! [`find`]: crate::tool::tools::Find
//! [`SearchTool`]: crate::search::SearchTool
//!
//! # Plugging in an MCP search backend
//!
//! The MCP wiring is environment-specific (which server, which transport, which
//! auth), so this crate ships only the contract and the stub. To connect a real
//! backend, implement [`SearchProvider`] over your session-connected MCP client
//! and hand it to [`SearchTool::new`]:
//!
//! ```
//! use std::sync::Arc;
//! use ai_write::search::{SearchError, SearchHit, SearchProvider, SearchResults, SearchTool};
//!
//! // An adapter over whatever MCP client the host session exposes. The real
//! // implementation would forward `query` to the connected MCP search server's
//! // `search`/`web_search` tool and map its JSON results into `SearchHit`s.
//! struct McpSearch {
//!     // mcp: Arc<SomeMcpClient>,   // the session-connected MCP handle
//! }
//!
//! impl SearchProvider for McpSearch {
//!     fn name(&self) -> &str {
//!         "mcp"
//!     }
//!
//!     fn search(&self, query: &str, limit: usize) -> Result<SearchResults, SearchError> {
//!         // let raw = self.mcp.call("web_search", json!({ "query": query }))
//!         //     .map_err(|e| SearchError::Backend(e.to_string()))?;
//!         // let hits = raw.results.into_iter().take(limit).map(|r| SearchHit {
//!         //     title: r.title, url: r.url, snippet: r.snippet,
//!         // }).collect();
//!         // Ok(SearchResults::found(query, hits))
//!         let _ = (query, limit);
//!         Ok(SearchResults::found(query, Vec::<SearchHit>::new()))
//!     }
//! }
//!
//! let tool = SearchTool::new(Arc::new(McpSearch {}));
//! assert_eq!(tool.provider_name(), "mcp");
//! ```
//!
//! Until such a backend is wired, [`SearchTool::with_stub`] gives a tool whose
//! contract and schema are identical but whose every answer is the documented
//! unconfigured result — so prompts, schemas, and tests can treat search as
//! always present.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::req::{FunctionDef, Tool as ReqTool};
use crate::tool::{Tool, ToolCtx, ToolError, ToolResult};

/// The default cap on the number of [`SearchHit`]s a query returns when the
/// caller does not request a specific `limit`.
pub const DEFAULT_LIMIT: usize = 5;

/// The hard upper bound on a query's `limit`, applied even when a larger value
/// is requested, to keep a single tool reply bounded.
pub const MAX_LIMIT: usize = 25;

/// An error raised while serving a search query.
///
/// Kept deliberately small: the stub never errors, and a real backend collapses
/// transport / protocol / auth failures into [`SearchError::Backend`] so the
/// search tool can surface them to the model uniformly as a recoverable
/// [`ToolError`].
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum SearchError {
    /// The query was empty or otherwise unusable.
    #[error("invalid search query: {0}")]
    InvalidQuery(String),
    /// The underlying search backend (e.g. the connected MCP server) failed.
    /// The message is the rendered backend error.
    #[error("search backend failed: {0}")]
    Backend(String),
}

/// A single search result: a titled, linked snippet of external material.
///
/// The shape is intentionally backend-agnostic — title, URL, and a short
/// snippet are the common denominator of web and reference search results — so
/// any [`SearchProvider`] can map its native results into it without loss of the
/// fields a writer actually needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The result's display title.
    pub title: String,
    /// The result's canonical URL (or other locator the backend uses).
    pub url: String,
    /// A short extract or summary of the result's content.
    pub snippet: String,
}

/// The outcome of a search: the echoed query, whether a real backend served it,
/// and the ranked [`SearchHit`]s.
///
/// The [`configured`](SearchResults::configured) flag is the key signal: when it
/// is `false` (the [`StubProvider`] answered), `hits` is empty and `note`
/// explains that no search backend is wired. A model reading the tool reply can
/// therefore tell "no results found" apart from "search is not available here".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResults {
    /// The query these results answer (echoed back for traceability).
    pub query: String,
    /// `true` when a real search backend served the query; `false` when the
    /// no-network [`StubProvider`] answered.
    pub configured: bool,
    /// The ranked results. Always empty when `configured` is `false`.
    pub hits: Vec<SearchHit>,
    /// An optional human-readable note (e.g. the unconfigured explanation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl SearchResults {
    /// Builds a results set served by a real backend, carrying `hits`.
    ///
    /// Sets [`configured`](SearchResults::configured) to `true` and leaves
    /// [`note`](SearchResults::note) empty.
    pub fn found(query: impl Into<String>, hits: impl Into<Vec<SearchHit>>) -> Self {
        SearchResults {
            query: query.into(),
            configured: true,
            hits: hits.into(),
            note: None,
        }
    }

    /// Builds the canonical "no search backend configured" results set for
    /// `query`: [`configured`](SearchResults::configured) is `false`, `hits` is
    /// empty, and `note` carries [`StubProvider::UNCONFIGURED_NOTE`].
    pub fn unconfigured(query: impl Into<String>) -> Self {
        SearchResults {
            query: query.into(),
            configured: false,
            hits: Vec::new(),
            note: Some(StubProvider::UNCONFIGURED_NOTE.to_string()),
        }
    }
}

/// A web / reference search backend: a query in, ranked [`SearchHit`]s out.
///
/// This is the single seam the kernel's "搜索（经第三方 MCP）" capability is
/// expressed through. Implementors adapt a concrete backend — most often a
/// session-connected MCP search server — to the crate's uniform result shape.
/// The crate ships exactly one implementation, [`StubProvider`], which performs
/// no network I/O; real backends are supplied by the host (see the
/// [module guide](self#plugging-in-an-mcp-search-backend)).
///
/// Providers must be `Send + Sync`: a single provider is shared (behind an
/// [`Arc`]) by the master and every slave so search is uniform across writers.
pub trait SearchProvider: Send + Sync {
    /// A short, stable identifier for this backend (e.g. `"stub"`, `"mcp"`),
    /// used in logs and tool replies so a reader can tell which backend served a
    /// query.
    fn name(&self) -> &str;

    /// Runs `query` and returns at most `limit` ranked [`SearchHit`]s.
    ///
    /// `limit` is the caller's already-clamped cap (`1..=`[`MAX_LIMIT`]);
    /// implementors should return no more than `limit` hits but may return
    /// fewer. The query is non-empty (the [`SearchTool`] validates it before
    /// calling).
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::InvalidQuery`] if the backend rejects the query,
    /// or [`SearchError::Backend`] for any transport / protocol / auth failure
    /// of the underlying backend.
    fn search(&self, query: &str, limit: usize) -> Result<SearchResults, SearchError>;
}

/// The default, **no-network** [`SearchProvider`].
///
/// Every query is answered with [`SearchResults::unconfigured`]: an explicit
/// "no search backend configured" result (empty `hits`, `configured = false`, a
/// descriptive `note`). This keeps the library and its test suite entirely
/// offline while still exposing a fully-formed search *tool*, so a host that has
/// not wired an MCP backend yet still presents a stable surface to the model —
/// and the model is told plainly that search is unavailable rather than seeing a
/// silent empty result it might mistake for "nothing found".
#[derive(Debug, Clone, Copy, Default)]
pub struct StubProvider;

impl StubProvider {
    /// The note attached to every [`StubProvider`] result, explaining that no
    /// real search backend is wired and pointing at the integration seam.
    pub const UNCONFIGURED_NOTE: &'static str = "no search backend configured: this build has no MCP search provider wired, \
         so web/reference search is unavailable. Connect an MCP search server and \
         supply it as a SearchProvider to enable it.";

    /// Creates a stub provider.
    pub fn new() -> Self {
        StubProvider
    }
}

impl SearchProvider for StubProvider {
    fn name(&self) -> &str {
        "stub"
    }

    fn search(&self, query: &str, _limit: usize) -> Result<SearchResults, SearchError> {
        Ok(SearchResults::unconfigured(query))
    }
}

/// The native **search** tool: a [`Tool`] that delegates to a [`SearchProvider`].
///
/// It advertises a `search` function distinct from the local `find` tool: `find`
/// scans the on-disk workspace, `search` reaches outward for web / reference
/// material via the configured backend. The tool owns its provider (shared
/// behind an [`Arc`]), so it can be registered into any
/// [`ToolRegistry`](crate::tool::ToolRegistry) without touching the
/// [`ToolCtx`].
///
/// With the default [`StubProvider`] the tool is fully present and schema-valid
/// but answers every query with the documented unconfigured result; swap in a
/// real provider to make it live.
///
/// # Examples
///
/// ```
/// use ai_write::search::SearchTool;
///
/// let tool = SearchTool::with_stub();
/// assert_eq!(tool.provider_name(), "stub");
/// ```
pub struct SearchTool {
    provider: Arc<dyn SearchProvider>,
}

impl SearchTool {
    /// Creates a search tool backed by `provider`.
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        SearchTool { provider }
    }

    /// Creates a search tool backed by the no-network [`StubProvider`].
    ///
    /// This is the default a host uses until it wires a real MCP search backend:
    /// the tool is present and callable, but every query returns the documented
    /// "no search backend configured" result.
    pub fn with_stub() -> Self {
        SearchTool::new(Arc::new(StubProvider::new()))
    }

    /// The [`name`](SearchProvider::name) of the backing provider (e.g. `"stub"`
    /// or `"mcp"`), exposed for diagnostics and tests.
    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }
}

/// Maps a [`SearchError`] onto the [`ToolError`] the model sees, so a search
/// failure is reported in the same recoverable shape as any other tool error.
fn search_error_to_tool(err: SearchError) -> ToolError {
    match err {
        SearchError::InvalidQuery(msg) => ToolError::InvalidArgs(msg),
        SearchError::Backend(msg) => ToolError::Other(format!("search backend: {msg}")),
    }
}

/// The model-supplied arguments for a [`SearchTool`] call.
#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }

    fn schema(&self) -> ReqTool {
        ReqTool::function(FunctionDef {
            name: "search".to_string(),
            description: Some(
                "Search the web / reference material for a query, returning titled, \
                 linked snippets. This reaches OUTSIDE the workspace (via a third-party \
                 search backend) and is distinct from `find`, which only scans the local \
                 articles you are writing. If no search backend is configured for this \
                 session the result says so explicitly (an empty result with a note) — \
                 treat that as 'search unavailable here', not 'nothing found'."
                    .to_string(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to search for (a natural-language query or keywords).",
                    },
                    "limit": {
                        "type": "integer",
                        "description": format!(
                            "Maximum number of results to return (1..={MAX_LIMIT}; \
                             defaults to {DEFAULT_LIMIT})."
                        ),
                    },
                },
                "required": ["query"],
            })),
        })
    }

    fn call(&self, args: Value, _ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: SearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let query = a.query.trim();
        if query.is_empty() {
            return Err(ToolError::InvalidArgs("query must not be empty".into()));
        }
        // Clamp the requested limit into `1..=MAX_LIMIT`, defaulting when absent.
        let limit = a.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        let results = self
            .provider
            .search(query, limit)
            .map_err(search_error_to_tool)?;
        // Defensively cap the backend's hits at the requested limit.
        let mut results = results;
        results.hits.truncate(limit);
        serde_json::to_value(results)
            .map_err(|e| ToolError::Other(format!("cannot serialize search results: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCtx;
    use crate::tool::workspace::{Workspace, WriterId};

    /// A fake provider that records the queries it saw and returns canned hits,
    /// exercising the [`SearchProvider`] contract without any network I/O.
    struct FakeProvider {
        hits: Vec<SearchHit>,
        seen: std::sync::Mutex<Vec<(String, usize)>>,
    }

    impl FakeProvider {
        fn new(hits: Vec<SearchHit>) -> Self {
            FakeProvider {
                hits,
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl SearchProvider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }

        fn search(&self, query: &str, limit: usize) -> Result<SearchResults, SearchError> {
            self.seen.lock().unwrap().push((query.to_string(), limit));
            Ok(SearchResults::found(query, self.hits.clone()))
        }
    }

    /// A fake provider that always fails, to exercise error mapping.
    struct FailingProvider;

    impl SearchProvider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }

        fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, SearchError> {
            Err(SearchError::Backend("connection refused".into()))
        }
    }

    fn hit(title: &str) -> SearchHit {
        SearchHit {
            title: title.to_string(),
            url: format!("https://example.com/{title}"),
            snippet: format!("snippet for {title}"),
        }
    }

    /// Dispatches one `search` tool call against a throwaway workspace context.
    fn call_search(tool: &SearchTool, args: Value) -> ToolResult {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");
        let mut ctx = ToolCtx::new(&mut ws, WriterId::Human);
        tool.call(args, &mut ctx)
    }

    #[test]
    fn stub_provider_returns_unconfigured_result() {
        let stub = StubProvider::new();
        let r = stub.search("anything", DEFAULT_LIMIT).unwrap();
        assert!(!r.configured);
        assert!(r.hits.is_empty());
        assert_eq!(r.note.as_deref(), Some(StubProvider::UNCONFIGURED_NOTE));
        assert_eq!(r.query, "anything");
    }

    #[test]
    fn stub_tool_reports_unconfigured_explicitly() {
        let tool = SearchTool::with_stub();
        assert_eq!(tool.provider_name(), "stub");
        let out = call_search(&tool, json!({ "query": "rust async" })).unwrap();
        // The documented empty/unconfigured shape: configured=false, no hits, a note.
        assert_eq!(out["configured"], json!(false));
        assert!(out["hits"].as_array().unwrap().is_empty());
        assert_eq!(out["query"], "rust async");
        assert!(
            out["note"]
                .as_str()
                .unwrap()
                .contains("no search backend configured")
        );
    }

    #[test]
    fn search_schema_is_distinct_from_find() {
        let tool = SearchTool::with_stub();
        assert_eq!(tool.name(), "search");
        let schema = tool.schema();
        assert_eq!(schema.function.name, "search");
        // The description makes the find-vs-search distinction explicit.
        let desc = schema.function.description.unwrap();
        assert!(desc.contains("find"));
        assert!(desc.to_lowercase().contains("outside"));
    }

    #[test]
    fn fake_provider_is_exercised_through_the_trait() {
        let provider = Arc::new(FakeProvider::new(vec![hit("a"), hit("b")]));
        let tool = SearchTool::new(provider.clone());
        assert_eq!(tool.provider_name(), "fake");

        let out = call_search(&tool, json!({ "query": "  topic  ", "limit": 10 })).unwrap();
        assert_eq!(out["configured"], json!(true));
        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["title"], "a");
        assert_eq!(hits[1]["url"], "https://example.com/b");

        // The trait saw the trimmed query and the requested (clamped) limit.
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "topic");
        assert_eq!(seen[0].1, 10);
    }

    #[test]
    fn limit_defaults_and_clamps() {
        let provider = Arc::new(FakeProvider::new(Vec::new()));
        let tool = SearchTool::new(provider.clone());

        // No limit -> DEFAULT_LIMIT.
        call_search(&tool, json!({ "query": "x" })).unwrap();
        // Oversized limit -> clamped to MAX_LIMIT.
        call_search(&tool, json!({ "query": "x", "limit": 9999 })).unwrap();
        // Zero -> clamped up to 1.
        call_search(&tool, json!({ "query": "x", "limit": 0 })).unwrap();

        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen[0].1, DEFAULT_LIMIT);
        assert_eq!(seen[1].1, MAX_LIMIT);
        assert_eq!(seen[2].1, 1);
    }

    #[test]
    fn hits_truncated_to_limit() {
        let provider = Arc::new(FakeProvider::new(vec![
            hit("a"),
            hit("b"),
            hit("c"),
            hit("d"),
        ]));
        let tool = SearchTool::new(provider);
        let out = call_search(&tool, json!({ "query": "x", "limit": 2 })).unwrap();
        assert_eq!(out["hits"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn empty_query_rejected() {
        let tool = SearchTool::with_stub();
        let err = call_search(&tool, json!({ "query": "   " })).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn backend_error_maps_to_tool_error() {
        let tool = SearchTool::new(Arc::new(FailingProvider));
        let err = call_search(&tool, json!({ "query": "x" })).unwrap_err();
        match err {
            ToolError::Other(msg) => assert!(msg.contains("connection refused")),
            other => panic!("expected ToolError::Other, got {other:?}"),
        }
    }

    #[test]
    fn results_serialize_omits_absent_note() {
        let r = SearchResults::found("q", vec![hit("a")]);
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("note").is_none());
        assert_eq!(v["configured"], json!(true));
    }

    #[test]
    fn search_error_display_is_descriptive() {
        assert!(
            SearchError::Backend("boom".into())
                .to_string()
                .contains("boom")
        );
        assert!(
            SearchError::InvalidQuery("empty".into())
                .to_string()
                .contains("empty")
        );
    }
}
