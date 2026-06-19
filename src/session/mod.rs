//! General-purpose, stateful agentic session over the stateless [`req`] client.
//!
//! A [`Session`] is the **business-agnostic** turn engine that both the `Master`
//! and `Slave` roles in [`engine`](crate::engine) are built on. It owns a system
//! prompt (a fixed string, or — via [`Session::set_system_provider`] — a closure
//! re-evaluated each round so the prompt can be re-read from disk; kernel §4), the
//! running message history, cumulative token usage, and a
//! handle to a [`ToolRegistry`]. Each *round* assembles
//! the messages, performs one [`req`] chat completion (advertising the registered
//! tools), branches on the returned [`FinishReason`],
//! dispatches any requested tools, feeds their results back, and repeats —
//! bounded by [`SessionOptions::max_rounds`].
//!
//! The session deliberately knows nothing about writing, themes, or articles;
//! that knowledge lives entirely in the tools it is configured with and in its
//! system prompt. "Research → write → revise" is therefore an *emergent* loop,
//! not a hard-coded state machine.
//!
//! # Persistence
//!
//! A session can be snapshotted to a serializable [`SessionSnapshot`] (system
//! prompt + history + usage + options) and later restored, allowing a theme to
//! be closed and reopened without losing context. The live [`Client`] and
//! [`ToolRegistry`] are *not* part of the snapshot and must be supplied again on
//! restore.
//!
//! [`req`]: crate::req
//! [`Client`]: crate::req::blocking::Client

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::coordinator::Coordinator;
use crate::observe::{Event, EventSink, NullSink};
use crate::req::blocking::Client;
use crate::req::{ChatRequest, FinishReason, Message, Model, Thinking, ToolChoice, Usage};
use crate::tool::workspace::{Workspace, WriterId};
use crate::tool::{ToolCtx, ToolRegistry};
use crate::vcs::Vcs;

/// The maximum number of system-prompt characters carried in an
/// [`Event::SessionStarted`] excerpt, to keep the event small for a UI.
const SYSTEM_EXCERPT_LEN: usize = 200;

/// Cumulative token usage across every round of a [`Session`].
///
/// Each field is the running sum of the corresponding [`Usage`] field reported
/// by the server; this crate never estimates token counts. The struct is
/// serializable so it survives a snapshot/restore cycle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    /// Total input tokens billed across all rounds.
    pub prompt_tokens: u64,
    /// Total output tokens (including thinking tokens) across all rounds.
    pub completion_tokens: u64,
    /// `prompt_tokens + completion_tokens` across all rounds.
    pub total_tokens: u64,
    /// Total input tokens served from the prefix cache.
    pub prompt_cache_hit_tokens: u64,
    /// Total input tokens not served from the prefix cache.
    pub prompt_cache_miss_tokens: u64,
    /// Total thinking-chain tokens, when the server reported them.
    pub reasoning_tokens: u64,
    /// Number of completed chat completions folded into these totals.
    pub rounds: u64,
}

impl UsageTotals {
    /// Folds a single round's [`Usage`] into the running totals, incrementing the
    /// round counter.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ai_write::session::UsageTotals;
    /// use ai_write::req::Usage;
    ///
    /// let mut totals = UsageTotals::default();
    /// let usage = Usage {
    ///     prompt_tokens: 10,
    ///     completion_tokens: 5,
    ///     total_tokens: 15,
    ///     prompt_cache_hit_tokens: 0,
    ///     prompt_cache_miss_tokens: 10,
    ///     completion_tokens_details: None,
    ///     prompt_tokens_details: None,
    /// };
    /// totals.add(&usage);
    /// assert_eq!(totals.total_tokens, 15);
    /// assert_eq!(totals.rounds, 1);
    /// ```
    pub fn add(&mut self, usage: &Usage) {
        self.prompt_tokens += u64::from(usage.prompt_tokens);
        self.completion_tokens += u64::from(usage.completion_tokens);
        self.total_tokens += u64::from(usage.total_tokens);
        self.prompt_cache_hit_tokens += u64::from(usage.prompt_cache_hit_tokens);
        self.prompt_cache_miss_tokens += u64::from(usage.prompt_cache_miss_tokens);
        self.reasoning_tokens += u64::from(usage.reasoning_tokens().unwrap_or(0));
        self.rounds += 1;
    }
}

/// The smallest externally observable step a [`Session`] can take, returned by
/// [`Session::run_round`] and [`Session::run_until_done`].
///
/// A round maps onto exactly one [`FinishReason`] branch (with
/// transient errors retried internally). Tool dispatch is summarized by the names
/// of the tools invoked, never the raw tool transcript.
///
/// Not `Clone`: the [`Step::Failed`] arm carries a
/// [`req::Error`](crate::req::Error), which is intentionally not `Clone`.
#[derive(Debug)]
#[non_exhaustive]
pub enum Step {
    /// The model requested one or more tools; the listed tools were dispatched
    /// and their results fed back into the history. The loop should continue.
    Tool(Vec<String>),
    /// The model produced an intermediate assistant message (no tool call, not
    /// yet a self-assessed completion). Carries the message text.
    Message(String),
    /// The model self-assessed the task as complete (`stop`). Carries the final
    /// assistant text.
    Done(String),
    /// The session needs human intervention before it can proceed (for example,
    /// the round budget was exhausted or a tool signalled an escalation).
    NeedHuman,
    /// A fatal error ended the session. Transient errors are retried internally
    /// and do not surface here.
    Failed(crate::req::Error),
}

/// Configuration for a [`Session`], fixed at construction and carried through a
/// snapshot.
///
/// The system prompt is intentionally **not** part of these options (it is passed
/// separately to [`Session::new`], or supplied per round by a
/// [system-prompt provider](Session::set_system_provider) for the kernel §4
/// re-read-from-disk behaviour).
///
/// Only [`Serialize`] is derived: the [`model`](SessionOptions::model) and
/// [`thinking`](SessionOptions::thinking) fields reuse the [`req`](crate::req)
/// types verbatim for an ergonomic public API, and those types are
/// `Serialize`-only (this crate must not modify `req`). A snapshot can therefore
/// be written out, and a session is reconstructed via [`Session::restore`] from a
/// snapshot value rather than by re-deserializing these `req` types in isolation.
#[derive(Debug, Clone, Serialize)]
pub struct SessionOptions {
    /// The model to use for every round.
    pub model: crate::req::Model,
    /// Thinking-mode control applied to every request. `None` uses the server
    /// default (thinking enabled).
    pub thinking: Option<Thinking>,
    /// Maximum number of tokens to generate per round.
    pub max_tokens: Option<u32>,
    /// Hard ceiling on the number of rounds [`Session::run_until_done`] will run
    /// before yielding [`Step::NeedHuman`].
    pub max_rounds: u32,
    /// Maximum number of transient-error retries within a single round before the
    /// error is treated as fatal.
    pub max_retries: u32,
}

impl Default for SessionOptions {
    /// Returns the default options: [`Model::V4Flash`],
    /// server-default thinking, a 20-round ceiling, and 3 transient retries.
    fn default() -> Self {
        SessionOptions {
            model: Model::V4Flash,
            thinking: None,
            max_tokens: None,
            max_rounds: 20,
            max_retries: 3,
        }
    }
}

/// One named, ordered fragment appended to the effective system prompt by the
/// per-round composition (kernel §10: dynamic insertion / modification of the
/// system prompt while a run is in progress).
///
/// Segments are held in a [`Session`] by unique `name` and re-read on every round
/// in [`Session::build_request`], so a segment pushed, overridden, or removed
/// *between* rounds takes effect on the next round's system message. They are
/// layered **on top of** the base prompt (the static string passed to
/// [`Session::new`] or whatever the [system-prompt
/// provider](Session::set_system_provider) returns), never replacing it.
///
/// Ordering is deterministic and independent of `BTreeMap` key order: segments
/// are emitted by ascending [`ordinal`](SystemSegment::ordinal), then by `name`
/// to break ties. A caller that does not care about position can let every
/// segment default to ordinal `0` and rely on the name tiebreaker for a stable,
/// reproducible sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SystemSegment {
    /// The explicit sort key. Lower ordinals are emitted first; equal ordinals
    /// fall back to the segment `name` for a total, reproducible order.
    ordinal: i32,
    /// The fragment text appended to the composed system prompt.
    text: String,
}

/// A stateful, business-agnostic agentic session over the [`req`](crate::req)
/// client.
///
/// See the [module documentation](self) for the overall design. Construct one
/// with [`Session::new`], push a user turn with [`Session::push_user`], then
/// drive it with [`Session::run_round`] or [`Session::run_until_done`].
pub struct Session {
    /// The stateless client performing each HTTP round trip.
    client: Client,
    /// The static system prompt prepended to every request, and the fallback used
    /// when no [`system_provider`](Session::system_provider) is installed.
    ///
    /// It also seeds a [`SessionSnapshot`]: a provider (a live closure) cannot be
    /// serialized, so a snapshot captures this string. A round re-reads the
    /// effective prompt from the provider when one is set (kernel §4: the system
    /// prompt is state on disk, re-read each step, not pinned in context).
    system: String,
    /// An optional provider re-evaluated at the start of **every** round to
    /// recompose the effective system prompt from its source of truth (typically a
    /// skill file on disk).
    ///
    /// `None` — the default — means the prompt is the fixed
    /// [`system`](Session::system) string, so behaviour is byte-identical to a
    /// session without this hook. When `Some`, the closure is called once per round
    /// (in [`Session::build_request`]) and its return value is used as the system
    /// message for that round, so a mid-run edit to the underlying skill file
    /// affects every subsequent round (kernel §4, SSOT = disk, re-read per step).
    /// Only `Send` is required (a session is moved onto its own slave thread, never
    /// shared), and the provider is never part of a [`SessionSnapshot`].
    system_provider: Option<Box<dyn Fn() -> String + Send>>,
    /// Named, dynamically managed system-prompt segments layered on top of the
    /// base prompt (kernel §10). Keyed by unique segment name; each round's
    /// [`build_request`](Session::build_request) re-reads this map and appends the
    /// segments — ordered by [`SystemSegment::ordinal`] then name — after the base
    /// prompt. A segment inserted, overridden, or removed between rounds therefore
    /// changes only subsequent rounds' system message, exactly like an edit to the
    /// on-disk skill behind a [provider](Session::set_system_provider).
    ///
    /// Empty by default, so a session with no segments composes a system message
    /// byte-identical to one before G9 existed. Never part of a
    /// [`SessionSnapshot`] (the layering is a live runtime concern; a restored
    /// session re-installs whatever segments it needs).
    system_segments: BTreeMap<String, SystemSegment>,
    /// The running conversation history. Assistant turns are back-filled via
    /// [`RespMessage::to_history`](crate::req::RespMessage::to_history), which
    /// strips `reasoning_content`.
    history: Vec<Message>,
    /// The tools advertised to the model and dispatched on `tool_calls`.
    tools: ToolRegistry,
    /// Cumulative token usage across rounds.
    usage: UsageTotals,
    /// Behavioural configuration.
    options: SessionOptions,
    /// Filesystem root the [`Workspace`] is opened at when a tool call needs to
    /// touch the workspace. Not part of the public API nor of a
    /// [`SessionSnapshot`]; defaulted (to `"workspace"`) on
    /// [`Session::new`]/[`Session::restore`] and overridden by the
    /// [`engine`](crate::engine) layer via [`Session::set_workspace`]. It is only
    /// read on the `tool_calls` branch — never during the offline message-assembly
    /// path exercised by unit tests.
    workspace_root: std::path::PathBuf,
    /// The identity tool calls are dispatched under. Defaults to
    /// [`WriterId::Human`]; the engine sets the slave's agent identity.
    writer: WriterId,
    /// The workspace handle, opened lazily on the first tool-dispatching round
    /// and reused for the rest of the session's life. Reuse is essential: the
    /// single-writer article lock lives in memory on this handle, so re-opening
    /// it per round would silently drop a lock acquired in an earlier round.
    /// [`Session::set_workspace`] resets it to `None` so a new root re-opens.
    /// Never part of a [`SessionSnapshot`].
    workspace: Option<Workspace>,
    /// The workspace's version-control handle, opened lazily alongside
    /// [`workspace`](Session::workspace) at the same root and reused for the rest
    /// of the session. It is what turns each successful edit into a git commit
    /// authored by the session's [`writer`](Session::writer). Reset to `None` by
    /// [`Session::set_workspace`] so a new root re-opens its own repository, and
    /// never part of a [`SessionSnapshot`]. A `Vcs` wraps a non-`Sync`
    /// [`git2::Repository`](git2::Repository), but each session (and thus each
    /// slave thread) owns its own handle, so nothing is shared across threads.
    ///
    /// When a [`coordinator`](Session::coordinator) is attached, this fallback
    /// `Vcs` is **not** opened: edits and commits go through the coordinator's
    /// single shared handle instead.
    vcs: Option<Vcs>,
    /// The shared transaction coordinator (kernel §6), when the
    /// [`engine`](crate::engine) installed one. A master and all of its slaves
    /// share a single `Arc<Coordinator>`, so every mutating edit across every
    /// thread funnels through one operation-level lock table and one [`Vcs`]. When
    /// set, the session attaches it to each [`ToolCtx`] and skips opening its own
    /// per-session `Vcs`. Never part of a [`SessionSnapshot`].
    coordinator: Option<Arc<Coordinator>>,
    /// The push-based observability sink. Defaults to [`NullSink`] (every event
    /// discarded), so observability is transparent to callers that never install
    /// a real sink — the v0 offline tests are unaffected. The
    /// [`engine`](crate::engine) and WebUI layers install a sink with
    /// [`Session::set_event_sink`] to render the live operation feed. Held as an
    /// `Arc<dyn EventSink>` so it can be cloned to a slave thread and emitted to
    /// concurrently. Never part of a [`SessionSnapshot`].
    events: Arc<dyn EventSink>,
    /// A short role label (e.g. `"slave"`) carried in the
    /// [`Event::SessionStarted`] this session emits on its first round. Defaults
    /// to `"session"`; the engine sets it per role.
    role: String,
    /// The 1-based round counter for the current run, incremented at the start of
    /// every [`Session::run_round`] and reported in [`Event::RoundStarted`]. It is
    /// `0` before the first round; the transition `0 → 1` is what triggers the
    /// once-per-run [`Event::SessionStarted`].
    round: u32,
}

impl Session {
    /// Creates a new session with a fixed system prompt, a tool registry, and
    /// behavioural options.
    ///
    /// The history starts empty; call [`Session::push_user`] to add the first
    /// user turn before running a round.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ai_write::session::{Session, SessionOptions};
    /// use ai_write::tool::ToolRegistry;
    /// use ai_write::req::blocking::Client;
    ///
    /// let client = Client::from_env()?;
    /// let session = Session::new(
    ///     client,
    ///     "You are a careful writing assistant.",
    ///     ToolRegistry::new(),
    ///     SessionOptions::default(),
    /// );
    /// # Ok::<(), ai_write::req::Error>(())
    /// ```
    pub fn new(
        client: Client,
        system: impl Into<String>,
        tools: ToolRegistry,
        options: SessionOptions,
    ) -> Self {
        Session {
            client,
            system: system.into(),
            system_provider: None,
            system_segments: BTreeMap::new(),
            history: Vec::new(),
            tools,
            usage: UsageTotals::default(),
            options,
            workspace_root: std::path::PathBuf::from("workspace"),
            writer: WriterId::Human,
            workspace: None,
            vcs: None,
            coordinator: None,
            events: Arc::new(NullSink),
            role: "session".to_string(),
            round: 0,
        }
    }

    /// Installs the shared transaction [`Coordinator`] (kernel §6) every mutating
    /// edit routes through, returning the session for chaining.
    ///
    /// The [`engine`](crate::engine) layer calls this so a master and all of its
    /// slaves share one coordinator: locking becomes implicit per edit, and the
    /// single commit path is owned by the coordinator's exclusive [`Vcs`]. When a
    /// coordinator is set, the session does not open its own per-session `Vcs` for
    /// committing; the history / diff / undo tools read through the coordinator's
    /// handle. It affects only the `tool_calls` branch of a round.
    pub fn set_coordinator(&mut self, coordinator: Arc<Coordinator>) -> &mut Self {
        self.coordinator = Some(coordinator);
        self
    }

    /// Installs a **system-prompt provider** that is re-evaluated at the start of
    /// every round, so the effective system prompt is recomposed from its source of
    /// truth (typically a skill file on disk) instead of being pinned once at
    /// construction.
    ///
    /// This realizes kernel §4: the system prompt — like the article and the skill
    /// — is state on disk read fresh each step, not frozen into the model's
    /// context. A mid-run edit to the underlying file therefore takes effect on the
    /// next round. The [`engine`](crate::engine) installs a provider that re-reads
    /// the chosen [`Skill`](crate::skill::Skill) and recomposes the slave prompt
    /// each round.
    ///
    /// Without a provider (the default) the prompt is the fixed string passed to
    /// [`Session::new`], so existing behaviour is byte-identical. The provider is a
    /// live closure and is never captured by a [`SessionSnapshot`]; a snapshot
    /// records the static [`system`](Session::system) string instead. Returns the
    /// session for chaining.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicU64, Ordering};
    /// use ai_write::session::{Session, SessionOptions};
    /// use ai_write::tool::ToolRegistry;
    /// use ai_write::req::blocking::Client;
    ///
    /// let client = Client::from_env()?;
    /// let mut session = Session::new(client, "static", ToolRegistry::new(), SessionOptions::default());
    /// let round = Arc::new(AtomicU64::new(0));
    /// let r = Arc::clone(&round);
    /// session.set_system_provider(move || format!("prompt for round {}", r.fetch_add(1, Ordering::SeqCst)));
    /// assert_eq!(session.system(), "prompt for round 0");
    /// # Ok::<(), ai_write::req::Error>(())
    /// ```
    pub fn set_system_provider(
        &mut self,
        provider: impl Fn() -> String + Send + 'static,
    ) -> &mut Self {
        self.system_provider = Some(Box::new(provider));
        self
    }

    /// Inserts or overrides a named system-prompt **segment** layered on top of the
    /// base prompt, taking effect on the **next** round (kernel §10: dynamic
    /// insertion / modification of the system prompt while a run is in progress).
    ///
    /// The base prompt (the static [`system`](Session::system) string or the
    /// [provider](Session::set_system_provider)'s output) is left untouched;
    /// segments are appended after it in a deterministic order — ascending
    /// `ordinal`, then `name` to break ties — and joined with a blank line. Calling
    /// this with a `name` that already exists **overrides** that segment in place,
    /// keeping its position determined by the new `ordinal`; calling it with a new
    /// `name` inserts a new segment.
    ///
    /// Because the effective prompt is recomposed each round (when the next round's
    /// request is assembled), a segment pushed or overridden
    /// between rounds appears in every subsequent round's system message but does
    /// not retroactively alter rounds already sent — mirroring kernel §4's
    /// re-read-from-disk semantics for the base prompt. Returns the session for
    /// chaining.
    ///
    /// Segments are a live runtime concern and are **not** captured by a
    /// [`SessionSnapshot`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ai_write::session::{Session, SessionOptions};
    /// use ai_write::tool::ToolRegistry;
    /// use ai_write::req::blocking::Client;
    ///
    /// let client = Client::from_env()?;
    /// let mut session = Session::new(client, "base prompt", ToolRegistry::new(), SessionOptions::default());
    /// // Insert a high-priority constraint that the next round will see.
    /// session.push_system_segment("style", -10, "Write in plain, concrete language.");
    /// assert!(session.system().contains("Write in plain, concrete language."));
    /// // Overriding the same name replaces its text in place.
    /// session.push_system_segment("style", -10, "Write tersely.");
    /// assert!(session.system().contains("Write tersely."));
    /// assert!(!session.system().contains("plain, concrete"));
    /// # Ok::<(), ai_write::req::Error>(())
    /// ```
    pub fn push_system_segment(
        &mut self,
        name: impl Into<String>,
        ordinal: i32,
        text: impl Into<String>,
    ) -> &mut Self {
        self.system_segments.insert(
            name.into(),
            SystemSegment {
                ordinal,
                text: text.into(),
            },
        );
        self
    }

    /// Removes the named system-prompt [segment](Session::push_system_segment),
    /// returning `true` if a segment by that `name` was present (and thus removed).
    ///
    /// Like an insert or override, removal takes effect on the **next** round: the
    /// segment disappears from every subsequent round's composed system message.
    /// Removing a name that is not present is a no-op and returns `false`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ai_write::session::{Session, SessionOptions};
    /// use ai_write::tool::ToolRegistry;
    /// use ai_write::req::blocking::Client;
    ///
    /// let client = Client::from_env()?;
    /// let mut session = Session::new(client, "base", ToolRegistry::new(), SessionOptions::default());
    /// session.push_system_segment("note", 0, "Temporary note.");
    /// assert!(session.remove_system_segment("note"));
    /// assert!(!session.remove_system_segment("note")); // already gone
    /// assert_eq!(session.system(), "base");
    /// # Ok::<(), ai_write::req::Error>(())
    /// ```
    pub fn remove_system_segment(&mut self, name: &str) -> bool {
        self.system_segments.remove(name).is_some()
    }

    /// Returns the names of the currently installed system-prompt
    /// [segments](Session::push_system_segment), in the same deterministic order
    /// they are appended to the system message (ascending ordinal, then name).
    ///
    /// Useful for inspection and tests; the empty slice means no segments are
    /// layered and the effective prompt equals the base prompt.
    pub fn system_segment_names(&self) -> Vec<&str> {
        let mut by_name: Vec<(&str, &SystemSegment)> = self
            .system_segments
            .iter()
            .map(|(n, s)| (n.as_str(), s))
            .collect();
        // Stable sort by ordinal; `BTreeMap::iter` already yields name order, so
        // equal ordinals keep their name-sorted positions.
        by_name.sort_by_key(|(_, s)| s.ordinal);
        by_name.into_iter().map(|(n, _)| n).collect()
    }

    /// Returns the **base** system prompt for the current round: the value of the
    /// installed [provider](Session::set_system_provider) if any, otherwise the
    /// fixed [`system`](Session::system) string.
    ///
    /// When a provider is installed this re-evaluates it, so it reflects whatever
    /// the source of truth (e.g. a skill file) currently holds. The dynamic
    /// [segments](Session::push_system_segment) are *not* included here; they are
    /// layered on by [`effective_system`](Session::effective_system).
    fn base_system(&self) -> String {
        match &self.system_provider {
            Some(provider) => provider(),
            None => self.system.clone(),
        }
    }

    /// Returns the effective system prompt for the **current** round: the
    /// [base prompt](Session::base_system) with every dynamic
    /// [segment](Session::push_system_segment) appended in deterministic order.
    ///
    /// Composition is recomputed on every call (and thus every round, since
    /// [`build_request`](Session::build_request) calls it), so a provider
    /// re-evaluation or a mid-run segment edit is reflected immediately. Segments
    /// are ordered by ascending [`ordinal`](SystemSegment::ordinal) then by name,
    /// and joined to the base — and to one another — with a blank line. With no
    /// segments the result equals the base prompt exactly, so behaviour is
    /// unchanged from before G9.
    fn effective_system(&self) -> String {
        let base = self.base_system();
        if self.system_segments.is_empty() {
            return base;
        }
        // Deterministic order: ascending ordinal, then name as the tiebreaker.
        // `BTreeMap` already iterates by name; a stable sort by ordinal therefore
        // yields (ordinal, name) lexicographic order without disturbing equal-ordinal
        // ties, which stay in name order.
        let mut ordered: Vec<&SystemSegment> = self.system_segments.values().collect();
        ordered.sort_by_key(|seg| seg.ordinal);

        let mut out = base;
        for seg in ordered {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&seg.text);
        }
        out
    }

    /// Sets the filesystem root and writer identity used to build a
    /// [`ToolCtx`] when a round dispatches workspace tools.
    ///
    /// The [`engine`](crate::engine) layer calls this so a slave dispatches under
    /// its agent identity against the workspace it owns. It affects only the
    /// `tool_calls` branch of a round; the system prompt, history, and usage are
    /// untouched. Returns the session for chaining.
    pub fn set_workspace(
        &mut self,
        workspace_root: impl Into<std::path::PathBuf>,
        writer: WriterId,
    ) -> &mut Self {
        self.workspace_root = workspace_root.into();
        self.writer = writer;
        // Drop any cached handles so the next tool round re-opens both the
        // workspace and its version-control repository at the new root.
        self.workspace = None;
        self.vcs = None;
        self
    }

    /// Installs an [`EventSink`] so this session narrates its run as a stream of
    /// [`Event`]s, and tags the session with a short `role` label for
    /// [`Event::SessionStarted`].
    ///
    /// By default a session emits to a [`NullSink`] (every event discarded), so
    /// observability is opt-in: the [`engine`](crate::engine) and WebUI layers
    /// call this to render the live operation feed, while ordinary callers and the
    /// offline tests leave it unset and observe no behavioural change. The sink is
    /// shared (`Arc`), so the same one can be installed on a master and all its
    /// slaves to multiplex their events into one feed. Returns the session for
    /// chaining.
    pub fn set_event_sink(
        &mut self,
        role: impl Into<String>,
        events: Arc<dyn EventSink>,
    ) -> &mut Self {
        self.role = role.into();
        self.events = events;
        self
    }

    /// Returns a clone of the installed [`EventSink`] handle.
    ///
    /// Cloning an `Arc<dyn EventSink>` is cheap (a refcount bump). The
    /// [`engine`](crate::engine) layer uses this to hand a master's sink to a
    /// spawned slave, so both fan their events into the same downstream consumer.
    pub fn event_sink(&self) -> Arc<dyn EventSink> {
        Arc::clone(&self.events)
    }

    /// Appends a `user` message to the history.
    ///
    /// This does not perform any network call; it only stages the next turn.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.history.push(Message::user(text));
    }

    /// Runs exactly one round and returns the resulting [`Step`].
    ///
    /// A round assembles the system prompt and history into a request, performs
    /// one chat completion (retrying transient errors up to
    /// [`SessionOptions::max_retries`]), branches on the
    /// [`FinishReason`], dispatches any requested tools through
    /// the registry, and folds the round's [`Usage`] into the cumulative totals.
    ///
    /// # Errors
    ///
    /// Fatal [`req`](crate::req) errors are returned as [`Step::Failed`] rather
    /// than as an `Err`, so a single signature covers both control flow and
    /// failure. Transient errors are retried internally.
    pub fn run_round(&mut self) -> Step {
        // Advance the round counter and announce the round (and, on the very
        // first round of this session, that the session itself started).
        self.round += 1;
        if self.round == 1 {
            self.events.emit(Event::SessionStarted {
                role: self.role.clone(),
                system_excerpt: system_excerpt(&self.effective_system()),
            });
        }
        self.events.emit(Event::RoundStarted { round: self.round });

        let request = match self.build_request() {
            Ok(req) => req,
            Err(e) => return self.finish(Step::Failed(e)),
        };

        // One chat completion, retrying transient errors with a small backoff.
        let response = match self.chat_with_retries(&request) {
            Ok(resp) => resp,
            Err(e) => return self.finish(Step::Failed(e)),
        };

        // Fold this round's usage into the running totals before branching.
        if let Some(usage) = response.usage.as_ref() {
            self.usage.add(usage);
        } else {
            // Server omitted usage; still count the round.
            self.usage.rounds += 1;
        }

        let Some(choice) = response.choices.into_iter().next() else {
            return self.finish(Step::Failed(crate::req::Error::Decode {
                context: "chat",
                source: <serde_json::Error as serde::de::Error>::custom(
                    "chat response contained no choices",
                ),
            }));
        };

        // Back-fill the assistant turn (reasoning_content stripped) into history.
        let assistant = choice.message.to_history();
        let text = assistant.content.clone().unwrap_or_default();
        let tool_calls = assistant.tool_calls.clone();
        self.history.push(assistant);

        // Surface any assistant text the model produced this round, whether it is
        // an intermediate message, a final answer, or the preamble to a tool call.
        if !text.is_empty() {
            self.events.emit(Event::ModelMessage { text: text.clone() });
        }

        let step = match choice.finish_reason {
            Some(FinishReason::ToolCalls) => self.dispatch_tool_calls(tool_calls),
            Some(FinishReason::Stop) | None => Step::Done(text),
            Some(FinishReason::Length) => Step::Message(text),
            Some(FinishReason::ContentFilter) => Step::NeedHuman,
            // Transient resource shortage that survived the retry loop, or any
            // unrecognized reason: hand control back to the caller.
            Some(FinishReason::InsufficientSystemResource) | Some(FinishReason::Unknown(_)) => {
                Step::NeedHuman
            }
        };
        self.finish(step)
    }

    /// Emits an [`Event::Finished`] for `step` when it is a terminal outcome
    /// ([`Step::Done`] / [`Step::NeedHuman`] / [`Step::Failed`]), then returns
    /// `step` unchanged.
    ///
    /// Non-terminal steps ([`Step::Tool`] / [`Step::Message`]) emit nothing — the
    /// run continues, so it has not finished. This keeps every exit path of
    /// [`Session::run_round`] funnelled through one place, so a `Finished` event
    /// is emitted exactly once per terminal round regardless of which branch
    /// produced it.
    fn finish(&self, step: Step) -> Step {
        let outcome = match &step {
            Step::Done(_) => "done",
            Step::NeedHuman => "need_human",
            Step::Failed(_) => "failed",
            Step::Tool(_) | Step::Message(_) => return step,
        };
        self.events.emit(Event::Finished {
            outcome: outcome.to_string(),
        });
        step
    }

    /// Runs rounds until the session reaches a terminal step
    /// ([`Step::Done`], [`Step::NeedHuman`], or [`Step::Failed`]) or the
    /// [`SessionOptions::max_rounds`] ceiling is hit (yielding
    /// [`Step::NeedHuman`]).
    ///
    /// # Errors
    ///
    /// As with [`Session::run_round`], fatal errors surface as [`Step::Failed`].
    pub fn run_until_done(&mut self) -> Step {
        for _ in 0..self.options.max_rounds {
            match self.run_round() {
                // Non-terminal: a tool was dispatched (results already fed back)
                // or the model emitted an intermediate / auto-continue message.
                // Loop and run the next round.
                Step::Tool(_) | Step::Message(_) => continue,
                terminal => return terminal,
            }
        }
        // Round budget exhausted without a self-assessed completion.
        Step::NeedHuman
    }

    /// Returns the cumulative token usage so far.
    pub fn usage(&self) -> &UsageTotals {
        &self.usage
    }

    /// Returns a clone of the underlying [`Client`].
    ///
    /// Cloning a [`Client`] is cheap (the `ureq` agent shares its connection
    /// pool). The [`engine`](crate::engine) layer uses this to hand a master's
    /// client to a spawned slave thread, so both drive the same backend without
    /// re-reading credentials from the environment.
    pub fn client_clone(&self) -> Client {
        self.client.clone()
    }

    /// Returns the conversation history accumulated so far.
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Returns the effective system prompt for the current round.
    ///
    /// When a [system-prompt provider](Session::set_system_provider) is installed,
    /// this re-evaluates it and returns the freshly composed prompt (so it reflects
    /// the current on-disk state, per kernel §4); otherwise it returns the fixed
    /// string passed to [`Session::new`]. The return is owned because a provider
    /// produces a new `String` each call.
    pub fn system(&self) -> String {
        self.effective_system()
    }

    /// Captures a serializable snapshot of the session's persistent state
    /// (system prompt, history, usage, options).
    ///
    /// The live [`Client`] and
    /// [`ToolRegistry`] are **not** captured; restore
    /// them with [`Session::restore`]. A
    /// [system-prompt provider](Session::set_system_provider) is a live closure and
    /// is likewise not captured: the snapshot records the static
    /// [`system`](Session::system) string, and a restored session re-installs its
    /// provider (e.g. via the engine) if it needs one.
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            system: self.system.clone(),
            history: self.history.clone(),
            usage: self.usage,
            options: self.options.clone(),
        }
    }

    /// Restores a session from a [`SessionSnapshot`], re-supplying the live
    /// client and tool registry that a snapshot cannot carry.
    ///
    /// The workspace context (root + writer) is **not** carried by a snapshot; it
    /// is reset to the defaults (`"workspace"` and [`WriterId::Human`]). Re-apply
    /// it with [`Session::set_workspace`] if the restored session will dispatch
    /// workspace tools.
    pub fn restore(snapshot: SessionSnapshot, client: Client, tools: ToolRegistry) -> Self {
        Session {
            client,
            system: snapshot.system,
            system_provider: None,
            system_segments: BTreeMap::new(),
            history: snapshot.history,
            tools,
            usage: snapshot.usage,
            options: snapshot.options,
            workspace_root: std::path::PathBuf::from("workspace"),
            writer: WriterId::Human,
            workspace: None,
            vcs: None,
            coordinator: None,
            events: Arc::new(NullSink),
            role: "session".to_string(),
            round: 0,
        }
    }

    /// Assembles the system prompt and history into a [`ChatRequest`], advertising
    /// every registered tool.
    ///
    /// The system prompt is always the first message; the running history follows.
    /// When a [system-prompt provider](Session::set_system_provider) is installed it
    /// is evaluated here, so every round re-reads the effective prompt from its
    /// source of truth (kernel §4). Tools are attached (with `tool_choice = auto`)
    /// only when the registry is non-empty, so a tool-free session sends no `tools`
    /// array.
    fn build_request(&self) -> crate::req::Result<ChatRequest> {
        let mut builder =
            ChatRequest::builder(self.options.model.clone()).system(self.effective_system());
        for message in &self.history {
            builder = builder.message(message.clone());
        }
        if let Some(thinking) = self.options.thinking {
            builder = builder.thinking(thinking);
        }
        if let Some(max_tokens) = self.options.max_tokens {
            builder = builder.max_tokens(max_tokens);
        }
        if !self.tools.is_empty() {
            builder = builder
                .tools(self.tools.definitions())
                .tool_choice(ToolChoice::Auto);
        }
        builder.build()
    }

    /// Performs one chat completion, retrying transient errors
    /// ([`Error::is_transient`](crate::req::Error::is_transient)) up to
    /// [`SessionOptions::max_retries`] times with a short linear backoff.
    fn chat_with_retries(
        &self,
        request: &ChatRequest,
    ) -> crate::req::Result<crate::req::ChatResponse> {
        let mut attempt = 0u32;
        loop {
            match self.client.chat(request) {
                Ok(resp) => return Ok(resp),
                Err(e) if e.is_transient() && attempt < self.options.max_retries => {
                    attempt += 1;
                    thread::sleep(Duration::from_millis(250 * u64::from(attempt)));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Dispatches every requested tool call against a freshly opened
    /// [`Workspace`], appending each result (success JSON or serialized
    /// [`ToolError`](crate::tool::ToolError)) as a `tool` message, and returns the
    /// list of dispatched tool names as [`Step::Tool`].
    ///
    /// If the workspace cannot be opened, the open error is fed back to the model
    /// as the content of every pending `tool` reply (rather than aborting the
    /// session), so the model can adapt — consistent with the "guard rails in the
    /// tools, recovery in the model" contract.
    ///
    /// A [`Vcs`] is opened lazily at the same root and attached to each
    /// [`ToolCtx`], so a successful content edit is recorded as a commit authored
    /// by the session's [`writer`](Session::writer). If version control cannot be
    /// opened it is simply left disabled (edits still persist to disk), never
    /// aborting the session.
    fn dispatch_tool_calls(&mut self, tool_calls: Option<Vec<crate::req::ToolCall>>) -> Step {
        let calls = tool_calls.unwrap_or_default();
        if calls.is_empty() {
            // `tool_calls` finish reason with no calls is degenerate; treat the
            // (possibly empty) assistant text as an intermediate message.
            return Step::Message(String::new());
        }

        // Open the workspace once for the session (not per round): the
        // single-writer article lock lives in memory on the `Workspace` handle,
        // so it must survive across rounds for a lock acquired in one round to
        // still hold when a later round writes.
        if self.workspace.is_none() {
            match Workspace::open(&self.workspace_root) {
                Ok(ws) => self.workspace = Some(ws),
                Err(e) => {
                    let payload = serde_json::json!({ "error": e.to_string() });
                    for call in &calls {
                        self.push_tool_reply(&call.id, &payload);
                    }
                    let names = calls.into_iter().map(|c| c.function.name).collect();
                    return Step::Tool(names);
                }
            }
        }

        // Open the version-control handle once, at the same root, alongside the
        // workspace — unless a coordinator is installed, in which case committing
        // (and the implicit per-edit lock) is the coordinator's job and this
        // per-session `Vcs` is never used. A failure here is non-fatal: edits
        // still write to disk through the workspace, they just are not committed
        // (and the history / diff / undo tools then report version control as
        // disabled). This keeps the "guard rails in the tools, recovery in the
        // model" contract — a missing git repository never aborts a session.
        if self.coordinator.is_none()
            && self.vcs.is_none()
            && let Some(ws) = self.workspace.as_ref()
            && let Ok(vcs) = Vcs::open_or_init(ws.root())
        {
            self.vcs = Some(vcs);
        }

        let mut names = Vec::with_capacity(calls.len());
        for call in &calls {
            names.push(call.function.name.clone());

            // Announce the call before dispatching, with its arguments as parsed
            // JSON (or `null` when empty / unparseable) for the UI.
            let name = call.function.name.clone();
            let args = parse_tool_args(&call.function.arguments);
            self.events.emit(Event::ToolCalled {
                name: name.clone(),
                args,
            });

            let writer = self.writer.clone();
            let result = {
                // Borrow the two workspace handles disjointly (different fields).
                let ws = self.workspace.as_mut().expect("workspace opened above");
                let mut ctx = ToolCtx::new(ws, writer.clone());
                // A coordinator, when present, owns committing and the implicit
                // per-edit lock; otherwise fall back to the per-session `Vcs`.
                if let Some(coord) = self.coordinator.as_ref() {
                    ctx = ctx.with_coordinator(Arc::clone(coord));
                } else if let Some(vcs) = self.vcs.as_ref() {
                    ctx = ctx.with_vcs(vcs);
                }
                self.tools.dispatch(&call.function, &mut ctx)
            };

            // Report the outcome, and — when the call recorded a commit — emit a
            // dedicated `EditCommitted` so the UI can render the version event.
            match &result {
                Ok(value) => {
                    self.events.emit(Event::ToolResult {
                        name: name.clone(),
                        ok: true,
                        summary: summarize_ok(value),
                    });
                    if let Some((article, sha)) = committed_edit(value) {
                        self.events.emit(Event::EditCommitted {
                            article,
                            author: writer.provenance_tag(),
                            sha,
                        });
                    }
                }
                Err(e) => {
                    self.events.emit(Event::ToolResult {
                        name: name.clone(),
                        ok: false,
                        summary: e.to_string(),
                    });
                }
            }

            let payload = match result {
                Ok(value) => value,
                Err(e) => serde_json::json!({ "error": e.to_string() }),
            };
            self.push_tool_reply(&call.id, &payload);
        }
        Step::Tool(names)
    }

    /// Appends a `tool` reply message for `tool_call_id`, encoding `payload` as a
    /// compact JSON string (falling back to its `Display` form on the practically
    /// impossible serialization failure).
    fn push_tool_reply(&mut self, tool_call_id: &str, payload: &serde_json::Value) {
        let content = serde_json::to_string(payload).unwrap_or_else(|_| payload.to_string());
        self.history.push(Message::tool(tool_call_id, content));
    }
}

/// Truncates a system prompt to at most [`SYSTEM_EXCERPT_LEN`] characters for an
/// [`Event::SessionStarted`] excerpt, appending an ellipsis when it was cut.
///
/// Truncation is on `char` boundaries (not bytes), so it never splits a
/// multi-byte UTF-8 character.
fn system_excerpt(system: &str) -> String {
    if system.chars().count() <= SYSTEM_EXCERPT_LEN {
        return system.to_string();
    }
    let head: String = system.chars().take(SYSTEM_EXCERPT_LEN).collect();
    format!("{head}…")
}

/// Parses a tool call's JSON-encoded `arguments` string into a
/// [`serde_json::Value`] for an [`Event::ToolCalled`], mapping empty or
/// unparseable arguments to [`serde_json::Value::Null`].
///
/// This is best-effort and never fails: the event is for display, so malformed
/// arguments simply surface as `null` rather than aborting emission. The actual
/// dispatch path does its own strict parsing and reports argument errors back to
/// the model.
fn parse_tool_args(arguments: &str) -> serde_json::Value {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_str(trimmed).unwrap_or(serde_json::Value::Null)
}

/// Builds a short, display-oriented summary of a successful tool result for an
/// [`Event::ToolResult`].
///
/// A compact JSON encoding of the result object is used, truncated so a large
/// payload (e.g. a full article body returned by `read_article`) does not bloat
/// the event stream.
fn summarize_ok(value: &serde_json::Value) -> String {
    /// Maximum characters of a result summary carried in an event.
    const MAX: usize = 200;
    let rendered = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    if rendered.chars().count() <= MAX {
        rendered
    } else {
        let head: String = rendered.chars().take(MAX).collect();
        format!("{head}…")
    }
}

/// Extracts the `(article, sha)` pair from a tool result that recorded a commit,
/// or `None` if the result did not commit anything.
///
/// The lock-guarded editors return a payload shaped like
/// `{"edited": "<theme>/<file>", "committed": "<sha>", …}` when version control
/// is enabled (and omit `committed` when it is not). This reads those two fields,
/// returning `None` whenever either is absent — so a read-only tool, or an edit
/// made with version control disabled, emits no [`Event::EditCommitted`].
fn committed_edit(value: &serde_json::Value) -> Option<(String, String)> {
    let article = value.get("edited")?.as_str()?.to_string();
    let sha = value.get("committed")?.as_str()?.to_string();
    Some((article, sha))
}

/// A serializable point-in-time capture of a [`Session`]'s persistent state.
///
/// Produced by [`Session::snapshot`] and consumed by [`Session::restore`]. It
/// holds everything needed to reconstruct a session *except* the live runtime
/// handles (the [`Client`] and the
/// [`ToolRegistry`]), which must be provided again at
/// restore time.
///
/// Only [`Serialize`] is derived: [`history`](SessionSnapshot::history) holds
/// [`req::Message`](crate::req::Message) values, which are `Serialize`-only
/// because this crate must not modify the `req` module. A snapshot is thus a
/// one-way capture for inspection and on-disk persistence; rehydration into a
/// live [`Session`] is done by [`Session::restore`].
#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshot {
    /// The static system prompt (the fixed string passed to [`Session::new`]). A
    /// live [system-prompt provider](Session::set_system_provider) is not captured.
    pub system: String,
    /// The full conversation history at snapshot time.
    pub history: Vec<Message>,
    /// Cumulative token usage at snapshot time.
    pub usage: UsageTotals,
    /// The behavioural options the session was running with.
    pub options: SessionOptions,
}

#[cfg(test)]
mod tests {
    //! Offline unit tests for the agentic engine.
    //!
    //! These exercise the parts of a round that do not require the network: usage
    //! accumulation, request assembly (system-first ordering, history replay, tool
    //! export, thinking/token propagation), history stripping, and
    //! snapshot/restore. The network-driven branches of `run_round` and tool
    //! dispatch against a live workspace are covered by the engine stage's
    //! `#[ignore]`d live tests, never here.

    use super::*;
    use crate::req::types::request::Role;
    use crate::req::types::response::RespMessage;
    use crate::req::{Effort, FunctionDef, ToolCall};

    /// A network-free client used purely so `Session::new` has something to hold;
    /// no test in this module performs a chat completion.
    fn test_client() -> Client {
        Client::builder()
            .api_key("test-key")
            .build()
            .expect("offline client")
    }

    /// A trivial tool that echoes its arguments back. Its [`Tool::call`] never
    /// touches the workspace, so the registry can be inspected without one.
    struct EchoTool;

    impl crate::tool::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn schema(&self) -> crate::req::Tool {
            crate::req::Tool::function(FunctionDef {
                name: "echo".to_string(),
                description: Some("Echoes its arguments.".to_string()),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                })),
            })
        }

        fn call(
            &self,
            args: serde_json::Value,
            _ctx: &mut crate::tool::ToolCtx<'_>,
        ) -> crate::tool::ToolResult {
            Ok(serde_json::json!({ "echo": args }))
        }
    }

    fn usage(prompt: u32, completion: u32, reasoning: Option<u32>) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            prompt_cache_hit_tokens: 0,
            prompt_cache_miss_tokens: prompt,
            completion_tokens_details: reasoning.map(|r| crate::req::CompletionTokensDetails {
                reasoning_tokens: Some(r),
            }),
            prompt_tokens_details: None,
        }
    }

    #[test]
    fn default_options_match_spec() {
        let o = SessionOptions::default();
        assert_eq!(o.model, Model::V4Flash);
        assert!(o.thinking.is_none());
        assert!(o.max_tokens.is_none());
        assert_eq!(o.max_rounds, 20);
        assert_eq!(o.max_retries, 3);
    }

    #[test]
    fn usage_totals_accumulate_across_rounds() {
        let mut totals = UsageTotals::default();
        totals.add(&usage(10, 5, Some(2)));
        totals.add(&usage(7, 3, None));

        assert_eq!(totals.prompt_tokens, 17);
        assert_eq!(totals.completion_tokens, 8);
        assert_eq!(totals.total_tokens, 25);
        assert_eq!(totals.prompt_cache_miss_tokens, 17);
        assert_eq!(totals.reasoning_tokens, 2); // None contributes 0
        assert_eq!(totals.rounds, 2);
    }

    #[test]
    fn push_user_appends_user_message() {
        let mut s = Session::new(
            test_client(),
            "system prompt",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        assert!(s.history().is_empty());
        s.push_user("hello");
        assert_eq!(s.history().len(), 1);
        assert_eq!(s.history()[0].role, Role::User);
        assert_eq!(s.history()[0].content.as_deref(), Some("hello"));
        assert_eq!(s.system(), "system prompt");
    }

    #[test]
    fn build_request_puts_system_first_then_history() {
        let mut s = Session::new(
            test_client(),
            "you are careful",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        s.push_user("first");
        s.history.push(Message::assistant("ack"));
        s.push_user("second");

        let req = s.build_request().expect("valid request");
        assert_eq!(req.messages.len(), 4);
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[0].content.as_deref(), Some("you are careful"));
        assert_eq!(req.messages[1].role, Role::User);
        assert_eq!(req.messages[1].content.as_deref(), Some("first"));
        assert_eq!(req.messages[2].role, Role::Assistant);
        assert_eq!(req.messages[3].content.as_deref(), Some("second"));
    }

    #[test]
    fn build_request_omits_tools_when_registry_empty() {
        let mut s = Session::new(
            test_client(),
            "sys",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        s.push_user("hi");
        let req = s.build_request().expect("valid request");
        assert!(req.tools.is_none());
        assert!(req.tool_choice.is_none());
    }

    #[test]
    fn build_request_exports_tool_definitions_with_auto_choice() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());

        let mut s = Session::new(test_client(), "sys", registry, SessionOptions::default());
        s.push_user("hi");
        let req = s.build_request().expect("valid request");

        let tools = req.tools.expect("tools attached");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "echo");
        assert_eq!(req.tool_choice, Some(ToolChoice::Auto));
    }

    #[test]
    fn build_request_propagates_thinking_and_max_tokens() {
        let opts = SessionOptions {
            model: Model::V4Pro,
            thinking: Some(Thinking::Enabled {
                effort: Effort::High,
            }),
            max_tokens: Some(1024),
            max_rounds: 5,
            max_retries: 1,
        };
        let mut s = Session::new(test_client(), "sys", ToolRegistry::new(), opts);
        s.push_user("hi");
        let req = s.build_request().expect("valid request");

        assert_eq!(req.model, Model::V4Pro);
        assert_eq!(req.max_tokens, Some(1024));
        assert!(matches!(req.thinking, Some(Thinking::Enabled { .. })));
    }

    #[test]
    fn history_back_fill_strips_reasoning_content_keeps_tool_calls() {
        // Simulate the server's assistant message carrying a thinking chain plus
        // tool calls; `to_history` must drop reasoning_content but keep tool_calls.
        let resp = RespMessage {
            role: "assistant".to_string(),
            content: Some("answer".to_string()),
            reasoning_content: Some("secret chain of thought".to_string()),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::req::FunctionCall {
                    name: "echo".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
        };
        let msg = resp.to_history();
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content.as_deref(), Some("answer"));
        assert!(
            msg.reasoning_content.is_none(),
            "reasoning_content must be stripped from multi-turn history"
        );
        assert!(msg.tool_calls.is_some());

        // And the serialized wire form must not contain reasoning_content.
        let wire = serde_json::to_string(&msg).expect("serialize");
        assert!(!wire.contains("reasoning_content"));
        assert!(wire.contains("call_1"));
    }

    #[test]
    fn snapshot_restore_round_trip_preserves_state() {
        let mut s = Session::new(
            test_client(),
            "persisted system",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        s.push_user("turn one");
        s.usage.add(&usage(100, 50, Some(10)));

        let snap = s.snapshot();
        assert_eq!(snap.system, "persisted system");
        assert_eq!(snap.history.len(), 1);
        assert_eq!(snap.usage.rounds, 1);

        // Snapshot is serializable (one-way capture).
        let json = serde_json::to_string(&snap).expect("snapshot serializes");
        assert!(json.contains("persisted system"));

        let restored = Session::restore(snap, test_client(), ToolRegistry::new());
        assert_eq!(restored.system(), "persisted system");
        assert_eq!(restored.history().len(), 1);
        assert_eq!(restored.usage().total_tokens, 150);
        assert_eq!(restored.usage().reasoning_tokens, 10);
    }

    #[test]
    fn empty_registry_exports_no_definitions() {
        let registry = ToolRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.definitions().is_empty());
    }

    // --- Event emission over a fake (loopback, no live API) session run ---

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use crate::observe::{Event, EventSink};

    /// An [`EventSink`] that records every event it receives, for asserting the
    /// emission sequence of a run.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Event>>);

    impl Recorder {
        /// The ordered list of event kind labels captured so far.
        fn kinds(&self) -> Vec<&'static str> {
            self.0
                .lock()
                .expect("not poisoned")
                .iter()
                .map(event_kind)
                .collect()
        }
    }

    impl EventSink for Recorder {
        fn emit(&self, event: Event) {
            self.0.lock().expect("not poisoned").push(event);
        }
    }

    /// A stable short kind label for an [`Event`], used only by these tests.
    fn event_kind(e: &Event) -> &'static str {
        match e {
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
        }
    }

    /// Spawns a one-shot loopback HTTP server that replies to each incoming POST
    /// with the next canned body in `bodies` (200 OK), then returns the bound
    /// `base_url` to point a [`Client`] at.
    ///
    /// This is a fake, not the live DeepSeek API: it serves fixed JSON over
    /// `127.0.0.1`, so a full [`Session::run_round`] can be exercised with zero
    /// network egress. The server thread handles exactly `bodies.len()` requests
    /// and then exits.
    fn spawn_fake_api(bodies: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().expect("accept");
                // Drain the request: read headers, then the Content-Length body.
                let mut buf = [0u8; 4096];
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

    /// A canned tool-calls response invoking the `echo` tool once.
    fn tool_call_response() -> String {
        serde_json::json!({
            "id": "resp-1",
            "object": "chat.completion",
            "created": 0,
            "model": "deepseek-v4-flash",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "calling echo",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "echo", "arguments": "{\"text\":\"hi\"}" }
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

    /// A canned final `stop` response.
    fn stop_response() -> String {
        serde_json::json!({
            "id": "resp-2",
            "object": "chat.completion",
            "created": 0,
            "model": "deepseek-v4-flash",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "all done" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 6, "completion_tokens": 2, "total_tokens": 8,
                "prompt_cache_hit_tokens": 0, "prompt_cache_miss_tokens": 6
            }
        })
        .to_string()
    }

    fn fake_client(base_url: String) -> Client {
        Client::builder()
            .api_key("test-key")
            .base_url(base_url)
            .build()
            .expect("fake client")
    }

    #[test]
    fn run_emits_full_event_sequence_over_a_fake_round() {
        // Two rounds: the model calls `echo` (round 1), then stops (round 2).
        let base = spawn_fake_api(vec![tool_call_response(), stop_response()]);

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        let mut session = Session::new(
            fake_client(base),
            "You are a careful writing assistant.",
            registry,
            SessionOptions::default(),
        );
        let recorder: Arc<Recorder> = Arc::new(Recorder::default());
        let sink: Arc<dyn EventSink> = Arc::clone(&recorder) as Arc<dyn EventSink>;
        session.set_event_sink("slave", sink);
        session.push_user("do the thing");

        let terminal = session.run_until_done();
        assert!(matches!(terminal, Step::Done(_)), "run should finish Done");

        // The session-start event fires once, the round/tool events bracket the
        // echo call, and the run finishes — in this exact order.
        assert_eq!(
            recorder.kinds(),
            [
                "SessionStarted", // first round only
                "RoundStarted",   // round 1
                "ModelMessage",   // "calling echo"
                "ToolCalled",     // echo
                "ToolResult",     // echo ok
                "RoundStarted",   // round 2
                "ModelMessage",   // "all done"
                "Finished",       // Done
            ]
        );

        // Spot-check the payloads of the structured events.
        let events = recorder.0.lock().expect("not poisoned");
        match &events[0] {
            Event::SessionStarted {
                role,
                system_excerpt,
            } => {
                assert_eq!(role, "slave");
                assert!(system_excerpt.starts_with("You are a careful"));
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }
        match &events[3] {
            Event::ToolCalled { name, args } => {
                assert_eq!(name, "echo");
                assert_eq!(args["text"], "hi");
            }
            other => panic!("expected ToolCalled, got {other:?}"),
        }
        match &events[4] {
            Event::ToolResult { name, ok, .. } => {
                assert_eq!(name, "echo");
                assert!(ok);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        match &events[7] {
            Event::Finished { outcome } => assert_eq!(outcome, "done"),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn default_sink_is_silent_and_changes_no_behaviour() {
        // Without a sink installed (the default NullSink), a full run still
        // completes exactly as before; observability is transparent.
        let base = spawn_fake_api(vec![stop_response()]);
        let mut session = Session::new(
            fake_client(base),
            "sys",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        session.push_user("hello");
        let terminal = session.run_round();
        match terminal {
            Step::Done(text) => assert_eq!(text, "all done"),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    // --- G8: system-prompt provider re-evaluated per round (kernel §4) ---

    /// Spawns a loopback HTTP server that, like [`spawn_fake_api`], replies with the
    /// next canned `body` per request — but also **records each request body** into
    /// the returned shared buffer, so a test can inspect what the session actually
    /// sent (notably the `system` message) on each round.
    fn spawn_recording_api(bodies: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 4096];
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
                // Record the request body (the JSON ChatRequest) for inspection.
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

    /// Extracts the `system` message content from a recorded JSON ChatRequest body
    /// (the first message whose `role` is `"system"`).
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
    fn no_provider_is_byte_identical_to_fixed_prompt() {
        // With no provider installed, the request carries the exact fixed prompt.
        let (base, recorded) = spawn_recording_api(vec![stop_response()]);
        let mut session = Session::new(
            fake_client(base),
            "the fixed system prompt",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        session.push_user("go");
        let _ = session.run_round();

        let bodies = recorded.lock().expect("not poisoned");
        assert_eq!(bodies.len(), 1);
        assert_eq!(system_of(&bodies[0]), "the fixed system prompt");
        // And the accessor reflects the same fixed prompt.
        assert_eq!(session.system(), "the fixed system prompt");
    }

    #[test]
    fn provider_is_reevaluated_each_round_and_changes_the_system_message() {
        // A provider backed by mutable shared state: each round it returns the
        // *current* value, simulating a skill file edited between rounds. Two rounds
        // (echo then stop) let us inspect the system message sent on each.
        let (base, recorded) = spawn_recording_api(vec![tool_call_response(), stop_response()]);

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        let mut session = Session::new(
            fake_client(base),
            "ignored static fallback",
            registry,
            SessionOptions::default(),
        );

        // The provider reads `current`, which a "human" edits between rounds.
        let current: Arc<Mutex<String>> = Arc::new(Mutex::new("PROMPT v1".to_string()));
        let provider_state = Arc::clone(&current);
        session.set_system_provider(move || provider_state.lock().expect("not poisoned").clone());

        session.push_user("do the thing");

        // Round 1: the model calls `echo`; the system message must be v1.
        let step1 = session.run_round();
        assert!(matches!(step1, Step::Tool(_)), "round 1 dispatches echo");

        // A "human" edits the underlying source between rounds (kernel §4: SSOT on
        // disk, re-read each step).
        *current.lock().expect("not poisoned") = "PROMPT v2 (edited mid-run)".to_string();

        // Round 2: the model stops; the system message must now reflect the edit.
        let step2 = session.run_round();
        assert!(matches!(step2, Step::Done(_)), "round 2 finishes");

        let bodies = recorded.lock().expect("not poisoned");
        assert_eq!(bodies.len(), 2, "two requests were sent");
        let sys1 = system_of(&bodies[0]);
        let sys2 = system_of(&bodies[1]);
        assert_eq!(sys1, "PROMPT v1");
        assert_eq!(sys2, "PROMPT v2 (edited mid-run)");
        assert_ne!(
            sys1, sys2,
            "editing the source between rounds must change the next round's system message"
        );
    }

    // --- G9: dynamic system-prompt segments (kernel §10) ---

    #[test]
    fn no_segments_compose_to_the_base_prompt_exactly() {
        // With no segments installed, the effective prompt is byte-identical to the
        // base — G9 is fully transparent until a segment is pushed.
        let s = Session::new(
            test_client(),
            "base only",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        assert!(s.system_segment_names().is_empty());
        assert_eq!(s.system(), "base only");
    }

    #[test]
    fn pushing_a_segment_appends_it_after_the_base() {
        let mut s = Session::new(
            test_client(),
            "BASE",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        s.push_system_segment("rules", 0, "Be terse.");
        assert_eq!(s.system(), "BASE\n\nBe terse.");
        assert_eq!(s.system_segment_names(), ["rules"]);
    }

    #[test]
    fn segments_are_ordered_by_ordinal_then_name() {
        let mut s = Session::new(
            test_client(),
            "BASE",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        // Insert out of order; composition must be deterministic by (ordinal, name).
        s.push_system_segment("zeta", 0, "Z"); // ordinal 0, name "zeta"
        s.push_system_segment("alpha", 0, "A"); // ordinal 0, name "alpha" -> before zeta
        s.push_system_segment("high", -5, "H"); // lowest ordinal -> first
        s.push_system_segment("low", 10, "L"); // highest ordinal -> last

        assert_eq!(s.system_segment_names(), ["high", "alpha", "zeta", "low"]);
        assert_eq!(s.system(), "BASE\n\nH\n\nA\n\nZ\n\nL");
    }

    #[test]
    fn pushing_same_name_overrides_in_place() {
        let mut s = Session::new(
            test_client(),
            "BASE",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        s.push_system_segment("style", -10, "Write in plain language.");
        assert!(s.system().contains("Write in plain language."));

        // Override the same name: replaces text, no duplicate segment.
        s.push_system_segment("style", -10, "Write tersely.");
        assert_eq!(s.system_segment_names(), ["style"]);
        assert!(s.system().contains("Write tersely."));
        assert!(!s.system().contains("plain language"));
    }

    #[test]
    fn removing_a_segment_drops_it_and_reports_presence() {
        let mut s = Session::new(
            test_client(),
            "BASE",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        s.push_system_segment("note", 0, "Temporary.");
        assert_eq!(s.system(), "BASE\n\nTemporary.");

        assert!(s.remove_system_segment("note"), "present -> removed");
        assert!(
            !s.remove_system_segment("note"),
            "already gone -> no-op false"
        );
        assert_eq!(s.system(), "BASE");
        assert!(s.system_segment_names().is_empty());
    }

    #[test]
    fn segments_compose_on_top_of_a_provider_base() {
        // Segments layer on whatever the provider currently returns, not the static
        // fallback — so a provider edit and a segment edit are independent.
        let mut s = Session::new(
            test_client(),
            "static fallback (ignored)",
            ToolRegistry::new(),
            SessionOptions::default(),
        );
        let base = Arc::new(Mutex::new("PROVIDER v1".to_string()));
        let b = Arc::clone(&base);
        s.set_system_provider(move || b.lock().expect("not poisoned").clone());
        s.push_system_segment("seg", 0, "SEG");

        assert_eq!(s.system(), "PROVIDER v1\n\nSEG");
        // Editing the provider source moves the base but keeps the segment.
        *base.lock().expect("not poisoned") = "PROVIDER v2".to_string();
        assert_eq!(s.system(), "PROVIDER v2\n\nSEG");
    }

    #[test]
    fn segment_inserted_mid_run_appears_in_subsequent_rounds_only() {
        // Three rounds: echo, echo, stop. We inspect the system message the session
        // actually sent each round to assert insert/override/remove all take effect
        // on the *next* round, never retroactively.
        let (base, recorded) = spawn_recording_api(vec![
            tool_call_response(),
            tool_call_response(),
            stop_response(),
        ]);

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        let mut session = Session::new(
            fake_client(base),
            "BASE",
            registry,
            SessionOptions::default(),
        );
        session.push_user("go");

        // Round 1: no segments yet -> just the base.
        let step1 = session.run_round();
        assert!(matches!(step1, Step::Tool(_)));

        // Insert a segment between round 1 and round 2.
        session.push_system_segment("rule", 0, "RULE-A");

        // Round 2: the segment must now be present.
        let step2 = session.run_round();
        assert!(matches!(step2, Step::Tool(_)));

        // Override the segment and add a second one, then remove the override-less one.
        session.push_system_segment("rule", 0, "RULE-B");
        session.push_system_segment("extra", -1, "EXTRA");
        // Remove a non-existent name to confirm it does not disturb the rest.
        assert!(!session.remove_system_segment("nope"));

        // Round 3: base + reordered (extra has lower ordinal) + overridden text.
        let step3 = session.run_round();
        assert!(matches!(step3, Step::Done(_)));

        let bodies = recorded.lock().expect("not poisoned");
        assert_eq!(bodies.len(), 3, "three requests were sent");
        assert_eq!(system_of(&bodies[0]), "BASE", "round 1: base only");
        assert_eq!(
            system_of(&bodies[1]),
            "BASE\n\nRULE-A",
            "round 2: segment inserted mid-run is now present"
        );
        assert_eq!(
            system_of(&bodies[2]),
            "BASE\n\nEXTRA\n\nRULE-B",
            "round 3: override + reorder by ordinal take effect"
        );
    }
}
