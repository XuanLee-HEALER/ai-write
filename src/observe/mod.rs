//! Observability of the AI writing process: a push-based event stream.
//!
//! The collaborative-writing layers ([`session`](crate::session) and
//! [`engine`](crate::engine)) narrate what the model is doing — round by round,
//! tool call by tool call, commit by commit — by emitting structured [`Event`]
//! values to an [`EventSink`]. This is the runtime source of `docs/impl-v1.md`
//! §2's "humans can observe the master/slave operations": a UI subscribes by
//! injecting a sink, and the engine never has to know about HTTP, SSE, or any
//! particular front end (decision V4).
//!
//! # Design
//!
//! - **Push, not poll.** Producers call [`EventSink::emit`] at well-defined
//!   points; they never block on a consumer. The default sink ([`NullSink`])
//!   discards everything, so wiring observability into [`Session`](crate::session::Session)
//!   changes nothing for code that does not opt in — the v0 offline session and
//!   engine tests keep passing unchanged.
//! - **Serializable events.** [`Event`] derives [`Serialize`], so a WebUI sink
//!   can forward each event to the browser as JSON without re-shaping it.
//! - **`Send + Sync` sinks.** A sink is shared as an `Arc<dyn EventSink>` and may
//!   be emitted to from a slave thread, so it must be thread-safe. The WebUI's
//!   channel-backed sink (a later stage) fans each event out to every SSE
//!   subscriber.
//!
//! # Where events come from
//!
//! | Event | Emitted by | When |
//! |---|---|---|
//! | [`Event::SessionStarted`] | [`Session`](crate::session::Session) | the first round of a run |
//! | [`Event::RoundStarted`] | [`Session`](crate::session::Session) | the start of every round |
//! | [`Event::ModelMessage`] | [`Session`](crate::session::Session) | the model returns assistant text |
//! | [`Event::ToolCalled`] | [`Session`](crate::session::Session) | before each tool is dispatched |
//! | [`Event::ToolResult`] | [`Session`](crate::session::Session) | after each tool returns |
//! | [`Event::EditCommitted`] | [`Session`](crate::session::Session) | a tool call produced a git commit |
//! | [`Event::Finished`] | [`Session`](crate::session::Session) | a round reaches a terminal step |
//! | [`Event::SlaveSpawned`] | [`engine`](crate::engine) | a slave thread is dispatched |
//! | [`Event::SlaveReported`] | [`engine`](crate::engine) | a slave's report is collected |
//!
//! # Examples
//!
//! A recording sink that captures the event sequence for assertions:
//!
//! ```
//! use std::sync::{Arc, Mutex};
//! use ai_write::observe::{Event, EventSink};
//!
//! #[derive(Default)]
//! struct Recorder(Mutex<Vec<Event>>);
//!
//! impl EventSink for Recorder {
//!     fn emit(&self, event: Event) {
//!         self.0.lock().expect("not poisoned").push(event);
//!     }
//! }
//!
//! let sink: Arc<dyn EventSink> = Arc::new(Recorder::default());
//! sink.emit(Event::RoundStarted { round: 1 });
//! ```

use serde::Serialize;

/// One observable step in the AI writing process.
///
/// Each variant is a structured, [`Serialize`]-able snapshot of a single moment
/// — a round starting, the model speaking, a tool being called, an edit being
/// committed, a run finishing. Producers emit these to an [`EventSink`]; a UI
/// renders them as a live operation feed.
///
/// The enum is `#[non_exhaustive]`: new event kinds may be added in later
/// stages, so consumers matching on it must include a wildcard arm.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub enum Event {
    /// A session began a run. Carries the session's role label (e.g. `"slave"`)
    /// and a short excerpt of its system prompt, so a UI can show *which* agent
    /// started without dumping the whole prompt.
    SessionStarted {
        /// A short role label for the session (e.g. `"slave"` / `"master"`).
        role: String,
        /// The leading characters of the system prompt, truncated for display.
        system_excerpt: String,
    },
    /// A new round started. `round` is 1-based and increments every round of a
    /// single [`run_until_done`](crate::session::Session::run_until_done).
    RoundStarted {
        /// The 1-based round number within the current run.
        round: u32,
    },
    /// The model produced assistant text (an intermediate or final message,
    /// distinct from a tool call). Carries the text verbatim.
    ModelMessage {
        /// The assistant text the model returned this round.
        text: String,
    },
    /// A tool is about to be dispatched. Carries the tool name and the
    /// model-supplied arguments as parsed JSON (or [`serde_json::Value::Null`]
    /// when the arguments were empty or unparseable).
    ToolCalled {
        /// The name of the tool being dispatched.
        name: String,
        /// The arguments the model passed, as JSON.
        args: serde_json::Value,
    },
    /// A tool finished. `ok` reflects whether it returned `Ok`; `summary` is a
    /// short human-readable description of the outcome (a compact form of the
    /// result payload, or the error message).
    ToolResult {
        /// The name of the tool that ran.
        name: String,
        /// `true` if the tool returned `Ok`, `false` if it returned a
        /// [`ToolError`](crate::tool::ToolError).
        ok: bool,
        /// A short, display-oriented summary of the result or error.
        summary: String,
    },
    /// A tool call produced a version-control commit (an article edit landed in
    /// git). Carries the edited article path, the commit author, and the short
    /// commit SHA.
    EditCommitted {
        /// The edited article, as `"<theme>/<file_name>"`.
        article: String,
        /// The commit author, rendered as the writer's provenance identity.
        author: String,
        /// The abbreviated commit SHA the edit was recorded under.
        sha: String,
    },
    /// A slave thread was dispatched to write one article. Emitted by the
    /// [`engine`](crate::engine) layer.
    SlaveSpawned {
        /// The theme the slave's article lives in.
        theme: String,
        /// The article file the slave will write.
        file: String,
        /// The writer identity the slave operates under.
        writer: String,
    },
    /// A slave finished and its [`SlaveReport`](crate::engine::SlaveReport) was
    /// collected. Emitted by the [`engine`](crate::engine) layer.
    SlaveReported {
        /// The slave's terminal status (`"done"` / `"needs_human"` / `"failed"`).
        status: String,
        /// The slave's short summary of what happened.
        summary: String,
    },
    /// A run reached a terminal step. `outcome` is one of `"done"`,
    /// `"need_human"`, or `"failed"`.
    Finished {
        /// The terminal outcome of the run.
        outcome: String,
    },
}

/// A consumer of [`Event`]s emitted by the writing layers.
///
/// A sink is shared behind an `Arc<dyn EventSink>` and may be emitted to from
/// multiple threads (a master and its slaves), so it must be `Send + Sync`.
/// Implementations should make [`emit`](EventSink::emit) cheap and
/// non-blocking — for example, sending into an unbounded or lossy channel — so a
/// slow consumer never stalls the writing engine.
///
/// # Examples
///
/// ```
/// use ai_write::observe::{Event, EventSink, NullSink};
///
/// fn announce(sink: &dyn EventSink) {
///     sink.emit(Event::Finished { outcome: "done".into() });
/// }
///
/// announce(&NullSink);
/// ```
pub trait EventSink: Send + Sync {
    /// Consumes one [`Event`]. Implementations must not block the caller; a
    /// slow consumer should drop or buffer rather than stall the producer.
    fn emit(&self, event: Event);
}

/// An [`EventSink`] that discards every event.
///
/// This is the default sink wired into a [`Session`](crate::session::Session),
/// so observability is opt-in: code that never installs a real sink behaves
/// exactly as it did before events existed (the v0 offline tests are unaffected).
///
/// # Examples
///
/// ```
/// use ai_write::observe::{Event, EventSink, NullSink};
///
/// let sink = NullSink;
/// sink.emit(Event::RoundStarted { round: 1 }); // discarded
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct NullSink;

impl EventSink for NullSink {
    /// Discards `event`.
    fn emit(&self, _event: Event) {}
}

#[cfg(test)]
mod tests {
    //! Unit tests for the event model and the recording-sink pattern.

    use super::*;
    use std::sync::{Arc, Mutex};

    /// A sink that records every event it receives, for sequence assertions.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Event>>);

    impl Recorder {
        fn names(&self) -> Vec<&'static str> {
            self.0
                .lock()
                .expect("not poisoned")
                .iter()
                .map(Event::kind)
                .collect()
        }
    }

    impl EventSink for Recorder {
        fn emit(&self, event: Event) {
            self.0.lock().expect("not poisoned").push(event);
        }
    }

    impl Event {
        /// A stable short kind label, used only by tests for readable assertions.
        fn kind(&self) -> &'static str {
            match self {
                Event::SessionStarted { .. } => "SessionStarted",
                Event::RoundStarted { .. } => "RoundStarted",
                Event::ModelMessage { .. } => "ModelMessage",
                Event::ToolCalled { .. } => "ToolCalled",
                Event::ToolResult { .. } => "ToolResult",
                Event::EditCommitted { .. } => "EditCommitted",
                Event::SlaveSpawned { .. } => "SlaveSpawned",
                Event::SlaveReported { .. } => "SlaveReported",
                Event::Finished { .. } => "Finished",
            }
        }
    }

    #[test]
    fn null_sink_discards_without_panicking() {
        let sink = NullSink;
        sink.emit(Event::RoundStarted { round: 1 });
        sink.emit(Event::Finished {
            outcome: "done".into(),
        });
    }

    #[test]
    fn recorder_captures_events_in_order() {
        let rec = Recorder::default();
        rec.emit(Event::SessionStarted {
            role: "slave".into(),
            system_excerpt: "You are a focused writing agent".into(),
        });
        rec.emit(Event::RoundStarted { round: 1 });
        rec.emit(Event::Finished {
            outcome: "done".into(),
        });
        assert_eq!(rec.names(), ["SessionStarted", "RoundStarted", "Finished"]);
    }

    #[test]
    fn event_serializes_to_json() {
        let event = Event::EditCommitted {
            article: "rust/intro.md".into(),
            author: "deepseek-v4-pro/s1".into(),
            sha: "0123456789".into(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("EditCommitted"));
        assert!(json.contains("rust/intro.md"));
        assert!(json.contains("0123456789"));
    }

    #[test]
    fn sink_is_shareable_across_threads() {
        // The `Arc<dyn EventSink>` is `Send + Sync`, so a spawned thread can emit.
        let sink: Arc<dyn EventSink> = Arc::new(Recorder::default());
        let handle = {
            let sink = Arc::clone(&sink);
            std::thread::spawn(move || {
                sink.emit(Event::SlaveSpawned {
                    theme: "rust".into(),
                    file: "intro.md".into(),
                    writer: "deepseek-v4-pro/s1".into(),
                });
            })
        };
        handle.join().expect("thread joined");
        sink.emit(Event::SlaveReported {
            status: "done".into(),
            summary: "wrote it".into(),
        });
    }
}
