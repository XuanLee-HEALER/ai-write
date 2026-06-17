//! The presentation WebUI backend: an `axum` HTTP server that visualizes the AI
//! writing process (`docs/impl-v1.md` §3).
//!
//! This module is the async front door to the otherwise synchronous writing
//! engine. It exposes a small REST + [Server-Sent Events][sse] API over the
//! workspace:
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
//! | `GET /` | [`index_page`] | the embedded single-page UI |
//! | `GET /api/themes` | [`list_themes`] | list theme names |
//! | `GET /api/themes/:theme/articles` | [`list_articles`] | list a theme's articles (reading order) |
//! | `GET /api/articles/:theme/:file` | [`get_article`] | an article's current content |
//! | `POST /api/tasks` | [`start_task`] | start a writing run, returns `{task_id}` |
//! | `GET /api/events` | [`events`] | **SSE** stream of [`Event`] JSON |
//! | `GET /api/articles/:theme/:file/history` | [`article_history`] | git history (versions) |
//! | `GET /api/articles/:theme/:file/diff` | [`article_diff`] | unified diff between two versions |
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
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::engine::{Master, SlaveTask};
use crate::observe::{Event, EventSink};
use crate::req::blocking::Client;
use crate::session::{Session, SessionOptions};
use crate::tool::ToolRegistry;
use crate::tool::workspace::{Workspace, WriterId};
use crate::vcs::Vcs;

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
    /// The stateless DeepSeek client a writing run drives. Read once from the
    /// environment at server construction; cloning shares the connection pool.
    client: Client,
    /// The broadcast channel every [`Event`] flows through. [`events`] subscribes
    /// to it for SSE; a [`BroadcastSink`] built from
    /// [`AppState::event_sink`] sends into it from the blocking write task.
    events: broadcast::Sender<Event>,
    /// A monotonic counter handing out a `task_id` to each started writing run.
    next_task_id: Arc<AtomicU64>,
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
            client,
            events,
            next_task_id: Arc::new(AtomicU64::new(1)),
        }
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
        Ok(Self::new(workspace_root, Client::from_env()?))
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
        .route("/", get(index_page))
        .route("/api/themes", get(list_themes))
        .route("/api/themes/{theme}/articles", get(list_articles))
        .route("/api/articles/{theme}/{file}", get(get_article))
        .route("/api/tasks", post(start_task))
        .route("/api/events", get(events))
        .route("/api/articles/{theme}/{file}/history", get(article_history))
        .route("/api/articles/{theme}/{file}/diff", get(article_diff))
        .route("/api/articles/{theme}/{file}/undo", post(article_undo))
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

/// `GET /` — the embedded single-page UI.
///
/// Serves a self-contained (vanilla HTML/CSS/JS, no build step) page embedded at
/// compile time via [`include_str!`]. It renders the workspace
/// tree, a "new writing run" form, a live `EventSource('/api/events')` operation
/// timeline, and a versions / diff / undo panel — driving every route in this
/// module from the browser.
pub async fn index_page() -> Html<&'static str> {
    Html(INDEX_PAGE)
}

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
    /// The article file names, in the theme index's reading order.
    pub articles: Vec<String>,
}

/// `GET /api/themes/{theme}/articles` — list a theme's articles in reading order.
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
    let articles = ws.list_articles(&theme).map_err(ApiError::workspace)?;
    Ok(Json(ArticlesResponse { theme, articles }))
}

/// The response body for [`get_article`].
#[derive(Debug, Serialize)]
pub struct ArticleResponse {
    /// The theme the article belongs to.
    pub theme: String,
    /// The article file name.
    pub file: String,
    /// The article's current full text content.
    pub content: String,
}

/// `GET /api/articles/{theme}/{file}` — an article's current content.
///
/// # Errors
///
/// Returns an [`ApiError`]: `404` if the article is missing, `400` for an
/// illegal name or a sandbox violation, or `500` on a read failure or
/// unsupported (binary / oversized) content.
pub async fn get_article(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
) -> Result<Json<ArticleResponse>, ApiError> {
    let ws = state.open_workspace()?;
    let content = ws
        .read_article(&theme, &file)
        .map_err(ApiError::workspace)?;
    Ok(Json(ArticleResponse {
        theme,
        file,
        content,
    }))
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
/// Unlike the model-facing `undo_last` tool, this endpoint does **not** require
/// an article lock: a writing run holds its lock only on its own thread, and the
/// human operator driving the UI is the authority here. Returns the new commit id
/// on success, or `{"undone": false}` when the article has only one version.
///
/// # Errors
///
/// Returns an [`ApiError`]: `500` if version control cannot be opened, or the
/// mapped [`VcsError`](crate::vcs::VcsError) status if the revert fails.
pub async fn article_undo(
    State(state): State<AppState>,
    AxumPath((theme, file)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let vcs = state.open_vcs()?;
    let rel = Path::new(&theme).join(&file);
    let reverted = vcs
        .undo_last(&rel, &WriterId::Human)
        .map_err(ApiError::vcs)?;
    match reverted {
        Some(sha) => Ok(Json(serde_json::json!({
            "undone": format!("{theme}/{file}"),
            "committed": sha,
        }))),
        None => Ok(Json(serde_json::json!({
            "undone": false,
            "reason": "nothing to undo (article has only one version)",
        }))),
    }
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
    };

    if let Err(e) = master.run_one(slave_task) {
        eprintln!("webui: writing task setup failed: {e}");
    }
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
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

/// The single-page WebUI served at `GET /`.
///
/// Embedded at compile time from `src/webui/index.html` via [`include_str!`], so
/// the binary is self-contained (no static-file directory, no build step). The
/// page is vanilla HTML/CSS/JS: it lists the workspace tree, starts writing runs,
/// renders the live [`Event`] feed from `GET /api/events`, and drives the
/// history / diff / undo endpoints.
const INDEX_PAGE: &str = include_str!("index.html");

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
        assert_eq!(resp.articles, vec!["a.md", "b.md"]);
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
        let Json(resp) = get_article(
            State(state),
            AxumPath(("rust".to_string(), "a.md".to_string())),
        )
        .await
        .expect("content ok");
        assert_eq!(resp.content, "hello body");
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
}
