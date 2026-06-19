//! The presentation WebUI backend: an `axum` HTTP server that visualizes the AI
//! writing process (`docs/impl-v1.md` §3).
//!
//! This module is the async front door to the otherwise synchronous writing
//! engine. It is a **pure JSON / SSE API** (no embedded HTML — the front-end is
//! the separate `web/` SvelteKit app, served independently). It exposes a small
//! REST + [Server-Sent Events][sse] API over the workspace:
//!
//! - read the theme / article tree and an article's current content,
//! - start a writing task (a [`Master`] → slave run) in the background,
//! - stream every [`Event`] the run emits to the browser in real time,
//! - browse an article's git history, diff two versions, and undo the last edit.
//!
//! # The sync engine × async server bridge (decision V8)
//!
//! The engine is synchronous (`ureq` + `std::thread` + `git2`), and `axum` is
//! asynchronous (`tokio`). The two are bridged without making the engine async:
//!
//! 1. **Events flow through a [`tokio::sync::broadcast`] channel.** A
//!    [`BroadcastSink`] implements [`EventSink`] by sending each event into a
//!    `broadcast::Sender<Event>`. The engine only ever sees an
//!    `Arc<dyn EventSink>` — it knows nothing about HTTP, SSE, or tokio
//!    (decision V4).
//! 2. **A writing run executes on [`tokio::task::spawn_blocking`].** `POST
//!    /api/tasks` hands the synchronous `Master::run_one` to the blocking thread
//!    pool with the broadcast sink installed, so it never stalls the async
//!    runtime, and returns a `task_id` immediately.
//! 3. **`GET /api/events` subscribes to the broadcast** and turns each received
//!    [`Event`] into an SSE `data:` line of JSON. Every connected browser shares
//!    one broadcast, so a single run fans out to all subscribers.
//!
//! # Routes
//!
//! | Method & path | Handler | Purpose |
//! |---|---|---|
//! | `GET /api/themes` | [`list_themes`] | list theme names |
//! | `GET /api/themes/:theme/articles` | [`list_articles`] | a theme's article hierarchy outline (reading order) |
//! | `GET` / `PUT /api/themes/:theme/config` | [`get_theme_config`] / [`put_theme_config`] | read / write the theme's global config |
//! | `POST /api/themes/:theme/reorder` | [`reorder_articles`] | set the reading order |
//! | `POST /api/themes/:theme/articles/:file/parent` | [`set_article_parent`] | set / clear an article's parent |
//! | `POST /api/themes/:theme/chat` | [`master_chat`] | run an LLM master over a goal, returns its outcome + a structured plan |
//! | `GET /api/skills` | [`list_skills`] | list available writing skills |
//! | `GET /api/articles/:theme/:file` | [`get_article`] | an article's current content |
//! | `PUT /api/articles/:theme/:file` | [`put_article`] | human-write the article body through the coordinator |
//! | `POST /api/tasks` | [`start_task`] | start a single writing run, returns `{task_id}` |
//! | `GET /api/events` | [`events`] | **SSE** stream of [`Event`] JSON |
//! | `GET /api/articles/:theme/:file/history` | [`article_history`] | git history (versions) |
//! | `GET /api/articles/:theme/:file/diff` | [`article_diff`] | unified diff between two versions |
//! | `GET /api/articles/:theme/:file/blame` | [`article_blame`] | per-line authorship (git blame) |
//! | `GET /api/articles/:theme/:file/contributions` | [`article_contributions`] | per-author contribution shares |
//! | `POST /api/articles/:theme/:file/undo` | [`article_undo`] | undo to the previous version |
//!
//! # Security
//!
//! The theme and file path segments are validated by the workspace sandbox
//! ([`Workspace::read_article`](crate::tool::workspace::Workspace::read_article)
//! and friends reject `..`, absolute paths, and separators), so a request cannot
//! escape the workspace root. The `DEEPSEEK_API_KEY` is read once at server start
//! via [`Client::from_env`](crate::req::blocking::Client::from_env) and never
//! echoed back over the wire.
//!
//! [sse]: https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::coordinator::{CoordError, Coordinator, LockSet, TxnRequest};
use crate::engine::{Master, SlaveReport, SlaveSkill, SlaveStatus, SlaveTask};
use crate::observe::{Event, EventSink};
use crate::req::Model;
use crate::req::blocking::Client;
use crate::session::{Session, SessionOptions};
use crate::skill;
use crate::tool::ToolRegistry;
use crate::tool::workspace::{ArticleOutline, ThemeConfig, Workspace, WriterId};
use crate::vcs::Vcs;

/// The default directory writing skills are loaded from (overridable via the
/// `AI_WRITE_SKILLS` environment variable in [`AppState::from_env`]).
const DEFAULT_SKILLS_DIR: &str = "skills";

/// The number of events buffered per [`broadcast`] channel before the oldest are
/// dropped for a slow subscriber.
///
/// A writing run emits a few events per round; this buffer is generous enough
/// that a browser briefly behind on its SSE stream still catches up. A lagging
/// subscriber that overflows it simply misses the oldest events (the live feed is
/// best-effort, not a durable log).
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Shared application state handed to every `axum` handler.
///
/// It is cloneable (every field is cheap to clone — `Arc`, a `broadcast::Sender`,
/// or a [`Client`] sharing its connection pool) so `axum` can hand a copy to each
/// request. It owns no `Workspace` or [`Vcs`] handle: those are opened per
/// request (read endpoints) or per task (the write run), since the workspace lock
/// is process-local in-memory state and `Vcs` wraps a non-`Sync`
/// [`git2::Repository`].
#[derive(Clone)]
pub struct AppState {
    /// The workspace root every request resolves themes and articles under.
    workspace_root: PathBuf,
    /// The directory writing skills are loaded from (for `GET /api/skills` and
    /// the master-chat skill selection).
    skills_dir: PathBuf,
    /// The stateless DeepSeek client a writing run drives. Read once from the
    /// environment at server construction; cloning shares the connection pool.
    client: Client,
    /// The broadcast channel every [`Event`] flows through. [`events`] subscribes
    /// to it for SSE; a [`BroadcastSink`] built from
    /// [`AppState::event_sink`] sends into it from the blocking write task.
    events: broadcast::Sender<Event>,
    /// A monotonic counter handing out a `task_id` to each started writing run.
    next_task_id: Arc<AtomicU64>,
    /// The single, process-long [`Coordinator`] every mutating WebUI endpoint
    /// (human write, undo, edit reservation) funnels through (B3; closes
    /// kernel-impl-results §3.1).
    ///
    /// It is shared across requests so the operation-level lock table and the
    /// standing edit reservations persist between calls: a
    /// [`request_edit`](Coordinator::request_edit) registered by one request is
    /// honoured by the human's later `PUT`. Created lazily on first mutating use
    /// (so [`AppState::new`] stays infallible) with the state's broadcast
    /// [`event_sink`](AppState::event_sink) installed, then reused. Held behind a
    /// [`Mutex`](std::sync::Mutex) only for the one-time initialization; the
    /// `Arc<Coordinator>` itself is cloned out and used lock-free.
    coordinator: Arc<std::sync::Mutex<Option<Arc<Coordinator>>>>,
}

impl AppState {
    /// Builds the shared state for a workspace rooted at `workspace_root`, driven
    /// by `client`.
    ///
    /// A fresh [`broadcast`] channel and task counter are created. Use
    /// [`AppState::from_env`] to read the client from the environment instead of
    /// constructing one yourself.
    pub fn new(workspace_root: impl Into<PathBuf>, client: Client) -> Self {
        let (events, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        AppState {
            workspace_root: workspace_root.into(),
            skills_dir: PathBuf::from(DEFAULT_SKILLS_DIR),
            client,
            events,
            next_task_id: Arc::new(AtomicU64::new(1)),
            coordinator: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Sets the directory writing skills are loaded from, returning `self` for
    /// chaining. Defaults to `./skills`.
    pub fn with_skills_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.skills_dir = dir.into();
        self
    }

    /// Builds the shared state, reading the DeepSeek client from the environment
    /// (`DEEPSEEK_API_KEY`).
    ///
    /// # Errors
    ///
    /// Returns the [`req::Error`](crate::req::Error) from
    /// [`Client::from_env`](crate::req::blocking::Client::from_env) if the key is
    /// missing or the client cannot be built.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ai_write::webui::AppState;
    ///
    /// let state = AppState::from_env("workspace")?;
    /// # let _ = state;
    /// # Ok::<(), ai_write::req::Error>(())
    /// ```
    pub fn from_env(workspace_root: impl Into<PathBuf>) -> crate::req::Result<Self> {
        let mut state = Self::new(workspace_root, Client::from_env()?);
        if let Ok(dir) = std::env::var("AI_WRITE_SKILLS") {
            state = state.with_skills_dir(dir);
        }
        Ok(state)
    }

    /// Returns an [`EventSink`] that forwards every emitted [`Event`] into this
    /// state's broadcast channel, so the synchronous engine can narrate a run to
    /// every SSE subscriber.
    ///
    /// The returned `Arc<dyn EventSink>` is installed on the master (and thus,
    /// via the engine, on every slave) of a writing run.
    pub fn event_sink(&self) -> Arc<dyn EventSink> {
        Arc::new(BroadcastSink(self.events.clone()))
    }

    /// Returns the shared [`Coordinator`], opening it on first mutating use.
    ///
    /// Every mutating WebUI endpoint (human `PUT`, undo, edit reservation) shares
    /// this one coordinator, so they funnel through a single operation-level lock
    /// table and a single [`Vcs`] and the standing edit reservations persist
    /// across requests (B3). The coordinator is created with the state's broadcast
    /// [`event_sink`](AppState::event_sink) installed so its transaction lifecycle
    /// streams to every SSE subscriber.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] (`500`) if the coordinator cannot be opened (the git
    /// repository at the workspace root is unavailable).
    fn coordinator(&self) -> Result<Arc<Coordinator>, ApiError> {
        let mut guard = self
            .coordinator
            .lock()
            .expect("coordinator init mutex poisoned");
        if let Some(coord) = guard.as_ref() {
            return Ok(Arc::clone(coord));
        }
        let coord = Coordinator::open(&self.workspace_root)
            .map_err(ApiError::coord)?
            .with_event_sink(self.event_sink());
        let coord = Arc::new(coord);
        *guard = Some(Arc::clone(&coord));
        Ok(coord)
    }

    /// Opens a fresh [`Workspace`] handle at the configured root.
    ///
    /// A new handle is opened per request because the single-writer article lock
    /// is in-memory, process-local state that must not be shared across the
    /// independent read endpoints.
    fn open_workspace(&self) -> Result<Workspace, ApiError> {
        Workspace::open(&self.workspace_root).map_err(ApiError::workspace)
    }

    /// Opens a fresh [`Vcs`] handle at the configured root, for the
    /// history / diff / undo endpoints.
    fn open_vcs(&self) -> Result<Vcs, ApiError> {
        Vcs::open_or_init(&self.workspace_root).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("version control unavailable: {e}"),
        })
    }
}

/// An [`EventSink`] that publishes each [`Event`] to a [`broadcast`] channel.
///
/// [`emit`](EventSink::emit) is non-blocking and lossy by design: a send with no
/// live subscribers (or one that has lagged past the channel capacity) is simply
/// dropped, so a slow or absent browser never stalls the writing engine. This is
/// the concrete bridge from the engine's push-based observability to the WebUI's
/// SSE fan-out.
#[derive(Clone)]
pub struct BroadcastSink(broadcast::Sender<Event>);

impl EventSink for BroadcastSink {
    /// Publishes `event` to the broadcast channel, ignoring the "no subscribers"
    /// result so emission never fails or blocks the producer.
    fn emit(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

/// Builds the `axum` [`Router`] wiring every route to its handler over `state`.
///
/// This is the single place the route table is declared; the binary
/// (`src/bin/webui.rs`) only binds a listener and serves the returned router, and
/// the handler unit tests build it to exercise endpoints in-process.
///
/// # Examples
///
/// ```no_run
/// use ai_write::webui::{app, AppState};
///
/// let state = AppState::from_env("workspace")?;
/// let _router = app(state);
/// # Ok::<(), ai_write::req::Error>(())
/// ```
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/api/themes", get(list_themes))
        .route("/api/themes/{theme}/articles", get(list_articles))
        .route(
            "/api/themes/{theme}/config",
            get(get_theme_config).put(put_theme_config),
        )
        .route("/api/themes/{theme}/reorder", post(reorder_articles))
        .route(
            "/api/themes/{theme}/articles/{file}/parent",
            post(set_article_parent),
        )
        .route("/api/themes/{theme}/chat", post(master_chat))
        .route("/api/skills", get(list_skills))
        .route(
            "/api/articles/{theme}/{file}",
            get(get_article).put(put_article),
        )
        .route("/api/tasks", post(start_task))
        .route("/api/events", get(events))
        .route("/api/articles/{theme}/{file}/history", get(article_history))
        .route("/api/articles/{theme}/{file}/diff", get(article_diff))
        .route("/api/articles/{theme}/{file}/blame", get(article_blame))
        .route(
            "/api/articles/{theme}/{file}/contributions",
            get(article_contributions),
        )
        .route("/api/articles/{theme}/{file}/undo", post(article_undo))
        .route(
            "/api/articles/{theme}/{file}/request-edit",
            post(request_edit).delete(cancel_request_edit),
        )
        .with_state(state)
}

// ===========================================================================
// Error handling
// ===========================================================================

/// A handler error rendered as a JSON body with an HTTP status.
///
/// Workspace and version-control failures map onto an HTTP status (a missing
/// theme/article becomes `404`, a sandbox violation `400`, anything else `500`),
/// and the message is returned to the client as `{"error": "<message>"}`.
#[derive(Debug)]
pub struct ApiError {
    /// The HTTP status to respond with.
    status: StatusCode,
    /// The human-readable error message.
    message: String,
}

impl ApiError {
    /// Maps a workspace [`ToolError`](crate::tool::ToolError) onto an
    /// [`ApiError`], choosing the HTTP status from the error kind.
    fn workspace(err: crate::tool::ToolError) -> Self {
        use crate::tool::ToolError;
        let status = match &err {
            ToolError::NotFound(_) => StatusCode::NOT_FOUND,
            ToolError::SandboxViolation(_) | ToolError::InvalidArgs(_) => StatusCode::BAD_REQUEST,
            ToolError::Lock(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError {
            status,
            message: err.to_string(),
        }
    }

    /// Maps a version-control [`VcsError`](crate::vcs::VcsError) onto an
    /// [`ApiError`] (a missing revision/history becomes `404`, otherwise `500`).
    fn vcs(err: crate::vcs::VcsError) -> Self {
        use crate::vcs::VcsError;
        let status = match &err {
            VcsError::NoHistory(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError {
            status,
            message: err.to_string(),
        }
    }

    /// Maps a coordinator [`CoordError`](crate::coordinator::CoordError) onto an
    /// [`ApiError`].
    ///
    /// A workspace failure inside the transaction is mapped by the workspace
    /// error kind (a missing article becomes `404`, a sandbox violation `400`); a
    /// declared-bounds violation, version-control failure, or an aborted body are
    /// `500` — they indicate a server-side problem, not a malformed request.
    fn coord(err: CoordError) -> Self {
        match err {
            CoordError::Workspace(e) => ApiError::workspace(e),
            // A declared-bounds violation, version-control failure, or an aborted
            // body are server-side problems, not malformed requests. `CoordError`
            // is `#[non_exhaustive]`, so a `ref` binding keeps `err` usable for the
            // message while remaining exhaustive within this crate.
            ref other => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: other.to_string(),
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

// ===========================================================================
// Read endpoints
// ===========================================================================

/// The response body for [`list_themes`].
#[derive(Debug, Serialize)]
pub struct ThemesResponse {
    /// The theme names found under the workspace root, sorted.
    pub themes: Vec<String>,
}

/// `GET /api/themes` — list the theme directories under the workspace root.
///
/// A theme is any immediate subdirectory of the workspace root. The list is
/// sorted for a stable UI ordering.
///
/// # Errors
///
/// Returns an [`ApiError`] (`500`) if the workspace root cannot be opened or
/// read.
pub async fn list_themes(State(state): State<AppState>) -> Result<Json<ThemesResponse>, ApiError> {
    let ws = state.open_workspace()?;
    let mut themes = Vec::new();
    let entries = std::fs::read_dir(ws.root()).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("cannot read workspace root: {e}"),
    })?;
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            && let Some(name) = entry.file_name().to_str()
            && !name.starts_with('.')
        {
            themes.push(name.to_string());
        }
    }
    themes.sort();
    Ok(Json(ThemesResponse { themes }))
}

/// The response body for [`list_articles`].
#[derive(Debug, Serialize)]
pub struct ArticlesResponse {
    /// The theme the articles belong to.
    pub theme: String,
    /// The theme's articles as a flat hierarchy outline (file, title, parent,
    /// depth), in reading order — the front-end indents on `depth` to draw the
    /// logical tree.
    pub articles: Vec<ArticleOutline>,
}

/// `GET /api/themes/{theme}/articles` — list a theme's articles as a hierarchy
/// outline in reading order.
///
/// # Errors
///
/// Returns an [`ApiError`]: `404` if the theme does not exist, `400` for an
/// illegal theme name, or `500` on a filesystem failure.
pub async fn list_articles(
    State(state): State<AppState>,
    AxumPath(theme): AxumPath<String>,
) -> Result<Json<ArticlesResponse>, ApiError> {
    let ws = state.open_workspace()?;
    let articles = ws.article_outline(&theme).map_err(ApiError::workspace)?;
    Ok(Json(ArticlesResponse { theme, articles }))
}

/// The response body for [`get_article`] in the default (plain) format.
#[derive(Debug, Serialize)]
pub struct ArticleResponse {
    /// The theme the article belongs to.
    pub theme: String,
    /// The article file name.
    pub file: String,
    /// The article's current full text content.
    pub content: String,
}

/// Query parameters for [`get_article`]: the optional response `format`.
#[derive(Debug, Default, Deserialize)]
pub struct GetArticleQuery {
    /// `"rich"` selects the character-level authorship view (B2); any other value
    /// (or omission) returns the plain `{theme, file, content}` body.
    #[serde(default)]
    pub format: Option<String>,
}

/// One author-attributed run of text in a [`RichBlock`]: the run's text and the
/// provenance tag of whoever wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct RichRun {
    /// The run's verbatim text.
    pub text: String,
    /// The author's provenance tag (`"human"` or `"<model>/<label>"`), which the
    /// front-end maps to a colour.
    pub author: String,
}

/// One block of the rich article view: its kind and its author-attributed runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct RichBlock {
    /// The block kind (`"paragraph"`, `"heading"`, `"list_item"`, `"quote"`, or
    /// `"code"`).
    pub kind: String,
    /// The block's text, split into runs by author (a code block is a single run
    /// with no per-character authorship).
    pub runs: Vec<RichRun>,
}

/// One distinct author of an article, for the rich view's author legend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct RichAuthor {
    /// The author's provenance tag (the identity the runs reference).
    pub id: String,
    /// The author's display label — the same tag, which the front-end resolves to
    /// a human-readable name and colour.
    pub label: String,
}

/// The response body for [`get_article`] in the rich (`?format=rich`) format (B2).
///
/// Each block carries its author-attributed [`runs`](RichBlock::runs), and
/// [`authors`](RichArticleResponse::authors) is the distinct contributor list in
/// first-seen reading order, so the front-end can colour every run by its author
/// tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct RichArticleResponse {
    /// The theme the article belongs to.
    pub theme: String,
    /// The article file name.
    pub file: String,
    /// The article's blocks, in reading order, each with author-attributed runs.
    pub blocks: Vec<RichBlock>,
    /// The distinct authors of the article, in first-seen reading order.
    pub authors: Vec<RichAuthor>,
}

/// `GET /api/articles/{theme}/{file}` — an article's current content.
///
/// The default response is the back-compatible `{theme, file, content}`, where
/// `content` is the article's plain body. With `?format=rich` the response is the
/// character-level authorship view ([`RichArticleResponse`], B2): the body split
/// into blocks, each block into author-attributed runs, plus the distinct author
/// list. The rich view is built from the article's provenance sidecar (or a
/// single-author reconstruction for a legacy article), so an article that has only
/// ever been written through the authored edit paths attributes each run to whoever
/// actually wrote it.
///
/// # Errors
///
/// Returns an [`ApiError`]: `404` if the article is missing, `400` for an
/// illegal name or a sandbox violation, or `500` on a read failure or
/// unsupported (binary / oversized) content.
pub async fn get_article(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
    Query(query): Query<GetArticleQuery>,
) -> Result<Response, ApiError> {
    let ws = state.open_workspace()?;
    if query.format.as_deref() == Some("rich") {
        let doc = ws
            .read_document(&theme, &file)
            .map_err(ApiError::workspace)?;
        return Ok(Json(rich_article(theme, file, &doc)).into_response());
    }
    let content = ws
        .read_article(&theme, &file)
        .map_err(ApiError::workspace)?;
    Ok(Json(ArticleResponse {
        theme,
        file,
        content,
    })
    .into_response())
}

/// Builds the rich article view (B2) from a character-level authorship
/// [`Document`](crate::content::Document).
///
/// Each block is rendered with its kind and its runs (a code block as one
/// author-less-by-character run), and the distinct author tags are collected in
/// first-seen reading order for the legend. Pure (no IO), so it is unit-tested
/// directly.
fn rich_article(
    theme: String,
    file: String,
    doc: &crate::content::Document,
) -> RichArticleResponse {
    use crate::content::Block;

    let runs_of = |rt: &crate::content::RichText| -> Vec<RichRun> {
        rt.runs
            .iter()
            .map(|r| RichRun {
                text: r.text.clone(),
                author: r.author.tag(),
            })
            .collect()
    };

    let mut blocks = Vec::with_capacity(doc.blocks.len());
    for block in &doc.blocks {
        let (kind, runs) = match block {
            Block::Paragraph(t) => ("paragraph", runs_of(t)),
            Block::Heading { text, .. } => ("heading", runs_of(text)),
            Block::ListItem(t) => ("list_item", runs_of(t)),
            Block::Quote(t) => ("quote", runs_of(t)),
            Block::CodeBlock { code, .. } => (
                "code",
                if code.is_empty() {
                    Vec::new()
                } else {
                    // A code block carries no per-character authorship; surface it
                    // as a single run tagged with the document's primary author so
                    // the front-end still has a tag to colour by.
                    vec![RichRun {
                        text: code.clone(),
                        author: primary_author_tag(doc),
                    }]
                },
            ),
        };
        blocks.push(RichBlock {
            kind: kind.to_string(),
            runs,
        });
    }

    let authors = crate::provenance::contributors(doc)
        .into_iter()
        .map(|tag| RichAuthor {
            id: tag.clone(),
            label: tag,
        })
        .collect();

    RichArticleResponse {
        theme,
        file,
        blocks,
        authors,
    }
}

/// The provenance tag of the first authored run in `doc`, used only to give a
/// code block (which has no per-character authorship) a colour to render under.
/// Falls back to `"human"` for a document with no authored prose.
fn primary_author_tag(doc: &crate::content::Document) -> String {
    crate::provenance::contributors(doc)
        .into_iter()
        .next()
        .unwrap_or_else(|| crate::content::AuthorId::Human.tag())
}

/// The request body for [`put_article`].
#[derive(Debug, Deserialize)]
pub struct PutArticleRequest {
    /// The full new article body the human is writing.
    pub text: String,
}

/// The response body for [`put_article`].
#[derive(Debug, Serialize)]
pub struct PutArticleResponse {
    /// The theme the article belongs to.
    pub theme: String,
    /// The article file name.
    pub file: String,
    /// The abbreviated SHA of the single commit the write produced.
    ///
    /// A human write always records the human as a contributor (touching the
    /// theme `index.json`), so in practice a commit is always made and this is
    /// `Some`. The field is nullable only to mirror the coordinator's general
    /// "a transaction that touched nothing makes no commit" contract, which a
    /// body write never triggers.
    pub committed: Option<String>,
}

/// `PUT /api/articles/{theme}/{file}` — human-write an article body through the
/// coordinator, as one transaction (kernel §6, B1).
///
/// The write goes through [`Coordinator::submit`] at [`WriterId::Human`] priority
/// (it jumps to the head of the wait queue but never preempts a running
/// transaction), declaring the article body and the theme `index.json` as its
/// lock set, and lands as exactly **one** git commit inside the critical section.
/// This is the human-facing counterpart to a dispatched writer's edit: the same
/// lock table and the same single-commit discipline, so a human revision and an
/// agent edit never race the git index. The commit is authored as
/// [`WriterId::Human`], so the article's per-line provenance records the human.
///
/// Returns the new commit's short SHA, or `committed: null` when the submitted
/// text matched the committed version (the coordinator makes no empty commit).
///
/// # Errors
///
/// Returns an [`ApiError`]: `404` if the article does not exist, `400` for an
/// illegal theme/file name or a sandbox violation, or `500` if the workspace
/// cannot be opened, the commit fails, or the transaction is otherwise aborted.
pub async fn put_article(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
    Json(req): Json<PutArticleRequest>,
) -> Result<Json<PutArticleResponse>, ApiError> {
    // Route the human write through the shared coordinator (with the WebUI's
    // broadcast sink installed), so its transaction lifecycle (acquire / queue /
    // release / handoff, B3) streams to every SSE subscriber alongside the commit
    // and it consumes any standing edit reservation for this article. The critical
    // section is synchronous (git2 + std::sync), so run it on the blocking pool
    // rather than stalling a tokio worker; the closure owns every input it needs.
    let coord = state.coordinator()?;
    let outcome = tokio::task::spawn_blocking(move || {
        // Declare the article body plus the theme manifest (the write records the
        // human as a contributor, touching `index.json`), exactly as the engine's
        // single-article edit does.
        let locks = LockSet::new()
            .with(Path::new(&theme).join(&file))
            .with(Path::new(&theme).join("index.json"));
        let label = format!("human edit({theme}/{file})");
        let request = TxnRequest::new(WriterId::Human, locks, label);
        let theme_for_body = theme.clone();
        let file_for_body = file.clone();
        let text = req.text;
        let txn = coord
            .submit(request, move |ctx| {
                ctx.write_article(&theme_for_body, &file_for_body, &text)?;
                Ok(format!("human edit({theme_for_body}/{file_for_body})"))
            })
            .map_err(ApiError::coord)?;
        Ok::<_, ApiError>(PutArticleResponse {
            theme,
            file,
            committed: txn.sha,
        })
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("write task panicked: {e}"),
    })??;

    Ok(Json(outcome))
}

// ===========================================================================
// Theme config, hierarchy, and skills
// ===========================================================================

/// `GET /api/themes/{theme}/config` — a theme's global configuration.
///
/// # Errors
///
/// Returns an [`ApiError`]: `404` if the theme does not exist, `400` for an
/// illegal name, or `500` on a read failure.
pub async fn get_theme_config(
    State(state): State<AppState>,
    AxumPath(theme): AxumPath<String>,
) -> Result<Json<ThemeConfig>, ApiError> {
    let ws = state.open_workspace()?;
    let config = ws.load_config(&theme).map_err(ApiError::workspace)?;
    Ok(Json(config))
}

/// `PUT /api/themes/{theme}/config` — replace a theme's global configuration.
///
/// The body is a full [`ThemeConfig`] (description, default skill, slave model);
/// the article order and metadata are left untouched. Returns the stored config.
///
/// # Errors
///
/// Returns an [`ApiError`]: `404` if the theme does not exist, `400` for an
/// illegal name, or `500` on a write failure.
pub async fn put_theme_config(
    State(state): State<AppState>,
    AxumPath(theme): AxumPath<String>,
    Json(config): Json<ThemeConfig>,
) -> Result<Json<ThemeConfig>, ApiError> {
    let mut ws = state.open_workspace()?;
    ws.save_config(&theme, config.clone())
        .map_err(ApiError::workspace)?;
    Ok(Json(config))
}

/// The request body for [`set_article_parent`].
#[derive(Debug, Deserialize)]
pub struct SetParentRequest {
    /// The parent article's file name, or `null` to make the article top-level.
    #[serde(default)]
    pub parent: Option<String>,
}

/// `POST /api/themes/{theme}/articles/{file}/parent` — set (or clear) an
/// article's parent in the theme's logical hierarchy.
///
/// This is the human-driven counterpart to the master's `organize_articles`
/// tool. The reading order is left untouched.
///
/// # Errors
///
/// Returns an [`ApiError`]: `404` if the article or proposed parent is missing,
/// `400` for an illegal name, a self-parent, or a cycle, or `500` on a write
/// failure.
pub async fn set_article_parent(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
    Json(req): Json<SetParentRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut ws = state.open_workspace()?;
    ws.set_parent(&theme, &file, req.parent.as_deref())
        .map_err(ApiError::workspace)?;
    Ok(Json(serde_json::json!({
        "theme": theme,
        "file": file,
        "parent": req.parent,
    })))
}

/// The request body for [`reorder_articles`].
#[derive(Debug, Deserialize)]
pub struct ReorderRequest {
    /// The full new reading order: every article in the theme, exactly once.
    pub order: Vec<String>,
}

/// `POST /api/themes/{theme}/reorder` — replace a theme's reading order.
///
/// The body's `order` must be a permutation of the theme's current articles.
///
/// # Errors
///
/// Returns an [`ApiError`]: `404` if the theme does not exist, `400` if `order`
/// is not a permutation of the current articles, or `500` on a write failure.
pub async fn reorder_articles(
    State(state): State<AppState>,
    AxumPath(theme): AxumPath<String>,
    Json(req): Json<ReorderRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut ws = state.open_workspace()?;
    ws.reorder(&theme, req.order.clone())
        .map_err(ApiError::workspace)?;
    Ok(Json(
        serde_json::json!({ "theme": theme, "order": req.order }),
    ))
}

/// One entry in the [`list_skills`] response: a skill's id, name, and
/// description (the prompt body is intentionally omitted from the listing).
#[derive(Debug, Serialize)]
pub struct SkillSummary {
    /// The skill id (its file stem).
    pub id: String,
    /// The human-readable name.
    pub name: String,
    /// The one-line description.
    pub description: String,
}

/// `GET /api/skills` — list the writing skills available under the skills
/// directory.
///
/// A missing skills directory yields an empty list rather than an error.
///
/// # Errors
///
/// Returns an [`ApiError`] (`500`) if the skills directory exists but cannot be
/// read.
pub async fn list_skills(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let skills = skill::load_skills(&state.skills_dir).map_err(skill_error)?;
    let out: Vec<SkillSummary> = skills
        .into_iter()
        .map(|s| SkillSummary {
            id: s.id,
            name: s.name,
            description: s.description,
        })
        .collect();
    Ok(Json(serde_json::json!({ "skills": out })))
}

/// Maps a [`skill::SkillError`](crate::skill::SkillError) onto an [`ApiError`] (a
/// missing skill becomes `404`, an I/O failure `500`).
fn skill_error(err: skill::SkillError) -> ApiError {
    use skill::SkillError;
    let status = match &err {
        SkillError::NotFound(_) => StatusCode::NOT_FOUND,
        SkillError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError {
        status,
        message: err.to_string(),
    }
}

/// `GET /api/articles/{theme}/{file}/history` — an article's git history.
///
/// Returns the commits that touched the article, newest first (the shape
/// produced by [`Vcs::history`](crate::vcs::Vcs::history)).
///
/// # Errors
///
/// Returns an [`ApiError`]: `500` if version control cannot be opened, or the
/// mapped [`VcsError`](crate::vcs::VcsError) status if reading history fails.
pub async fn article_history(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let vcs = state.open_vcs()?;
    let rel = Path::new(&theme).join(&file);
    let history = vcs.history(&rel).map_err(ApiError::vcs)?;
    Ok(Json(serde_json::json!({ "history": history })))
}

/// Query parameters for [`article_diff`]: the two revisions to compare.
#[derive(Debug, Deserialize)]
pub struct DiffQuery {
    /// The base revision (a commit id from history, or `HEAD~n`). Omit to diff
    /// against the empty file (full content as additions).
    #[serde(default)]
    pub from: Option<String>,
    /// The target revision. Omit to diff a committed version against the current
    /// working file.
    #[serde(default)]
    pub to: Option<String>,
}

/// `GET /api/articles/{theme}/{file}/diff?from=&to=` — a unified diff between two
/// versions.
///
/// `from` / `to` mirror [`Vcs::diff`](crate::vcs::Vcs::diff): both optional, both
/// commit ids or `HEAD`-relative revisions.
///
/// # Errors
///
/// Returns an [`ApiError`]: `404` if a named revision cannot be resolved, or
/// `500` on a version-control failure.
pub async fn article_diff(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let vcs = state.open_vcs()?;
    let rel = Path::new(&theme).join(&file);
    let patch = vcs
        .diff(&rel, query.from.as_deref(), query.to.as_deref())
        .map_err(ApiError::vcs)?;
    Ok(Json(serde_json::json!({ "diff": patch })))
}

/// `GET /api/articles/{theme}/{file}/blame` — per-line authorship of an article.
///
/// Returns the line-by-line attribution of the article's last committed version
/// (the shape produced by [`Vcs::blame`](crate::vcs::Vcs::blame)): one entry per
/// line, each carrying a 1-based line number, the author that last touched it,
/// and the short commit SHA. This is the line-level provenance the kernel calls
/// for (`docs/ai-write-kernel.html` §9). Reflects committed content only.
///
/// # Errors
///
/// Returns an [`ApiError`]: `500` if version control cannot be opened, or the
/// mapped [`VcsError`](crate::vcs::VcsError) status if computing the blame fails.
pub async fn article_blame(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let vcs = state.open_vcs()?;
    let rel = Path::new(&theme).join(&file);
    let blame = vcs.blame(&rel).map_err(ApiError::vcs)?;
    Ok(Json(serde_json::json!({ "blame": blame })))
}

/// One author's share of an article, for the signature card (B1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Contribution {
    /// The author as recorded in git blame, `"<name> <email>"` (e.g.
    /// `"human <human@ai-write.local>"` or
    /// `"deepseek-v4-pro/slave-1 <agent@ai-write.local>"`).
    pub author: String,
    /// The author identity tag — the `<name>` portion of [`author`](Contribution::author)
    /// (`"human"` or `"<model>/<label>"`) — which the front-end maps to a color.
    pub label: String,
    /// This author's whole-percent share of the article's attributed lines. The
    /// percentages across all contributors sum to exactly 100 (the largest
    /// remainders absorb the rounding so the total is exact).
    pub pct: u32,
    /// The number of attributed lines this author last touched.
    pub lines: usize,
}

/// `GET /api/articles/{theme}/{file}/contributions` — per-author contribution
/// shares for an article, aggregated from git blame (B1).
///
/// Aggregates [`Vcs::blame`](crate::vcs::Vcs::blame) by author into a per-author
/// line count and a whole-percent share, ordered by share descending (ties broken
/// by author name) so the front-end renders the signature card largest-first.
/// The `pct` values are integers that sum to exactly 100 via largest-remainder
/// rounding; an article with no committed lines yields an empty list. Reflects
/// committed content only.
///
/// # Errors
///
/// Returns an [`ApiError`]: `500` if version control cannot be opened, or the
/// mapped [`VcsError`](crate::vcs::VcsError) status if computing the blame fails.
pub async fn article_contributions(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let vcs = state.open_vcs()?;
    let rel = Path::new(&theme).join(&file);
    let blame = vcs.blame(&rel).map_err(ApiError::vcs)?;
    let contributions = aggregate_contributions(blame.iter().map(|b| b.author.as_str()));
    Ok(Json(serde_json::json!({ "contributions": contributions })))
}

/// Aggregates an iterator of blame authors (`"<name> <email>"`) into per-author
/// [`Contribution`]s with integer percentages summing to exactly 100.
///
/// Authors are tallied by their full blame string (preserving first-seen order for
/// deterministic tie-breaking), then each author's share is the largest-remainder
/// rounding of `lines / total * 100`, so the percentages sum to exactly 100. The
/// result is sorted by `pct` descending, ties broken by author name ascending. An
/// empty input (an article with no committed lines) yields an empty `Vec`.
fn aggregate_contributions<'a>(authors: impl Iterator<Item = &'a str>) -> Vec<Contribution> {
    use std::collections::BTreeMap;

    // Tally lines per author. A BTreeMap keeps the pre-rounding order stable
    // (author-name ascending), which makes the largest-remainder pass and the
    // final tie-breaking deterministic.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0usize;
    for author in authors {
        *counts.entry(author.to_string()).or_insert(0) += 1;
        total += 1;
    }
    if total == 0 {
        return Vec::new();
    }

    // First pass: the floored percentage and the fractional remainder per author.
    // `floor(lines * 100 / total)` is the base share; the leftover percentage
    // points (100 - sum of floors) go to the authors with the largest remainders.
    let mut rows: Vec<(String, usize, u32, u64)> = counts
        .into_iter()
        .map(|(author, lines)| {
            let scaled = (lines as u64) * 100;
            let floor = (scaled / total as u64) as u32;
            let remainder = scaled % total as u64;
            (author, lines, floor, remainder)
        })
        .collect();

    let floored_sum: u32 = rows.iter().map(|r| r.2).sum();
    let mut leftover = 100u32.saturating_sub(floored_sum);

    // Hand each leftover point to the next-largest remainder (ties broken by the
    // author order already imposed by the BTreeMap, i.e. name ascending).
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| rows[b].3.cmp(&rows[a].3).then(a.cmp(&b)));
    for &i in &order {
        if leftover == 0 {
            break;
        }
        rows[i].2 += 1;
        leftover -= 1;
    }

    let mut out: Vec<Contribution> = rows
        .into_iter()
        .map(|(author, lines, pct, _)| {
            let label = author
                .rsplit_once(' ')
                .map(|(name, _email)| name.to_string())
                .unwrap_or_else(|| author.clone());
            Contribution {
                author,
                label,
                pct,
                lines,
            }
        })
        .collect();
    // Largest share first; ties broken by author name ascending for stability.
    out.sort_by(|a, b| b.pct.cmp(&a.pct).then_with(|| a.author.cmp(&b.author)));
    out
}

// ===========================================================================
// Mutating endpoints: undo and task start
// ===========================================================================

/// `POST /api/articles/{theme}/{file}/undo` — undo the article's last edit.
///
/// Reverts the article to its previous committed version and records the revert
/// as a new commit (article-level undo, never a history rewrite — see
/// [`Vcs::undo_last`](crate::vcs::Vcs::undo_last)). The undo is authored as
/// [`WriterId::Human`], since it is a human-driven action from the UI.
///
/// The revert is routed through [`Coordinator::undo_article`] (B3), so it acquires
/// the article's operation-level lock at human priority and reverts on the single
/// shared [`Vcs`] — an undo and a concurrent agent edit can never
/// race the git index, and the transaction lifecycle streams to the SSE feed.
/// Returns the new commit id on success, or `{"undone": false}` when the article
/// has only one version.
///
/// # Errors
///
/// Returns an [`ApiError`]: `500` if the coordinator cannot be opened or the
/// revert fails (a coordinator error mapped to an HTTP status).
pub async fn article_undo(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // The coordinator's critical section is synchronous (git2 + std::sync), so run
    // it on the blocking pool rather than stalling a tokio worker. The shared
    // coordinator routes the undo through the same lock table and Vcs as every
    // other mutation (B3).
    let coord = state.coordinator()?;
    let reverted = tokio::task::spawn_blocking(move || {
        coord
            .undo_article(WriterId::Human, &theme, &file)
            .map(|sha| (theme, file, sha))
            .map_err(ApiError::coord)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("undo task panicked: {e}"),
    })??;

    match reverted {
        (theme, file, Some(sha)) => Ok(Json(serde_json::json!({
            "undone": format!("{theme}/{file}"),
            "committed": sha,
        }))),
        (_, _, None) => Ok(Json(serde_json::json!({
            "undone": false,
            "reason": "nothing to undo (article has only one version)",
        }))),
    }
}

/// The response body for [`request_edit`] (B3): the standing reservation was
/// queued, and how many transactions sit ahead of the human's turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct RequestEditResponse {
    /// Always `true`: the reservation was recorded at the head of the wait queue.
    pub queued: bool,
    /// How many transactions must finish before the human's turn (`0` = up now —
    /// the critical section is idle). See
    /// [`RequestEditOutcome::ahead`](crate::coordinator::RequestEditOutcome::ahead).
    pub ahead: usize,
}

/// `POST /api/articles/{theme}/{file}/request-edit` — register a standing human
/// edit reservation at the head of the coordinator's queue (B3).
///
/// This is the non-blocking "I want to edit this article next" signal: it routes
/// to [`Coordinator::request_edit`] on the shared coordinator, so the coordinator
/// stops admitting agent transactions until the human's actual `PUT` arrives or
/// the reservation is cancelled via [`cancel_request_edit`]. Per mechanism 6.C the
/// reservation jumps ahead of every waiting agent but never preempts a running
/// transaction; an [`Event::HandoffToHuman`] streams to the SSE feed when it is the
/// human's turn. Returns `{queued: true, ahead}`.
///
/// # Errors
///
/// Returns an [`ApiError`] (`500`) if the shared coordinator cannot be opened.
pub async fn request_edit(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
) -> Result<Json<RequestEditResponse>, ApiError> {
    let coord = state.coordinator()?;
    let outcome = coord.request_edit(&theme, &file);
    Ok(Json(RequestEditResponse {
        queued: outcome.queued,
        ahead: outcome.ahead,
    }))
}

/// `DELETE /api/articles/{theme}/{file}/request-edit` — cancel a standing human
/// edit reservation (B3).
///
/// Routes to [`Coordinator::cancel_request_edit`] on the shared coordinator,
/// removing the oldest pending reservation for the article and letting any agent
/// transactions that were held behind it proceed. Returns
/// `{cancelled: <bool>}` — `false` when no matching reservation was pending (an
/// idempotent no-op).
///
/// # Errors
///
/// Returns an [`ApiError`] (`500`) if the shared coordinator cannot be opened.
pub async fn cancel_request_edit(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let coord = state.coordinator()?;
    let cancelled = coord.cancel_request_edit(&theme, &file);
    Ok(Json(serde_json::json!({ "cancelled": cancelled })))
}

/// The request body for [`start_task`].
#[derive(Debug, Deserialize)]
pub struct StartTaskRequest {
    /// The theme the article lives in (created if absent by the master).
    pub theme: String,
    /// The article file name to write (created if absent by the master).
    pub file: String,
    /// The natural-language writing task the slave should carry out.
    pub task: String,
}

/// The response body for [`start_task`].
#[derive(Debug, Serialize)]
pub struct StartTaskResponse {
    /// The id assigned to this writing run; correlate it with the SSE feed.
    pub task_id: u64,
    /// The theme the run targets.
    pub theme: String,
    /// The article file the run targets.
    pub file: String,
}

/// `POST /api/tasks` — start a writing run in the background.
///
/// The synchronous [`Master::run_one`] is dispatched onto
/// [`tokio::task::spawn_blocking`] with this state's broadcast sink installed, so
/// the run's [`Event`]s stream to every SSE subscriber while the async runtime
/// stays unblocked. The handler returns a `task_id` immediately, without waiting
/// for the run to finish; progress and completion are observed through
/// [`events`].
///
/// The slave writes under an [`WriterId::Agent`] identity tagged with the default
/// model id, matching the `demo` binary's provenance convention.
///
/// # Errors
///
/// This handler does not fail synchronously: it always accepts the task and
/// returns `202 Accepted` with the id. A failure inside the background run
/// surfaces as a `Finished { outcome: "failed" }` (or `SlaveReported`) event on
/// the SSE stream rather than as an HTTP error, since the HTTP response has
/// already been sent.
pub async fn start_task(
    State(state): State<AppState>,
    Json(req): Json<StartTaskRequest>,
) -> impl IntoResponse {
    let task_id = state.next_task_id.fetch_add(1, Ordering::SeqCst);

    let workspace_root = state.workspace_root.clone();
    let client = state.client.clone();
    let events = state.event_sink();
    let theme = req.theme.clone();
    let file = req.file.clone();

    // Run the synchronous Master/Slave orchestration off the async runtime so it
    // never blocks a tokio worker. The run narrates itself onto the broadcast
    // channel via `events`; its `SlaveReport` is logged from the blocking task
    // (the HTTP response has already been sent by then).
    tokio::task::spawn_blocking(move || {
        run_writing_task(workspace_root, client, events, req);
    });

    (
        StatusCode::ACCEPTED,
        Json(StartTaskResponse {
            task_id,
            theme,
            file,
        }),
    )
}

/// Executes one synchronous writing run on the blocking thread pool.
///
/// Builds a tool-less master [`Session`] sharing `client`, installs `events` on
/// it under the `"master"` role (so the run's events — and the slave's, which the
/// engine propagates — flow to every SSE subscriber), and drives
/// [`Master::run_one`] for the requested article. Setup or run failures are
/// already reflected on the event stream (`Finished`/`SlaveReported`); this
/// helper returns nothing because the caller has already answered the HTTP
/// request.
fn run_writing_task(
    workspace_root: PathBuf,
    client: Client,
    events: Arc<dyn EventSink>,
    req: StartTaskRequest,
) {
    let ws = match Workspace::open(&workspace_root) {
        Ok(ws) => ws,
        Err(e) => {
            // No workspace, no run: announce the failure so the UI can react.
            events.emit(Event::Finished {
                outcome: "failed".to_string(),
            });
            eprintln!("webui: cannot open workspace for task: {e}");
            return;
        }
    };

    // A tool-less master session that only shares the client and the event sink
    // (v0 orchestration is deterministic Rust). The sink is installed under the
    // "master" role; the engine hands the same sink to the spawned slave.
    let mut master_session = Session::new(
        client,
        "You are the orchestrator. You create themes and dispatch writing agents.",
        ToolRegistry::new(),
        SessionOptions::default(),
    );
    master_session.set_event_sink("master", events);
    let mut master = Master::new(master_session, ws);

    let writer = WriterId::Agent {
        model: SessionOptions::default().model.as_str().to_string(),
        label: "slave-1".to_string(),
    };
    let slave_task = SlaveTask {
        theme: req.theme,
        file_name: req.file,
        task: req.task,
        writer,
        system_prompt: None,
        skill: None,
    };

    if let Err(e) = master.run_one(slave_task) {
        eprintln!("webui: writing task setup failed: {e}");
    }
}

/// The request body for [`master_chat`].
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// The natural-language goal the master should plan and delegate for the
    /// theme (e.g. "outline and draft a three-part guide to X").
    pub goal: String,
    /// The id of the writing skill dispatched writers should run under, or `None`
    /// for the engine's built-in writer prompt.
    ///
    /// Back-compat single-skill field. Prefer [`skill_ids`](ChatRequest::skill_ids)
    /// for an ordered multi-skill stack; when both are given, `skill_ids` wins and
    /// this is ignored.
    #[serde(default)]
    pub skill_id: Option<String>,
    /// The ordered stack of writing-skill ids dispatched writers should run under
    /// (kernel §10), earliest first; on conflicting directives a **later** id
    /// overrides an earlier one. Empty (the default) falls back to
    /// [`skill_id`](ChatRequest::skill_id), then to the engine's built-in writer
    /// prompt.
    #[serde(default)]
    pub skill_ids: Vec<String>,
    /// The model id slaves write under, or `None` for the default.
    ///
    /// This may be a bare family id (e.g. `deepseek-v4-pro`) or — per kernel §9,
    /// for reproducible attribution — an **exact dated snapshot** such as
    /// `deepseek-v4-pro-2026-05-01`. Whatever is supplied is normalized through
    /// [`Model::pinned`] and flows through to the slave's wire request, its git
    /// author, and the article's contributor list, so the recorded provenance
    /// names the precise model that produced the text.
    #[serde(default)]
    pub slave_model: Option<String>,
}

/// One article the master created during a [`master_chat`] run, as surfaced in the
/// run [`Plan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanCreated {
    /// The theme the article belongs to.
    pub theme: String,
    /// The created article's file name.
    pub file: String,
    /// The article's human-readable title.
    pub title: String,
    /// The parent article's file name in the theme hierarchy, or `null` for a
    /// top-level article.
    pub parent: Option<String>,
}

/// One writer dispatch the master performed during a [`master_chat`] run, as
/// surfaced in the run [`Plan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanDispatched {
    /// The article file the writer was dispatched for, when it could be paired to
    /// a created article (best-effort, in dispatch order); empty when unknown.
    pub file: String,
    /// The writer identity tag the article was written under
    /// (`"<model>/<label>"`), matching the article's provenance.
    pub writer: String,
    /// The writer's terminal status (`"done"` / `"needs_human"` / `"failed"`).
    pub status: String,
    /// The writer's short summary of what it produced.
    pub summary: String,
}

/// The structured editorial plan a [`master_chat`] run produced (B1): the articles
/// it created and the writer dispatches it performed.
///
/// It is derived deterministically from the run's products — `created` from the
/// theme outline the run grew, `dispatched` from the collected
/// [`SlaveReport`]s — so the front-end can render the plan without re-parsing the
/// master's prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// The articles created during the run, in reading order.
    pub created: Vec<PlanCreated>,
    /// The writer dispatches performed during the run, in dispatch order.
    pub dispatched: Vec<PlanDispatched>,
}

/// The response body for [`master_chat`]: the master's terminal outcome and the
/// structured [`Plan`] it produced.
#[derive(Debug, Serialize)]
pub struct ChatResponse {
    /// The master run's outcome label (`"done"` / `"need_human"`).
    pub outcome: String,
    /// The master's closing message (its summary of what it produced).
    pub message: String,
    /// Every writer's structured report collected during the run, in dispatch
    /// order.
    pub reports: Vec<SlaveReport>,
    /// The structured plan derived from the run's products: created articles and
    /// writer dispatches (B1).
    pub plan: Plan,
}

/// A writing-skill stack resolved from a [`ChatRequest`], ready to drive a run.
///
/// It carries both the loaded `bodies` (the static fallback prompt) and the
/// `dir` + `ids` (the on-disk source each writer re-reads per round, kernel §4),
/// so they travel together through [`run_master_chat`] without inflating its
/// argument list.
struct ResolvedSkillStack {
    /// The skills directory the `ids` live in.
    dir: PathBuf,
    /// The ordered stack of skill ids, earliest first (later overrides earlier).
    ids: Vec<String>,
    /// The loaded skill bodies, in the same order as `ids`.
    bodies: Vec<String>,
}

impl ResolvedSkillStack {
    /// Resolves the requested skill stack against `skills_dir`.
    ///
    /// `skill_ids` (multi, kernel §10) takes precedence; otherwise the single
    /// back-compat `skill_id` is treated as a one-element stack; otherwise the
    /// stack is empty (the engine's built-in writer prompt). Each id is loaded
    /// now so a bad id fails fast.
    fn resolve(skills_dir: &std::path::Path, req: &ChatRequest) -> Result<Self, skill::SkillError> {
        let ids: Vec<String> = if !req.skill_ids.is_empty() {
            req.skill_ids.clone()
        } else {
            req.skill_id.clone().into_iter().collect()
        };
        let bodies = skill::load_skills_ordered(skills_dir, &ids)?
            .into_iter()
            .map(|s| s.body)
            .collect();
        Ok(Self {
            dir: skills_dir.to_path_buf(),
            ids,
            bodies,
        })
    }
}

/// `POST /api/themes/{theme}/chat` — drive an LLM master to plan and delegate a
/// goal within a theme, returning its outcome.
///
/// Unlike [`start_task`] (a single fire-and-forget writer run), this runs the
/// full [`Master::run_goal`] orchestration: the model plans the article set,
/// organizes the hierarchy, dispatches one writer per article, and decides when
/// the goal is met. The chosen writing skills — an ordered stack via `skill_ids`
/// (kernel §10; later overrides earlier on conflict), or the single back-compat
/// `skill_id` — set the persona every dispatched writer runs under, and
/// `slave_model` selects the model they write with. The theme is created if absent
/// so the master operates within it.
///
/// The handler **blocks** until the run finishes and returns the master's
/// [`ChatResponse`] (its reply); live progress streams to every SSE subscriber
/// through [`events`] meanwhile. The synchronous run executes on
/// [`tokio::task::spawn_blocking`] so it never stalls the async runtime.
///
/// # Errors
///
/// Returns an [`ApiError`]: `400` if the goal is empty, `404` if any requested
/// skill id names no skill, or `500` if the workspace cannot be set up, the master
/// task panics, or the run fails fatally.
pub async fn master_chat(
    State(state): State<AppState>,
    AxumPath(theme): AxumPath<String>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, ApiError> {
    if req.goal.trim().is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "goal must not be empty".to_string(),
        });
    }
    // Resolve the ordered writing-skill stack up front, so a bad id is a clean 404
    // rather than a failure deep inside the run.
    let skill_stack = ResolvedSkillStack::resolve(&state.skills_dir, &req).map_err(skill_error)?;
    // Normalize the requested model id through `Model::pinned`: a bare family
    // alias collapses to its canonical id, while a pinned dated snapshot (kernel
    // §9) is carried verbatim. The normalized id is what every dispatched slave
    // signs its commits and contributor entries with.
    let slave_model = match &req.slave_model {
        Some(id) => Model::pinned(id.clone()).as_str().to_string(),
        None => SessionOptions::default().model.as_str().to_string(),
    };

    let workspace_root = state.workspace_root.clone();
    let client = state.client.clone();
    let events = state.event_sink();
    let goal = req.goal.clone();
    // Snapshot the theme's articles *before* the run so the plan's `created` list
    // is exactly the articles this run added (the theme may already hold articles
    // from a previous run). A missing/empty theme yields an empty snapshot.
    let before: std::collections::BTreeSet<String> = outline_snapshot(&workspace_root, &theme)
        .into_iter()
        .map(|o| o.file)
        .collect();

    // `theme` and `slave_model` are needed again after the run to build the plan;
    // hand owned clones to the blocking closure.
    let run_theme = theme.clone();
    let run_model = slave_model.clone();
    let result = tokio::task::spawn_blocking(move || {
        run_master_chat(
            workspace_root,
            client,
            events,
            &run_theme,
            &goal,
            &run_model,
            skill_stack,
        )
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("master task panicked: {e}"),
    })?;

    let outcome = result.map_err(|message| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message,
    })?;

    // Derive the structured plan from the run's products: `created` from the
    // articles the theme outline grew by, `dispatched` from the collected reports.
    let after = outline_snapshot(&state.workspace_root, &theme);
    let plan = build_plan(&theme, &before, &after, &outcome.reports, &slave_model);

    Ok(Json(ChatResponse {
        outcome: outcome.outcome,
        message: outcome.message,
        reports: outcome.reports,
        plan,
    }))
}

/// Reads a theme's article outline, returning an empty list when the theme is
/// absent or unreadable.
///
/// This is a best-effort read used only to derive the [`master_chat`] plan, so a
/// failure (a theme that was never created, a transient I/O error) degrades to an
/// empty outline rather than failing the request — the run itself already
/// succeeded by the time this is called.
fn outline_snapshot(workspace_root: &Path, theme: &str) -> Vec<ArticleOutline> {
    Workspace::open(workspace_root)
        .and_then(|ws| ws.article_outline(theme))
        .unwrap_or_default()
}

/// Builds the structured [`Plan`] from a [`master_chat`] run's products.
///
/// `created` is every article present in `after` whose file was not in the
/// `before` snapshot, in reading order — exactly the articles this run added.
/// `dispatched` is one entry per collected [`SlaveReport`], in dispatch order:
/// each is paired (best-effort) to a created article's file by position, and
/// carries `writer` = `slave_model/slave-<n>` (the engine's dispatch label
/// convention), the report status, and its summary.
fn build_plan(
    theme: &str,
    before: &std::collections::BTreeSet<String>,
    after: &[ArticleOutline],
    reports: &[SlaveReport],
    slave_model: &str,
) -> Plan {
    let created: Vec<PlanCreated> = after
        .iter()
        .filter(|o| !before.contains(&o.file))
        .map(|o| PlanCreated {
            theme: theme.to_string(),
            file: o.file.clone(),
            title: o.title.clone(),
            parent: o.parent.clone(),
        })
        .collect();

    let dispatched: Vec<PlanDispatched> = reports
        .iter()
        .enumerate()
        .map(|(i, report)| {
            // Pair the n-th report to the n-th created article when one exists;
            // the file is best-effort context, not load-bearing for the run.
            let file = created.get(i).map(|c| c.file.clone()).unwrap_or_default();
            PlanDispatched {
                file,
                // The engine labels the n-th dispatched slave `slave-<n+1>`
                // (`OrchestratorState::dispatch`); mirror that so the writer tag
                // matches the article's recorded provenance.
                writer: format!("{slave_model}/slave-{}", i + 1),
                status: slave_status_str(&report.status).to_string(),
                summary: report.summary.clone(),
            }
        })
        .collect();

    Plan {
        created,
        dispatched,
    }
}

/// Renders a [`SlaveStatus`] as the wire string the contract uses
/// (`"done"` / `"needs_human"` / `"failed"`).
fn slave_status_str(status: &SlaveStatus) -> &'static str {
    match status {
        SlaveStatus::Done => "done",
        SlaveStatus::NeedsHuman => "needs_human",
        SlaveStatus::Failed => "failed",
    }
}

/// Runs one synchronous [`Master::run_goal`] orchestration on the blocking thread
/// pool, scoped to `theme`.
///
/// Opens the workspace, ensures `theme` exists (idempotently), builds a master
/// session sharing `client` and the `events` sink under the `"master"` role, and
/// drives the goal. The goal is framed so the master stays within `theme`.
///
/// The resolved `skills` stack supplies both the static fallback prompt (its
/// loaded bodies, earliest first; later overrides earlier on conflict, kernel §10)
/// and the on-disk source every dispatched writer re-reads each round (kernel §4)
/// — so editing any active skill mid-run affects subsequent rounds. An empty stack
/// reproduces the engine's built-in writer prompt. Returns the
/// [`GoalOutcome`](crate::engine::GoalOutcome) or a rendered error string.
fn run_master_chat(
    workspace_root: PathBuf,
    client: Client,
    events: Arc<dyn EventSink>,
    theme: &str,
    goal: &str,
    slave_model: &str,
    skills: ResolvedSkillStack,
) -> Result<crate::engine::GoalOutcome, String> {
    let mut ws =
        Workspace::open(&workspace_root).map_err(|e| format!("cannot open workspace: {e}"))?;
    // Ensure the theme exists so the master operates within it (a pre-existing
    // theme is fine).
    if let Err(e) = ws.create_theme(theme)
        && !matches!(e, crate::tool::ToolError::Lock(_))
    {
        return Err(format!("cannot create theme `{theme}`: {e}"));
    }

    let mut master_session = Session::new(
        client,
        "orchestrator",
        ToolRegistry::new(),
        SessionOptions::default(),
    );
    master_session.set_event_sink("master", events);
    let mut master = Master::new(master_session, ws);

    let scoped_goal = format!(
        "You are working within the existing theme `{theme}`. Every article you \
         create, organize, or dispatch a writer for must belong to this theme; do \
         not create other themes.\n\nGoal:\n{goal}"
    );
    // Install the same skills as an on-disk stack source so every dispatched
    // writer re-reads and re-stacks them per round (kernel §4, §10); the loaded
    // bodies are only the static fallback if a read fails. An empty stack means no
    // skill source (engine default writer prompt).
    let slave_skill = (!skills.ids.is_empty()).then_some(SlaveSkill {
        dir: skills.dir,
        ids: skills.ids,
    });
    master
        .run_goal_with_skills(
            &scoped_goal,
            SessionOptions::default(),
            slave_model,
            &skills.bodies,
            slave_skill,
        )
        .map_err(|e| format!("master run failed: {e}"))
}

// ===========================================================================
// SSE
// ===========================================================================

/// `GET /api/events` — the live Server-Sent Events feed of [`Event`]s.
///
/// Subscribes to the shared broadcast channel and streams every [`Event`] as an
/// SSE `data:` line carrying the event's JSON. The connection stays open; each
/// writing run started via [`start_task`] fans its events into this stream.
///
/// Events that cannot be serialized, and broadcast lag/closure errors, are
/// skipped rather than tearing down the stream — the feed is best-effort live
/// visualization, not a durable transcript. A periodic keep-alive comment holds
/// the connection open through idle gaps.
pub async fn events(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>> {
    use tokio_stream::StreamExt;

    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| {
        match item {
            // Encode each event as JSON; drop one that fails to serialize.
            Ok(event) => SseEvent::default().json_data(&event).ok().map(Ok),
            // A lagged or closed broadcast yields an error; skip it and keep the
            // stream alive for the next event.
            Err(_) => None,
        }
    });
    // A short keep-alive interval (2s) makes the first byte flow soon after the
    // headers, so a streaming proxy (the dev Vite proxy / the prod Hono BFF)
    // forwards the response promptly and the browser's `EventSource` fires
    // `onopen` quickly (the top-bar indicator reaches `live` instead of sticking
    // at `connecting` through an idle gap).
    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(2)),
    )
}

#[cfg(test)]
mod tests {
    //! Handler unit tests that drive the router in-process.
    //!
    //! These never perform a live DeepSeek call: they exercise the read endpoints
    //! (themes / articles / content / history / diff / undo) and the SSE bridge
    //! against a `tempdir` workspace, and assert that `POST /api/tasks` accepts a
    //! task and returns an id *without* awaiting the (network-bound) run — the
    //! blocking task it spawns is allowed to fail to start a chat, which the test
    //! does not observe.

    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::observe::{Event, EventSink};

    /// A network-free client so an [`AppState`] can be built; no test here drives
    /// a chat completion.
    fn offline_client() -> Client {
        Client::builder()
            .api_key("test-key")
            .build()
            .expect("offline client")
    }

    /// Collects an axum [`Response`]'s body and deserializes it as JSON, for the
    /// handlers that now return a `Response` (so they can vary their body shape by
    /// query parameter, e.g. plain vs `?format=rich`).
    async fn body_json<T: for<'de> Deserialize<'de>>(resp: Response) -> T {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("collect body");
        serde_json::from_slice(&bytes).expect("deserialize body")
    }

    /// Builds an [`AppState`] over a fresh `tempdir` workspace, returning the temp
    /// dir guard (which must outlive the state) alongside it.
    fn state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(dir.path().to_path_buf(), offline_client());
        (dir, state)
    }

    #[test]
    fn broadcast_sink_forwards_events_to_subscribers() {
        let (events, _keep) = broadcast::channel::<Event>(16);
        let mut rx = events.subscribe();
        let sink = BroadcastSink(events.clone());
        sink.emit(Event::RoundStarted { round: 7 });
        match rx.try_recv().expect("an event was buffered") {
            Event::RoundStarted { round } => assert_eq!(round, 7),
            other => panic!("expected RoundStarted, got {other:?}"),
        }
    }

    #[test]
    fn broadcast_sink_emit_is_lossy_without_subscribers() {
        // With no subscriber the send returns Err inside the sink, which it
        // swallows: emitting must never panic or block.
        let (events, _keep) = broadcast::channel::<Event>(16);
        drop(_keep);
        // Re-create with the sender only (no receivers alive).
        let (events2, rx) = broadcast::channel::<Event>(16);
        drop(rx);
        let sink = BroadcastSink(events2);
        sink.emit(Event::Finished {
            outcome: "done".to_string(),
        });
        let _ = events; // silence unused in case of edits
    }

    #[test]
    fn event_sink_installs_a_broadcast_backed_sink() {
        // The state's sink, when emitted to, reaches a fresh subscriber — proving
        // AppState::event_sink is wired to the same channel `events` subscribes to.
        let (_d, state) = state();
        let mut rx = state.events.subscribe();
        let sink = state.event_sink();
        sink.emit(Event::ModelMessage {
            text: "hi".to_string(),
        });
        match rx.try_recv().expect("event buffered") {
            Event::ModelMessage { text } => assert_eq!(text, "hi"),
            other => panic!("expected ModelMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_themes_reflects_workspace_dirs() {
        let (_d, state) = state();
        // Seed two themes through the workspace model.
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_theme("python").expect("theme");
        }
        let Json(resp) = list_themes(State(state)).await.expect("themes ok");
        assert_eq!(resp.themes, vec!["python", "rust"]);
    }

    #[tokio::test]
    async fn list_articles_returns_reading_order() {
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
            ws.create_article("rust", "b.md", "B", None).expect("b");
        }
        let Json(resp) = list_articles(State(state), AxumPath("rust".to_string()))
            .await
            .expect("articles ok");
        assert_eq!(resp.theme, "rust");
        let files: Vec<&str> = resp.articles.iter().map(|o| o.file.as_str()).collect();
        assert_eq!(files, vec!["a.md", "b.md"]);
        assert!(resp.articles.iter().all(|o| o.depth == 0));
    }

    #[tokio::test]
    async fn articles_outline_reflects_hierarchy() {
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
            ws.create_article("rust", "b.md", "B", None).expect("b");
            ws.set_parent("rust", "b.md", Some("a.md")).expect("parent");
        }
        let Json(resp) = list_articles(State(state), AxumPath("rust".to_string()))
            .await
            .expect("articles ok");
        let b = resp.articles.iter().find(|o| o.file == "b.md").unwrap();
        assert_eq!(b.parent.as_deref(), Some("a.md"));
        assert_eq!(b.depth, 1);
    }

    #[tokio::test]
    async fn theme_config_get_and_put_round_trip() {
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
        }
        // Default is empty.
        let Json(cfg) = get_theme_config(State(state.clone()), AxumPath("rust".to_string()))
            .await
            .expect("config ok");
        assert_eq!(cfg, ThemeConfig::default());

        // Put a config, then read it back.
        let new_cfg = ThemeConfig {
            description: "a guide".into(),
            default_skill: Some("functional-writing".into()),
            default_skill_ids: vec!["functional-writing".into(), "concise".into()],
            slave_model: Some("deepseek-v4-pro".into()),
        };
        let Json(stored) = put_theme_config(
            State(state.clone()),
            AxumPath("rust".to_string()),
            Json(new_cfg.clone()),
        )
        .await
        .expect("put ok");
        assert_eq!(stored, new_cfg);

        let Json(cfg) = get_theme_config(State(state), AxumPath("rust".to_string()))
            .await
            .expect("config ok");
        assert_eq!(cfg, new_cfg);
    }

    #[tokio::test]
    async fn set_parent_and_reorder_endpoints() {
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            for f in ["a.md", "b.md", "c.md"] {
                ws.create_article("rust", f, f, None).expect("article");
            }
        }
        // Set b.md's parent to a.md.
        set_article_parent(
            State(state.clone()),
            AxumPath(("rust".to_string(), "b.md".to_string())),
            Json(SetParentRequest {
                parent: Some("a.md".to_string()),
            }),
        )
        .await
        .expect("set parent ok");

        // Reorder to c, a, b.
        reorder_articles(
            State(state.clone()),
            AxumPath("rust".to_string()),
            Json(ReorderRequest {
                order: vec!["c.md".into(), "a.md".into(), "b.md".into()],
            }),
        )
        .await
        .expect("reorder ok");

        let Json(resp) = list_articles(State(state), AxumPath("rust".to_string()))
            .await
            .expect("articles ok");
        let files: Vec<&str> = resp.articles.iter().map(|o| o.file.as_str()).collect();
        assert_eq!(files, vec!["c.md", "a.md", "b.md"]);
        let b = resp.articles.iter().find(|o| o.file == "b.md").unwrap();
        assert_eq!(b.parent.as_deref(), Some("a.md"));
    }

    #[tokio::test]
    async fn reorder_rejects_non_permutation() {
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
        }
        let err = reorder_articles(
            State(state),
            AxumPath("rust".to_string()),
            Json(ReorderRequest {
                order: vec!["a.md".into(), "ghost.md".into()],
            }),
        )
        .await
        .expect_err("bad order rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn list_skills_reads_the_skills_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skills = tempfile::tempdir().expect("skills dir");
        std::fs::write(
            skills.path().join("functional-writing.md"),
            "---\nname: Functional\ndescription: plain prose\n---\nWrite plainly.",
        )
        .expect("write skill");
        let state = AppState::new(dir.path().to_path_buf(), offline_client())
            .with_skills_dir(skills.path().to_path_buf());

        let Json(resp) = list_skills(State(state)).await.expect("skills ok");
        let arr = resp["skills"].as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "functional-writing");
        assert_eq!(arr[0]["name"], "Functional");
    }

    #[tokio::test]
    async fn master_chat_rejects_empty_goal() {
        let (_d, state) = state();
        let err = master_chat(
            State(state),
            AxumPath("rust".to_string()),
            Json(ChatRequest {
                goal: "   ".to_string(),
                skill_id: None,
                skill_ids: Vec::new(),
                slave_model: None,
            }),
        )
        .await
        .expect_err("empty goal rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn master_chat_unknown_skill_is_404() {
        let (_d, state) = state();
        let err = master_chat(
            State(state),
            AxumPath("rust".to_string()),
            Json(ChatRequest {
                goal: "write something".to_string(),
                skill_id: Some("nope".to_string()),
                skill_ids: Vec::new(),
                slave_model: None,
            }),
        )
        .await
        .expect_err("unknown skill rejected");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn master_chat_unknown_skill_in_stack_is_404() {
        // A bad id anywhere in the multi-skill stack is a clean 404, even when an
        // earlier id in the same stack resolves.
        let (_d, mut state) = state();
        let skills = tempfile::tempdir().expect("skills dir");
        std::fs::write(skills.path().join("ok.md"), "OK voice.").expect("write ok skill");
        state = state.with_skills_dir(skills.path().to_path_buf());
        let err = master_chat(
            State(state),
            AxumPath("rust".to_string()),
            Json(ChatRequest {
                goal: "write something".to_string(),
                skill_id: None,
                skill_ids: vec!["ok".to_string(), "nope".to_string()],
                slave_model: None,
            }),
        )
        .await
        .expect_err("unknown skill in stack rejected");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_article_returns_content() {
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
            ws.acquire_lock("rust", "a.md", &WriterId::Human)
                .expect("lock");
            ws.write_article("rust", "a.md", "hello body", &WriterId::Human)
                .expect("write");
        }
        let resp = get_article(
            State(state),
            AxumPath(("rust".to_string(), "a.md".to_string())),
            Query(GetArticleQuery::default()),
        )
        .await
        .expect("content ok");
        let body: serde_json::Value = body_json(resp).await;
        assert_eq!(body["content"], "hello body");
    }

    #[tokio::test]
    async fn get_missing_article_is_404() {
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
        }
        let err = get_article(
            State(state),
            AxumPath(("rust".to_string(), "ghost.md".to_string())),
            Query(GetArticleQuery::default()),
        )
        .await
        .expect_err("missing article errors");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn history_and_diff_reflect_commits() {
        let (_d, state) = state();
        // Make two committed versions of an article through workspace + vcs, the
        // same way the editors do, so history has two entries and a diff exists.
        let theme = "rust";
        let file = "a.md";
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme(theme).expect("theme");
            ws.create_article(theme, file, "A", None).expect("article");
            ws.acquire_lock(theme, file, &WriterId::Human)
                .expect("lock");
            ws.write_article(theme, file, "version one\n", &WriterId::Human)
                .expect("w1");
        }
        let vcs = state.open_vcs().expect("vcs");
        let rel = Path::new(theme).join(file);
        vcs.commit_file(&rel, &WriterId::Human, "edit: v1")
            .expect("commit v1");
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.acquire_lock(theme, file, &WriterId::Human)
                .expect("lock");
            ws.write_article(theme, file, "version two\n", &WriterId::Human)
                .expect("w2");
        }
        vcs.commit_file(&rel, &WriterId::Human, "edit: v2")
            .expect("commit v2");

        // History endpoint: two commits, newest first.
        let Json(hist) = article_history(
            State(state.clone()),
            AxumPath((theme.to_string(), file.to_string())),
        )
        .await
        .expect("history ok");
        let entries = hist["history"].as_array().expect("array");
        assert_eq!(entries.len(), 2);

        // Diff endpoint: HEAD~1..HEAD shows the version-one → version-two change.
        let Json(diff) = article_diff(
            State(state),
            AxumPath((theme.to_string(), file.to_string())),
            Query(DiffQuery {
                from: Some("HEAD~1".to_string()),
                to: Some("HEAD".to_string()),
            }),
        )
        .await
        .expect("diff ok");
        let patch = diff["diff"].as_str().expect("diff string");
        assert!(patch.contains("version one"), "patch was: {patch}");
        assert!(patch.contains("version two"), "patch was: {patch}");
    }

    #[tokio::test]
    async fn blame_endpoint_attributes_each_line_to_its_writer() {
        // Kernel §9: the blame endpoint must report per-line authorship that
        // distinguishes a human edit from a model edit. Build a two-writer
        // history, then assert each line maps to the expected author.
        let (_d, state) = state();
        let theme = "rust";
        let file = "a.md";
        let agent = WriterId::Agent {
            model: "deepseek-v4-flash".to_string(),
            label: "slave-1".to_string(),
        };
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme(theme).expect("theme");
            ws.create_article(theme, file, "A", None).expect("article");
            ws.acquire_lock(theme, file, &agent).expect("lock");
            ws.write_article(
                theme,
                file,
                "agent first\nagent second\nagent third\n",
                &agent,
            )
            .expect("w1");
        }
        let vcs = state.open_vcs().expect("vcs");
        let rel = Path::new(theme).join(file);
        vcs.commit_file(&rel, &agent, "edit: v1")
            .expect("commit v1");
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.acquire_lock(theme, file, &WriterId::Human)
                .expect("lock");
            ws.write_article(
                theme,
                file,
                "agent first\nhuman revised\nagent third\n",
                &WriterId::Human,
            )
            .expect("w2");
        }
        vcs.commit_file(&rel, &WriterId::Human, "edit: v2")
            .expect("commit v2");

        let Json(out) = article_blame(
            State(state),
            AxumPath((theme.to_string(), file.to_string())),
        )
        .await
        .expect("blame ok");
        let lines = out["blame"].as_array().expect("array");
        assert_eq!(lines.len(), 3);

        const AGENT: &str = "deepseek-v4-flash/slave-1 <agent@ai-write.local>";
        const HUMAN: &str = "human <human@ai-write.local>";
        assert_eq!(lines[0]["line_no"].as_u64().unwrap(), 1);
        assert_eq!(lines[0]["author"].as_str().unwrap(), AGENT);
        assert_eq!(lines[1]["line_no"].as_u64().unwrap(), 2);
        assert_eq!(lines[1]["author"].as_str().unwrap(), HUMAN);
        assert_eq!(lines[2]["line_no"].as_u64().unwrap(), 3);
        assert_eq!(lines[2]["author"].as_str().unwrap(), AGENT);
    }

    #[tokio::test]
    async fn start_task_accepts_and_returns_an_id_without_running_live() {
        // POST /api/tasks must answer immediately with a task id. The blocking
        // run it spawns would need the network to make progress; we never await
        // it, and the offline client guarantees it cannot reach a live API.
        let (_d, state) = state();
        let resp = start_task(
            State(state),
            Json(StartTaskRequest {
                theme: "rust".to_string(),
                file: "intro.md".to_string(),
                task: "Write something.".to_string(),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn started_task_setup_emits_events_on_the_stream() {
        // Even with an offline client, the master's deterministic setup runs and
        // the slave thread emits its lifecycle (SlaveSpawned) before the first
        // round fails to reach the network. Subscribe first, then start, and
        // confirm at least one event lands — proving the spawn_blocking → sink →
        // broadcast bridge is connected end to end.
        let (_d, state) = state();
        let recorder: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

        // Bridge: drain the broadcast into the recorder on a background task.
        let mut rx = state.events.subscribe();
        let rec = Arc::clone(&recorder);
        let drain = tokio::spawn(async move {
            // Collect events for a short, bounded window.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, rx.recv()).await {
                    Ok(Ok(ev)) => {
                        let kind = match ev {
                            Event::SessionStarted { .. } => "SessionStarted",
                            Event::RoundStarted { .. } => "RoundStarted",
                            Event::ModelMessage { .. } => "ModelMessage",
                            Event::ToolCalled { .. } => "ToolCalled",
                            Event::ToolResult { .. } => "ToolResult",
                            Event::EditCommitted { .. } => "EditCommitted",
                            Event::SlaveSpawned { .. } => "SlaveSpawned",
                            Event::SlaveReported { .. } => "SlaveReported",
                            Event::Finished { .. } => "Finished",
                            Event::TxnAcquired { .. } => "TxnAcquired",
                            Event::TxnQueued { .. } => "TxnQueued",
                            Event::TxnReleased { .. } => "TxnReleased",
                            Event::HandoffToHuman { .. } => "HandoffToHuman",
                        };
                        rec.lock().expect("not poisoned").push(kind);
                        if kind == "SlaveReported" {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        });

        let _ = start_task(
            State(state),
            Json(StartTaskRequest {
                theme: "rust".to_string(),
                file: "intro.md".to_string(),
                task: "Write something.".to_string(),
            }),
        )
        .await;

        drain.await.expect("drain task");
        let kinds = recorder.lock().expect("not poisoned");
        assert!(
            kinds.contains(&"SlaveSpawned"),
            "expected at least a SlaveSpawned event, got {kinds:?}"
        );
    }

    // -- B1: human write through the coordinator (PUT /api/articles) ----------

    #[tokio::test]
    async fn put_article_writes_through_coordinator_and_commits() {
        // A human PUT must land as exactly one commit authored by the human, with
        // the new body persisted. The endpoint opens its own coordinator over the
        // same workspace root, so no live API is involved.
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
        }

        let Json(resp) = put_article(
            State(state.clone()),
            AxumPath(("rust".to_string(), "a.md".to_string())),
            Json(PutArticleRequest {
                text: "human body\n".to_string(),
            }),
        )
        .await
        .expect("put ok");
        assert_eq!(resp.theme, "rust");
        assert_eq!(resp.file, "a.md");
        let sha = resp.committed.expect("a commit was produced");

        // The body is persisted.
        let got: serde_json::Value = body_json(
            get_article(
                State(state.clone()),
                AxumPath(("rust".to_string(), "a.md".to_string())),
                Query(GetArticleQuery::default()),
            )
            .await
            .expect("read ok"),
        )
        .await;
        assert_eq!(got["content"], "human body\n");

        // Exactly one commit on the body, authored by the human, and the returned
        // sha matches it.
        let vcs = state.open_vcs().expect("vcs");
        let hist = vcs.history(Path::new("rust/a.md")).expect("history");
        assert_eq!(hist.len(), 1, "one human write is one commit");
        assert_eq!(hist[0].id, sha);
        assert!(
            hist[0].author.starts_with("human "),
            "human-authored, got {}",
            hist[0].author
        );
    }

    #[tokio::test]
    async fn put_missing_article_is_404() {
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
        }
        let err = put_article(
            State(state),
            AxumPath(("rust".to_string(), "ghost.md".to_string())),
            Json(PutArticleRequest {
                text: "x".to_string(),
            }),
        )
        .await
        .expect_err("missing article errors");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_article_twice_grows_history_by_one_each() {
        // Two successive human writes are two transactions, each its own single
        // commit (one cognitive unit = one commit, kernel §5), and the latest body
        // wins.
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
        }
        for text in ["first body\n", "second body\n"] {
            put_article(
                State(state.clone()),
                AxumPath(("rust".to_string(), "a.md".to_string())),
                Json(PutArticleRequest {
                    text: text.to_string(),
                }),
            )
            .await
            .expect("put ok");
        }

        let vcs = state.open_vcs().expect("vcs");
        let hist = vcs.history(Path::new("rust/a.md")).expect("history");
        assert_eq!(hist.len(), 2, "two human writes, two commits");

        let got: serde_json::Value = body_json(
            get_article(
                State(state),
                AxumPath(("rust".to_string(), "a.md".to_string())),
                Query(GetArticleQuery::default()),
            )
            .await
            .expect("read ok"),
        )
        .await;
        assert_eq!(got["content"], "second body\n");
    }

    // -- B2: rich (author-coloured) article view ------------------------------

    #[tokio::test]
    async fn get_article_rich_attributes_runs_across_writers() {
        // A human PUT writes the whole body, then an agent edits one word through
        // the coordinator. The rich view must blend the two authors run-by-run,
        // while the plain GET still returns the joined text (back-compatible).
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
        }

        // Human writes the full body through the coordinator-backed PUT.
        put_article(
            State(state.clone()),
            AxumPath(("rust".to_string(), "a.md".to_string())),
            Json(PutArticleRequest {
                text: "the quick brown fox".to_string(),
            }),
        )
        .await
        .expect("human put");

        // An agent rewrites one word through a coordinator transaction (the same
        // TxnCtx write path the dispatched editors use).
        {
            let coord = Coordinator::open(state.workspace_root.clone()).expect("coord");
            let agent = WriterId::Agent {
                model: "deepseek-v4-pro".to_string(),
                label: "slave-1".to_string(),
            };
            let locks = LockSet::new()
                .with(Path::new("rust").join("a.md"))
                .with(Path::new("rust").join("index.json"));
            let req = TxnRequest::new(agent, locks, "agent edit");
            coord
                .submit(req, |ctx| {
                    ctx.write_article("rust", "a.md", "the quick red fox")?;
                    Ok("agent edit".to_string())
                })
                .expect("agent submit");
        }

        // Plain GET: joined text, unchanged contract.
        let plain: serde_json::Value = body_json(
            get_article(
                State(state.clone()),
                AxumPath(("rust".to_string(), "a.md".to_string())),
                Query(GetArticleQuery::default()),
            )
            .await
            .expect("plain ok"),
        )
        .await;
        assert_eq!(plain["content"], "the quick red fox");

        // Rich GET: blocks with author-tagged runs + the author legend.
        let rich: RichArticleResponse = body_json(
            get_article(
                State(state),
                AxumPath(("rust".to_string(), "a.md".to_string())),
                Query(GetArticleQuery {
                    format: Some("rich".to_string()),
                }),
            )
            .await
            .expect("rich ok"),
        )
        .await;
        assert_eq!(rich.theme, "rust");
        assert_eq!(rich.file, "a.md");
        assert_eq!(rich.blocks.len(), 1);
        assert_eq!(rich.blocks[0].kind, "paragraph");
        let runs: Vec<(&str, &str)> = rich.blocks[0]
            .runs
            .iter()
            .map(|r| (r.text.as_str(), r.author.as_str()))
            .collect();
        assert_eq!(
            runs,
            vec![
                ("the quick ", "human"),
                ("red", "deepseek-v4-pro/slave-1"),
                (" fox", "human"),
            ]
        );
        // The author legend lists both contributors, in first-seen order.
        let ids: Vec<&str> = rich.authors.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["human", "deepseek-v4-pro/slave-1"]);
        assert!(rich.authors.iter().all(|a| a.id == a.label));
    }

    #[tokio::test]
    async fn get_article_rich_for_legacy_article_is_single_author() {
        // An article written directly through the workspace (no coordinator) and
        // then stripped of its sidecar still serves a rich view: one run attributed
        // to its last recorded contributor.
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
            ws.acquire_lock("rust", "a.md", &WriterId::Human)
                .expect("lock");
            ws.write_article("rust", "a.md", "plain legacy body", &WriterId::Human)
                .expect("write");
            // Drop the sidecar to simulate a pre-B2 article.
            let sidecar = ws.root().join("rust").join("a.md.prov.json");
            std::fs::remove_file(sidecar).expect("remove sidecar");
        }
        let rich: RichArticleResponse = body_json(
            get_article(
                State(state),
                AxumPath(("rust".to_string(), "a.md".to_string())),
                Query(GetArticleQuery {
                    format: Some("rich".to_string()),
                }),
            )
            .await
            .expect("rich ok"),
        )
        .await;
        assert_eq!(rich.blocks.len(), 1);
        assert_eq!(rich.blocks[0].runs.len(), 1);
        assert_eq!(rich.blocks[0].runs[0].text, "plain legacy body");
        assert_eq!(rich.blocks[0].runs[0].author, "human");
        assert_eq!(
            rich.authors,
            vec![RichAuthor {
                id: "human".to_string(),
                label: "human".to_string()
            }]
        );
    }

    #[test]
    fn rich_article_renders_blocks_and_author_legend() {
        // Pure builder test: a two-paragraph document with two authors yields two
        // paragraph blocks, author-tagged runs, and a deduped author legend.
        use crate::content::{AuthorId, Block, Document, RichText, Run};
        let agent = AuthorId::Agent {
            model: "m".into(),
            label: "a".into(),
        };
        let mut doc = Document::new();
        doc.push(Block::Paragraph(RichText {
            runs: vec![
                Run::new("hi ", AuthorId::Human),
                Run::new("there", agent.clone()),
            ],
        }))
        .push(Block::Paragraph(RichText::from_plain("second", agent)));

        let resp = rich_article("t".to_string(), "f.md".to_string(), &doc);
        assert_eq!(resp.blocks.len(), 2);
        assert_eq!(resp.blocks[0].runs.len(), 2);
        assert_eq!(resp.blocks[0].runs[0].author, "human");
        assert_eq!(resp.blocks[0].runs[1].author, "m/a");
        assert_eq!(resp.blocks[1].runs[0].author, "m/a");
        // Legend: first-seen order, deduped.
        let ids: Vec<&str> = resp.authors.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["human", "m/a"]);
    }

    // -- B1: contribution aggregation (GET .../contributions) -----------------

    #[test]
    fn aggregate_contributions_sums_to_100_and_orders_by_share() {
        // 2 human lines, 1 agent line, 1 more human line -> human 3/4, agent 1/4.
        let human = "human <human@ai-write.local>";
        let agent = "deepseek-v4-pro/slave-1 <agent@ai-write.local>";
        let authors = [human, human, agent, human];
        let out = aggregate_contributions(authors.into_iter());
        assert_eq!(out.len(), 2);
        // Largest share first.
        assert_eq!(out[0].author, human);
        assert_eq!(out[0].label, "human");
        assert_eq!(out[0].lines, 3);
        assert_eq!(out[0].pct, 75);
        assert_eq!(out[1].label, "deepseek-v4-pro/slave-1");
        assert_eq!(out[1].lines, 1);
        assert_eq!(out[1].pct, 25);
        let total: u32 = out.iter().map(|c| c.pct).sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn aggregate_contributions_rounds_so_total_is_exactly_100() {
        // Three authors with one line each: 33/33/33 floors to 99; the
        // largest-remainder pass hands the leftover point out so the sum is 100.
        let a = "a/s <a@x>";
        let b = "b/s <b@x>";
        let c = "c/s <c@x>";
        let out = aggregate_contributions([a, b, c].into_iter());
        assert_eq!(out.len(), 3);
        let total: u32 = out.iter().map(|c| c.pct).sum();
        assert_eq!(total, 100, "percentages must sum to exactly 100");
        // Each is 33 or 34, and exactly one carries the extra point.
        assert!(out.iter().all(|c| c.pct == 33 || c.pct == 34));
        assert_eq!(out.iter().filter(|c| c.pct == 34).count(), 1);
    }

    #[test]
    fn aggregate_contributions_empty_is_empty() {
        let out = aggregate_contributions(std::iter::empty());
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn contributions_endpoint_reflects_blame() {
        // Build a two-writer history, then assert the contributions endpoint
        // aggregates blame into per-author shares summing to 100.
        let (_d, state) = state();
        let theme = "rust";
        let file = "a.md";
        let agent = WriterId::Agent {
            model: "deepseek-v4-flash".to_string(),
            label: "slave-1".to_string(),
        };
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme(theme).expect("theme");
            ws.create_article(theme, file, "A", None).expect("article");
            ws.acquire_lock(theme, file, &agent).expect("lock");
            ws.write_article(theme, file, "agent one\nagent two\nagent three\n", &agent)
                .expect("w1");
        }
        let vcs = state.open_vcs().expect("vcs");
        let rel = Path::new(theme).join(file);
        vcs.commit_file(&rel, &agent, "edit: v1")
            .expect("commit v1");
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.acquire_lock(theme, file, &WriterId::Human)
                .expect("lock");
            ws.write_article(
                theme,
                file,
                "agent one\nhuman revised\nagent three\n",
                &WriterId::Human,
            )
            .expect("w2");
        }
        vcs.commit_file(&rel, &WriterId::Human, "edit: v2")
            .expect("commit v2");

        let Json(out) = article_contributions(
            State(state),
            AxumPath((theme.to_string(), file.to_string())),
        )
        .await
        .expect("contributions ok");
        let arr = out["contributions"].as_array().expect("array");
        // Two authors (agent owns 2 lines, human owns 1).
        assert_eq!(arr.len(), 2);
        let total: u64 = arr.iter().map(|c| c["pct"].as_u64().unwrap()).sum();
        assert_eq!(total, 100);
        // Largest share first: the agent (2/3).
        assert_eq!(arr[0]["label"], "deepseek-v4-flash/slave-1");
        assert_eq!(arr[0]["lines"].as_u64().unwrap(), 2);
        assert_eq!(arr[0]["pct"].as_u64().unwrap(), 67);
        assert_eq!(arr[1]["label"], "human");
        assert_eq!(arr[1]["pct"].as_u64().unwrap(), 33);
    }

    // -- B1: master_chat plan derivation --------------------------------------

    #[test]
    fn build_plan_derives_created_from_outline_diff_and_dispatched_from_reports() {
        use crate::engine::{SlaveReport, SlaveStatus};

        // `before` already held one article; the run added two more.
        let before: std::collections::BTreeSet<String> = ["intro.md".to_string()].into();
        let after = vec![
            ArticleOutline {
                file: "intro.md".to_string(),
                title: "Intro".to_string(),
                parent: None,
                depth: 0,
            },
            ArticleOutline {
                file: "part1.md".to_string(),
                title: "Part One".to_string(),
                parent: Some("intro.md".to_string()),
                depth: 1,
            },
            ArticleOutline {
                file: "part2.md".to_string(),
                title: "Part Two".to_string(),
                parent: Some("intro.md".to_string()),
                depth: 1,
            },
        ];
        let reports = vec![
            SlaveReport {
                status: SlaveStatus::Done,
                summary: "wrote part one".to_string(),
                result: Some("part1 body".to_string()),
                needs: None,
            },
            SlaveReport {
                status: SlaveStatus::NeedsHuman,
                summary: "stuck on part two".to_string(),
                result: None,
                needs: Some("a source".to_string()),
            },
        ];

        let plan = build_plan("rust", &before, &after, &reports, "deepseek-v4-pro");

        // `created` is exactly the two new articles, in reading order, with their
        // titles and parents.
        assert_eq!(plan.created.len(), 2);
        assert_eq!(plan.created[0].file, "part1.md");
        assert_eq!(plan.created[0].theme, "rust");
        assert_eq!(plan.created[0].title, "Part One");
        assert_eq!(plan.created[0].parent.as_deref(), Some("intro.md"));
        assert_eq!(plan.created[1].file, "part2.md");

        // `dispatched` mirrors the reports, paired to created files by position,
        // with the engine's `slave-<n>` writer label.
        assert_eq!(plan.dispatched.len(), 2);
        assert_eq!(plan.dispatched[0].file, "part1.md");
        assert_eq!(plan.dispatched[0].writer, "deepseek-v4-pro/slave-1");
        assert_eq!(plan.dispatched[0].status, "done");
        assert_eq!(plan.dispatched[0].summary, "wrote part one");
        assert_eq!(plan.dispatched[1].file, "part2.md");
        assert_eq!(plan.dispatched[1].writer, "deepseek-v4-pro/slave-2");
        assert_eq!(plan.dispatched[1].status, "needs_human");
    }

    #[test]
    fn build_plan_with_no_reports_or_creations_is_empty() {
        let before: std::collections::BTreeSet<String> = ["a.md".to_string()].into();
        let after = vec![ArticleOutline {
            file: "a.md".to_string(),
            title: "A".to_string(),
            parent: None,
            depth: 0,
        }];
        let plan = build_plan("t", &before, &after, &[], "m");
        assert!(plan.created.is_empty());
        assert!(plan.dispatched.is_empty());
    }

    // ----- B3: request-edit endpoints + coordinator-routed undo --------------

    #[tokio::test]
    async fn request_edit_on_idle_workspace_is_up_now() {
        // With no transaction running, a request-edit reserves the human's slot at
        // the head and reports ahead=0 (up immediately).
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
        }
        let Json(resp) = request_edit(
            State(state),
            AxumPath(("rust".to_string(), "a.md".to_string())),
        )
        .await
        .expect("request-edit ok");
        assert!(resp.queued);
        assert_eq!(resp.ahead, 0);
    }

    #[tokio::test]
    async fn request_edit_then_cancel_round_trips() {
        // A reservation can be cancelled; cancelling again is an idempotent no-op.
        // The shared coordinator persists the reservation across the two calls.
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
        }
        let _ = request_edit(
            State(state.clone()),
            AxumPath(("rust".to_string(), "a.md".to_string())),
        )
        .await
        .expect("request-edit ok");

        let Json(first) = cancel_request_edit(
            State(state.clone()),
            AxumPath(("rust".to_string(), "a.md".to_string())),
        )
        .await
        .expect("cancel ok");
        assert_eq!(
            first["cancelled"], true,
            "the pending reservation is cancelled"
        );

        let Json(second) = cancel_request_edit(
            State(state),
            AxumPath(("rust".to_string(), "a.md".to_string())),
        )
        .await
        .expect("cancel ok");
        assert_eq!(second["cancelled"], false, "second cancel is a no-op");
    }

    #[tokio::test]
    async fn request_edit_emits_handoff_on_the_sse_stream() {
        // The endpoint's reservation flows through the shared coordinator's sink to
        // the broadcast channel: an idle request-edit announces HandoffToHuman.
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
        }
        let mut rx = state.events.subscribe();
        let _ = request_edit(
            State(state),
            AxumPath(("rust".to_string(), "a.md".to_string())),
        )
        .await
        .expect("request-edit ok");
        match rx.try_recv().expect("a handoff event was broadcast") {
            Event::HandoffToHuman { theme, file } => {
                assert_eq!(theme, "rust");
                assert_eq!(file, "a.md");
            }
            other => panic!("expected HandoffToHuman, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn put_emits_coordinator_lifecycle_on_the_sse_stream() {
        // The human PUT path is wired to the shared coordinator's sink, so its
        // transaction lifecycle (TxnAcquired … TxnReleased) reaches SSE subscribers.
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
        }
        let mut rx = state.events.subscribe();
        put_article(
            State(state.clone()),
            AxumPath(("rust".to_string(), "a.md".to_string())),
            Json(PutArticleRequest {
                text: "human body\n".to_string(),
            }),
        )
        .await
        .expect("put ok");

        let mut kinds = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            kinds.push(match ev {
                Event::TxnAcquired { .. } => "TxnAcquired",
                Event::TxnReleased { .. } => "TxnReleased",
                _ => "other",
            });
        }
        assert!(
            kinds.contains(&"TxnAcquired") && kinds.contains(&"TxnReleased"),
            "PUT should narrate its txn lifecycle to SSE, got {kinds:?}"
        );
    }

    #[tokio::test]
    async fn undo_routes_through_coordinator_and_reverts() {
        // The undo endpoint goes through Coordinator::undo_article (B3): seed two
        // human writes via the PUT path, then undo restores the previous body and
        // narrates its lifecycle to the SSE stream.
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
        }
        for body in ["one\n", "two\n"] {
            put_article(
                State(state.clone()),
                AxumPath(("rust".to_string(), "a.md".to_string())),
                Json(PutArticleRequest {
                    text: body.to_string(),
                }),
            )
            .await
            .expect("put ok");
        }

        let mut rx = state.events.subscribe();
        let Json(resp) = article_undo(
            State(state.clone()),
            AxumPath(("rust".to_string(), "a.md".to_string())),
        )
        .await
        .expect("undo ok");
        assert_eq!(resp["undone"], "rust/a.md");
        assert!(resp["committed"].is_string());

        // The body reverted to the previous version.
        let got: serde_json::Value = body_json(
            get_article(
                State(state),
                AxumPath(("rust".to_string(), "a.md".to_string())),
                Query(GetArticleQuery::default()),
            )
            .await
            .expect("read ok"),
        )
        .await;
        assert_eq!(got["content"], "one\n");

        // The undo narrated its coordinator lifecycle.
        let mut saw_acquire = false;
        let mut saw_release = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::TxnAcquired { .. } => saw_acquire = true,
                Event::TxnReleased { .. } => saw_release = true,
                _ => {}
            }
        }
        assert!(
            saw_acquire && saw_release,
            "undo narrates its txn lifecycle"
        );
    }

    #[tokio::test]
    async fn undo_with_single_version_reports_nothing_to_undo() {
        let (_d, state) = state();
        {
            let mut ws = state.open_workspace().expect("ws");
            ws.create_theme("rust").expect("theme");
            ws.create_article("rust", "a.md", "A", None).expect("a");
        }
        // One human write = one commit; there is no prior version to undo.
        put_article(
            State(state.clone()),
            AxumPath(("rust".to_string(), "a.md".to_string())),
            Json(PutArticleRequest {
                text: "only\n".to_string(),
            }),
        )
        .await
        .expect("put ok");

        let Json(resp) = article_undo(
            State(state),
            AxumPath(("rust".to_string(), "a.md".to_string())),
        )
        .await
        .expect("undo ok");
        assert_eq!(resp["undone"], false);
    }
}
