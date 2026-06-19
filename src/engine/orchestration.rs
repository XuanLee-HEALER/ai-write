//! The master's LLM-driven orchestration tools and their shared state.
//!
//! Where a slave is configured with the [`writing_tools`](crate::tool::tools::writing_tools)
//! that edit one article, the **master** is configured with the smaller,
//! higher-level tool set defined here so the model can *plan and delegate* an
//! entire writing goal:
//!
//! | Tool | Purpose |
//! |---|---|
//! | [`CreateTheme`] | Create a theme directory the articles will live in. |
//! | [`CreateArticle`] | Create one empty article file inside a theme. |
//! | [`ListArticles`] | List the article files currently in a theme. |
//! | [`OrganizeArticles`] | Set the articles' parent/child hierarchy and reading order. |
//! | [`DispatchWriter`] | Spawn a slave (its own thread + sandboxed session) to write one article, blocking until it reports, and record the [`SlaveReport`]. |
//! | [`ListReports`] | Surface every [`SlaveReport`] collected so far back to the model. |
//!
//! # Why a shared state object
//!
//! The theme / article tools operate through the sandboxed
//! [`Workspace`](crate::tool::workspace::Workspace) handle the
//! [`session`](crate::session) layer already threads into every tool call as
//! [`ToolCtx::ws`](crate::tool::ToolCtx::ws), so they need nothing extra. But
//! dispatching a writer needs three things a [`ToolCtx`] does not carry: the
//! [`Client`] to drive the slave's rounds, the workspace **root path** so the
//! slave can open its own sandbox handle, and the [`EventSink`] so the slave's
//! lifecycle and inner steps narrate into the same feed as the master's. Those,
//! plus the growing list of collected reports, live in a single (crate-internal)
//! orchestrator-state object shared (behind an [`Arc`]) by every dispatch/report
//! tool. That state is the seam through which
//! [`Master::run_goal`](crate::engine::Master::run_goal) reads back the reports
//! the model accumulated.
//!
//! # Safety
//!
//! These tools never relax a guard rail. Theme / article creation goes through
//! the same sandboxed workspace API the writing tools use (so `..` traversal,
//! absolute paths, and system paths are rejected). [`DispatchWriter`] spawns a
//! slave whose own [`Session`](crate::session::Session) is fully sandboxed and
//! whose writer identity is derived deterministically from the master's
//! configuration — the model chooses only the *task text*, never a path outside
//! the workspace or an arbitrary writer identity.

use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::coordinator::Coordinator;
use crate::observe::EventSink;
use crate::req::blocking::Client;
use crate::req::{FunctionDef, Tool as ReqTool};
use crate::tool::workspace::WriterId;
use crate::tool::{Tool, ToolCtx, ToolError, ToolResult};

use super::{SlaveReport, SlaveSkill, SlaveTask, spawn_slave_with_coordinator};

/// The shared state behind the master's dispatch / report tools.
///
/// Every orchestration tool that needs more than the [`Workspace`](crate::tool::workspace::Workspace)
/// handle holds an [`Arc`] to one of these. It carries the runtime handles a
/// spawned slave needs (the [`Client`], the workspace root, the [`EventSink`])
/// and accumulates the [`SlaveReport`]s produced so far, which
/// [`Master::run_goal`](crate::engine::Master::run_goal) reads back after the run.
///
/// It is `Send + Sync` (a `Mutex` guards the reports) so the same state can be
/// shared by several tool objects in one registry; in v2 the master drives its
/// rounds on a single thread, so the mutex is essentially uncontended.
pub(crate) struct OrchestratorState {
    /// The client handed to each spawned slave so it drives the same backend as
    /// the master without re-reading credentials.
    client: Client,
    /// The workspace root path spawned slaves open their own sandbox handle at.
    workspace_root: String,
    /// The sink each slave's lifecycle and inner-step events flow into.
    events: Arc<dyn EventSink>,
    /// The model id slaves run under, recorded as their writer identity so the
    /// article's contributor provenance names the model that produced it.
    slave_model: String,
    /// The composed system prompt every dispatched slave runs under (the selected
    /// writing skill ahead of the fixed operational rules, or the engine default).
    ///
    /// This is the static fallback. When [`slave_skill`](OrchestratorState::slave_skill)
    /// is set, each slave instead recomposes its prompt from that skill file every
    /// round (kernel §4), and this string is only the fallback on a read failure.
    slave_prompt: String,
    /// An optional on-disk skill source each dispatched slave re-reads per round
    /// (kernel §4). `None` keeps slaves pinned to [`slave_prompt`](OrchestratorState::slave_prompt).
    slave_skill: Option<SlaveSkill>,
    /// The single transaction [`Coordinator`] (kernel §6) every dispatched slave
    /// shares with the master, so all edits funnel through one lock table + Vcs.
    coordinator: Arc<Coordinator>,
    /// Every report collected so far, in dispatch order. Guarded by a mutex so
    /// the dispatch tool can append while [`Master::run_goal`](crate::engine::Master::run_goal)
    /// reads them back.
    reports: Mutex<Vec<SlaveReport>>,
}

impl OrchestratorState {
    /// Creates orchestration state with no reports collected yet.
    pub(crate) fn new(
        client: Client,
        workspace_root: String,
        events: Arc<dyn EventSink>,
        slave_model: String,
        slave_prompt: String,
        coordinator: Arc<Coordinator>,
    ) -> Self {
        OrchestratorState {
            client,
            workspace_root,
            events,
            slave_model,
            slave_prompt,
            slave_skill: None,
            coordinator,
            reports: Mutex::new(Vec::new()),
        }
    }

    /// Sets the on-disk skill source every dispatched slave re-reads per round
    /// (kernel §4), returning `self` for chaining.
    ///
    /// With a skill source set, each slave gets a
    /// [system-prompt provider](crate::session::Session::set_system_provider) that
    /// recomposes its prompt from the skill file each round; the static
    /// [`slave_prompt`](OrchestratorState::slave_prompt) remains the fallback on a
    /// read failure. `None` (the default) leaves slaves on the static prompt.
    pub(crate) fn with_slave_skill(mut self, skill: Option<SlaveSkill>) -> Self {
        self.slave_skill = skill;
        self
    }

    /// Returns a snapshot of every [`SlaveReport`] collected so far, in dispatch
    /// order.
    pub(crate) fn reports(&self) -> Vec<SlaveReport> {
        self.reports
            .lock()
            .expect("reports mutex not poisoned")
            .clone()
    }

    /// Dispatches a slave for one article on its own thread, blocks until it
    /// reports, records the report, and returns it.
    ///
    /// The slave is spawned through
    /// [`super::spawn_slave_with_coordinator`] so it
    /// shares the master's single coordinator and its lifecycle / inner steps
    /// narrate into the master's event feed. Joining a panicked slave thread yields
    /// a `Failed` report rather than propagating the panic, matching
    /// [`Master::run_one`](crate::engine::Master::run_one).
    fn dispatch(&self, theme: &str, file_name: &str, task: &str, label: &str) -> SlaveReport {
        let writer = WriterId::Agent {
            model: self.slave_model.clone(),
            label: label.to_string(),
        };
        let slave_task = SlaveTask {
            theme: theme.to_string(),
            file_name: file_name.to_string(),
            task: task.to_string(),
            writer,
            system_prompt: Some(self.slave_prompt.clone()),
            skill: self.slave_skill.clone(),
        };
        let handle = spawn_slave_with_coordinator(
            self.client.clone(),
            self.workspace_root.clone(),
            slave_task,
            Arc::clone(&self.events),
            Arc::clone(&self.coordinator),
        );
        let report = handle
            .join()
            .unwrap_or_else(|_| SlaveReport::failed("Slave thread panicked."));
        self.reports
            .lock()
            .expect("reports mutex not poisoned")
            .push(report.clone());
        report
    }
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

/// Deserializes a tool's typed argument struct from the model-supplied JSON,
/// mapping any parse failure to [`ToolError::InvalidArgs`].
fn parse_args<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))
}

/// Renders a [`SlaveReport`] as the JSON object an orchestration tool returns to
/// the model (and that an [`Event`](crate::observe::Event) summary is built from).
fn report_json(report: &SlaveReport) -> Value {
    json!({
        "status": super::report_status_str(&report.status),
        "summary": report.summary,
        "result": report.result,
        "needs": report.needs,
    })
}

// ===========================================================================
// Theme / article structure (operate on the sandboxed workspace directly)
// ===========================================================================

/// Creates a theme directory the master's articles will live in.
///
/// Idempotent for the model's purposes: an already-existing theme is reported as
/// `created: false` rather than erroring, so re-planning the same theme does not
/// derail the run.
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
            "Create a theme (a top-level directory grouping related articles) for \
             this writing goal. If the theme already exists this is a no-op. Create \
             the theme before creating articles or dispatching writers into it.",
            json!({
                "type": "object",
                "properties": { "theme": string_prop("The theme name (a single path segment).") },
                "required": ["theme"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: ThemeArgs = parse_args(args)?;
        match ctx.ws.create_theme(&a.theme) {
            Ok(theme) => Ok(json!({ "theme": theme.name, "created": true })),
            // `Lock` is the workspace's "already exists" signal for create_theme;
            // surface it as a benign no-op so the planner can proceed.
            Err(ToolError::Lock(_)) => Ok(json!({ "theme": a.theme, "created": false })),
            Err(e) => Err(e),
        }
    }
}

/// Creates one empty article file inside a theme and records it in the index.
///
/// Like [`CreateTheme`], an already-existing article is reported as
/// `created: false` rather than erroring, so the planner may declare the full
/// article set without tracking which already exist.
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
            "Create an empty article file inside a theme and add it to the \
             reading-order index, so a writer can later be dispatched to fill it \
             in. If the article already exists this is a no-op.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file name (a single path segment, e.g. `intro.md`)."),
                    "title": string_prop("A human-readable title for the article."),
                    "notes": string_prop("Optional notes, e.g. what this article should cover."),
                },
                "required": ["theme", "file_name"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: CreateArticleArgs = parse_args(args)?;
        let title = if a.title.is_empty() {
            a.file_name.clone()
        } else {
            a.title
        };
        match ctx
            .ws
            .create_article(&a.theme, &a.file_name, &title, a.notes)
        {
            Ok(()) => Ok(json!({
                "article": format!("{}/{}", a.theme, a.file_name),
                "created": true,
            })),
            // `Lock` is the workspace's "already exists" signal for
            // create_article; treat it as a benign no-op.
            Err(ToolError::Lock(_)) => Ok(json!({
                "article": format!("{}/{}", a.theme, a.file_name),
                "created": false,
            })),
            Err(e) => Err(e),
        }
    }
}

/// Lists the article file names currently in a theme, in reading order.
pub struct ListArticles;

impl Tool for ListArticles {
    fn name(&self) -> &str {
        "list_articles"
    }

    fn schema(&self) -> ReqTool {
        def(
            "list_articles",
            "List the article file names that currently exist in a theme, in \
             reading order. Use this to see what you have already created before \
             planning more.",
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

/// Organizes a theme's articles into a logical hierarchy (parent/child) and an
/// optional reading order, in one call.
///
/// This is how the master records *structure* on the article set it created: who
/// nests under whom, and the sequence a reader should follow. Both are written
/// through the sandboxed workspace ([`Workspace::set_parent`](crate::tool::workspace::Workspace::set_parent)
/// / [`Workspace::reorder`](crate::tool::workspace::Workspace::reorder)), so the
/// same guards apply — articles and parents must exist, no cycles, and a reorder
/// must be a permutation of the current articles.
pub struct OrganizeArticles;

/// One parent assignment in an [`OrganizeArticles`] call.
#[derive(Deserialize)]
struct Relation {
    file_name: String,
    #[serde(default)]
    parent: Option<String>,
}

#[derive(Deserialize)]
struct OrganizeArgs {
    theme: String,
    #[serde(default)]
    relations: Vec<Relation>,
    #[serde(default)]
    order: Option<Vec<String>>,
}

impl Tool for OrganizeArticles {
    fn name(&self) -> &str {
        "organize_articles"
    }

    fn schema(&self) -> ReqTool {
        def(
            "organize_articles",
            "Organize a theme's articles into a logical hierarchy and reading \
             order. Provide `relations` to set each article's parent (use a null \
             or omitted parent for a top-level article), and/or `order` to set the \
             full reading order (must list every article in the theme exactly \
             once). Articles and parents must already exist; a parent may not form \
             a cycle. Use this after creating the articles to express how they \
             relate and the sequence a reader should follow.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme whose articles to organize."),
                    "relations": {
                        "type": "array",
                        "description": "Parent assignments; each sets one article's parent.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "file_name": string_prop("The article whose parent to set."),
                                "parent": string_prop("The parent article's file name; null or omitted for top-level."),
                            },
                            "required": ["file_name"],
                        },
                    },
                    "order": {
                        "type": "array",
                        "description": "Optional full reading order: every article file name in the theme, exactly once.",
                        "items": { "type": "string" },
                    },
                },
                "required": ["theme"],
            }),
        )
    }

    fn call(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: OrganizeArgs = parse_args(args)?;
        let mut relations_set = 0usize;
        for rel in &a.relations {
            ctx.ws
                .set_parent(&a.theme, &rel.file_name, rel.parent.as_deref())?;
            relations_set += 1;
        }
        let reordered = match a.order {
            Some(order) => {
                ctx.ws.reorder(&a.theme, order)?;
                true
            }
            None => false,
        };
        Ok(json!({
            "theme": a.theme,
            "relations_set": relations_set,
            "reordered": reordered,
        }))
    }
}

// ===========================================================================
// Dispatch / report (operate on the shared OrchestratorState)
// ===========================================================================

/// Spawns a slave to write one article and records its [`SlaveReport`].
///
/// This is the master's delegation primitive: each call dispatches one
/// [`super::spawn_slave_with_coordinator`] thread
/// for the named article with the given task, **blocks** until the slave reports,
/// appends the report to the shared orchestrator state, and returns the report to
/// the model so it can decide what to do next.
pub struct DispatchWriter {
    /// The shared orchestration state the dispatched report is recorded into.
    state: Arc<OrchestratorState>,
}

impl DispatchWriter {
    /// Creates the dispatch tool over shared `state`.
    pub(crate) fn new(state: Arc<OrchestratorState>) -> Self {
        DispatchWriter { state }
    }
}

#[derive(Deserialize)]
struct DispatchWriterArgs {
    theme: String,
    file_name: String,
    task: String,
    #[serde(default)]
    label: Option<String>,
}

impl Tool for DispatchWriter {
    fn name(&self) -> &str {
        "dispatch_writer"
    }

    fn schema(&self) -> ReqTool {
        def(
            "dispatch_writer",
            "Dispatch a writing agent (a slave) to research, write, and revise ONE \
             article, then report back. The article and its theme must already \
             exist (call create_theme / create_article first). This blocks until \
             the writer finishes and returns its structured report (status, \
             summary, result, needs). Dispatch one writer per article; inspect each \
             report and decide whether to dispatch more, then finish.",
            json!({
                "type": "object",
                "properties": {
                    "theme": string_prop("The theme the article belongs to."),
                    "file_name": string_prop("The article file the writer should fill in."),
                    "task": string_prop("The writing task: what this article should cover, in natural language."),
                    "label": string_prop("Optional short label distinguishing this writer (e.g. `intro-writer`). Defaults to the file name."),
                },
                "required": ["theme", "file_name", "task"],
            }),
        )
    }

    fn call(&self, args: Value, _ctx: &mut ToolCtx<'_>) -> ToolResult {
        let a: DispatchWriterArgs = parse_args(args)?;
        if a.task.trim().is_empty() {
            return Err(ToolError::InvalidArgs(
                "`task` must not be empty".to_string(),
            ));
        }
        let label = a.label.unwrap_or_else(|| a.file_name.clone());
        let report = self.state.dispatch(&a.theme, &a.file_name, &a.task, &label);
        Ok(json!({
            "article": format!("{}/{}", a.theme, a.file_name),
            "report": report_json(&report),
        }))
    }
}

/// Surfaces every [`SlaveReport`] collected so far back to the model.
///
/// The model uses this to review the outcomes of the writers it has dispatched —
/// for example to confirm every planned article is `done` before it finishes, or
/// to spot a `needs_human` / `failed` report it should react to.
pub struct ListReports {
    /// The shared orchestration state whose collected reports are returned.
    state: Arc<OrchestratorState>,
}

impl ListReports {
    /// Creates the report-listing tool over shared `state`.
    pub(crate) fn new(state: Arc<OrchestratorState>) -> Self {
        ListReports { state }
    }
}

impl Tool for ListReports {
    fn name(&self) -> &str {
        "list_reports"
    }

    fn schema(&self) -> ReqTool {
        def(
            "list_reports",
            "List the structured reports of every writer dispatched so far, in \
             dispatch order. Use this to review progress before deciding whether \
             the goal is complete.",
            json!({
                "type": "object",
                "properties": {},
            }),
        )
    }

    fn call(&self, _args: Value, _ctx: &mut ToolCtx<'_>) -> ToolResult {
        let reports: Vec<Value> = self.state.reports().iter().map(report_json).collect();
        Ok(json!({ "count": reports.len(), "reports": reports }))
    }
}

#[cfg(test)]
mod tests {
    //! Offline unit tests for the orchestration tools.
    //!
    //! Theme / article / list tools run against a real temp-dir workspace (no
    //! network). The dispatch / report tools are exercised through a loopback fake
    //! DeepSeek server so a real slave round completes with zero live calls.

    use super::*;
    use crate::tool::workspace::Workspace;

    fn agent() -> WriterId {
        WriterId::Agent {
            model: "deepseek-v4-pro".to_string(),
            label: "master".to_string(),
        }
    }

    /// Dispatches one tool call by name against a workspace context.
    fn call(ws: &mut Workspace, tool: &dyn Tool, args: Value) -> ToolResult {
        let mut ctx = ToolCtx::new(ws, agent());
        tool.call(args, &mut ctx)
    }

    #[test]
    fn create_theme_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");

        let out = call(&mut ws, &CreateTheme, json!({ "theme": "rust" })).unwrap();
        assert_eq!(out["created"], json!(true));
        assert_eq!(out["theme"], "rust");

        // Re-creating the same theme is a benign no-op, not an error.
        let out = call(&mut ws, &CreateTheme, json!({ "theme": "rust" })).unwrap();
        assert_eq!(out["created"], json!(false));
    }

    #[test]
    fn create_article_is_idempotent_and_listed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");
        call(&mut ws, &CreateTheme, json!({ "theme": "rust" })).unwrap();

        let out = call(
            &mut ws,
            &CreateArticle,
            json!({ "theme": "rust", "file_name": "intro.md", "title": "Intro" }),
        )
        .unwrap();
        assert_eq!(out["created"], json!(true));

        // Second create is a no-op.
        let out = call(
            &mut ws,
            &CreateArticle,
            json!({ "theme": "rust", "file_name": "intro.md" }),
        )
        .unwrap();
        assert_eq!(out["created"], json!(false));

        let listed = call(&mut ws, &ListArticles, json!({ "theme": "rust" })).unwrap();
        assert_eq!(listed["articles"], json!(["intro.md"]));
    }

    #[test]
    fn create_theme_rejects_sandbox_escape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");
        let err = call(&mut ws, &CreateTheme, json!({ "theme": "../evil" })).unwrap_err();
        assert!(matches!(err, ToolError::SandboxViolation(_)));
    }

    #[test]
    fn organize_articles_sets_hierarchy_and_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");
        call(&mut ws, &CreateTheme, json!({ "theme": "rust" })).unwrap();
        for f in ["a.md", "b.md", "c.md"] {
            call(
                &mut ws,
                &CreateArticle,
                json!({ "theme": "rust", "file_name": f }),
            )
            .unwrap();
        }

        let out = call(
            &mut ws,
            &OrganizeArticles,
            json!({
                "theme": "rust",
                "relations": [
                    { "file_name": "b.md", "parent": "a.md" },
                    { "file_name": "c.md", "parent": "b.md" }
                ],
                "order": ["a.md", "b.md", "c.md"]
            }),
        )
        .unwrap();
        assert_eq!(out["relations_set"], json!(2));
        assert_eq!(out["reordered"], json!(true));

        let outline = ws.article_outline("rust").unwrap();
        assert_eq!(outline[1].file, "b.md");
        assert_eq!(outline[1].parent.as_deref(), Some("a.md"));
        assert_eq!(outline[2].depth, 2);
    }

    #[test]
    fn organize_articles_propagates_workspace_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");
        call(&mut ws, &CreateTheme, json!({ "theme": "rust" })).unwrap();
        call(
            &mut ws,
            &CreateArticle,
            json!({ "theme": "rust", "file_name": "a.md" }),
        )
        .unwrap();
        // Parent does not exist -> NotFound bubbles up from set_parent.
        let err = call(
            &mut ws,
            &OrganizeArticles,
            json!({
                "theme": "rust",
                "relations": [ { "file_name": "a.md", "parent": "ghost.md" } ]
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));

        // A reorder that is not a permutation -> InvalidArgs.
        let err = call(
            &mut ws,
            &OrganizeArticles,
            json!({ "theme": "rust", "order": ["a.md", "b.md"] }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn create_article_missing_theme_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");
        let err = call(
            &mut ws,
            &CreateArticle,
            json!({ "theme": "ghost", "file_name": "a.md" }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[test]
    fn dispatch_writer_rejects_empty_task() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");
        let state = Arc::new(OrchestratorState::new(
            offline_client(),
            dir.path().to_string_lossy().into_owned(),
            Arc::new(crate::observe::NullSink),
            "deepseek-v4-flash".to_string(),
            String::new(),
            test_coordinator(dir.path()),
        ));
        let tool = DispatchWriter::new(Arc::clone(&state));
        let err = call(
            &mut ws,
            &tool,
            json!({ "theme": "rust", "file_name": "a.md", "task": "   " }),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn list_reports_starts_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");
        let state = Arc::new(OrchestratorState::new(
            offline_client(),
            dir.path().to_string_lossy().into_owned(),
            Arc::new(crate::observe::NullSink),
            "deepseek-v4-flash".to_string(),
            String::new(),
            test_coordinator(dir.path()),
        ));
        let tool = ListReports::new(Arc::clone(&state));
        let out = call(&mut ws, &tool, json!({})).unwrap();
        assert_eq!(out["count"], json!(0));
        assert_eq!(out["reports"], json!([]));
    }

    /// A network-free client used only so state can be constructed; the tests in
    /// this group that need a real slave round point a client at a loopback fake.
    fn offline_client() -> Client {
        Client::builder()
            .api_key("test-key")
            .build()
            .expect("offline client")
    }

    /// A coordinator rooted at `root`, for constructing orchestration state.
    fn test_coordinator(root: &std::path::Path) -> Arc<Coordinator> {
        Arc::new(Coordinator::open(root).expect("coordinator"))
    }

    #[test]
    fn dispatch_writer_runs_a_slave_and_records_the_report() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        // A loopback fake returning a single `stop` response, so the slave's one
        // round finishes immediately with no live API call.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let body = serde_json::json!({
                "id": "r", "object": "chat.completion", "created": 0,
                "model": "deepseek-v4-flash",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "wrote the intro" },
                    "finish_reason": "stop"
                }]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).expect("write");
        });

        let client = Client::builder()
            .api_key("test-key")
            .base_url(base)
            .build()
            .expect("client");

        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = Workspace::open(dir.path()).expect("open");
        // The article must exist for a realistic dispatch (the slave locks it).
        ws.create_theme("rust").unwrap();
        ws.create_article("rust", "intro.md", "Intro", None)
            .unwrap();

        let state = Arc::new(OrchestratorState::new(
            client,
            dir.path().to_string_lossy().into_owned(),
            Arc::new(crate::observe::NullSink),
            "deepseek-v4-flash".to_string(),
            crate::engine::default_slave_prompt(),
            test_coordinator(dir.path()),
        ));
        let dispatch = DispatchWriter::new(Arc::clone(&state));

        let out = call(
            &mut ws,
            &dispatch,
            json!({ "theme": "rust", "file_name": "intro.md", "task": "Write an intro." }),
        )
        .unwrap();
        // The slave stopped without calling `report`, so the engine synthesized a
        // `done` report from the terminal step.
        assert_eq!(out["report"]["status"], "done");
        assert_eq!(out["article"], "rust/intro.md");

        // The report was recorded in shared state and is visible via list_reports.
        let list = ListReports::new(Arc::clone(&state));
        let listed = call(&mut ws, &list, json!({})).unwrap();
        assert_eq!(listed["count"], json!(1));
        assert_eq!(listed["reports"][0]["status"], "done");
    }
}
