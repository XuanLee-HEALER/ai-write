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

pub mod orchestration;

use std::sync::Arc;
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};

use crate::coordinator::Coordinator;
use crate::observe::{Event, EventSink, NullSink};
use crate::req::Message;
use crate::req::Model;
use crate::req::blocking::Client;
use crate::req::types::request::Role;
use crate::session::{Session, SessionOptions, Step};
use crate::tool::ToolRegistry;
use crate::tool::tools::writing_tools;
use crate::tool::workspace::{Workspace, WriterId};

use orchestration::OrchestratorState;

/// The fixed operational rules every slave runs under, regardless of which
/// writing skill governs its voice.
///
/// These encode the mechanics a writer must never skip — reading before editing,
/// staying in the sandbox, and finishing with exactly one `report` call. Locking
/// is **implicit**: every edit is one atomic transaction the coordinator handles
/// (kernel §6), so there are no `acquire_lock` / `release_lock` steps to teach.
/// They are appended *after* the writing skill by [`compose_slave_prompt`], so a
/// chosen [`Skill`](crate::skill::Skill) shapes *how* the writer writes without
/// ever relaxing *how it must operate*.
const SLAVE_OPERATIONAL_RULES: &str = "\
Rules you must follow:
- The article already exists. Change it with `write_article`, `edit_article`, or \
  `apply_edits`. Locking and version control are automatic — each edit is one \
  atomic commit, so you never acquire or release a lock yourself.
- Read the current article with `read_article` before editing it so you do not \
  clobber existing work.
- Keep the article as plain text (Markdown is fine). Do not invent files or themes \
  outside the task.
- When the article is complete, call `report` with status `done`, a short \
  `summary`, and the final article path or text in `result`. If you are blocked \
  and need a human, call `report` with status `needs_human` and describe what you \
  need in `needs`.

Always finish by calling `report` exactly once.";

/// The default writing role used when no [`Skill`](crate::skill::Skill) is
/// selected.
///
/// It frames the emergent "research → write → revise" loop at a neutral voice. A
/// selected skill's body replaces this leading section while the fixed
/// [`SLAVE_OPERATIONAL_RULES`] stay appended.
const DEFAULT_SLAVE_ROLE: &str = "\
You are a focused writing agent working on exactly one article inside a sandboxed \
workspace. Your job is to research from your own knowledge, write, and revise that \
article until it is good, then report back.";

/// Composes a slave's full system prompt from a writing-skill `body` and the
/// fixed operational rules (`SLAVE_OPERATIONAL_RULES`).
///
/// The skill body (role + voice + refusals) leads and the operational rules — the
/// lock discipline and the `report` obligation — are appended, so the persona a
/// skill defines can never override the mechanics a writer must follow. Passing
/// the engine's built-in role reproduces the default writer prompt; callers
/// usually pass a loaded [`Skill::body`](crate::skill::Skill::body).
///
/// # Examples
///
/// ```
/// use ai_write::engine::compose_slave_prompt;
///
/// let prompt = compose_slave_prompt("You write terse functional prose.");
/// assert!(prompt.starts_with("You write terse functional prose."));
/// assert!(prompt.contains("write_article"));
/// ```
pub fn compose_slave_prompt(skill_body: &str) -> String {
    format!("{}\n\n{SLAVE_OPERATIONAL_RULES}", skill_body.trim())
}

/// Composes a slave's full system prompt from an **ordered stack** of writing
/// skill `bodies` and the fixed operational rules (kernel §10).
///
/// The bodies are first folded into one voice block by
/// [`compose_stack`](crate::skill::compose_stack) — laid out in order with a
/// documented precedence directive so a *later* skill overrides an earlier one on
/// conflict — and that block is then composed with the operational rules exactly
/// as [`compose_slave_prompt`] does, so the operational rules still win over any
/// skill. A one-element stack is identical to calling [`compose_slave_prompt`]
/// with that single body; an empty stack reproduces the engine's default writer
/// prompt.
///
/// # Examples
///
/// ```
/// use ai_write::engine::compose_slave_prompt_multi;
///
/// let prompt = compose_slave_prompt_multi(&["Write tersely.", "Use a warm tone."]);
/// // Stacked voice leads; operational rules are still appended and authoritative.
/// assert!(prompt.contains("Write tersely."));
/// assert!(prompt.contains("Use a warm tone."));
/// assert!(prompt.contains("write_article"));
/// // The later skill is documented as the override on conflict.
/// assert!(prompt.contains("LATER in the stack overrides"));
/// ```
pub fn compose_slave_prompt_multi(bodies: &[impl AsRef<str>]) -> String {
    let stacked = crate::skill::compose_stack(bodies);
    if stacked.is_empty() {
        default_slave_prompt()
    } else {
        compose_slave_prompt(&stacked)
    }
}

/// The default composed slave prompt (built-in role + operational rules), used
/// when a [`SlaveTask`] carries no `system_prompt` override.
fn default_slave_prompt() -> String {
    compose_slave_prompt(DEFAULT_SLAVE_ROLE)
}

/// Recomposes a slave system prompt by **reading the skill stack from disk now**,
/// used as the per-round body of a
/// [system-prompt provider](crate::session::Session::set_system_provider).
///
/// It loads every `<skill.dir>/<id>.md` in [`skill.ids`](SlaveSkill::ids) order
/// and folds their bodies into the operational prompt via
/// [`compose_slave_prompt_multi`] (kernel §10: a later id overrides an earlier one
/// on conflict). If **any** id in the stack cannot be loaded at this instant
/// (e.g. a file is missing or momentarily unreadable), it falls back to
/// `fallback` so a transient read failure never produces a partial or malformed
/// prompt — the prior round's effective behaviour is preserved.
///
/// This is what realizes kernel §4 for the system prompt: because it re-reads the
/// files on every call, an edit to (or addition of) a skill mid-run changes the
/// prompt sent on the next round.
fn compose_slave_prompt_from_skill(skill: &SlaveSkill, fallback: &str) -> String {
    match crate::skill::load_skills_ordered(&skill.dir, &skill.ids) {
        Ok(loaded) => {
            let bodies: Vec<&str> = loaded.iter().map(|s| s.body.as_str()).collect();
            compose_slave_prompt_multi(&bodies)
        }
        Err(_) => fallback.to_string(),
    }
}

/// The system prompt the master's orchestration session is configured with.
///
/// It frames the master as a *planner and delegator*: it never writes article
/// prose itself, it sets up structure (themes / articles) and dispatches one
/// writer per article, reviews the structured reports, and stops when the goal is
/// met. It deliberately mirrors the slave prompt's discipline (finish cleanly,
/// adapt to tool errors) at the orchestration altitude.
const MASTER_SYSTEM_PROMPT: &str = "\
You are an orchestrator that turns a high-level writing goal into a finished set \
of articles by planning and delegating. You do NOT write article prose yourself.

Your tools:
- `create_theme` — create the theme directory the articles live in.
- `create_article` — create one empty article file inside a theme.
- `list_articles` — see which article files already exist in a theme.
- `organize_articles` — set the articles' parent/child hierarchy and reading \
  order, so the set reads as a structured whole rather than a flat list.
- `dispatch_writer` — spawn a writing agent to research, write, and revise ONE \
  article, then report back. It blocks and returns the writer's structured report.
- `list_reports` — review every writer's report collected so far.

How to work:
1. Decide on a theme and the set of articles the goal needs (it may be one \
   article, or several). Keep the plan focused; do not invent unrelated articles.
2. Create the theme, then create each planned article file.
3. If the goal has more than one article, call `organize_articles` to set their \
   parent/child structure and the reading order.
4. Dispatch exactly one writer per article, giving each a clear, self-contained \
   task. Read each report as it comes back.
5. If a writer reports `needs_human` or `failed`, note it; do not loop forever \
   re-dispatching the same article.
6. When every planned article has a writer report, finish with a short final \
   message summarizing what was produced. Do not keep calling tools once the \
   goal is met.

Stay within the workspace and the tools provided. If a tool returns an error, \
read it and adapt rather than repeating the same call.";

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

/// A reference to an ordered stack of writing [`Skill`](crate::skill::Skill)s on
/// disk, re-read each round to recompose a slave's system prompt (kernel §4, §10).
///
/// Unlike a frozen [`SlaveTask::system_prompt`] string, this names *where* the
/// skills live (a directory plus an ordered list of skill `ids`) so the prompt is
/// recomposed from the current file contents on every round. A mid-run edit to
/// any `<dir>/<id>.md` therefore takes effect on the slave's next round — the
/// skill files are state on disk, not pinned into the model's context.
///
/// When [`ids`](SlaveSkill::ids) holds more than one id the skills are stacked in
/// order via [`compose_stack`](crate::skill::compose_stack): a **later** id
/// overrides an earlier one on conflicting directives (kernel §10). A single id
/// is the ordinary single-skill case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlaveSkill {
    /// The directory the skills live in (the same `dir` passed to
    /// [`load_skill`](crate::skill::load_skill)).
    pub dir: std::path::PathBuf,
    /// The ordered stack of skill ids (each the file stem of `<dir>/<id>.md`),
    /// earliest first; on conflict a later id overrides an earlier one.
    pub ids: Vec<String>,
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
    /// The composed system prompt the slave runs under.
    ///
    /// `None` uses the engine's default writer prompt (built-in role +
    /// operational rules). A master that selected a writing
    /// [`Skill`](crate::skill::Skill) sets this to the skill composed with the
    /// operational rules via [`compose_slave_prompt`]. An older serialized task
    /// without this field deserializes to `None`.
    ///
    /// When [`skill`](SlaveTask::skill) is also set, that on-disk source wins: the
    /// prompt is recomposed from the skill file each round (kernel §4) and this
    /// field is used only as the fallback if the skill ever fails to load.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// An optional on-disk skill source to **re-read each round** (kernel §4).
    ///
    /// `None` — the default and the back-compat case for an older serialized task —
    /// pins the prompt to [`system_prompt`](SlaveTask::system_prompt) (or the engine
    /// default), exactly as before. When `Some`, the slave's session is given a
    /// [system-prompt provider](crate::session::Session::set_system_provider) that
    /// reloads the skill and recomposes the prompt every round, so an edit to the
    /// skill file mid-run affects subsequent rounds.
    #[serde(default)]
    pub skill: Option<SlaveSkill>,
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

/// Resolves the request [`Model`] a slave should run under from its writer
/// identity.
///
/// An [`WriterId::Agent`] carries an explicit model id (kernel §9) — typically a
/// pinned dated snapshot such as `"deepseek-v4-pro-2026-05-01"`; that id is
/// parsed with [`Model::pinned`] so the slave *requests the exact model it signs
/// its commits with*, keeping the wire request, the git author, and
/// [`ArticleMeta::contributors`](crate::tool::workspace::ArticleMeta::contributors)
/// in lockstep. A [`WriterId::Human`] has no model id, so the
/// [`SessionOptions::default`] model is used as a neutral fallback.
fn model_for_writer(writer: &WriterId) -> Model {
    match writer {
        WriterId::Agent { model, .. } => Model::pinned(model.clone()),
        WriterId::Human => SessionOptions::default().model,
    }
}

/// Builds the writing-configured slave [`Session`] for `task`, rooted at
/// `workspace_root`, narrating to `events`, sharing `coordinator`.
///
/// The session is given the full [`writing_tools`] registry, the slave system
/// prompt, and the task's [`WriterId`] so its tool calls are dispatched under
/// the slave's agent identity against the workspace it owns. The shared
/// [`Coordinator`] is installed so every edit routes through it (locking is
/// implicit and each edit is one commit, kernel §6); the session's own read tools
/// still use a per-thread workspace handle. The `events` sink is installed under
/// the `"slave"` role so the slave's per-round / per-tool / per-commit [`Event`]s
/// flow into the same feed as the master's slave-lifecycle events.
fn build_slave_session(
    client: Client,
    workspace_root: &str,
    task: &SlaveTask,
    events: Arc<dyn EventSink>,
    coordinator: Arc<Coordinator>,
) -> Session {
    let system_prompt = task
        .system_prompt
        .clone()
        .unwrap_or_else(default_slave_prompt);
    // Kernel §9: the slave requests the exact model id it signs its commits with,
    // so a pinned dated snapshot (carried in `WriterId::Agent.model`) drives both
    // the wire request and the git author / contributor provenance — they can
    // never drift apart. A human writer (no model id) falls back to the default.
    let options = SessionOptions {
        model: model_for_writer(&task.writer),
        ..SessionOptions::default()
    };
    let mut session = Session::new(
        client,
        // The static prompt is both the byte-identical default when no skill
        // source is set, and the fallback the provider uses if a skill read fails.
        system_prompt.clone(),
        writing_tools(),
        options,
    );
    session.set_workspace(workspace_root, task.writer.clone());
    session.set_event_sink("slave", events);
    session.set_coordinator(coordinator);
    // Kernel §4: when the task names an on-disk skill, recompose the system prompt
    // from that file every round, so a mid-run edit affects subsequent rounds. With
    // no skill source the static prompt above is used unchanged.
    if let Some(skill) = task.skill.clone() {
        session
            .set_system_provider(move || compose_slave_prompt_from_skill(&skill, &system_prompt));
    }
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
///     system_prompt: None,
///     skill: None,
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
///     system_prompt: None,
///     skill: None,
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
        // This standalone spawn owns its own coordinator at the workspace root —
        // a lone slave has no master to share one with. A failure to open it
        // becomes a `Failed` report (never a panic).
        let report = match Coordinator::open(&workspace_root) {
            Ok(coord) => {
                // Narrate the coordinator's transaction lifecycle (B3) to the same
                // sink the slave session uses.
                let coord = coord.with_event_sink(Arc::clone(&events));
                let session = build_slave_session(
                    client,
                    &workspace_root,
                    &task,
                    Arc::clone(&events),
                    Arc::new(coord),
                );
                run_slave_session(session, &task)
            }
            Err(e) => SlaveReport::failed(format!("coordinator unavailable: {e}")),
        };
        events.emit(Event::SlaveReported {
            status: report_status_str(&report.status).to_string(),
            summary: report.summary.clone(),
        });
        report
    })
}

/// Spawns a slave on its own [`std::thread`], sharing the master's
/// `coordinator`, and narrating its lifecycle and inner steps to `events`.
///
/// This is the form the [`Master`] uses: every slave it dispatches shares the
/// **same** [`Coordinator`], so all edits across all slaves funnel through one
/// operation-level lock table and one [`Vcs`](crate::vcs::Vcs) (kernel §6) — concurrent slaves
/// writing different articles never race the git index, and a human's edit can
/// jump the queue. It emits an [`Event::SlaveSpawned`] as the thread starts and
/// an [`Event::SlaveReported`] once the [`SlaveReport`] is distilled, and installs
/// `events` on the slave's [`Session`].
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use ai_write::coordinator::Coordinator;
/// use ai_write::engine::{spawn_slave_with_coordinator, SlaveTask};
/// use ai_write::observe::NullSink;
/// use ai_write::tool::workspace::WriterId;
/// use ai_write::req::blocking::Client;
///
/// let client = Client::from_env()?;
/// let coordinator = Arc::new(Coordinator::open("workspace")?);
/// let task = SlaveTask {
///     theme: "rust".into(),
///     file_name: "intro.md".into(),
///     task: "Write a short introduction to ownership.".into(),
///     writer: WriterId::Agent { model: "deepseek-v4-pro".into(), label: "s1".into() },
///     system_prompt: None,
///     skill: None,
/// };
/// let handle =
///     spawn_slave_with_coordinator(client, "workspace".into(), task, Arc::new(NullSink), coordinator);
/// let report = handle.join().expect("slave thread");
/// # let _ = report;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn spawn_slave_with_coordinator(
    client: Client,
    workspace_root: String,
    task: SlaveTask,
    events: Arc<dyn EventSink>,
    coordinator: Arc<Coordinator>,
) -> JoinHandle<SlaveReport> {
    std::thread::spawn(move || {
        events.emit(Event::SlaveSpawned {
            theme: task.theme.clone(),
            file: task.file_name.clone(),
            writer: task.writer.provenance_tag(),
        });
        let session = build_slave_session(
            client,
            &workspace_root,
            &task,
            Arc::clone(&events),
            coordinator,
        );
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

/// The master: the orchestrating session for one writing goal.
///
/// Wraps a [`Session`] together with the [`Workspace`] it manages. It offers two
/// entry points at different altitudes:
///
/// - [`Master::run_one`] — the **deterministic** v0 flow: ensure a theme and one
///   article exist, dispatch a single slave for a given [`SlaveTask`], and return
///   its [`SlaveReport`]. No master-side chat completion happens; the session is
///   used only to share the master's [`Client`] and [`EventSink`] with the slave.
/// - [`Master::run_goal`] — the **LLM-driven** v2 flow: the master is reconfigured
///   as a planning session with the [`orchestration`] tool set
///   (`create_theme` / `create_article` / `list_articles` / `organize_articles` /
///   `dispatch_writer` / `list_reports`) and an orchestrator system prompt, then
///   run to completion so the model itself plans the article set, dispatches one
///   writer per article, reviews the structured reports, and decides when the
///   goal is met.
///
/// Both flows share the same slave machinery ([`spawn_slave_with_sink`]) and the
/// same observability feed, so a UI sees a master's tool calls and every slave's
/// lifecycle in one stream regardless of which entry point was used.
pub struct Master {
    /// The orchestrating session.
    session: Session,
    /// The workspace this master governs.
    ws: Workspace,
    /// The workspace root path, handed to spawned slaves so they can open their
    /// own workspace handle.
    workspace_root: String,
    /// The single transaction [`Coordinator`] (kernel §6) this master and every
    /// slave it dispatches share. Created lazily on first use by
    /// [`Master::coordinator`] so [`Master::new`] stays infallible; once created
    /// it is reused for the master's whole life.
    coordinator: Option<Arc<Coordinator>>,
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
            coordinator: None,
        }
    }

    /// Returns the shared transaction [`Coordinator`], creating it on first use.
    ///
    /// Every slave dispatched by this master (and the master's own human-authored
    /// edits, if any) shares this one coordinator, so all mutating operations
    /// funnel through a single operation-level lock table and a single [`Vcs`](crate::vcs::Vcs).
    /// The coordinator is wired to the master session's [`EventSink`], so its
    /// transaction lifecycle (acquire / queue / release / handoff, B3) streams to
    /// the same observers the rest of the run narrates to.
    ///
    /// # Errors
    ///
    /// Returns a [`req::Error`](crate::req::Error) (wrapping the underlying
    /// coordinator open failure) if the git repository at the workspace root
    /// cannot be opened or initialized.
    fn coordinator(&mut self) -> Result<Arc<Coordinator>, crate::req::Error> {
        if let Some(coord) = &self.coordinator {
            return Ok(Arc::clone(coord));
        }
        let coord =
            Coordinator::open(&self.workspace_root).map_err(|e| crate::req::Error::Decode {
                context: "engine",
                source: <serde_json::Error as serde::de::Error>::custom(format!(
                    "coordinator open failed: {e}"
                )),
            })?;
        // Narrate the coordinator's transaction lifecycle to the same sink the
        // master session uses, so B3 lock events reach every observer.
        let coord = coord.with_event_sink(self.session.event_sink());
        let coord = Arc::new(coord);
        self.coordinator = Some(Arc::clone(&coord));
        Ok(coord)
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
        // the default `NullSink` this is transparent. It also shares the master's
        // single coordinator, so its edits route through one lock table + one Vcs.
        let coordinator = self.coordinator()?;
        let client = self.session.client_clone();
        let events = self.session.event_sink();
        let handle = spawn_slave_with_coordinator(
            client,
            self.workspace_root.clone(),
            task,
            events,
            coordinator,
        );

        // A panicked slave thread becomes a `Failed` report rather than an error.
        Ok(handle
            .join()
            .unwrap_or_else(|_| SlaveReport::failed("Slave thread panicked.")))
    }

    /// Runs the LLM-driven orchestration for a high-level `goal`.
    ///
    /// The master is (re)configured as a planning [`Session`]: it is given the
    /// [`orchestration`] tool set (`create_theme` / `create_article` /
    /// `list_articles` / `dispatch_writer` / `list_reports`) and the orchestrator
    /// system prompt, `goal` is pushed as the first user turn, and
    /// [`Session::run_until_done`] drives the model. The model itself plans the
    /// article set, creates the structure, dispatches one writer per article
    /// (each a real slave thread via [`spawn_slave_with_sink`]), reviews the
    /// structured [`SlaveReport`]s, and finishes when the goal is met.
    ///
    /// The session reuses the master's [`Client`] and [`EventSink`] and runs with
    /// `options` (the model, round budget, and retry policy for the *master's* own
    /// rounds). Dispatched slaves run with [`SessionOptions::default`]; `slave_model`
    /// selects the model identity slaves write under (recorded in each article's
    /// contributor provenance), and `slave_skill_body`, when `Some`, sets the
    /// writing skill (composed with the fixed operational rules via
    /// [`compose_slave_prompt`]) every dispatched slave runs under — `None` uses the
    /// engine's built-in writer prompt. The returned [`GoalOutcome`] bundles the run's
    /// terminal [`Step`] outcome label, the master's final assistant message, and
    /// every [`SlaveReport`] the model collected. After it returns, [`Master::usage`]
    /// reflects the master's orchestration token usage.
    ///
    /// # Errors
    ///
    /// Fatal [`req`](crate::req) errors (e.g. an unrecoverable API failure during a
    /// master round) are returned as `Err`. A slave that fails is **not** an error:
    /// its failure is carried inside its [`SlaveReport`] in the returned outcome,
    /// for the model — and the caller — to react to. A master run that exhausts its
    /// round budget yields a `GoalOutcome` with outcome `"need_human"` rather than
    /// an `Err`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ai_write::engine::Master;
    /// use ai_write::session::{Session, SessionOptions};
    /// use ai_write::req::blocking::Client;
    /// use ai_write::tool::ToolRegistry;
    /// use ai_write::tool::workspace::Workspace;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = Client::from_env()?;
    /// let ws = Workspace::open("workspace")?;
    /// // The session passed to `new` only needs to carry the client/sink; `run_goal`
    /// // reconfigures it with the orchestration tools and prompt.
    /// let session = Session::new(client, "orchestrator", ToolRegistry::new(), SessionOptions::default());
    /// let mut master = Master::new(session, ws);
    /// let outcome = master.run_goal(
    ///     "Write a two-article beginner guide to Rust ownership.",
    ///     SessionOptions::default(),
    ///     "deepseek-v4-pro",
    ///     None, // use the engine's built-in writer prompt
    /// )?;
    /// println!("{} ({} reports)", outcome.outcome, outcome.reports.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn run_goal(
        &mut self,
        goal: &str,
        options: SessionOptions,
        slave_model: &str,
        slave_skill_body: Option<&str>,
    ) -> Result<GoalOutcome, crate::req::Error> {
        self.run_goal_with_skill(goal, options, slave_model, slave_skill_body, None)
    }

    /// Like [`run_goal`](Master::run_goal), but additionally takes an on-disk
    /// [`SlaveSkill`] source that every dispatched slave **re-reads each round**
    /// (kernel §4).
    ///
    /// When `slave_skill` is `Some`, each slave's system prompt is recomposed from
    /// that skill (stack) on every round, so editing a skill mid-run changes the
    /// prompt slaves see on their next round. `slave_skill_body` then serves only as
    /// the static fallback used if the skill file fails to load. When `slave_skill`
    /// is `None` this is exactly [`run_goal`](Master::run_goal): the prompt is fixed
    /// at dispatch.
    ///
    /// This is the single-skill convenience over
    /// [`run_goal_with_skills`](Master::run_goal_with_skills): `slave_skill_body` is
    /// treated as a one-element stack.
    ///
    /// # Errors
    ///
    /// Identical to [`run_goal`](Master::run_goal): a fatal master-round
    /// [`req`](crate::req) error is returned as `Err`; slave failures and an
    /// exhausted round budget are carried in the [`GoalOutcome`].
    pub fn run_goal_with_skill(
        &mut self,
        goal: &str,
        options: SessionOptions,
        slave_model: &str,
        slave_skill_body: Option<&str>,
        slave_skill: Option<SlaveSkill>,
    ) -> Result<GoalOutcome, crate::req::Error> {
        let bodies: Vec<&str> = slave_skill_body.into_iter().collect();
        self.run_goal_with_skills(goal, options, slave_model, &bodies, slave_skill)
    }

    /// Like [`run_goal_with_skill`](Master::run_goal_with_skill), but takes an
    /// **ordered stack** of writing-skill bodies, activating more than one skill at
    /// once (kernel §10).
    ///
    /// The bodies are folded into one voice block by
    /// [`compose_slave_prompt_multi`] (earliest first; a later skill overrides an
    /// earlier one on conflicting directives) and that becomes the static fallback
    /// prompt. When `slave_skill` is `Some` — typically a [`SlaveSkill`] whose
    /// [`ids`](SlaveSkill::ids) match `slave_skill_bodies` in order — each slave
    /// re-reads and re-stacks those skills from disk every round (kernel §4), so a
    /// mid-run edit to any skill in the stack affects subsequent rounds. An empty
    /// `slave_skill_bodies` and a `None` `slave_skill` reproduce
    /// [`run_goal`](Master::run_goal).
    ///
    /// # Errors
    ///
    /// Identical to [`run_goal`](Master::run_goal): a fatal master-round
    /// [`req`](crate::req) error is returned as `Err`; slave failures and an
    /// exhausted round budget are carried in the [`GoalOutcome`].
    pub fn run_goal_with_skills(
        &mut self,
        goal: &str,
        options: SessionOptions,
        slave_model: &str,
        slave_skill_bodies: &[impl AsRef<str>],
        slave_skill: Option<SlaveSkill>,
    ) -> Result<GoalOutcome, crate::req::Error> {
        // The single coordinator the master session and every dispatched slave
        // share (kernel §6): one lock table, one Vcs, deadlock-free.
        let coordinator = self.coordinator()?;

        // Reuse the master's client and event sink so the orchestration session
        // drives the same backend and narrates into the same feed.
        let client = self.session.client_clone();
        let events = self.session.event_sink();

        // Compose the static fallback prompt every dispatched slave runs under: the
        // chosen writing-skill stack (voice) ahead of the fixed operational rules,
        // or the built-in default when no skill was selected. When `slave_skill`
        // names an on-disk source, each slave re-reads and re-stacks it per round
        // (kernel §4, §10) and this is only the fallback on a read failure.
        let slave_prompt = compose_slave_prompt_multi(slave_skill_bodies);

        // Shared state the dispatch / report tools record into and read from. It
        // carries the shared coordinator so each slave it spawns joins the same
        // transaction authority, and the optional skill source so each slave
        // re-reads its prompt from disk per round.
        let state = Arc::new(
            OrchestratorState::new(
                client.clone(),
                self.workspace_root.clone(),
                Arc::clone(&events),
                slave_model.to_string(),
                slave_prompt,
                Arc::clone(&coordinator),
            )
            .with_slave_skill(slave_skill),
        );

        // Build the orchestration session: master prompt + orchestration tools,
        // dispatching workspace tools under a human "master" identity against the
        // workspace this master governs, all routed through the shared coordinator.
        let mut session = Session::new(
            client,
            MASTER_SYSTEM_PROMPT,
            orchestration_tools(Arc::clone(&state)),
            options,
        );
        session.set_workspace(self.workspace_root.clone(), WriterId::Human);
        session.set_event_sink("master", events);
        session.set_coordinator(coordinator);
        session.push_user(goal);

        let terminal = session.run_until_done();

        // Surface a fatal master-round failure as an `Err`; everything else is a
        // structured outcome the caller (and model) can act on.
        if let Step::Failed(err) = terminal {
            // Adopt the session so `usage()` still reflects the partial run.
            self.session = session;
            return Err(err);
        }

        let (outcome, message) = match &terminal {
            Step::Done(text) => ("done", text.clone()),
            Step::NeedHuman => ("need_human", String::new()),
            // `run_until_done` only returns terminal steps; these are unreachable
            // in practice but keep the match exhaustive without a panic.
            Step::Tool(_) | Step::Message(_) | Step::Failed(_) => ("need_human", String::new()),
        };

        let reports = state.reports();
        // Adopt the orchestration session so `Master::usage` reports the master's
        // own token usage for this goal.
        self.session = session;
        Ok(GoalOutcome {
            outcome: outcome.to_string(),
            message,
            reports,
        })
    }

    /// Returns the cumulative token usage of the master's orchestration
    /// [`Session`].
    ///
    /// After [`Master::run_one`] these totals are zero: that flow is deterministic
    /// Rust and performs no master-side chat completion. After [`Master::run_goal`]
    /// they reflect the master's own planning rounds (the model deciding what to
    /// create and dispatch). In either case a slave's own token usage lives in the
    /// slave session on its thread and is not folded into the master.
    pub fn usage(&self) -> &crate::session::UsageTotals {
        self.session.usage()
    }
}

/// Builds the master's orchestration [`ToolRegistry`]: theme / article structure
/// tools plus the dispatch / report tools backed by shared `state`.
///
/// This is the tool set [`Master::run_goal`] configures its planning session
/// with. The structure tools ([`CreateTheme`](orchestration::CreateTheme) /
/// [`CreateArticle`](orchestration::CreateArticle) /
/// [`ListArticles`](orchestration::ListArticles)) operate through the sandboxed
/// workspace; the delegation tools ([`DispatchWriter`](orchestration::DispatchWriter)
/// / [`ListReports`](orchestration::ListReports)) share `state` so dispatched
/// reports accumulate where [`Master::run_goal`] can read them back.
fn orchestration_tools(state: Arc<OrchestratorState>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(orchestration::CreateTheme))
        .register(Box::new(orchestration::CreateArticle))
        .register(Box::new(orchestration::ListArticles))
        .register(Box::new(orchestration::OrganizeArticles))
        .register(Box::new(orchestration::DispatchWriter::new(Arc::clone(
            &state,
        ))))
        .register(Box::new(orchestration::ListReports::new(state)));
    registry
}

/// The result of a completed [`Master::run_goal`] orchestration.
///
/// It bundles how the master's own run ended with the concrete work product —
/// every [`SlaveReport`] the model collected while pursuing the goal — so a caller
/// can render the outcome without re-reading the master's transcript.
#[derive(Debug, Clone)]
pub struct GoalOutcome {
    /// The master run's terminal outcome label: `"done"` when the model finished
    /// cleanly, or `"need_human"` when it stopped early (e.g. the round budget was
    /// exhausted). A fatal error surfaces as an `Err` from
    /// [`Master::run_goal`] rather than here.
    pub outcome: String,
    /// The master's final assistant message (its closing summary), empty when the
    /// run ended without a final `stop` message.
    pub message: String,
    /// Every [`SlaveReport`] collected during the run, in dispatch order — the
    /// per-article outcomes the model gathered.
    pub reports: Vec<SlaveReport>,
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
    fn model_for_writer_pins_the_agents_dated_snapshot() {
        // Kernel §9: a slave runs the exact model id it signs commits with, so a
        // pinned dated snapshot drives the wire request as well as provenance.
        let pinned = WriterId::Agent {
            model: "deepseek-v4-pro-2026-05-01".to_string(),
            label: "s1".to_string(),
        };
        assert_eq!(
            model_for_writer(&pinned),
            Model::pinned("deepseek-v4-pro-2026-05-01")
        );
        // A bare family id normalizes to its named variant.
        assert_eq!(model_for_writer(&agent()), Model::V4Pro);
        // A human writer carries no model id, so the default is used.
        assert_eq!(
            model_for_writer(&WriterId::Human),
            SessionOptions::default().model
        );
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
        // A non-report tool reply (e.g. an edit tool's `{"locked": ...}`-style
        // echo) must not be mistaken for a report, and an unknown report status is
        // skipped.
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
            system_prompt: None,
            skill: None,
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
                    Event::TxnAcquired { .. } => "TxnAcquired",
                    Event::TxnQueued { .. } => "TxnQueued",
                    Event::TxnReleased { .. } => "TxnReleased",
                    Event::HandoffToHuman { .. } => "HandoffToHuman",
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
            system_prompt: None,
            skill: None,
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

    // --- run_goal: full LLM-driven orchestration over a loopback fake --------

    /// Spawns a one-shot loopback HTTP server that answers each incoming POST with
    /// the next canned body in `bodies` (200 OK), in order, then exits.
    ///
    /// This is the same fake pattern the session tests use: it serves fixed JSON
    /// over `127.0.0.1`, so a full master/slave run can be exercised with zero
    /// network egress. Because [`Master::run_goal`] dispatches each writer
    /// synchronously (the master round blocks until the slave joins), every
    /// request — master rounds and the interleaved slave round alike — arrives in
    /// a single deterministic order, so one ordered list of bodies suffices.
    fn spawn_ordered_fake_api(bodies: Vec<String>) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().expect("accept");
                // Drain the request: read headers, then the Content-Length body.
                let mut buf = [0u8; 8192];
                let mut data = Vec::new();
                loop {
                    let n = stream.read(&mut buf).expect("read request");
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&data);
                    if let Some(headers_end) = text.find("\r\n\r\n") {
                        let header_block = &text[..headers_end];
                        let content_len = header_block
                            .lines()
                            .find_map(|l| {
                                let l = l.to_ascii_lowercase();
                                l.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        let body_so_far = data.len() - (headers_end + 4);
                        if body_so_far >= content_len {
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.as_bytes().len(),
                    body
                );
                stream.write_all(response.as_bytes()).expect("write resp");
                stream.flush().ok();
            }
        });
        base
    }

    /// A canned assistant response that issues a single tool call to `name` with
    /// the given JSON `arguments`, with `tool_calls` as the finish reason.
    fn tool_call_body(call_id: &str, name: &str, arguments: serde_json::Value) -> String {
        serde_json::json!({
            "id": call_id,
            "object": "chat.completion",
            "created": 0,
            "model": "deepseek-v4-flash",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": format!("calling {name}"),
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments.to_string() }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8,
                "prompt_cache_hit_tokens": 0, "prompt_cache_miss_tokens": 5
            }
        })
        .to_string()
    }

    /// A canned final `stop` response carrying `text`.
    fn stop_body(text: &str) -> String {
        serde_json::json!({
            "id": "stop",
            "object": "chat.completion",
            "created": 0,
            "model": "deepseek-v4-flash",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": text },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 6, "completion_tokens": 2, "total_tokens": 8,
                "prompt_cache_hit_tokens": 0, "prompt_cache_miss_tokens": 6
            }
        })
        .to_string()
    }

    /// Like [`spawn_ordered_fake_api`], but also records each request body into the
    /// returned shared buffer, so a test can assert what the slave actually sent
    /// (notably the `system` message) on each round.
    fn spawn_recording_fake_api(
        bodies: Vec<String>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let recorded: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 8192];
                let mut data = Vec::new();
                let mut body_start = 0usize;
                let mut content_len = 0usize;
                loop {
                    let n = stream.read(&mut buf).expect("read request");
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&data);
                    if let Some(headers_end) = text.find("\r\n\r\n") {
                        let header_block = &text[..headers_end];
                        content_len = header_block
                            .lines()
                            .find_map(|l| {
                                let l = l.to_ascii_lowercase();
                                l.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        body_start = headers_end + 4;
                        if data.len() - body_start >= content_len {
                            break;
                        }
                    }
                }
                let req_body = String::from_utf8_lossy(&data[body_start..body_start + content_len])
                    .to_string();
                sink.lock().expect("not poisoned").push(req_body);

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.as_bytes().len(),
                    body
                );
                stream.write_all(response.as_bytes()).expect("write resp");
                stream.flush().ok();
            }
        });
        (base, recorded)
    }

    /// Extracts the `system` message content from a recorded JSON ChatRequest body.
    fn system_of(request_body: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(request_body).expect("request is JSON");
        v.get("messages")
            .and_then(|m| m.as_array())
            .and_then(|msgs| {
                msgs.iter()
                    .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
            })
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .expect("a system message with string content")
            .to_string()
    }

    #[test]
    fn slave_rereads_skill_from_disk_each_round_so_a_midrun_edit_takes_effect() {
        // G8 (kernel §4): a slave's system prompt is composed from the skill file on
        // disk and re-read each round. Editing the skill file between rounds changes
        // the system message sent on the next round.
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace_root = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_root).expect("mkdir workspace");
        let coordinator = Arc::new(Coordinator::open(&workspace_root).expect("coordinator"));

        // The skill lives on disk; its body leads the composed slave prompt.
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).expect("mkdir skills");
        let skill_file = skills_dir.join("voice.md");
        std::fs::write(&skill_file, "VOICE ONE: terse and plain.").expect("write skill v1");

        // Two rounds: the model calls `read_article` (so the run does not finish on
        // round 1), then stops on round 2 — letting us inspect both requests.
        let (base, recorded) = spawn_recording_fake_api(vec![
            tool_call_body(
                "c1",
                "read_article",
                serde_json::json!({ "theme": "t", "file_name": "a.md" }),
            ),
            stop_body("done"),
        ]);
        let client = Client::builder()
            .api_key("test-key")
            .base_url(base)
            .build()
            .expect("fake client");

        let task = SlaveTask {
            theme: "t".into(),
            file_name: "a.md".into(),
            task: "write it".into(),
            writer: agent(),
            system_prompt: None,
            skill: Some(SlaveSkill {
                dir: skills_dir.clone(),
                ids: vec!["voice".into()],
            }),
        };
        let mut session = build_slave_session(
            client,
            workspace_root.to_str().expect("utf-8 path"),
            &task,
            Arc::new(NullSink),
            coordinator,
        );
        session.push_user("go");

        // Round 1 reads the skill as it stands now (v1).
        let step1 = session.run_round();
        assert!(matches!(step1, Step::Tool(_)), "round 1 dispatches a tool");

        // A human edits the skill file mid-run (SSOT is disk, kernel §4).
        std::fs::write(&skill_file, "VOICE TWO: lyrical and expansive.").expect("write skill v2");

        // Round 2 must reflect the edited skill.
        let _ = session.run_round();

        let bodies = recorded.lock().expect("not poisoned");
        assert_eq!(bodies.len(), 2, "two requests were sent");
        let sys1 = system_of(&bodies[0]);
        let sys2 = system_of(&bodies[1]);

        // Each round's system prompt is the skill body composed with the fixed
        // operational rules (so the writer can never drop the `report` discipline).
        assert!(
            sys1.starts_with("VOICE ONE: terse and plain."),
            "round 1 uses v1"
        );
        assert!(
            sys1.contains("report"),
            "operational rules are always appended"
        );
        assert!(
            sys2.starts_with("VOICE TWO: lyrical and expansive."),
            "round 2 reflects the mid-run edit"
        );
        assert!(
            sys2.contains("report"),
            "operational rules still appended after edit"
        );
        assert_ne!(
            sys1, sys2,
            "editing the skill file between rounds changed the system message"
        );
    }

    #[test]
    fn slave_stacks_two_skills_in_order_with_later_wins_precedence() {
        // G10 (kernel §10): a slave configured with a two-skill stack composes both
        // bodies into the system prompt, earliest first, carrying the documented
        // later-overrides-earlier precedence directive — and the stack is re-read
        // from disk each round (kernel §4) so a mid-run edit takes effect.
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace_root = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_root).expect("mkdir workspace");
        let coordinator = Arc::new(Coordinator::open(&workspace_root).expect("coordinator"));

        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).expect("mkdir skills");
        let base_file = skills_dir.join("base.md");
        let refine_file = skills_dir.join("refine.md");
        std::fs::write(&base_file, "BASE: write functional prose.").expect("write base");
        std::fs::write(&refine_file, "REFINE: keep it under 200 words.").expect("write refine");

        let (base, recorded) = spawn_recording_fake_api(vec![
            tool_call_body(
                "c1",
                "read_article",
                serde_json::json!({ "theme": "t", "file_name": "a.md" }),
            ),
            stop_body("done"),
        ]);
        let client = Client::builder()
            .api_key("test-key")
            .base_url(base)
            .build()
            .expect("fake client");

        // The stack order is [base, refine]: refine wins on conflict.
        let task = SlaveTask {
            theme: "t".into(),
            file_name: "a.md".into(),
            task: "write it".into(),
            writer: agent(),
            system_prompt: None,
            skill: Some(SlaveSkill {
                dir: skills_dir.clone(),
                ids: vec!["base".into(), "refine".into()],
            }),
        };
        let mut session = build_slave_session(
            client,
            workspace_root.to_str().expect("utf-8 path"),
            &task,
            Arc::new(NullSink),
            coordinator,
        );
        session.push_user("go");

        let step1 = session.run_round();
        assert!(matches!(step1, Step::Tool(_)), "round 1 dispatches a tool");

        // Edit the *later* skill mid-run; the next round must reflect it.
        std::fs::write(&refine_file, "REFINE: keep it under 50 words.").expect("rewrite refine");
        let _ = session.run_round();

        let bodies = recorded.lock().expect("not poisoned");
        let sys1 = system_of(&bodies[0]);
        let sys2 = system_of(&bodies[1]);

        // Both skills are present, base before refine (precedence order positional).
        let base_at = sys1
            .find("BASE: write functional prose.")
            .expect("base present");
        let refine_at = sys1
            .find("REFINE: keep it under 200 words.")
            .expect("refine present");
        assert!(base_at < refine_at, "earlier skill composes first");
        // The precedence rule is stated in the prompt for the model to apply.
        assert!(
            sys1.contains("LATER in the stack overrides"),
            "stack precedence directive is present"
        );
        // Operational rules are still appended and authoritative over the stack.
        assert!(sys1.contains("report"), "operational rules appended");
        // The mid-run edit to the later skill takes effect on round 2.
        assert!(
            sys2.contains("keep it under 50 words."),
            "round 2 reflects the edited later skill"
        );
        assert!(
            !sys2.contains("keep it under 200 words."),
            "the stale later-skill text is gone"
        );
    }

    #[test]
    fn slave_without_skill_source_keeps_a_fixed_prompt_across_rounds() {
        // Back-compat: with no `skill` source, the slave prompt is fixed for the run
        // (byte-identical to prior behaviour), even though a provider hook exists.
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace_root = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace_root).expect("mkdir workspace");
        let coordinator = Arc::new(Coordinator::open(&workspace_root).expect("coordinator"));

        let (base, recorded) = spawn_recording_fake_api(vec![
            tool_call_body(
                "c1",
                "read_article",
                serde_json::json!({ "theme": "t", "file_name": "a.md" }),
            ),
            stop_body("done"),
        ]);
        let client = Client::builder()
            .api_key("test-key")
            .base_url(base)
            .build()
            .expect("fake client");

        let task = SlaveTask {
            theme: "t".into(),
            file_name: "a.md".into(),
            task: "write it".into(),
            writer: agent(),
            system_prompt: None,
            skill: None,
        };
        let mut session = build_slave_session(
            client,
            workspace_root.to_str().expect("utf-8 path"),
            &task,
            Arc::new(NullSink),
            coordinator,
        );
        session.push_user("go");
        let _ = session.run_round();
        let _ = session.run_round();

        let bodies = recorded.lock().expect("not poisoned");
        assert_eq!(bodies.len(), 2);
        let sys1 = system_of(&bodies[0]);
        let sys2 = system_of(&bodies[1]);
        // Identical across rounds, and equal to the engine's default slave prompt.
        assert_eq!(sys1, sys2);
        assert_eq!(sys1, default_slave_prompt());
    }

    #[test]
    fn run_goal_plans_dispatches_and_aggregates_over_a_fake() {
        // The scripted master conversation:
        //   r1: create_theme rust
        //   r2: create_article rust/intro.md
        //   r3: dispatch_writer rust/intro.md  -> spawns a slave (1 round) then
        //       the master round completes with the slave's report
        //   r4: stop (final summary)
        // Interleaved on the wire (the slave round runs *inside* r3's dispatch):
        //   master-r1, master-r2, master-r3, SLAVE, master-r4
        let bodies = vec![
            tool_call_body("c1", "create_theme", serde_json::json!({ "theme": "rust" })),
            tool_call_body(
                "c2",
                "create_article",
                serde_json::json!({ "theme": "rust", "file_name": "intro.md", "title": "Intro" }),
            ),
            tool_call_body(
                "c3",
                "dispatch_writer",
                serde_json::json!({
                    "theme": "rust",
                    "file_name": "intro.md",
                    "task": "Write a short intro to ownership."
                }),
            ),
            // The slave's single round: it stops without calling `report`, so the
            // engine synthesizes a `done` report from the terminal step.
            stop_body("intro drafted"),
            // The master's closing summary.
            stop_body("All articles written: rust/intro.md is done."),
        ];
        let base = spawn_ordered_fake_api(bodies);

        let client = Client::builder()
            .api_key("test-key")
            .base_url(base)
            .build()
            .expect("client");
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::open(dir.path()).expect("open workspace");

        // The session handed to `new` only needs to carry the client; `run_goal`
        // reconfigures it with the orchestration tools and master prompt.
        let session = Session::new(
            client,
            "placeholder",
            crate::tool::ToolRegistry::new(),
            SessionOptions::default(),
        );
        let mut master = Master::new(session, ws);

        let outcome = master
            .run_goal(
                "Write a short beginner guide to Rust ownership.",
                SessionOptions::default(),
                "deepseek-v4-pro",
                None,
            )
            .expect("goal runs without a fatal error");

        // The master finished cleanly with its summary.
        assert_eq!(outcome.outcome, "done");
        assert_eq!(
            outcome.message,
            "All articles written: rust/intro.md is done."
        );

        // Exactly one writer was dispatched and its report aggregated.
        assert_eq!(outcome.reports.len(), 1, "one writer dispatched");
        assert_eq!(outcome.reports[0].status, SlaveStatus::Done);

        // The plan's structural side effects landed on disk: the theme and the
        // article the model created exist and are listable.
        let probe = Workspace::open(dir.path()).expect("reopen");
        assert_eq!(probe.list_articles("rust").unwrap(), vec!["intro.md"]);

        // The master's own planning rounds accrued token usage (it really drove
        // the model), distinct from the deterministic `run_one` path.
        assert!(master.usage().rounds >= 1, "master ran real rounds");
    }

    #[test]
    fn run_goal_surfaces_a_collected_slave_failure_without_erroring() {
        // The master dispatches one writer whose slave round ends in a *fatal*
        // (non-transient) error — HTTP 400, which is never retried — so exactly
        // one slave request is served. The slave's failure becomes a `Failed`
        // report carried back in the outcome, NOT an `Err` from `run_goal`.
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            // master-r1: dispatch_writer; SLAVE: 400 (fatal); master-r2: stop.
            let dispatch = tool_call_body(
                "c1",
                "dispatch_writer",
                serde_json::json!({
                    "theme": "rust",
                    "file_name": "intro.md",
                    "task": "Write something."
                }),
            );
            let summary = stop_body("A writer failed; reporting back.");
            // (body, status_line). HTTP 400 is non-transient, so the slave does
            // not retry and the fake server serves exactly these three requests.
            let steps: Vec<(String, &str)> = vec![
                (dispatch, "200 OK"),
                (
                    String::from("{\"error\":{\"message\":\"bad request\"}}"),
                    "400 Bad Request",
                ),
                (summary, "200 OK"),
            ];
            for (body, status) in steps {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.as_bytes().len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                stream.flush().ok();
            }
        });

        let client = Client::builder()
            .api_key("test-key")
            .base_url(base)
            .build()
            .expect("client");
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::open(dir.path()).expect("open workspace");
        // Pre-create the article so the failure is the slave's round, not setup.
        let mut setup = Workspace::open(dir.path()).expect("setup ws");
        setup.create_theme("rust").unwrap();
        setup
            .create_article("rust", "intro.md", "Intro", None)
            .unwrap();

        let session = Session::new(
            client,
            "placeholder",
            crate::tool::ToolRegistry::new(),
            SessionOptions::default(),
        );
        let mut master = Master::new(session, ws);

        let outcome = master
            .run_goal(
                "Write one article.",
                SessionOptions::default(),
                "deepseek-v4-pro",
                None,
            )
            .expect("a collected slave failure is not a master error");

        assert_eq!(outcome.outcome, "done");
        assert_eq!(outcome.reports.len(), 1);
        assert_eq!(outcome.reports[0].status, SlaveStatus::Failed);
    }
}
