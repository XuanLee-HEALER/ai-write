//! The thin orchestration layer: the `Master` / `Slave` role composition.
//!
//! This is not a new machine — it is two configurations of
//! [`Session`] plus a little orchestration:
//!
//! - A **slave** is a [`Session`] configured with the
//!   writing tool set, a writing system prompt, and a target article. It runs
//!   [`run_until_done`](crate::session::Session::run_until_done): it acquires the
//!   article lock, writes, and releases the lock; the "research → write → revise"
//!   loop is emergent. Each slave runs on its own [`std::thread`] and finishes by
//!   producing a [`SlaveReport`].
//! - A **master** is a [`Session`] configured with the
//!   orchestration tool set (create themes/articles, spawn slaves, collect
//!   reports) plus supervision. In v0 it creates a theme, spawns a single slave
//!   thread, and collects the structured [`SlaveReport`] (a summary, never the
//!   slave's full transcript).
//!
//! Multiple slaves writing **different** articles never conflict (each holds its
//! own article lock); the single-writer invariant on one article is upheld by
//! the lock.
//!
//! # How a slave produces its report
//!
//! The slave's [`Session`] is configured with the
//! [`report`](crate::tool::tools::Report) tool. When the model decides it is
//! finished it calls `report`, which echoes a structured payload
//! (`status` / `summary` / `result` / `needs`) back into the slave's history as
//! the content of a `tool` reply. The engine reconstructs a [`SlaveReport`] by
//! scanning the finished slave's history for the **last** such `report` reply
//! (see [`report_from_history`]). If the slave terminated without calling
//! `report` (for example the round budget was exhausted, or a fatal error ended
//! the run), the engine synthesizes a report from the [terminal step](Step)
//! instead, so the master always receives a structured outcome.

use std::sync::Arc;
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};

use crate::observe::{Event, EventSink, NullSink};
use crate::req::Message;
use crate::req::blocking::Client;
use crate::req::types::request::Role;
use crate::session::{Session, SessionOptions, Step};
use crate::tool::tools::writing_tools;
use crate::tool::workspace::{Workspace, WriterId};

/// The system prompt a slave session is configured with.
///
/// It frames the emergent "research → write → revise" loop, the single-writer
/// lock discipline, and the obligation to finish by calling the `report` tool.
const SLAVE_SYSTEM_PROMPT: &str = "\
You are a focused writing agent working on exactly one article inside a sandboxed \
workspace. Your job is to research from your own knowledge, write, and revise that \
article until it is good, then report back.

Rules you must follow:
- The article already exists. To change it you must first call `acquire_lock` for \
  that theme and file, then use `write_article`, `edit_article`, or `apply_edits`, \
  and finally call `release_lock` when you are done editing.
- Only the lock holder may write. If a write is rejected because you do not hold \
  the lock, acquire it and retry.
- Read the current article with `read_article` before editing it so you do not \
  clobber existing work.
- Keep the article as plain text (Markdown is fine). Do not invent files or themes \
  outside the task.
- When the article is complete, release the lock and then call `report` with \
  status `done`, a short `summary`, and the final article path or text in `result`. \
  If you are blocked and need a human, call `report` with status `needs_human` and \
  describe what you need in `needs`.

Always finish by calling `report` exactly once.";

/// The outcome status a slave reports back to its master.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SlaveStatus {
    /// The slave self-assessed the article as complete.
    Done,
    /// The slave stopped needing human intervention.
    NeedsHuman,
    /// The slave failed; `result` / `summary` describe why.
    Failed,
}

impl SlaveStatus {
    /// Parses the `status` string used by the
    /// [`report`](crate::tool::tools::Report) tool (`"done"` / `"needs_human"` /
    /// `"failed"`) into a [`SlaveStatus`].
    ///
    /// Returns `None` for any other string.
    fn from_report_status(status: &str) -> Option<Self> {
        match status {
            "done" => Some(SlaveStatus::Done),
            "needs_human" => Some(SlaveStatus::NeedsHuman),
            "failed" => Some(SlaveStatus::Failed),
            _ => None,
        }
    }
}

/// The structured summary a slave sends to its master on completion.
///
/// Per the supervisor model the master sees only this summary, never the slave's
/// full execution history, so the master's context stays uncluttered. The slave
/// keeps its own full log as a fallback for post-mortem analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlaveReport {
    /// The terminal status of the slave run.
    pub status: SlaveStatus,
    /// A short human-readable summary of what happened.
    pub summary: String,
    /// The concrete result (e.g. the final article text or its path), when one
    /// was produced.
    pub result: Option<String>,
    /// What the slave needs next (e.g. human input, more sources), when
    /// applicable.
    pub needs: Option<String>,
}

impl SlaveReport {
    /// Builds a [`SlaveStatus::Failed`] report carrying `summary` as its message.
    fn failed(summary: impl Into<String>) -> Self {
        SlaveReport {
            status: SlaveStatus::Failed,
            summary: summary.into(),
            result: None,
            needs: None,
        }
    }
}

/// A description of the article a slave should write.
///
/// Identifies the target file within a theme and carries the writing task the
/// slave was dispatched with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlaveTask {
    /// The theme directory the article lives in.
    pub theme: String,
    /// The article file name within the theme.
    pub file_name: String,
    /// The natural-language writing task the slave should carry out.
    pub task: String,
    /// The identity the slave writes under (its model id and agent label).
    pub writer: WriterId,
}

/// Reconstructs a [`SlaveReport`] from a finished slave's message `history`.
///
/// The slave signals completion by calling the
/// [`report`](crate::tool::tools::Report) tool, whose echoed JSON payload lands
/// in history as the content of a `tool` reply. This scans `history` from the
/// end and returns the report parsed from the **most recent** `tool` reply whose
/// content is a JSON object with a recognized `status`
/// (`done` / `needs_human` / `failed`) and a `summary`.
///
/// Returns `None` if no such reply is present — the slave never called `report`,
/// so the caller should synthesize a report from the terminal [`Step`] instead.
///
/// # Examples
///
/// ```
/// use ai_write::engine::{report_from_history, SlaveStatus};
/// use ai_write::req::Message;
///
/// let history = vec![
///     Message::user("write the intro"),
///     Message::tool(
///         "call_1",
///         r#"{"status":"done","summary":"wrote it","result":"rust/intro.md","needs":null}"#,
///     ),
/// ];
/// let report = report_from_history(&history).expect("a report reply");
/// assert_eq!(report.status, SlaveStatus::Done);
/// assert_eq!(report.result.as_deref(), Some("rust/intro.md"));
/// ```
pub fn report_from_history(history: &[Message]) -> Option<SlaveReport> {
    for message in history.iter().rev() {
        if message.role != Role::Tool {
            continue;
        }
        let Some(content) = message.content.as_deref() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
            continue;
        };
        let Some(status) = value.get("status").and_then(|s| s.as_str()) else {
            continue;
        };
        let Some(status) = SlaveStatus::from_report_status(status) else {
            // A tool reply with a `status` field that is not one of the report
            // statuses (e.g. some other tool's payload) is not a report.
            continue;
        };
        // A genuine report always carries a `summary`.
        let Some(summary) = value.get("summary").and_then(|s| s.as_str()) else {
            continue;
        };
        return Some(SlaveReport {
            status,
            summary: summary.to_string(),
            result: value
                .get("result")
                .and_then(|r| r.as_str())
                .map(str::to_string),
            needs: value
                .get("needs")
                .and_then(|n| n.as_str())
                .map(str::to_string),
        });
    }
    None
}

/// Runs one configured slave [`Session`] to completion and distills a
/// [`SlaveReport`].
///
/// This is the body shared by [`spawn_slave`] (on its own thread) and the unit
/// tests (which inject a session whose tools never reach the network). It pushes
/// the task as the first user turn, runs [`Session::run_until_done`], then
/// prefers an explicit `report` tool reply
/// ([`report_from_history`]); failing that it synthesizes a report from the
/// terminal [`Step`].
fn run_slave_session(mut session: Session, task: &SlaveTask) -> SlaveReport {
    let prompt = format!(
        "Theme: {theme}\nArticle file: {file_name}\n\nTask:\n{task}",
        theme = task.theme,
        file_name = task.file_name,
        task = task.task,
    );
    session.push_user(prompt);

    let terminal = session.run_until_done();

    // The model's own structured report is authoritative when present.
    if let Some(report) = report_from_history(session.history()) {
        return report;
    }

    // Otherwise synthesize one from how the run ended.
    match terminal {
        Step::Done(text) => SlaveReport {
            status: SlaveStatus::Done,
            summary: "Slave finished without calling `report`.".to_string(),
            result: if text.is_empty() { None } else { Some(text) },
            needs: None,
        },
        Step::NeedHuman => SlaveReport {
            status: SlaveStatus::NeedsHuman,
            summary: "Slave stopped and needs human intervention (round budget \
                      exhausted or escalation)."
                .to_string(),
            result: None,
            needs: Some("human intervention".to_string()),
        },
        Step::Failed(err) => SlaveReport::failed(format!("Slave failed: {err}")),
        // `run_until_done` only ever returns a terminal step; these arms are
        // unreachable in practice but keep the match exhaustive without a panic.
        Step::Tool(_) | Step::Message(_) => SlaveReport::failed(
            "Slave returned a non-terminal step from run_until_done (unexpected).",
        ),
    }
}

/// Builds the writing-configured slave [`Session`] for `task`, rooted at
/// `workspace_root`, narrating to `events`.
///
/// The session is given the full [`writing_tools`] registry, the slave system
/// prompt, and the task's [`WriterId`] so its tool calls are dispatched under
/// the slave's agent identity against the workspace it owns. The `events` sink is
/// installed under the `"slave"` role so the slave's per-round / per-tool /
/// per-commit [`Event`]s flow into the same feed as the master's slave-lifecycle
/// events.
fn build_slave_session(
    client: Client,
    workspace_root: &str,
    task: &SlaveTask,
    events: Arc<dyn EventSink>,
) -> Session {
    let mut session = Session::new(
        client,
        SLAVE_SYSTEM_PROMPT,
        writing_tools(),
        SessionOptions::default(),
    );
    session.set_workspace(workspace_root, task.writer.clone());
    session.set_event_sink("slave", events);
    session
}

/// Spawns a slave on its own [`std::thread`] to write one article.
///
/// The slave opens its own [`Workspace`] handle at `workspace_root`, builds a
/// writing-configured [`Session`] from `client`, and
/// runs it to completion. The returned [`JoinHandle`] yields the slave's
/// [`SlaveReport`]; join it to collect the result.
///
/// A `String` workspace root (rather than a borrowed [`Workspace`]) is taken so
/// the spawned closure is `'static` and the slave owns its own sandbox handle —
/// concurrent slaves writing different articles do not share workspace state.
///
/// # Panics
///
/// The spawned thread itself does not panic on workspace errors: the session
/// opens the workspace lazily on its first tool call and surfaces any failure
/// back to the model as a tool result, so a bad `workspace_root` yields a
/// `Failed`/`NeedsHuman` [`SlaveReport`] rather than a panic. Joining the handle
/// can still observe an `Err` if the thread panics for an unrelated reason
/// (e.g. an allocation failure); [`Master::run_one`] converts that into a
/// `Failed` report.
///
/// # Examples
///
/// ```no_run
/// use ai_write::engine::{spawn_slave, SlaveTask};
/// use ai_write::tool::workspace::WriterId;
/// use ai_write::req::blocking::Client;
///
/// let client = Client::from_env()?;
/// let task = SlaveTask {
///     theme: "rust".into(),
///     file_name: "intro.md".into(),
///     task: "Write a short introduction to ownership.".into(),
///     writer: WriterId::Agent { model: "deepseek-v4-pro".into(), label: "s1".into() },
/// };
/// let handle = spawn_slave(client, "workspace".into(), task);
/// let report = handle.join().expect("slave thread");
/// println!("{:?}", report.status);
/// # Ok::<(), ai_write::req::Error>(())
/// ```
pub fn spawn_slave(
    client: Client,
    workspace_root: String,
    task: SlaveTask,
) -> JoinHandle<SlaveReport> {
    spawn_slave_with_sink(client, workspace_root, task, Arc::new(NullSink))
}

/// Spawns a slave on its own [`std::thread`], narrating its lifecycle and inner
/// steps to `events`.
///
/// This is the observable form of [`spawn_slave`]: it emits an
/// [`Event::SlaveSpawned`] as the thread starts and an [`Event::SlaveReported`]
/// once the slave's [`SlaveReport`] is distilled, and installs `events` on the
/// slave's [`Session`] so its per-round / per-tool / per-commit events flow into
/// the same feed. Both lifecycle events are emitted **on the slave thread**, so
/// they bracket the run regardless of when the caller joins the handle.
/// [`spawn_slave`] is the plain wrapper that passes a [`NullSink`].
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use ai_write::engine::{spawn_slave_with_sink, SlaveTask};
/// use ai_write::observe::NullSink;
/// use ai_write::tool::workspace::WriterId;
/// use ai_write::req::blocking::Client;
///
/// let client = Client::from_env()?;
/// let task = SlaveTask {
///     theme: "rust".into(),
///     file_name: "intro.md".into(),
///     task: "Write a short introduction to ownership.".into(),
///     writer: WriterId::Agent { model: "deepseek-v4-pro".into(), label: "s1".into() },
/// };
/// let handle = spawn_slave_with_sink(client, "workspace".into(), task, Arc::new(NullSink));
/// let report = handle.join().expect("slave thread");
/// println!("{:?}", report.status);
/// # Ok::<(), ai_write::req::Error>(())
/// ```
pub fn spawn_slave_with_sink(
    client: Client,
    workspace_root: String,
    task: SlaveTask,
    events: Arc<dyn EventSink>,
) -> JoinHandle<SlaveReport> {
    std::thread::spawn(move || {
        events.emit(Event::SlaveSpawned {
            theme: task.theme.clone(),
            file: task.file_name.clone(),
            writer: task.writer.provenance_tag(),
        });
        let session = build_slave_session(client, &workspace_root, &task, Arc::clone(&events));
        let report = run_slave_session(session, &task);
        events.emit(Event::SlaveReported {
            status: report_status_str(&report.status).to_string(),
            summary: report.summary.clone(),
        });
        report
    })
}

/// Maps a [`SlaveStatus`] to the lowercase string used in an
/// [`Event::SlaveReported`] (`"done"` / `"needs_human"` / `"failed"`), the
/// inverse of [`SlaveStatus::from_report_status`].
fn report_status_str(status: &SlaveStatus) -> &'static str {
    match status {
        SlaveStatus::Done => "done",
        SlaveStatus::NeedsHuman => "needs_human",
        SlaveStatus::Failed => "failed",
    }
}

/// The master: the orchestrating session for one theme.
///
/// Wraps a [`Session`] (the orchestration tool set +
/// supervision prompt) together with the [`Workspace`] it manages. In v0 it
/// creates a theme, spawns a slave, and collects the slave's [`SlaveReport`].
///
/// The master's own [`Session`] is held for forward compatibility (later stages
/// drive the orchestration through the model); the v0 [`Master::run_one`] flow
/// is deterministic Rust and uses the session only to share the master's
/// [`Client`] with the spawned slave.
pub struct Master {
    /// The orchestrating session.
    session: Session,
    /// The workspace this master governs.
    ws: Workspace,
    /// The workspace root path, handed to spawned slaves so they can open their
    /// own workspace handle.
    workspace_root: String,
}

impl Master {
    /// Creates a master governing `ws`, driven by `session`.
    ///
    /// The workspace root is captured (as a lossy UTF-8 string) so it can be
    /// handed to spawned slaves, which open their own workspace handle at the
    /// same root.
    pub fn new(session: Session, ws: Workspace) -> Self {
        let workspace_root = ws.root().to_string_lossy().into_owned();
        Master {
            session,
            ws,
            workspace_root,
        }
    }

    /// Runs the v0 orchestration: ensure the theme and target article exist,
    /// dispatch one slave for the given task on its own thread, and collect its
    /// [`SlaveReport`].
    ///
    /// The theme is created if absent (an already-existing theme is fine); the
    /// target article is created if absent so the slave can immediately acquire
    /// its lock. The slave runs on a separate thread sharing the master's
    /// [`Client`]; this method joins it and returns the report.
    ///
    /// A slave that fails does **not** surface as an `Err`: its failure is carried
    /// inside the returned [`SlaveReport`] (`status = Failed`). v0 terminates on a
    /// slave failure and reports it; automatic restart / re-dispatch is a
    /// deliberate TODO for a later stage.
    ///
    /// # Errors
    ///
    /// Returns a [`req::Error`](crate::req::Error) only if the master cannot set
    /// up the workspace for the slave (e.g. the theme or article cannot be
    /// created on disk). The setup error is wrapped as
    /// [`Error::Decode`](crate::req::Error::Decode) carrying the underlying
    /// [`ToolError`](crate::tool::ToolError) message, since v0's orchestration is
    /// deterministic and performs no master-side chat completion.
    pub fn run_one(&mut self, task: SlaveTask) -> Result<SlaveReport, crate::req::Error> {
        // Ensure the theme exists (idempotent: an existing theme is acceptable).
        if let Err(e) = self.ws.create_theme(&task.theme) {
            // `Lock` means "already exists" for create_theme; tolerate it.
            if !matches!(e, crate::tool::ToolError::Lock(_)) {
                return Err(setup_error("create theme", &e));
            }
        }

        // Ensure the target article exists so the slave can lock and write it.
        if let Err(e) = self.ws.create_article(
            &task.theme,
            &task.file_name,
            &task.file_name,
            Some(task.task.clone()),
        ) {
            // `Lock` means "already exists" for create_article; tolerate it.
            if !matches!(e, crate::tool::ToolError::Lock(_)) {
                return Err(setup_error("create article", &e));
            }
        }

        // TODO(v0+): supervise the slave (restart / re-dispatch on failure). v0
        // dispatches exactly one slave, joins it, and reports the outcome.
        //
        // The slave is given the master's event sink so its lifecycle and inner
        // steps narrate into the same feed the master (and any UI) observes. With
        // the default `NullSink` this is transparent.
        let client = self.session.client_clone();
        let events = self.session.event_sink();
        let handle = spawn_slave_with_sink(client, self.workspace_root.clone(), task, events);

        // A panicked slave thread becomes a `Failed` report rather than an error.
        Ok(handle
            .join()
            .unwrap_or_else(|_| SlaveReport::failed("Slave thread panicked.")))
    }

    /// Returns the cumulative token usage of the master's orchestration
    /// [`Session`].
    ///
    /// In v0 the master performs no chat completion (its orchestration is
    /// deterministic Rust), so these totals are zero. The accessor exists so a
    /// caller such as the `demo` binary can report the master's observed usage,
    /// and so later stages that do drive the master through the model can surface
    /// real totals through the same seam. A slave's own token usage lives in the
    /// slave session on its thread and is not folded into the master.
    pub fn usage(&self) -> &crate::session::UsageTotals {
        self.session.usage()
    }
}

/// Wraps a workspace-setup [`ToolError`](crate::tool::ToolError) as a
/// [`req::Error`](crate::req::Error) for [`Master::run_one`]'s `Result`.
fn setup_error(stage: &str, err: &crate::tool::ToolError) -> crate::req::Error {
    crate::req::Error::Decode {
        context: "engine",
        source: <serde_json::Error as serde::de::Error>::custom(format!(
            "workspace setup failed at {stage}: {err}"
        )),
    }
}

#[cfg(test)]
mod tests {
    //! Offline unit tests for the orchestration layer.
    //!
    //! These exercise report distillation and the master's deterministic
    //! workspace setup. They never perform a chat completion: `report_from_history`
    //! is tested directly against synthetic histories, and the master test only
    //! drives the on-disk setup path (theme/article creation), stopping before any
    //! slave round would hit the network.

    use super::*;
    use crate::req::Message;

    fn agent() -> WriterId {
        WriterId::Agent {
            model: "deepseek-v4-pro".to_string(),
            label: "s1".to_string(),
        }
    }

    #[test]
    fn report_from_history_reads_last_report_reply() {
        let history = vec![
            Message::user("write it"),
            // An earlier, superseded report.
            Message::tool(
                "call_a",
                r#"{"status":"needs_human","summary":"stuck","result":null,"needs":"a source"}"#,
            ),
            Message::assistant("trying again"),
            // The final report wins.
            Message::tool(
                "call_b",
                r#"{"status":"done","summary":"finished","result":"t/a.md","needs":null}"#,
            ),
        ];
        let report = report_from_history(&history).expect("report present");
        assert_eq!(report.status, SlaveStatus::Done);
        assert_eq!(report.summary, "finished");
        assert_eq!(report.result.as_deref(), Some("t/a.md"));
        assert!(report.needs.is_none());
    }

    #[test]
    fn report_from_history_maps_all_statuses() {
        for (raw, expected) in [
            ("done", SlaveStatus::Done),
            ("needs_human", SlaveStatus::NeedsHuman),
            ("failed", SlaveStatus::Failed),
        ] {
            let history = vec![Message::tool(
                "c",
                format!(r#"{{"status":"{raw}","summary":"s"}}"#),
            )];
            let report = report_from_history(&history).expect("report");
            assert_eq!(report.status, expected);
        }
    }

    #[test]
    fn report_from_history_ignores_non_report_tool_replies() {
        // A non-report tool reply (e.g. acquire_lock's echo) must not be mistaken
        // for a report, and a report status that is unknown is skipped.
        let history = vec![
            Message::tool("c1", r#"{"locked":"t/a.md"}"#),
            Message::tool("c2", r#"{"status":"weird","summary":"x"}"#),
            Message::tool("c3", r#"not json at all"#),
        ];
        assert!(report_from_history(&history).is_none());
    }

    #[test]
    fn report_from_history_requires_summary() {
        // A `status` without a `summary` is not a complete report.
        let history = vec![Message::tool("c", r#"{"status":"done"}"#)];
        assert!(report_from_history(&history).is_none());
    }

    #[test]
    fn report_from_history_empty_is_none() {
        assert!(report_from_history(&[]).is_none());
    }

    /// A network-free client used only so a `Session`/`Master` can be built; no
    /// test here performs a chat completion.
    fn offline_client() -> Client {
        Client::builder()
            .api_key("test-key")
            .build()
            .expect("offline client")
    }

    #[test]
    fn master_setup_creates_theme_and_article_idempotently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::open(dir.path()).expect("open workspace");
        let session = Session::new(
            offline_client(),
            "orchestrator",
            crate::tool::ToolRegistry::new(),
            SessionOptions::default(),
        );
        let mut master = Master::new(session, ws);

        // Drive only the deterministic setup, not the slave round. We reach into
        // the master's workspace via a fresh handle on the same root to assert the
        // on-disk effects.
        let task = SlaveTask {
            theme: "rust".into(),
            file_name: "intro.md".into(),
            task: "Write an introduction.".into(),
            writer: agent(),
        };

        // Replicate run_one's setup steps directly (so the test never spawns the
        // network-bound slave) and assert idempotency.
        master.ws.create_theme(&task.theme).expect("create theme");
        master
            .ws
            .create_article(&task.theme, &task.file_name, &task.file_name, None)
            .expect("create article");

        // Idempotent: creating again yields the "already exists" lock error, which
        // run_one tolerates.
        assert!(matches!(
            master.ws.create_theme(&task.theme),
            Err(crate::tool::ToolError::Lock(_))
        ));
        assert!(matches!(
            master
                .ws
                .create_article(&task.theme, &task.file_name, &task.file_name, None),
            Err(crate::tool::ToolError::Lock(_))
        ));

        // The article is on disk and lockable by the slave's writer identity.
        let mut probe = Workspace::open(dir.path()).expect("reopen");
        assert_eq!(probe.list_articles("rust").unwrap(), vec!["intro.md"]);
        probe
            .acquire_lock("rust", "intro.md", &task.writer)
            .expect("article is lockable");
    }

    #[test]
    fn report_from_history_absent_triggers_synthesis_path() {
        // When the slave never calls `report`, history has no report reply and
        // `run_slave_session` falls back to synthesizing from the terminal step.
        // The synthesis itself needs a live round, but its precondition — no
        // distillable report — is what we assert here.
        let history: Vec<Message> = vec![Message::assistant("done thinking, no report")];
        assert!(report_from_history(&history).is_none());
    }

    #[test]
    fn slave_report_failed_constructor() {
        let r = SlaveReport::failed("boom");
        assert_eq!(r.status, SlaveStatus::Failed);
        assert_eq!(r.summary, "boom");
        assert!(r.result.is_none());
        assert!(r.needs.is_none());
    }

    #[test]
    fn report_status_str_round_trips_with_from_report_status() {
        for s in [
            SlaveStatus::Done,
            SlaveStatus::NeedsHuman,
            SlaveStatus::Failed,
        ] {
            let raw = report_status_str(&s);
            assert_eq!(SlaveStatus::from_report_status(raw), Some(s));
        }
    }

    #[test]
    fn spawn_slave_with_sink_emits_lifecycle_events() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Mutex;

        use crate::observe::Event;

        /// Records the kind of every event for sequence assertions.
        #[derive(Default)]
        struct Recorder(Mutex<Vec<&'static str>>);
        impl EventSink for Recorder {
            fn emit(&self, event: Event) {
                let kind = match event {
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
                self.0.lock().expect("not poisoned").push(kind);
            }
        }

        // A loopback fake returning a single `stop` response, so the slave's one
        // round finishes immediately with no live API call.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = serde_json::json!({
                "id": "r", "object": "chat.completion", "created": 0,
                "model": "deepseek-v4-flash",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "finished" },
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
        let task = SlaveTask {
            theme: "rust".into(),
            file_name: "intro.md".into(),
            task: "write".into(),
            writer: agent(),
        };

        let recorder: Arc<Recorder> = Arc::new(Recorder::default());
        let sink: Arc<dyn EventSink> = Arc::clone(&recorder) as Arc<dyn EventSink>;
        let handle = spawn_slave_with_sink(
            client,
            dir.path().to_string_lossy().into_owned(),
            task,
            sink,
        );
        let report = handle.join().expect("slave thread");
        assert_eq!(report.status, SlaveStatus::Done);

        let kinds = recorder.0.lock().expect("not poisoned");
        // The slave lifecycle brackets the inner session events.
        assert_eq!(kinds.first(), Some(&"SlaveSpawned"));
        assert_eq!(kinds.last(), Some(&"SlaveReported"));
        assert!(kinds.contains(&"SessionStarted"));
        assert!(kinds.contains(&"Finished"));
    }
}
