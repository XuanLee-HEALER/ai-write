//! The operation-level transaction coordinator (kernel §6).
//!
//! The coordinator is the single authority for every mutating operation on the
//! workspace. It is three things at once
//! (`docs/coordinator-design.md` §1):
//!
//! 1. **A lock manager.** Locks are declared *up front* as a [`LockSet`] and
//!    granted **all-or-nothing**: a transaction either acquires its whole
//!    declared set or holds nothing and waits, so there is no hold-and-wait and
//!    therefore no deadlock (kernel mechanism 6.B).
//! 2. **A fair scheduler.** Humans and agents both go through it (kernel axiom
//!    II); a [`Priority::Human`] ticket jumps to the **head** of the wait queue
//!    but never preempts a running transaction (mechanism 6.C).
//! 3. **The committer.** Every transaction performs exactly **one** git commit
//!    *inside* the critical section, before releasing its locks — one cognitive
//!    unit is one commit (kernel §5, mechanism 6.D).
//!
//! Because the coordinator **owns** the single, non-`Sync` [`Vcs`] handle and the
//! operation-level lock state, all writes and commits enter through one door.
//! "No one may grab a file lock behind the coordinator's back" (mechanism 6.B) is
//! enforced structurally: the [`Workspace`] lock methods are reserved for the
//! coordinator, and the editing tools call [`Coordinator::submit`] rather than
//! committing themselves.
//!
//! # The single entry point
//!
//! [`Coordinator::submit`] runs a transaction body inside the critical section.
//! Its seven steps map one-to-one onto the kernel mechanisms
//! (`docs/coordinator-design.md` §4):
//!
//! 1. **Enqueue.** A human ticket is inserted at the queue head (ahead of every
//!    waiting agent); an agent ticket is appended at the tail (6.C).
//! 2. **Wait** until this ticket's declared lock set is fully free **and** it is
//!    this ticket's turn. Acquisition is all-or-nothing — never hold a partial
//!    set (6.B).
//! 3. **Acquire** the whole declared set for this writer.
//! 4. **Enter the critical section**, assembling a [`TxnCtx`] over the
//!    coordinator's workspace and exclusive [`Vcs`].
//! 5. **Run the body**, which writes the affected files and returns a commit
//!    message; `touched ⊆ declared` is asserted, and an out-of-bounds write
//!    aborts with [`CoordError::Undeclared`].
//! 6. **Commit once** (6.D): every path the transaction actually touched (the
//!    article body plus the theme manifest) is folded into a single
//!    [`Vcs::commit_paths`] call, executed *before* releasing the locks.
//! 7. **Release / dequeue / notify** the waiters.
//!
//! # Concurrency model
//!
//! Slaves are real concurrent threads, so the coordinator collapses what used to
//! be several independent [`Vcs`] handles (one per slave, all racing the same git
//! index) into one. v1 takes the simplest correct baseline from
//! `docs/coordinator-design.md` §5: the **entire** critical section is
//! serialized — one transaction runs at a time — while [`LockSet`], the wait
//! queue, and [`Priority`] are modelled faithfully so disjoint-lock-set
//! concurrency can be dropped in later without changing the public API.
//!
//! Deadlock is impossible: all-or-nothing acquisition means no transaction ever
//! holds one lock while waiting for another, so the wait-for graph has no cycle.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use crate::observe::{Event, EventSink, NullSink};
use crate::tool::ToolError;
use crate::tool::workspace::{Workspace, WriterId};
use crate::vcs::{Vcs, VcsError};

/// The scheduling priority of a transaction (kernel mechanism 6.C).
///
/// A [`Priority::Human`] ticket is inserted at the **head** of the wait queue,
/// ahead of every waiting [`Priority::Agent`], but it never preempts a
/// transaction already running — a human waits at most for the current
/// transaction's single commit. Agents queue first-in-first-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// A human-initiated transaction: jumps to the queue head, does not preempt.
    Human,
    /// An agent-initiated transaction: ordinary FIFO ordering.
    Agent,
}

impl Priority {
    /// Derives the scheduling priority from a [`WriterId`]: a human writer maps to
    /// [`Priority::Human`], an agent to [`Priority::Agent`].
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::coordinator::Priority;
    /// use ai_write::tool::workspace::WriterId;
    ///
    /// assert_eq!(Priority::for_writer(&WriterId::Human), Priority::Human);
    /// let agent = WriterId::Agent { model: "m".into(), label: "s1".into() };
    /// assert_eq!(Priority::for_writer(&agent), Priority::Agent);
    /// ```
    pub fn for_writer(writer: &WriterId) -> Self {
        match writer {
            WriterId::Human => Priority::Human,
            WriterId::Agent { .. } => Priority::Agent,
        }
    }
}

/// The set of workspace-relative paths a transaction declares it will touch.
///
/// Stored as an ordered set ([`BTreeSet`]): sorting *is* the canonical, total
/// path order the coordinator acquires locks in (kernel mechanism 6.B), so two
/// transactions can never grab the same pair of paths in opposite orders. The
/// declared set is also the boundary the transaction body is checked against —
/// any path the body writes that is not in the set aborts with
/// [`CoordError::Undeclared`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockSet(BTreeSet<PathBuf>);

impl LockSet {
    /// Creates an empty lock set.
    pub fn new() -> Self {
        LockSet(BTreeSet::new())
    }

    /// Adds a workspace-relative path to the declared set, returning the set for
    /// chaining. A duplicate path is a no-op.
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::coordinator::LockSet;
    /// use std::path::Path;
    ///
    /// let locks = LockSet::new()
    ///     .with(Path::new("rust/intro.md"))
    ///     .with(Path::new("rust/index.json"));
    /// assert_eq!(locks.len(), 2);
    /// ```
    pub fn with(mut self, path: impl AsRef<Path>) -> Self {
        self.0.insert(path.as_ref().to_path_buf());
        self
    }

    /// Inserts a workspace-relative path into the declared set, returning `true`
    /// if it was newly added.
    pub fn insert(&mut self, path: impl AsRef<Path>) -> bool {
        self.0.insert(path.as_ref().to_path_buf())
    }

    /// Returns `true` if the set declares `path`.
    pub fn contains(&self, path: impl AsRef<Path>) -> bool {
        self.0.contains(path.as_ref())
    }

    /// The number of declared paths.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if no paths are declared.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates over the declared paths in canonical (sorted) order.
    pub fn iter(&self) -> impl Iterator<Item = &PathBuf> {
        self.0.iter()
    }
}

/// A transaction request: who runs it, at what priority, which paths it may
/// touch, and a short label for the commit message / observability.
///
/// The `locks` field is the up-front declaration mechanism 6.A requires: the
/// full affected set is known before the operation begins. `priority` is normally
/// derived from `writer` via [`Priority::for_writer`] by [`TxnRequest::new`].
#[derive(Debug, Clone)]
pub struct TxnRequest {
    /// The identity running the transaction (the git author of its commit and the
    /// holder of its locks).
    pub writer: WriterId,
    /// The scheduling priority (queue-head for a human, FIFO for an agent).
    pub priority: Priority,
    /// The paths declared up front (mechanism 6.A): the body may write only these.
    pub locks: LockSet,
    /// A short label carried into observability and (by convention) the commit
    /// message.
    pub label: String,
}

impl TxnRequest {
    /// Builds a request, deriving [`priority`](TxnRequest::priority) from `writer`
    /// via [`Priority::for_writer`].
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::coordinator::{LockSet, Priority, TxnRequest};
    /// use ai_write::tool::workspace::WriterId;
    /// use std::path::Path;
    ///
    /// let req = TxnRequest::new(
    ///     WriterId::Human,
    ///     LockSet::new().with(Path::new("rust/intro.md")),
    ///     "human revision",
    /// );
    /// assert_eq!(req.priority, Priority::Human);
    /// ```
    pub fn new(writer: WriterId, locks: LockSet, label: impl Into<String>) -> Self {
        let priority = Priority::for_writer(&writer);
        TxnRequest {
            writer,
            priority,
            locks,
            label: label.into(),
        }
    }
}

/// The outcome of a committed transaction: the single commit's short SHA (when
/// version control recorded one) and the paths it actually changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnOutcome {
    /// The abbreviated SHA of the one commit this transaction produced, or `None`
    /// when the transaction touched no path (an empty commit is never created).
    pub sha: Option<String>,
    /// The workspace-relative paths the transaction touched, in canonical order.
    pub paths: Vec<PathBuf>,
}

/// The outcome of a [`Coordinator::request_edit`] call (B3): the standing human
/// edit reservation was queued, and how many transactions sit ahead of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestEditOutcome {
    /// Always `true`: the reservation was recorded at the head of the wait queue
    /// (the field mirrors the `{queued, ahead}` wire shape the WebUI consumes).
    pub queued: bool,
    /// How many transactions must finish before the human's turn: `1` for a
    /// currently-running transaction plus one per earlier pending reservation. `0`
    /// means the human is up immediately (the critical section is idle).
    pub ahead: usize,
}

/// A new article to be produced by a [`SplitPlan`] or a [`MergePlan`].
///
/// It names the output file, its human-readable title, the full body the
/// transaction writes into it, and its parent in the theme hierarchy (or `None`
/// for a top-level article). The output `file_name` is what the caller must
/// declare up front, satisfying the kernel's "affected set known before the
/// operation begins" rule (mechanism 6.A): the split/merge coordinator builds the
/// declared [`LockSet`] from exactly these names plus the sources and the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewArticle {
    /// The output article file name (a single path segment).
    pub file_name: String,
    /// The article's human-readable title.
    pub title: String,
    /// The full body to write into the new article.
    pub content: String,
    /// The parent article's file name in the theme hierarchy, or `None` for a
    /// top-level article. A non-`None` parent must name an article that exists
    /// once the transaction's source rewrites have settled.
    pub parent: Option<String>,
}

/// A declarative plan for splitting one source article into several.
///
/// The source article is **rewritten** to `source_content` (its retained /
/// overview portion — pass the source's existing text unchanged to keep it as-is,
/// or a shorter lead-in when the bulk moves into the children), and each entry in
/// `outputs` is created as a brand-new article. Per kernel mechanism 6.A the whole
/// affected set must be known up front, so the plan's output file names *are* the
/// declaration: [`Coordinator::split_article`] derives the declared [`LockSet`]
/// as `{ <theme>/<source>, <theme>/<each output>, <theme>/index.json }` and the
/// transaction body writes exactly that set — a body that strayed outside it would
/// abort with [`CoordError::Undeclared`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPlan {
    /// The theme the source and all outputs live in.
    pub theme: String,
    /// The source article file name.
    pub source_file: String,
    /// The new body the source article is rewritten to (its retained portion).
    pub source_content: String,
    /// The new articles to carve out, in creation (and reading) order. Must be
    /// non-empty, free of duplicates, and must not collide with the source.
    pub outputs: Vec<NewArticle>,
}

/// A declarative plan for merging several source articles into one target.
///
/// The `target` article is created with the merged body, then every entry in
/// `sources` is deleted and removed from the index. Per kernel mechanism 6.A the
/// affected set is known up front: [`Coordinator::merge_articles`] derives the
/// declared [`LockSet`] as
/// `{ <theme>/<each source>, <theme>/<target>, <theme>/index.json }` and the
/// transaction body touches exactly that set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    /// The theme the sources and the target live in.
    pub theme: String,
    /// The source article file names to merge and then delete. Must contain at
    /// least two distinct names.
    pub sources: Vec<String>,
    /// The new article the merged content is written into. Its `file_name` must
    /// not collide with a surviving article (it may reuse a source name only if
    /// that source is also being consumed).
    pub target: NewArticle,
}

/// An error from [`Coordinator::submit`].
///
/// It is `#[non_exhaustive]`: callers matching on it must include a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoordError {
    /// The transaction body wrote a path it did not declare in its [`LockSet`]
    /// (violates mechanism 6.A). Carries the offending path. The on-disk write may
    /// already have happened; the transaction is aborted and its locks released
    /// without committing.
    #[error("transaction wrote undeclared path: {0}")]
    Undeclared(PathBuf),
    /// A workspace operation inside the transaction failed (a sandbox violation, a
    /// missing article, an I/O error).
    #[error("workspace error: {0}")]
    Workspace(#[from] ToolError),
    /// The single commit (or another version-control step) failed. The files are
    /// already written to disk; only the git side failed.
    #[error("version control error: {0}")]
    Vcs(#[from] VcsError),
    /// The transaction body returned an error of its own (not one of the above),
    /// or the coordinator aborted the transaction for a stated reason.
    #[error("transaction aborted: {0}")]
    Aborted(String),
}

/// A monotonically increasing ticket sequence number, used to break ties between
/// two waiting tickets of the same priority (FIFO among agents; insertion order
/// among humans).
type Seq = u64;

/// One entry in the coordinator's wait queue: a parked transaction waiting for
/// its turn and for its declared locks to come free.
///
/// The ticket carries only what the *scheduler* needs — its sequence number (to
/// detect when it reaches the queue front) and its writer (whose [`Priority`]
/// fixes head-vs-tail placement). The declared [`LockSet`] is re-checked from the
/// owning [`TxnRequest`] on each wakeup, so it is not duplicated here.
#[derive(Debug, Clone)]
struct Ticket {
    /// The unique sequence number identifying this ticket.
    seq: Seq,
    /// The writer that owns the transaction; its [`Priority`] fixes queue order.
    writer: WriterId,
}

/// A standing human edit reservation (B3): a request to be handed the lock for
/// one article at the head of the queue, recorded without blocking a thread.
///
/// A reservation is created by [`Coordinator::request_edit`] and consumed either
/// by the human's actual edit (a [`Priority::Human`] [`Coordinator::submit`] that
/// touches the same article) or by [`Coordinator::cancel_request_edit`]. While it
/// is pending it holds back agent transactions, so the human keeps its head-of-
/// queue slot (mechanism 6.C, non-preemptive).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reservation {
    /// The theme of the reserved article.
    theme: String,
    /// The reserved article's file name.
    file: String,
}

/// The coordinator's mutable scheduling state, guarded by a single [`Mutex`].
///
/// Holds the operation-level lock table (which path is held by which writer) and
/// the FIFO/priority wait queue of parked tickets. v1 also serializes the whole
/// critical section via [`running`](CoordState::running).
struct CoordState {
    /// Paths currently locked, mapped to their holding writer. An absent path is
    /// free. This is the operation-level lock table the kernel's mechanism 6.B
    /// requires the coordinator to own exclusively.
    held: BTreeMap<PathBuf, WriterId>,
    /// Tickets waiting for their turn, ordered head-first: humans (in insertion
    /// order) precede agents (in FIFO order).
    queue: VecDeque<Ticket>,
    /// `true` while a transaction is inside the critical section. v1 admits one
    /// transaction at a time (the simplest correct baseline,
    /// `docs/coordinator-design.md` §5); disjoint-lock-set concurrency would relax
    /// this without touching the public API.
    running: bool,
    /// Standing human edit reservations (B3): a [`Coordinator::request_edit`]
    /// records one here without blocking a thread, and it sits ahead of every
    /// waiting agent. While any reservation is pending, an agent transaction may
    /// not enter the critical section (head priority, non-preemptive — a running
    /// agent still finishes), so the human's slot at the front is honoured. The
    /// human's actual edit (a [`Priority::Human`] [`Coordinator::submit`]) consumes
    /// the oldest matching reservation when it acquires; a
    /// [`Coordinator::cancel_request_edit`] removes one. Ordered front-first.
    reservations: VecDeque<Reservation>,
}

/// The operation-level transaction coordinator.
///
/// Construct one with [`Coordinator::open`] (which adopts or initializes the git
/// repository at the workspace root) and submit transactions through
/// [`Coordinator::submit`]. A single coordinator is shared (behind an
/// [`Arc`]) by a master and all of its slave threads, so every
/// mutating operation across every thread funnels through one lock table and one
/// [`Vcs`].
///
/// See the [module documentation](self) for the model.
pub struct Coordinator {
    /// The scheduling state (lock table + wait queue + the v1 run flag).
    state: Mutex<CoordState>,
    /// Signalled whenever locks are released or the running flag clears, so parked
    /// tickets re-check whether it is their turn.
    cv: Condvar,
    /// The single workspace handle the coordinator owns. Mutating operations
    /// inside a transaction body go through it; its in-memory per-article lock is
    /// used transiently to satisfy [`Workspace::write_article`]'s precondition
    /// while the coordinator's operation-level lock provides the real exclusion.
    ws: Mutex<Workspace>,
    /// The single, exclusive version-control handle. [`Vcs`] is not `Sync`, so it
    /// lives behind this mutex and every commit is serialized through it.
    vcs: Mutex<Vcs>,
    /// The next ticket sequence number to hand out.
    next_seq: AtomicU64,
    /// The observability sink the coordinator narrates its lock lifecycle to
    /// (B3): [`Event::TxnQueued`] when a ticket parks, [`Event::TxnAcquired`] when
    /// it enters the critical section, [`Event::HandoffToHuman`] when a parked
    /// human acquires after an agent released, and [`Event::TxnReleased`] when the
    /// transaction commits and frees its locks. Defaults to [`NullSink`], so a
    /// coordinator opened without a sink emits nothing.
    events: Arc<dyn EventSink>,
}

impl Coordinator {
    /// Opens a coordinator over the workspace rooted at `root`, adopting (or
    /// initializing) the git repository there.
    ///
    /// The coordinator opens its own [`Workspace`] and [`Vcs`] handle at `root`;
    /// these are the *only* handles through which mutations and commits happen, so
    /// the lock table and the git index are never contended by a second handle.
    ///
    /// # Errors
    ///
    /// Returns [`CoordError::Workspace`] if the workspace root cannot be opened, or
    /// [`CoordError::Vcs`] if the git repository cannot be opened or initialized.
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::coordinator::Coordinator;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let coord = Coordinator::open(dir.path()).unwrap();
    /// # let _ = coord;
    /// ```
    pub fn open(root: impl AsRef<Path>) -> Result<Self, CoordError> {
        let ws = Workspace::open(root.as_ref())?;
        let vcs = Vcs::open_or_init(ws.root())?;
        Ok(Coordinator {
            state: Mutex::new(CoordState {
                held: BTreeMap::new(),
                queue: VecDeque::new(),
                running: false,
                reservations: VecDeque::new(),
            }),
            cv: Condvar::new(),
            ws: Mutex::new(ws),
            vcs: Mutex::new(vcs),
            next_seq: AtomicU64::new(0),
            events: Arc::new(NullSink),
        })
    }

    /// Installs an [`EventSink`] the coordinator narrates its transaction
    /// lifecycle to (B3), returning `self` for chaining.
    ///
    /// Once a sink is installed, every [`submit`](Coordinator::submit) (and the
    /// [`request_edit`](Coordinator::request_edit) tickets it serves) emits the
    /// coordinator observability events — [`Event::TxnQueued`],
    /// [`Event::TxnAcquired`], [`Event::HandoffToHuman`], and
    /// [`Event::TxnReleased`] — so a WebUI can drive its busy / queued / your-turn
    /// banners from the live lock state. The default sink ([`NullSink`]) emits
    /// nothing, so observability is opt-in.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use ai_write::coordinator::Coordinator;
    /// use ai_write::observe::NullSink;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let coord = Coordinator::open(dir.path())
    ///     .unwrap()
    ///     .with_event_sink(Arc::new(NullSink));
    /// # let _ = coord;
    /// ```
    pub fn with_event_sink(mut self, events: Arc<dyn EventSink>) -> Self {
        self.events = events;
        self
    }

    /// Runs `op` against the coordinator's workspace under a short-lived lock.
    ///
    /// This is the read/structure path: tools that do **not** mutate an article's
    /// body (creating themes/articles, reading, listing, reordering) borrow the
    /// shared workspace through here rather than holding their own handle, so the
    /// coordinator stays the single owner of workspace state.
    ///
    /// # Errors
    ///
    /// Propagates whatever [`ToolError`] `op` returns.
    pub fn with_workspace<R>(
        &self,
        op: impl FnOnce(&mut Workspace) -> Result<R, ToolError>,
    ) -> Result<R, ToolError> {
        let mut ws = self
            .ws
            .lock()
            .expect("coordinator workspace mutex poisoned");
        op(&mut ws)
    }

    /// Runs `op` against the coordinator's exclusive [`Vcs`] handle.
    ///
    /// This is how the read-only version-control tools (history / diff) reach git
    /// without opening a competing repository handle. The closure receives the
    /// single [`Vcs`] the coordinator owns; the call is serialized with every
    /// commit, so a history read never races a transaction's commit.
    ///
    /// # Errors
    ///
    /// Propagates whatever [`VcsError`] `op` returns.
    pub fn with_vcs<R>(&self, op: impl FnOnce(&Vcs) -> Result<R, VcsError>) -> Result<R, VcsError> {
        let vcs = self.vcs.lock().expect("coordinator vcs mutex poisoned");
        op(&vcs)
    }

    /// Submits a transaction: declare locks, wait for the whole set, run `body`
    /// inside the critical section, commit once, release.
    ///
    /// `body` receives a [`TxnCtx`] bound to the coordinator's workspace and
    /// declared lock set. It writes the affected files (through
    /// [`TxnCtx::write_article`] or the lower-level [`TxnCtx::record_path`] for an
    /// out-of-band write) and returns the commit message. The coordinator then
    /// checks `touched ⊆ declared`, makes **one** [`Vcs::commit_paths`] commit
    /// covering every touched path, releases the locks, and wakes the next waiter.
    ///
    /// Ordering and fairness follow the [module documentation](self): a human
    /// ticket waits at the queue head, an agent ticket at the tail, and a ticket
    /// only proceeds when its **entire** declared set is free — it never holds a
    /// partial set, so the coordinator cannot deadlock.
    ///
    /// # Errors
    ///
    /// - [`CoordError::Undeclared`] if `body` wrote a path outside the declared
    ///   [`LockSet`].
    /// - [`CoordError::Workspace`] / [`CoordError::Vcs`] for a workspace or commit
    ///   failure.
    /// - [`CoordError::Aborted`] if `body` itself returned an error.
    ///
    /// In every error case the transaction's locks are released before returning,
    /// so a failed transaction never strands a lock.
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::coordinator::{Coordinator, LockSet, TxnRequest};
    /// use ai_write::tool::workspace::WriterId;
    /// use std::path::Path;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let coord = Coordinator::open(dir.path()).unwrap();
    /// coord.with_workspace(|ws| {
    ///     ws.create_theme("t")?;
    ///     ws.create_article("t", "a.md", "A", None)
    /// }).unwrap();
    ///
    /// let req = TxnRequest::new(
    ///     WriterId::Human,
    ///     LockSet::new().with(Path::new("t/a.md")).with(Path::new("t/index.json")),
    ///     "write a.md",
    /// );
    /// let outcome = coord.submit(req, |ctx| {
    ///     ctx.write_article("t", "a.md", "hello")?;
    ///     Ok("edit(t/a.md): hello".to_string())
    /// }).unwrap();
    /// assert!(outcome.sha.is_some());
    /// ```
    pub fn submit<F>(&self, req: TxnRequest, body: F) -> Result<TxnOutcome, CoordError>
    where
        F: FnOnce(&mut TxnCtx<'_>) -> Result<String, CoordError>,
    {
        // Step 1–3: enqueue, wait for the whole declared set to be free and for
        // this ticket's turn, then acquire it all-or-nothing.
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        self.acquire(seq, &req);

        // Steps 4–7 run with the locks held; `run_critical_section` always
        // releases them (success or failure) before returning.
        let result = self.run_critical_section(&req, body);
        self.release(&req.locks, &req.writer);
        result
    }

    /// Enqueues a ticket and blocks until its whole declared lock set is free and
    /// it is at the head of the runnable order, then marks the set held and the
    /// coordinator running (steps 1–3 of [`Coordinator::submit`]).
    ///
    /// Emits the B3 coordinator observability events as it goes: an
    /// [`Event::TxnQueued`] (with the number of tickets ahead) if the ticket has
    /// to park, then — once granted — an [`Event::HandoffToHuman`] when a parked
    /// human acquires after waiting (the "your turn" signal), and finally an
    /// [`Event::TxnAcquired`] naming the writer and the held paths.
    fn acquire(&self, seq: Seq, req: &TxnRequest) {
        let ticket = Ticket {
            seq,
            writer: req.writer.clone(),
        };
        let mut state = self.state.lock().expect("coordinator state mutex poisoned");

        // Step 1: insert at the queue head (human) or tail (agent). A human is
        // placed ahead of every waiting agent but behind earlier-queued humans, so
        // humans keep their own arrival order while still preceding agents.
        let insert_pos = match req.priority {
            Priority::Agent => {
                let pos = state.queue.len();
                state.queue.push_back(ticket);
                pos
            }
            Priority::Human => {
                let pos = state
                    .queue
                    .iter()
                    .position(|t| Priority::for_writer(&t.writer) == Priority::Agent)
                    .unwrap_or(state.queue.len());
                state.queue.insert(pos, ticket);
                pos
            }
        };

        // Whether this ticket can run *right now* without parking: the critical
        // section is idle, it landed at the very front, every declared path is
        // free, and (for an agent) no human reservation is pending ahead of it. If
        // not, it will block — and a human that blocks here is the queue-head
        // human that gets a handoff once an agent releases.
        let runnable_immediately = !state.running
            && insert_pos == 0
            && state.queue.front().map(|t| t.seq) == Some(seq)
            && req.locks.iter().all(|p| !state.held.contains_key(p))
            && self.may_run(&state, req);
        let mut parked = false;
        if !runnable_immediately {
            // Step 1 (cont.): announce the wait. `ahead` is how many tickets sit in
            // front of this one at the moment it parks.
            parked = true;
            self.events.emit(Event::TxnQueued {
                writer: req.writer.provenance_tag(),
                ahead: insert_pos,
            });
        }

        // Step 2: wait until (a) no transaction is running (v1 serializes the
        // whole critical section), (b) this ticket is at the front of the queue,
        // (c) all of its declared paths are free, and (d) no pending human
        // reservation outranks it. Acquisition is all-or-nothing: until every path
        // is free this ticket takes nothing.
        loop {
            let is_front = state.queue.front().map(|t| t.seq) == Some(seq);
            let all_free = !state.running
                && is_front
                && req.locks.iter().all(|p| !state.held.contains_key(p))
                && self.may_run(&state, req);
            if all_free {
                break;
            }
            state = self
                .cv
                .wait(state)
                .expect("coordinator state mutex poisoned");
        }

        // Step 3: acquire the whole set and mark the critical section busy. Pop
        // this ticket off the queue now that it is running, and (for a human)
        // consume the oldest reservation it satisfies — its standing request has
        // now been served.
        state.queue.pop_front();
        state.running = true;
        for path in req.locks.iter() {
            state.held.insert(path.clone(), req.writer.clone());
        }
        // A human submit consumes the oldest standing reservation it satisfies. If
        // it had one, the reservation flow already announced the handoff (at
        // `request_edit` time or when the prior transaction released), so this
        // submit must not announce it a second time.
        let consumed_reservation =
            req.priority == Priority::Human && consume_reservation(&mut state, req);
        drop(state);

        // A human that had to wait *without* a standing reservation and now holds
        // the lock is the handoff: control passed to the human after the running
        // transaction released (mechanism 6.C). Announce it per declared article so
        // the UI can raise the "your turn" banner for the right file. (A human with
        // a reservation already got its handoff through the reservation flow.)
        if parked && req.priority == Priority::Human && !consumed_reservation {
            for (theme, file) in article_paths(&req.locks) {
                self.events.emit(Event::HandoffToHuman { theme, file });
            }
        }
        self.events.emit(Event::TxnAcquired {
            writer: req.writer.provenance_tag(),
            paths: req
                .locks
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        });
    }

    /// Releases every path in `locks`, clears the running flag, wakes parked
    /// tickets so the next runnable one can proceed, and emits an
    /// [`Event::TxnReleased`] for `writer` (step 7).
    fn release(&self, locks: &LockSet, writer: &WriterId) {
        let mut state = self.state.lock().expect("coordinator state mutex poisoned");
        for path in locks.iter() {
            state.held.remove(path);
        }
        state.running = false;
        // If a standing human reservation is now at the head of the idle
        // coordinator, this release is the moment control passes to the human
        // (mechanism 6.C): the AI's transaction committed and the human's turn has
        // come. Snapshot it to announce after dropping the lock.
        let handoff = state.reservations.front().cloned();
        // Wake everyone: each parked ticket re-checks its own condition and only
        // one will find the queue front + its locks free (and agents yield to any
        // pending reservation).
        drop(state);
        self.cv.notify_all();
        self.events.emit(Event::TxnReleased {
            writer: writer.provenance_tag(),
        });
        if let Some(r) = handoff {
            self.events.emit(Event::HandoffToHuman {
                theme: r.theme,
                file: r.file,
            });
        }
    }

    /// Whether `req` is allowed to enter the critical section given the pending
    /// human reservations, ignoring the lock-table and running checks (those are
    /// tested separately by the caller).
    ///
    /// A [`Priority::Human`] ticket always may run — its own standing reservation,
    /// if any, is what it is here to consume. An agent may run only when **no**
    /// reservation is pending: a standing human request sits at the head of the
    /// queue (mechanism 6.C), so an agent waits behind it.
    fn may_run(&self, state: &CoordState, req: &TxnRequest) -> bool {
        match req.priority {
            Priority::Human => true,
            Priority::Agent => state.reservations.is_empty(),
        }
    }

    /// Runs steps 4–6 with the locks already held: build the [`TxnCtx`], run the
    /// body, check `touched ⊆ declared`, and make the single commit.
    fn run_critical_section<F>(&self, req: &TxnRequest, body: F) -> Result<TxnOutcome, CoordError>
    where
        F: FnOnce(&mut TxnCtx<'_>) -> Result<String, CoordError>,
    {
        let mut ws = self
            .ws
            .lock()
            .expect("coordinator workspace mutex poisoned");
        let mut touched = BTreeSet::new();

        // Step 4–5: run the body against the bounded context.
        let message = {
            let mut ctx = TxnCtx {
                ws: &mut ws,
                writer: &req.writer,
                declared: &req.locks,
                touched: &mut touched,
            };
            body(&mut ctx)?
        };

        // Step 5 (boundary check): every touched path must be declared.
        for path in &touched {
            if !req.locks.contains(path) {
                return Err(CoordError::Undeclared(path.clone()));
            }
        }

        // A transaction that touched nothing makes no commit (no empty commits).
        if touched.is_empty() {
            return Ok(TxnOutcome {
                sha: None,
                paths: Vec::new(),
            });
        }

        // Step 6: one commit covering every touched path, inside the critical
        // section, before the locks are released.
        let paths: Vec<PathBuf> = touched.into_iter().collect();
        let path_refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let vcs = self.vcs.lock().expect("coordinator vcs mutex poisoned");
        let sha = vcs.commit_paths(&path_refs, &req.writer, &message)?;
        Ok(TxnOutcome {
            sha: Some(sha),
            paths,
        })
    }

    /// Splits one source article into several, as a single cross-file transaction
    /// (kernel §6; `docs/coordinator-design.md` §6, G5).
    ///
    /// The source article is rewritten to [`SplitPlan::source_content`] and every
    /// entry in [`SplitPlan::outputs`] is created as a new article, all under one
    /// commit. The declared lock set is built up front from the source, the output
    /// file names, and the theme `index.json` — so the operation satisfies the
    /// declared-lock rule (mechanism 6.A): the caller must enumerate the output
    /// file names, and a body that wrote anything outside that set would abort.
    ///
    /// The manifest is updated to add each output to the reading order and to set
    /// its parent pointer, keeping the hierarchy consistent. All of this — the
    /// source rewrite, the new files, and the index update — lands in **one**
    /// commit (one cognitive unit, kernel §5).
    ///
    /// # Errors
    ///
    /// - [`CoordError::Aborted`] if the plan is malformed: empty `outputs`, a
    ///   duplicated output name, an output colliding with the source, or an output
    ///   that names an already-existing article.
    /// - [`CoordError::Workspace`] if the source is missing or a write fails.
    /// - [`CoordError::Vcs`] if the single commit fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::coordinator::{Coordinator, NewArticle, SplitPlan};
    /// use ai_write::tool::workspace::WriterId;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let coord = Coordinator::open(dir.path()).unwrap();
    /// coord.with_workspace(|ws| {
    ///     ws.create_theme("t")?;
    ///     ws.create_article("t", "all.md", "All", None)
    /// }).unwrap();
    ///
    /// let plan = SplitPlan {
    ///     theme: "t".into(),
    ///     source_file: "all.md".into(),
    ///     source_content: "overview\n".into(),
    ///     outputs: vec![NewArticle {
    ///         file_name: "part1.md".into(),
    ///         title: "Part 1".into(),
    ///         content: "first part\n".into(),
    ///         parent: Some("all.md".into()),
    ///     }],
    /// };
    /// let outcome = coord.split_article(WriterId::Human, plan).unwrap();
    /// assert!(outcome.sha.is_some());
    /// ```
    pub fn split_article(
        &self,
        writer: WriterId,
        plan: SplitPlan,
    ) -> Result<TxnOutcome, CoordError> {
        // Validate the plan before declaring locks: the affected set must be known
        // and self-consistent up front (mechanism 6.A).
        if plan.outputs.is_empty() {
            return Err(CoordError::Aborted(
                "split requires at least one output article".to_string(),
            ));
        }
        let mut seen = BTreeSet::new();
        for out in &plan.outputs {
            if out.file_name == plan.source_file {
                return Err(CoordError::Aborted(format!(
                    "output `{}` collides with the source article",
                    out.file_name
                )));
            }
            if !seen.insert(out.file_name.clone()) {
                return Err(CoordError::Aborted(format!(
                    "duplicate output article `{}`",
                    out.file_name
                )));
            }
        }

        // Declared lock set: source + every output + the theme manifest.
        let mut locks = LockSet::new()
            .with(Path::new(&plan.theme).join(&plan.source_file))
            .with(Path::new(&plan.theme).join("index.json"));
        for out in &plan.outputs {
            locks.insert(Path::new(&plan.theme).join(&out.file_name));
        }

        let label = format!(
            "split({}/{}): {} parts",
            plan.theme,
            plan.source_file,
            plan.outputs.len()
        );
        let req = TxnRequest::new(writer, locks, label);
        let message = format!(
            "split({}/{}): into {} new articles",
            plan.theme,
            plan.source_file,
            plan.outputs.len()
        );
        self.submit(req, move |ctx| {
            // Rewrite the source to its retained portion first, so a parent that
            // points at the source resolves against an existing article.
            ctx.write_article(&plan.theme, &plan.source_file, &plan.source_content)?;
            for out in &plan.outputs {
                ctx.create_article(
                    &plan.theme,
                    &out.file_name,
                    &out.content,
                    &out.title,
                    out.parent.as_deref(),
                )?;
            }
            Ok(message)
        })
    }

    /// Merges several source articles into one target, as a single cross-file
    /// transaction (kernel §6; `docs/coordinator-design.md` §6, G5).
    ///
    /// The [`MergePlan::target`] article is created with the merged body, then
    /// every [`MergePlan::sources`] article is deleted and removed from the index,
    /// all under one commit. The declared lock set is built up front from the
    /// sources, the target file name, and the theme `index.json` (mechanism 6.A).
    ///
    /// The manifest is updated so the target is in the reading order and the
    /// consumed sources are gone; any article that was parented under a deleted
    /// source is lifted to that source's own parent (the existing index
    /// re-parenting rule), keeping the hierarchy free of dangling pointers.
    ///
    /// # Errors
    ///
    /// - [`CoordError::Aborted`] if the plan is malformed: fewer than two sources,
    ///   a duplicated source, or a target colliding with a *surviving* article.
    /// - [`CoordError::Workspace`] if a source is missing or a write/delete fails.
    /// - [`CoordError::Vcs`] if the single commit fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::coordinator::{Coordinator, MergePlan, NewArticle};
    /// use ai_write::tool::workspace::WriterId;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let coord = Coordinator::open(dir.path()).unwrap();
    /// coord.with_workspace(|ws| {
    ///     ws.create_theme("t")?;
    ///     ws.create_article("t", "a.md", "A", None)?;
    ///     ws.create_article("t", "b.md", "B", None)
    /// }).unwrap();
    ///
    /// let plan = MergePlan {
    ///     theme: "t".into(),
    ///     sources: vec!["a.md".into(), "b.md".into()],
    ///     target: NewArticle {
    ///         file_name: "merged.md".into(),
    ///         title: "Merged".into(),
    ///         content: "a + b\n".into(),
    ///         parent: None,
    ///     },
    /// };
    /// let outcome = coord.merge_articles(WriterId::Human, plan).unwrap();
    /// assert!(outcome.sha.is_some());
    /// ```
    pub fn merge_articles(
        &self,
        writer: WriterId,
        plan: MergePlan,
    ) -> Result<TxnOutcome, CoordError> {
        if plan.sources.len() < 2 {
            return Err(CoordError::Aborted(
                "merge requires at least two source articles".to_string(),
            ));
        }
        let mut seen = BTreeSet::new();
        for src in &plan.sources {
            if !seen.insert(src.clone()) {
                return Err(CoordError::Aborted(format!(
                    "duplicate source article `{src}`"
                )));
            }
        }
        // The target may reuse a source's name (that source is consumed), but it
        // must not collide with an article that survives the merge.
        if !plan.sources.contains(&plan.target.file_name)
            && self
                .ws
                .lock()
                .expect("coordinator workspace mutex poisoned")
                .root()
                .join(&plan.theme)
                .join(&plan.target.file_name)
                .exists()
        {
            return Err(CoordError::Aborted(format!(
                "target `{}` collides with an existing article",
                plan.target.file_name
            )));
        }

        // Declared lock set: every source + the target + the theme manifest.
        let mut locks = LockSet::new()
            .with(Path::new(&plan.theme).join(&plan.target.file_name))
            .with(Path::new(&plan.theme).join("index.json"));
        for src in &plan.sources {
            locks.insert(Path::new(&plan.theme).join(src));
        }

        let label = format!(
            "merge({}): {} -> {}",
            plan.theme,
            plan.sources.len(),
            plan.target.file_name
        );
        let req = TxnRequest::new(writer, locks, label);
        let message = format!(
            "merge({}): {} articles into {}",
            plan.theme,
            plan.sources.len(),
            plan.target.file_name
        );
        self.submit(req, move |ctx| {
            // Delete every source first. When the target reuses a source's name we
            // must free that name before creating the target.
            for src in &plan.sources {
                ctx.delete_article(&plan.theme, src)?;
            }
            // A target parent that is itself a consumed source has just been
            // removed; clear it to avoid a dangling parent pointer.
            let parent = match &plan.target.parent {
                Some(p) if plan.sources.contains(p) => None,
                other => other.clone(),
            };
            ctx.create_article(
                &plan.theme,
                &plan.target.file_name,
                &plan.target.content,
                &plan.target.title,
                parent.as_deref(),
            )?;
            Ok(message)
        })
    }

    /// Undoes an article's last edit under the operation-level lock, as one
    /// human-priority transaction (B3; closes kernel-impl-results §3.1).
    ///
    /// This routes the WebUI's undo through the **same** lock table and the
    /// **same** single [`Vcs`] every other mutation uses, instead of opening a
    /// competing repository handle: the undo acquires the article's lock (jumping
    /// the queue at [`Priority::Human`], never preempting a running transaction),
    /// reverts the last edit via [`Vcs::undo_last`] on the coordinator's exclusive
    /// handle (a new revert commit, never a history rewrite), then releases — so an
    /// undo and a concurrent agent edit can never race the git index. The
    /// transaction lifecycle events (acquire / queue / release / handoff) are
    /// emitted exactly as for [`Coordinator::submit`].
    ///
    /// Returns the revert commit's short SHA, or `None` when the article has only
    /// one version (nothing to undo).
    ///
    /// # Errors
    ///
    /// Returns [`CoordError::Vcs`] if the revert fails for a reason other than
    /// "nothing to undo".
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::coordinator::Coordinator;
    /// use ai_write::tool::workspace::WriterId;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let coord = Coordinator::open(dir.path()).unwrap();
    /// // An article with no history yields `None` (nothing to undo).
    /// let undone = coord.undo_article(WriterId::Human, "t", "ghost.md").unwrap();
    /// assert!(undone.is_none());
    /// ```
    pub fn undo_article(
        &self,
        writer: WriterId,
        theme: &str,
        file: &str,
    ) -> Result<Option<String>, CoordError> {
        let rel = Path::new(theme).join(file);
        let locks = LockSet::new().with(&rel);
        let label = format!("undo({theme}/{file})");
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let req = TxnRequest::new(writer.clone(), locks.clone(), label);

        // Hold the article lock for the whole revert: acquire (emitting the B3
        // lifecycle events), revert on the owned Vcs, then release — undo never
        // races an agent edit on the git index.
        self.acquire(seq, &req);
        let result = {
            let vcs = self.vcs.lock().expect("coordinator vcs mutex poisoned");
            vcs.undo_last(&rel, &writer)
        };
        self.release(&locks, &writer);
        result.map_err(CoordError::from)
    }

    /// Records a standing human edit reservation for `<theme>/<file>` at the head
    /// of the wait queue (B3), returning how many transactions are ahead of it.
    ///
    /// This is the non-blocking counterpart to [`Coordinator::submit`]: it does
    /// **not** run a transaction body or hold a thread — it registers the human's
    /// intent to edit so the coordinator stops admitting agent transactions until
    /// the human's actual edit arrives (a [`Priority::Human`] submit touching the
    /// same article) or the reservation is cancelled via
    /// [`Coordinator::cancel_request_edit`]. Per mechanism 6.C the reservation sits
    /// ahead of every waiting agent but never preempts a running transaction.
    ///
    /// The returned [`RequestEditOutcome::ahead`] is how many transactions must
    /// finish before the human's turn: `1` if a transaction is currently running,
    /// plus one for each earlier still-pending reservation. When `ahead` is `0`
    /// (the critical section is idle and this is the only reservation) the human is
    /// up immediately and an [`Event::HandoffToHuman`] is emitted at once;
    /// otherwise the handoff fires later, when the running transaction releases and
    /// this reservation reaches the front.
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::coordinator::Coordinator;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let coord = Coordinator::open(dir.path()).unwrap();
    /// // No transaction running: the human is up immediately.
    /// let out = coord.request_edit("t", "a.md");
    /// assert!(out.queued);
    /// assert_eq!(out.ahead, 0);
    /// ```
    pub fn request_edit(&self, theme: &str, file: &str) -> RequestEditOutcome {
        let mut state = self.state.lock().expect("coordinator state mutex poisoned");
        // `ahead` is how many transactions must complete before this reservation's
        // turn: the running transaction (if any) plus every reservation already
        // queued ahead of this one.
        let ahead = usize::from(state.running) + state.reservations.len();
        state.reservations.push_back(Reservation {
            theme: theme.to_string(),
            file: file.to_string(),
        });
        // A reservation freshly at the head of an idle coordinator means the human
        // is up now: signal the handoff immediately. Wake any parked agents so they
        // re-check and yield to the reservation.
        let up_now = ahead == 0;
        drop(state);
        self.cv.notify_all();
        if up_now {
            self.events.emit(Event::HandoffToHuman {
                theme: theme.to_string(),
                file: file.to_string(),
            });
        }
        RequestEditOutcome {
            queued: true,
            ahead,
        }
    }

    /// Cancels a standing human edit reservation for `<theme>/<file>` (B3),
    /// returning `true` if one was pending and removed.
    ///
    /// Removes the oldest matching reservation made by
    /// [`Coordinator::request_edit`] and wakes any agent transactions that were
    /// parked behind it, so they may proceed if no other reservation outranks them.
    /// Cancelling when no matching reservation is pending is a no-op that returns
    /// `false`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::coordinator::Coordinator;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let coord = Coordinator::open(dir.path()).unwrap();
    /// coord.request_edit("t", "a.md");
    /// assert!(coord.cancel_request_edit("t", "a.md"));
    /// assert!(!coord.cancel_request_edit("t", "a.md")); // already gone
    /// ```
    pub fn cancel_request_edit(&self, theme: &str, file: &str) -> bool {
        let mut state = self.state.lock().expect("coordinator state mutex poisoned");
        let pos = state
            .reservations
            .iter()
            .position(|r| r.theme == theme && r.file == file);
        let removed = pos.is_some();
        if let Some(pos) = pos {
            state.reservations.remove(pos);
        }
        drop(state);
        if removed {
            // Agents may have been waiting behind this reservation; let them
            // re-check now that it is gone.
            self.cv.notify_all();
        }
        removed
    }
}

/// Removes the oldest pending reservation a human transaction `req` satisfies,
/// returning `true` if one was consumed.
///
/// When a human edit acquires the lock, any standing [`Coordinator::request_edit`]
/// it covers (a reservation whose `<theme>/<file>` is in the human's declared
/// lock set) has been served, so it is dropped — front-first, removing exactly
/// one. A human submit that matches no reservation leaves the queue untouched and
/// returns `false`.
fn consume_reservation(state: &mut CoordState, req: &TxnRequest) -> bool {
    let pos = state
        .reservations
        .iter()
        .position(|r| req.locks.contains(Path::new(&r.theme).join(&r.file)));
    if let Some(pos) = pos {
        state.reservations.remove(pos);
        true
    } else {
        false
    }
}

/// Extracts the `(theme, file)` article pairs from a declared [`LockSet`].
///
/// A lock set declares `<theme>/<file>` article paths alongside the theme's
/// `<theme>/index.json` manifest; this keeps only the real articles (every
/// two-segment path whose file name is not `index.json`), so a handoff event is
/// raised per article the human acquired rather than for the manifest. The
/// returned pairs are in the lock set's canonical (sorted) order.
fn article_paths(locks: &LockSet) -> Vec<(String, String)> {
    locks
        .iter()
        .filter_map(|p| {
            let file = p.file_name()?.to_str()?;
            if file == "index.json" {
                return None;
            }
            let theme = p.parent()?.to_str()?;
            if theme.is_empty() {
                return None;
            }
            Some((theme.to_string(), file.to_string()))
        })
        .collect()
}

/// The bounded context handed to a transaction body inside the critical section.
///
/// It exposes sandboxed workspace **reads and writes**, but deliberately not lock
/// or commit control — acquiring locks and committing are the coordinator's job
/// (`docs/coordinator-design.md` §3). Every path the body writes is recorded in
/// `touched`; the coordinator later asserts `touched ⊆ declared` and commits
/// exactly that set.
pub struct TxnCtx<'a> {
    /// The coordinator's workspace, borrowed for the duration of the body.
    ws: &'a mut Workspace,
    /// The writer the transaction runs under (lock holder + commit author).
    writer: &'a WriterId,
    /// The paths declared up front; writes must stay within this set.
    declared: &'a LockSet,
    /// The paths the body has actually written so far, recorded for the commit and
    /// the `touched ⊆ declared` boundary check.
    touched: &'a mut BTreeSet<PathBuf>,
}

impl TxnCtx<'_> {
    /// The writer this transaction runs under.
    pub fn writer(&self) -> &WriterId {
        self.writer
    }

    /// The declared lock set; writes outside it abort the transaction.
    pub fn declared(&self) -> &LockSet {
        self.declared
    }

    /// Reads an article's full text through the sandboxed workspace.
    ///
    /// Reading does not require the path to be declared — a transaction may read
    /// any article for context — but writing does.
    ///
    /// # Errors
    ///
    /// Returns [`CoordError::Workspace`] if the article is missing, too large,
    /// binary, or unreadable.
    pub fn read_article(&self, theme: &str, file_name: &str) -> Result<String, CoordError> {
        Ok(self.ws.read_article(theme, file_name)?)
    }

    /// Overwrites article `<theme>/<file_name>` with `text`, recording both the
    /// article and the theme's `index.json` as touched.
    ///
    /// The coordinator's operation-level lock already guarantees exclusive access,
    /// so this transiently takes and releases the workspace's own in-memory
    /// per-article lock purely to satisfy
    /// [`Workspace::write_article`](crate::tool::workspace::Workspace::write_article)'s
    /// precondition. Writing also records the writer as a contributor in the theme
    /// index, so `index.json` is part of the touched set and lands in the same
    /// commit as the body (one cognitive unit = one commit, kernel §5).
    ///
    /// # Errors
    ///
    /// Returns [`CoordError::Undeclared`] if `<theme>/<file_name>` (or the theme's
    /// `index.json`) was not declared in the transaction's [`LockSet`], or
    /// [`CoordError::Workspace`] on a write failure.
    pub fn write_article(
        &mut self,
        theme: &str,
        file_name: &str,
        text: &str,
    ) -> Result<(), CoordError> {
        let article_rel = Path::new(theme).join(file_name);
        let index_rel = Path::new(theme).join("index.json");
        // Boundary check before mutating anything: both the body and the manifest
        // must be declared (the manifest is recorded as touched because the write
        // updates contributors).
        if !self.declared.contains(&article_rel) {
            return Err(CoordError::Undeclared(article_rel));
        }
        if !self.declared.contains(&index_rel) {
            return Err(CoordError::Undeclared(index_rel));
        }

        // The coordinator owns exclusion; take the workspace's own lock only to
        // satisfy `write_article`'s precondition, then drop it immediately.
        self.ws.acquire_lock(theme, file_name, self.writer)?;
        let write = self.ws.write_article(theme, file_name, text, self.writer);
        let _ = self.ws.release_lock(theme, file_name, self.writer);
        write?;

        self.touched.insert(article_rel);
        // Record the manifest as touched only if it exists on disk (a theme always
        // has one after `create_theme`, but guard for robustness).
        if self.ws.root().join(&index_rel).exists() {
            self.touched.insert(index_rel);
        }
        Ok(())
    }

    /// Records `path` (workspace-relative) as touched without writing it here.
    ///
    /// For transactions that mutate the workspace through a side channel (a future
    /// split/merge writing a brand-new file, say): the body performs the write via
    /// the workspace and calls this so the path is folded into the single commit
    /// and checked against the declared set.
    ///
    /// # Errors
    ///
    /// Returns [`CoordError::Undeclared`] if `path` is not in the declared
    /// [`LockSet`].
    pub fn record_path(&mut self, path: impl AsRef<Path>) -> Result<(), CoordError> {
        let path = path.as_ref().to_path_buf();
        if !self.declared.contains(&path) {
            return Err(CoordError::Undeclared(path));
        }
        self.touched.insert(path);
        Ok(())
    }

    /// Creates a brand-new article `<theme>/<file_name>` with `content`, a `title`,
    /// and an optional `parent`, recording both the new file and the theme
    /// `index.json` as touched.
    ///
    /// This is the structural primitive the split/merge transactions build their
    /// output articles with. Both the new article path **and** the theme manifest
    /// must be declared in the transaction's [`LockSet`] (the manifest is rewritten
    /// to add the article to the reading order and record its contributor), so they
    /// land in the same single commit as the rest of the operation. The write is
    /// attributed to the transaction's writer.
    ///
    /// # Errors
    ///
    /// Returns [`CoordError::Undeclared`] if the new article path or the theme
    /// `index.json` was not declared, or [`CoordError::Workspace`] if the article
    /// already exists, the theme or `parent` is missing, or the write fails.
    pub fn create_article(
        &mut self,
        theme: &str,
        file_name: &str,
        content: &str,
        title: &str,
        parent: Option<&str>,
    ) -> Result<(), CoordError> {
        let article_rel = Path::new(theme).join(file_name);
        let index_rel = Path::new(theme).join("index.json");
        if !self.declared.contains(&article_rel) {
            return Err(CoordError::Undeclared(article_rel));
        }
        if !self.declared.contains(&index_rel) {
            return Err(CoordError::Undeclared(index_rel));
        }
        self.ws.create_article_with_content(
            theme,
            file_name,
            content,
            title,
            parent,
            self.writer,
        )?;
        self.touched.insert(article_rel);
        self.touched.insert(index_rel);
        Ok(())
    }

    /// Deletes article `<theme>/<file_name>` and removes it from the theme index,
    /// recording both the removed file and the theme `index.json` as touched.
    ///
    /// This is the counterpart [`TxnCtx::create_article`] uses to retire a source
    /// article after its content has been folded elsewhere (e.g. the consumed
    /// sources of a merge). Both the article path and the theme manifest must be
    /// declared in the transaction's [`LockSet`] so the deletion and the index
    /// update commit together.
    ///
    /// # Errors
    ///
    /// Returns [`CoordError::Undeclared`] if the article path or the theme
    /// `index.json` was not declared, or [`CoordError::Workspace`] if the article
    /// is missing or the deletion fails.
    pub fn delete_article(&mut self, theme: &str, file_name: &str) -> Result<(), CoordError> {
        let article_rel = Path::new(theme).join(file_name);
        let index_rel = Path::new(theme).join("index.json");
        if !self.declared.contains(&article_rel) {
            return Err(CoordError::Undeclared(article_rel));
        }
        if !self.declared.contains(&index_rel) {
            return Err(CoordError::Undeclared(index_rel));
        }
        self.ws.delete_article(theme, file_name)?;
        self.touched.insert(article_rel);
        self.touched.insert(index_rel);
        Ok(())
    }

    /// Borrows the underlying workspace for an operation the higher-level helpers
    /// do not cover (used by future multi-file transactions).
    ///
    /// Any path the operation mutates must still be declared and recorded via
    /// [`TxnCtx::record_path`], or the `touched ⊆ declared` check will reject it.
    ///
    /// # Errors
    ///
    /// Propagates whatever [`ToolError`] `op` returns, wrapped as
    /// [`CoordError::Workspace`].
    pub fn with_workspace<R>(
        &mut self,
        op: impl FnOnce(&mut Workspace) -> Result<R, ToolError>,
    ) -> Result<R, CoordError> {
        Ok(op(self.ws)?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn agent(label: &str) -> WriterId {
        WriterId::Agent {
            model: "deepseek-v4-pro".to_string(),
            label: label.to_string(),
        }
    }

    /// Opens a coordinator over a fresh temp dir with a theme `t` and the named
    /// articles already created (committed nowhere yet).
    fn coord_with(articles: &[&str]) -> (tempfile::TempDir, Coordinator) {
        let dir = tempfile::tempdir().expect("tempdir");
        let coord = Coordinator::open(dir.path()).expect("open coordinator");
        coord
            .with_workspace(|ws| {
                ws.create_theme("t")?;
                for a in articles {
                    ws.create_article("t", a, a, None)?;
                }
                Ok(())
            })
            .expect("setup");
        (dir, coord)
    }

    /// A lock set covering an article body plus its theme manifest.
    fn article_locks(theme: &str, file: &str) -> LockSet {
        LockSet::new()
            .with(Path::new(theme).join(file))
            .with(Path::new(theme).join("index.json"))
    }

    /// An [`EventSink`] that records every event it receives, for B3 sequence and
    /// payload assertions.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Event>>);

    impl EventSink for Recorder {
        fn emit(&self, event: Event) {
            self.0.lock().expect("recorder not poisoned").push(event);
        }
    }

    impl Recorder {
        /// The ordered kind labels of every event recorded so far.
        fn kinds(&self) -> Vec<&'static str> {
            self.0
                .lock()
                .expect("recorder not poisoned")
                .iter()
                .map(event_kind)
                .collect()
        }

        /// A snapshot clone of every recorded event, for payload assertions.
        fn events(&self) -> Vec<Event> {
            self.0.lock().expect("recorder not poisoned").clone()
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

    /// Opens a coordinator (with theme `t` and the named articles) wired to a
    /// fresh [`Recorder`] sink, returning the temp dir, the coordinator, and the
    /// shared recorder so a test can both submit and inspect the emitted events.
    fn coord_with_sink(articles: &[&str]) -> (tempfile::TempDir, Coordinator, Arc<Recorder>) {
        let (dir, coord) = coord_with(articles);
        let recorder = Arc::new(Recorder::default());
        let coord = coord.with_event_sink(Arc::clone(&recorder) as Arc<dyn EventSink>);
        (dir, coord, recorder)
    }

    #[test]
    fn priority_derives_from_writer() {
        assert_eq!(Priority::for_writer(&WriterId::Human), Priority::Human);
        assert_eq!(Priority::for_writer(&agent("s1")), Priority::Agent);
    }

    #[test]
    fn lock_set_is_a_sorted_dedup_set() {
        let mut ls = LockSet::new();
        assert!(ls.insert("b/x.md"));
        assert!(ls.insert("a/y.md"));
        assert!(!ls.insert("a/y.md")); // duplicate
        let order: Vec<_> = ls
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(order, vec!["a/y.md", "b/x.md"], "iteration is sorted");
        assert_eq!(ls.len(), 2);
        assert!(ls.contains("a/y.md"));
    }

    #[test]
    fn single_edit_commits_body_and_index_as_one_commit() {
        let (_d, coord) = coord_with(&["a.md"]);
        let req = TxnRequest::new(agent("s1"), article_locks("t", "a.md"), "write a");
        let outcome = coord
            .submit(req, |ctx| {
                ctx.write_article("t", "a.md", "hello body")?;
                Ok("edit(t/a.md): write".to_string())
            })
            .expect("submit");

        let sha = outcome.sha.clone().expect("one commit produced");

        // Both the article and the manifest were touched and committed together.
        assert!(outcome.paths.iter().any(|p| p.ends_with("a.md")));
        assert!(outcome.paths.iter().any(|p| p.ends_with("index.json")));

        // The article's git history grew by exactly ONE commit (not two), and the
        // index shares that very commit.
        coord
            .with_vcs(|vcs| {
                let hist_a = vcs.history(Path::new("t/a.md"))?;
                let hist_i = vcs.history(Path::new("t/index.json"))?;
                assert_eq!(hist_a.len(), 1, "one edit is one commit on the body");
                assert_eq!(hist_i.len(), 1, "manifest shares the same single commit");
                assert_eq!(hist_a[0].id, sha);
                assert_eq!(hist_i[0].id, sha);
                Ok(())
            })
            .expect("history");
    }

    #[test]
    fn two_edits_grow_history_by_one_each() {
        let (_d, coord) = coord_with(&["a.md"]);
        for text in ["v1", "v2"] {
            let req = TxnRequest::new(agent("s1"), article_locks("t", "a.md"), "edit");
            coord
                .submit(req, |ctx| {
                    ctx.write_article("t", "a.md", text)?;
                    Ok(format!("edit(t/a.md): {text}"))
                })
                .expect("submit");
        }
        coord
            .with_vcs(|vcs| {
                let hist = vcs.history(Path::new("t/a.md"))?;
                assert_eq!(hist.len(), 2, "two edits, two commits (one each)");
                Ok(())
            })
            .expect("history");
    }

    #[test]
    fn writing_an_undeclared_path_is_rejected() {
        let (_d, coord) = coord_with(&["a.md", "b.md"]);
        // Declare only a.md (+ index); the body tries to write b.md.
        let req = TxnRequest::new(agent("s1"), article_locks("t", "a.md"), "sneaky");
        let err = coord
            .submit(req, |ctx| {
                ctx.write_article("t", "b.md", "not allowed")?;
                Ok("should not reach".to_string())
            })
            .expect_err("undeclared write must fail");
        assert!(matches!(err, CoordError::Undeclared(_)), "got {err:?}");

        // And no commit was made for b.md.
        coord
            .with_vcs(|vcs| {
                assert!(vcs.history(Path::new("t/b.md"))?.is_empty());
                Ok(())
            })
            .expect("history");
    }

    #[test]
    fn empty_transaction_makes_no_commit() {
        let (_d, coord) = coord_with(&["a.md"]);
        let req = TxnRequest::new(agent("s1"), article_locks("t", "a.md"), "noop");
        let outcome = coord
            .submit(req, |_ctx| Ok("nothing".to_string()))
            .expect("submit");
        assert!(outcome.sha.is_none());
        assert!(outcome.paths.is_empty());
    }

    #[test]
    fn body_error_is_aborted_and_releases_locks() {
        let (_d, coord) = coord_with(&["a.md"]);
        let req = TxnRequest::new(agent("s1"), article_locks("t", "a.md"), "boom");
        let err = coord
            .submit(req, |_ctx| Err(CoordError::Aborted("body failed".into())))
            .expect_err("body error surfaces");
        assert!(matches!(err, CoordError::Aborted(_)));

        // The lock was released despite the failure: a fresh transaction proceeds.
        let req2 = TxnRequest::new(agent("s2"), article_locks("t", "a.md"), "after");
        coord
            .submit(req2, |ctx| {
                ctx.write_article("t", "a.md", "recovered")?;
                Ok("edit".to_string())
            })
            .expect("second submit proceeds, lock was freed");
    }

    #[test]
    fn all_or_nothing_blocks_until_whole_set_is_free() {
        // T1 holds {a, index}. T2 wants {a, b, index}; it must NOT acquire a
        // partial set (e.g. b alone) while a is held — it waits for the whole set.
        let (_d, coord) = coord_with(&["a.md", "b.md"]);
        let coord = Arc::new(coord);

        let enter = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));

        // T1: hold a + index until told to release.
        let c1 = Arc::clone(&coord);
        let e1 = Arc::clone(&enter);
        let r1 = Arc::clone(&release);
        let t1 = thread::spawn(move || {
            let req = TxnRequest::new(agent("s1"), article_locks("t", "a.md"), "hold a");
            c1.submit(req, |ctx| {
                ctx.write_article("t", "a.md", "from t1")?;
                e1.wait(); // signal: locks are held
                r1.wait(); // wait: hold until the main thread checks
                Ok("edit a".to_string())
            })
            .expect("t1 submit");
        });

        // Wait until T1 holds its locks.
        enter.wait();

        // While T1 holds a, T2 (wants a+b+index) must not have grabbed b: b is
        // still completely free in the lock table (no partial acquisition).
        {
            let state = coord.state.lock().expect("state");
            assert!(state.held.contains_key(Path::new("t/a.md")), "T1 holds a");
            assert!(
                !state.held.contains_key(Path::new("t/b.md")),
                "b must be untouched: T2 holds no partial set"
            );
        }

        // Launch T2 wanting the intersecting superset {a, b, index}.
        let c2 = Arc::clone(&coord);
        let t2 = thread::spawn(move || {
            let req = TxnRequest::new(
                agent("s2"),
                LockSet::new()
                    .with(Path::new("t/a.md"))
                    .with(Path::new("t/b.md"))
                    .with(Path::new("t/index.json")),
                "a and b",
            );
            c2.submit(req, |ctx| {
                ctx.write_article("t", "a.md", "from t2")?;
                ctx.write_article("t", "b.md", "from t2")?;
                Ok("edit a and b".to_string())
            })
            .expect("t2 submit");
        });

        // Give T2 a moment to park, then confirm it still holds NOTHING (a is
        // held by T1, so all-or-nothing keeps b free too).
        thread::sleep(Duration::from_millis(50));
        {
            let state = coord.state.lock().expect("state");
            assert!(
                !state.held.contains_key(Path::new("t/b.md")),
                "T2 must not have partially acquired b while waiting for a"
            );
        }

        // Release T1; T2 can now take the whole set and finish.
        release.wait();
        t1.join().expect("t1");
        t2.join().expect("t2");

        // Both edits landed; b.md ends with T2's content.
        let body = coord
            .with_workspace(|ws| ws.read_article("t", "b.md"))
            .expect("read b");
        assert_eq!(body, "from t2");
    }

    #[test]
    fn human_waits_at_queue_head_but_does_not_preempt() {
        // A running agent transaction holds the lock. While it runs, a human and a
        // second agent both queue for the same lock. The human must run BEFORE the
        // waiting agent (queue-head priority) but only AFTER the running agent
        // commits (no preemption). We assert the completion order is: running
        // agent, then human, then the late agent.
        let (_d, coord) = coord_with(&["a.md"]);
        let coord = Arc::new(coord);

        let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let holding = Arc::new(Barrier::new(2)); // running agent signals it holds
        let queued = Arc::new(Barrier::new(3)); // human + late agent are enqueued
        let let_go = Arc::new(Barrier::new(2)); // release the running agent

        // The running agent: acquires first, holds until both contenders are
        // queued, records itself, then commits (releasing the lock).
        let c0 = Arc::clone(&coord);
        let o0 = Arc::clone(&order);
        let h0 = Arc::clone(&holding);
        let g0 = Arc::clone(&let_go);
        let runner = thread::spawn(move || {
            let req = TxnRequest::new(agent("running"), article_locks("t", "a.md"), "run");
            c0.submit(req, |ctx| {
                ctx.write_article("t", "a.md", "running")?;
                h0.wait(); // "I hold the lock"
                g0.wait(); // wait until contenders are queued + a beat
                o0.lock().unwrap().push("agent-running");
                Ok("edit running".to_string())
            })
            .expect("runner submit");
        });

        // Wait until the runner holds the lock before queueing the contenders.
        holding.wait();

        // Late agent: queues for the same lock.
        let c_a = Arc::clone(&coord);
        let o_a = Arc::clone(&order);
        let q_a = Arc::clone(&queued);
        let late_agent = thread::spawn(move || {
            // Ensure this agent enqueues *before* we release the runner.
            q_a.wait();
            let req = TxnRequest::new(agent("late"), article_locks("t", "a.md"), "late");
            c_a.submit(req, |ctx| {
                ctx.write_article("t", "a.md", "late")?;
                o_a.lock().unwrap().push("agent-late");
                Ok("edit late".to_string())
            })
            .expect("late agent submit");
        });

        // Human: queues for the same lock; must jump ahead of the late agent.
        let c_h = Arc::clone(&coord);
        let o_h = Arc::clone(&order);
        let q_h = Arc::clone(&queued);
        let human = thread::spawn(move || {
            q_h.wait();
            let req = TxnRequest::new(WriterId::Human, article_locks("t", "a.md"), "human");
            c_h.submit(req, |ctx| {
                ctx.write_article("t", "a.md", "human")?;
                o_h.lock().unwrap().push("human");
                Ok("edit human".to_string())
            })
            .expect("human submit");
        });

        // Release the queue barrier so both contenders call submit, then give them
        // a moment to actually park in the wait queue before releasing the runner.
        queued.wait();
        thread::sleep(Duration::from_millis(80));
        let_go.wait();

        runner.join().expect("runner");
        human.join().expect("human");
        late_agent.join().expect("late agent");

        let seen = order.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec!["agent-running", "human", "agent-late"],
            "running agent finishes first (no preemption), then the human \
             (queue-head priority), then the late agent"
        );
    }

    #[test]
    fn deadlock_stress_random_intersecting_sets_all_complete() {
        // N threads each run several transactions over random, intersecting lock
        // sets. All-or-nothing acquisition makes deadlock impossible, so every
        // thread must complete. A tiny xorshift PRNG keeps the test dependency-free
        // and deterministic per seed.
        const ARTICLES: [&str; 5] = ["a.md", "b.md", "c.md", "d.md", "e.md"];
        const THREADS: usize = 8;
        const TXNS_PER_THREAD: usize = 12;

        let (_d, coord) = coord_with(&ARTICLES);
        let coord = Arc::new(coord);

        let mut handles = Vec::new();
        for tid in 0..THREADS {
            let c = Arc::clone(&coord);
            handles.push(thread::spawn(move || {
                // Seed per thread so each picks a different stream of sets.
                let mut rng =
                    0x9E3779B97F4A7C15u64 ^ (tid as u64 + 1).wrapping_mul(0xD1B54A32D192ED03);
                let mut next = || {
                    // xorshift64
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    rng
                };
                for _ in 0..TXNS_PER_THREAD {
                    // Pick 1..=3 distinct articles for the set (always + index).
                    let count = 1 + (next() % 3) as usize;
                    let mut locks = LockSet::new().with(Path::new("t/index.json"));
                    let mut chosen = Vec::new();
                    for _ in 0..count {
                        let idx = (next() % ARTICLES.len() as u64) as usize;
                        let file = ARTICLES[idx];
                        if !chosen.contains(&file) {
                            chosen.push(file);
                            locks = locks.with(Path::new("t").join(file));
                        }
                    }
                    let writer = if next() % 5 == 0 {
                        WriterId::Human
                    } else {
                        agent(&format!("s{tid}"))
                    };
                    let req = TxnRequest::new(writer, locks, "stress");
                    c.submit(req, |ctx| {
                        for file in &chosen {
                            ctx.write_article("t", file, "stress write")?;
                        }
                        Ok("stress edit".to_string())
                    })
                    .expect("stress submit completes (no deadlock)");
                }
            }));
        }

        // Every thread completes — no deadlock, no stranded lock.
        for h in handles {
            h.join().expect("stress thread joined");
        }

        // The lock table is fully drained after all transactions complete.
        let state = coord.state.lock().expect("state");
        assert!(state.held.is_empty(), "no lock left held");
        assert!(state.queue.is_empty(), "no ticket left parked");
        assert!(!state.running, "coordinator idle");
    }

    // ----- G5: split / merge cross-file transactions ------------------------

    /// Counts the commits in the repository (length of the HEAD history walk).
    fn commit_count(coord: &Coordinator) -> usize {
        coord
            .with_vcs(|vcs| {
                // Any one path's history is per-file; instead count all commits by
                // walking from HEAD via the index manifest, which every structural
                // op touches. Fall back to 0 on an empty repo.
                Ok(vcs.history(Path::new("t/index.json")).map(|h| h.len())?)
            })
            .expect("history")
    }

    #[test]
    fn split_produces_one_commit_touching_source_each_new_file_and_index() {
        let (_d, coord) = coord_with(&["all.md"]);
        // Seed the source with content (commit 1).
        coord
            .submit(
                TxnRequest::new(agent("s1"), article_locks("t", "all.md"), "seed"),
                |ctx| {
                    ctx.write_article("t", "all.md", "intro\npart a\npart b\n")?;
                    Ok("seed".to_string())
                },
            )
            .expect("seed");

        let plan = SplitPlan {
            theme: "t".into(),
            source_file: "all.md".into(),
            source_content: "intro\n".into(),
            outputs: vec![
                NewArticle {
                    file_name: "a.md".into(),
                    title: "Part A".into(),
                    content: "part a\n".into(),
                    parent: Some("all.md".into()),
                },
                NewArticle {
                    file_name: "b.md".into(),
                    title: "Part B".into(),
                    content: "part b\n".into(),
                    parent: Some("all.md".into()),
                },
            ],
        };
        let outcome = coord
            .split_article(agent("s1"), plan)
            .expect("split succeeds");
        let sha = outcome.sha.clone().expect("split commits once");

        // The outcome's touched set is exactly source + the two new files + index.
        let touched: BTreeSet<String> = outcome
            .paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let expected: BTreeSet<String> = ["t/all.md", "t/a.md", "t/b.md", "t/index.json"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(touched, expected, "split touches exactly the declared set");

        // Every touched file's newest commit is the *same single* split commit.
        coord
            .with_vcs(|vcs| {
                for rel in ["t/all.md", "t/a.md", "t/b.md", "t/index.json"] {
                    let hist = vcs.history(Path::new(rel))?;
                    assert_eq!(hist[0].id, sha, "{rel} newest commit is the split commit");
                }
                // a.md and b.md are brand-new: their whole history is this one commit.
                assert_eq!(vcs.history(Path::new("t/a.md"))?.len(), 1);
                assert_eq!(vcs.history(Path::new("t/b.md"))?.len(), 1);
                Ok(())
            })
            .expect("histories");

        // The source was rewritten and the new files carry their content.
        coord
            .with_workspace(|ws| {
                assert_eq!(ws.read_article("t", "all.md")?, "intro\n");
                assert_eq!(ws.read_article("t", "a.md")?, "part a\n");
                assert_eq!(ws.read_article("t", "b.md")?, "part b\n");
                Ok(())
            })
            .expect("read split outputs");

        // The manifest reflects the new structure: the children are present, in
        // reading order after the source, and parented under it.
        let outline = coord
            .with_workspace(|ws| ws.article_outline("t"))
            .expect("outline");
        let files: Vec<&str> = outline.iter().map(|o| o.file.as_str()).collect();
        assert_eq!(files, vec!["all.md", "a.md", "b.md"]);
        assert_eq!(outline[1].parent.as_deref(), Some("all.md"));
        assert_eq!(outline[1].depth, 1);
        assert_eq!(outline[2].parent.as_deref(), Some("all.md"));
    }

    #[test]
    fn split_is_exactly_one_commit() {
        let (_d, coord) = coord_with(&["all.md"]);
        let before = commit_count(&coord);
        coord
            .split_article(
                agent("s1"),
                SplitPlan {
                    theme: "t".into(),
                    source_file: "all.md".into(),
                    source_content: "keep\n".into(),
                    outputs: vec![NewArticle {
                        file_name: "x.md".into(),
                        title: "X".into(),
                        content: "x\n".into(),
                        parent: None,
                    }],
                },
            )
            .expect("split");
        // Exactly one new commit, regardless of how many files it touched.
        assert_eq!(commit_count(&coord), before + 1, "split is a single commit");
    }

    #[test]
    fn merge_produces_one_commit_touching_sources_target_and_index() {
        let (_d, coord) = coord_with(&["a.md", "b.md"]);
        // Seed the two sources (one txn each).
        for (file, body) in [("a.md", "body a\n"), ("b.md", "body b\n")] {
            coord
                .submit(
                    TxnRequest::new(agent("s1"), article_locks("t", file), "seed"),
                    |ctx| {
                        ctx.write_article("t", file, body)?;
                        Ok("seed".to_string())
                    },
                )
                .expect("seed");
        }
        let before = commit_count(&coord);

        let plan = MergePlan {
            theme: "t".into(),
            sources: vec!["a.md".into(), "b.md".into()],
            target: NewArticle {
                file_name: "merged.md".into(),
                title: "Merged".into(),
                content: "body a\nbody b\n".into(),
                parent: None,
            },
        };
        let outcome = coord
            .merge_articles(agent("s1"), plan)
            .expect("merge succeeds");
        let sha = outcome.sha.clone().expect("merge commits once");

        // Exactly one new commit.
        assert_eq!(commit_count(&coord), before + 1, "merge is a single commit");

        // The touched set is the two sources + the target + the manifest.
        let touched: BTreeSet<String> = outcome
            .paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let expected: BTreeSet<String> = ["t/a.md", "t/b.md", "t/merged.md", "t/index.json"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(touched, expected, "merge touches exactly the declared set");

        // The target's newest commit and the index share the one merge commit.
        coord
            .with_vcs(|vcs| {
                assert_eq!(vcs.history(Path::new("t/merged.md"))?[0].id, sha);
                assert_eq!(vcs.history(Path::new("t/index.json"))?[0].id, sha);
                Ok(())
            })
            .expect("histories");

        // The manifest reflects the new structure: sources gone, target present.
        let files = coord.with_workspace(|ws| ws.list_articles("t")).unwrap();
        assert_eq!(files, vec!["merged.md"]);
        assert_eq!(
            coord
                .with_workspace(|ws| ws.read_article("t", "merged.md"))
                .unwrap(),
            "body a\nbody b\n"
        );
    }

    #[test]
    fn split_rejects_a_malformed_plan() {
        let (_d, coord) = coord_with(&["all.md", "exists.md"]);
        // Empty outputs.
        assert!(matches!(
            coord.split_article(
                agent("s1"),
                SplitPlan {
                    theme: "t".into(),
                    source_file: "all.md".into(),
                    source_content: "x".into(),
                    outputs: vec![],
                },
            ),
            Err(CoordError::Aborted(_))
        ));
        // Output collides with the source.
        assert!(matches!(
            coord.split_article(
                agent("s1"),
                SplitPlan {
                    theme: "t".into(),
                    source_file: "all.md".into(),
                    source_content: "x".into(),
                    outputs: vec![NewArticle {
                        file_name: "all.md".into(),
                        title: "dup".into(),
                        content: "y".into(),
                        parent: None,
                    }],
                },
            ),
            Err(CoordError::Aborted(_))
        ));
        // Output already exists -> workspace error from create.
        assert!(matches!(
            coord.split_article(
                agent("s1"),
                SplitPlan {
                    theme: "t".into(),
                    source_file: "all.md".into(),
                    source_content: "x".into(),
                    outputs: vec![NewArticle {
                        file_name: "exists.md".into(),
                        title: "E".into(),
                        content: "y".into(),
                        parent: None,
                    }],
                },
            ),
            Err(CoordError::Workspace(_))
        ));
    }

    #[test]
    fn merge_rejects_a_malformed_plan() {
        let (_d, coord) = coord_with(&["a.md", "b.md", "keep.md"]);
        // Fewer than two sources.
        assert!(matches!(
            coord.merge_articles(
                agent("s1"),
                MergePlan {
                    theme: "t".into(),
                    sources: vec!["a.md".into()],
                    target: NewArticle {
                        file_name: "m.md".into(),
                        title: "M".into(),
                        content: "x".into(),
                        parent: None,
                    },
                },
            ),
            Err(CoordError::Aborted(_))
        ));
        // Target collides with a surviving article (keep.md is not a source).
        assert!(matches!(
            coord.merge_articles(
                agent("s1"),
                MergePlan {
                    theme: "t".into(),
                    sources: vec!["a.md".into(), "b.md".into()],
                    target: NewArticle {
                        file_name: "keep.md".into(),
                        title: "K".into(),
                        content: "x".into(),
                        parent: None,
                    },
                },
            ),
            Err(CoordError::Aborted(_))
        ));
    }

    #[test]
    fn declaring_a_short_output_list_is_rejected() {
        // The declared-lock rule (mechanism 6.A): a transaction body that produces
        // an output the caller did NOT declare aborts with `Undeclared` and makes
        // no commit. We drive `submit` directly with a deliberately short lock set
        // (source + ONE output + index) while the body tries to create TWO outputs.
        let (_d, coord) = coord_with(&["all.md"]);
        let locks = LockSet::new()
            .with(Path::new("t/all.md"))
            .with(Path::new("t/a.md")) // only a.md declared
            .with(Path::new("t/index.json"));
        let req = TxnRequest::new(agent("s1"), locks, "short split");
        let err = coord
            .submit(req, |ctx| {
                ctx.write_article("t", "all.md", "intro\n")?;
                ctx.create_article("t", "a.md", "a\n", "A", None)?;
                // b.md was never declared: this must abort the whole transaction.
                ctx.create_article("t", "b.md", "b\n", "B", None)?;
                Ok("should not reach".to_string())
            })
            .expect_err("undeclared output must be rejected");
        assert!(
            matches!(&err, CoordError::Undeclared(p) if p.ends_with("b.md")),
            "got {err:?}"
        );

        // No commit landed for the undeclared file.
        coord
            .with_vcs(|vcs| {
                assert!(vcs.history(Path::new("t/b.md"))?.is_empty());
                Ok(())
            })
            .expect("history");
    }

    #[test]
    fn merge_reusing_a_source_name_as_target_is_allowed() {
        // The target may reuse a consumed source's file name: the source is deleted
        // first, freeing the name, then the target is created under it.
        let (_d, coord) = coord_with(&["a.md", "b.md"]);
        let outcome = coord
            .merge_articles(
                agent("s1"),
                MergePlan {
                    theme: "t".into(),
                    sources: vec!["a.md".into(), "b.md".into()],
                    target: NewArticle {
                        file_name: "a.md".into(), // reuse a source name
                        title: "Merged into A".into(),
                        content: "merged\n".into(),
                        parent: None,
                    },
                },
            )
            .expect("merge into a reused source name");
        assert!(outcome.sha.is_some());
        let files = coord.with_workspace(|ws| ws.list_articles("t")).unwrap();
        assert_eq!(files, vec!["a.md"]);
        assert_eq!(
            coord
                .with_workspace(|ws| ws.read_article("t", "a.md"))
                .unwrap(),
            "merged\n"
        );
    }

    // ----- B2: authorship persists through the TxnCtx write path ------------

    #[test]
    fn txn_write_path_persists_char_level_authorship() {
        // The coordinator's TxnCtx::write_article (the path human PUT and the agent
        // editors both bottom out in) must record character-level authorship: a
        // human writes the body, then an agent rewrites one word, and the untouched
        // text stays the human's.
        let (_d, coord) = coord_with(&["a.md"]);

        let req = TxnRequest::new(WriterId::Human, article_locks("t", "a.md"), "human write");
        coord
            .submit(req, |ctx| {
                ctx.write_article("t", "a.md", "the quick brown fox")?;
                Ok("human edit".to_string())
            })
            .expect("human submit");

        let req = TxnRequest::new(agent("s1"), article_locks("t", "a.md"), "agent write");
        coord
            .submit(req, |ctx| {
                ctx.write_article("t", "a.md", "the quick red fox")?;
                Ok("agent edit".to_string())
            })
            .expect("agent submit");

        // Plain read is the joined latest text.
        assert_eq!(
            coord
                .with_workspace(|ws| ws.read_article("t", "a.md"))
                .unwrap(),
            "the quick red fox"
        );

        // Char-level authorship blends the two writers.
        let doc = coord
            .with_workspace(|ws| ws.read_document("t", "a.md"))
            .unwrap();
        assert_eq!(doc.to_plain_string(), "the quick red fox");
        let tags: Vec<(String, String)> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                crate::content::Block::Paragraph(t) => Some(t),
                _ => None,
            })
            .flat_map(|t| {
                t.runs
                    .iter()
                    .map(|r| (r.text.clone(), r.author.tag()))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            tags,
            vec![
                ("the quick ".to_string(), "human".to_string()),
                ("red".to_string(), "deepseek-v4-pro/s1".to_string()),
                (" fox".to_string(), "human".to_string()),
            ]
        );
    }

    // ----- B3: coordinator observability events ------------------------------

    #[test]
    fn submit_emits_acquired_then_released_in_order() {
        // An uncontended submit emits exactly TxnAcquired then TxnReleased (no
        // queueing, no handoff), carrying the writer tag and the held paths.
        let (_d, coord, rec) = coord_with_sink(&["a.md"]);
        coord
            .submit(
                TxnRequest::new(agent("s1"), article_locks("t", "a.md"), "write"),
                |ctx| {
                    ctx.write_article("t", "a.md", "body")?;
                    Ok("edit".to_string())
                },
            )
            .expect("submit");

        assert_eq!(rec.kinds(), ["TxnAcquired", "TxnReleased"]);
        match &rec.events()[0] {
            Event::TxnAcquired { writer, paths } => {
                assert_eq!(writer, "deepseek-v4-pro/s1");
                assert_eq!(paths, &["t/a.md".to_string(), "t/index.json".to_string()]);
            }
            other => panic!("expected TxnAcquired, got {other:?}"),
        }
        match &rec.events()[1] {
            Event::TxnReleased { writer } => assert_eq!(writer, "deepseek-v4-pro/s1"),
            other => panic!("expected TxnReleased, got {other:?}"),
        }
    }

    #[test]
    fn a_queued_writer_emits_txn_queued_with_ahead_count() {
        // While an agent holds the lock, a second agent that submits must park and
        // emit TxnQueued before it eventually acquires. The full event order across
        // both transactions is: T1 acquires, T2 queues (ahead=0, nothing else in
        // the queue), T1 releases, T2 acquires, T2 releases.
        let (_d, coord, rec) = coord_with_sink(&["a.md"]);
        let coord = Arc::new(coord);

        let holding = Arc::new(Barrier::new(2));
        let queued = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));

        let c1 = Arc::clone(&coord);
        let h1 = Arc::clone(&holding);
        let r1 = Arc::clone(&release);
        let t1 = thread::spawn(move || {
            c1.submit(
                TxnRequest::new(agent("s1"), article_locks("t", "a.md"), "hold"),
                |ctx| {
                    ctx.write_article("t", "a.md", "t1")?;
                    h1.wait(); // lock held
                    r1.wait(); // hold until T2 has parked
                    Ok("edit t1".to_string())
                },
            )
            .expect("t1");
        });

        holding.wait();

        let c2 = Arc::clone(&coord);
        let q2 = Arc::clone(&queued);
        let t2 = thread::spawn(move || {
            q2.wait();
            c2.submit(
                TxnRequest::new(agent("s2"), article_locks("t", "a.md"), "wait"),
                |ctx| {
                    ctx.write_article("t", "a.md", "t2")?;
                    Ok("edit t2".to_string())
                },
            )
            .expect("t2");
        });

        // Let T2 call submit, give it a beat to park, then release T1.
        queued.wait();
        thread::sleep(Duration::from_millis(60));
        release.wait();
        t1.join().expect("t1 join");
        t2.join().expect("t2 join");

        assert_eq!(
            rec.kinds(),
            [
                "TxnAcquired", // T1 enters
                "TxnQueued",   // T2 parks behind T1
                "TxnReleased", // T1 commits
                "TxnAcquired", // T2 enters
                "TxnReleased", // T2 commits
            ]
        );
        // The queued agent reported nothing ahead of it in the wait queue itself
        // (T1 was running, not queued).
        let queued_ev = rec
            .events()
            .into_iter()
            .find(|e| matches!(e, Event::TxnQueued { .. }))
            .expect("a TxnQueued event");
        match queued_ev {
            Event::TxnQueued { writer, ahead } => {
                assert_eq!(writer, "deepseek-v4-pro/s2");
                assert_eq!(ahead, 0);
            }
            other => panic!("expected TxnQueued, got {other:?}"),
        }
    }

    #[test]
    fn request_edit_on_idle_coordinator_is_up_now_and_emits_handoff() {
        // With nothing running, a request-edit reserves the human's slot at the
        // head, reports ahead=0, and immediately announces the handoff.
        let (_d, coord, rec) = coord_with_sink(&["a.md"]);
        let out = coord.request_edit("t", "a.md");
        assert!(out.queued);
        assert_eq!(out.ahead, 0, "idle coordinator: human is up immediately");
        assert_eq!(rec.kinds(), ["HandoffToHuman"]);
        match &rec.events()[0] {
            Event::HandoffToHuman { theme, file } => {
                assert_eq!(theme, "t");
                assert_eq!(file, "a.md");
            }
            other => panic!("expected HandoffToHuman, got {other:?}"),
        }
    }

    #[test]
    fn request_edit_queues_human_at_head_ahead_of_a_waiting_agent() {
        // A running agent holds the lock. A human request-edit and a second agent
        // submit both arrive while it runs. The reservation must make the waiting
        // agent yield: when the running agent releases, the human is handed off
        // (HandoffToHuman) and only then does the late agent get to run. We assert
        // the late agent does NOT acquire before the human's handoff fires.
        let (_d, coord, rec) = coord_with_sink(&["a.md"]);
        let coord = Arc::new(coord);

        let holding = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));

        let c1 = Arc::clone(&coord);
        let h1 = Arc::clone(&holding);
        let r1 = Arc::clone(&release);
        let runner = thread::spawn(move || {
            c1.submit(
                TxnRequest::new(agent("running"), article_locks("t", "a.md"), "run"),
                |ctx| {
                    ctx.write_article("t", "a.md", "running")?;
                    h1.wait();
                    r1.wait();
                    Ok("edit running".to_string())
                },
            )
            .expect("runner");
        });

        holding.wait();

        // A request-edit lands while the agent runs: ahead=1 (one running txn),
        // queued, no immediate handoff (the agent is still in the section).
        let out = coord.request_edit("t", "a.md");
        assert!(out.queued);
        assert_eq!(
            out.ahead, 1,
            "one running transaction is ahead of the human"
        );

        // A late agent submits for the same lock; it must NOT jump ahead of the
        // pending human reservation.
        let c_a = Arc::clone(&coord);
        let late = thread::spawn(move || {
            c_a.submit(
                TxnRequest::new(agent("late"), article_locks("t", "a.md"), "late"),
                |ctx| {
                    ctx.write_article("t", "a.md", "late")?;
                    Ok("edit late".to_string())
                },
            )
            .expect("late");
        });

        // Give the late agent time to park behind the reservation, then release the
        // runner. The human reservation, not the late agent, gets the handoff.
        thread::sleep(Duration::from_millis(60));
        release.wait();
        runner.join().expect("runner join");

        // The human's actual edit now arrives and consumes the reservation; this
        // must run before the late agent (the reservation held the agent back).
        coord
            .submit(
                TxnRequest::new(WriterId::Human, article_locks("t", "a.md"), "human"),
                |ctx| {
                    ctx.write_article("t", "a.md", "human")?;
                    Ok("edit human".to_string())
                },
            )
            .expect("human edit");
        late.join().expect("late join");

        // The handoff for the human must appear before the late agent ever
        // acquires. Find the first HandoffToHuman and assert no late-agent
        // TxnAcquired precedes it.
        let events = rec.events();
        let handoff_idx = events
            .iter()
            .position(|e| matches!(e, Event::HandoffToHuman { .. }))
            .expect("a handoff to the human");
        let late_acquire_idx = events.iter().position(
            |e| matches!(e, Event::TxnAcquired { writer, .. } if writer == "deepseek-v4-pro/late"),
        );
        if let Some(idx) = late_acquire_idx {
            assert!(
                handoff_idx < idx,
                "human handoff must precede the late agent acquiring; events: {:?}",
                rec.kinds()
            );
        }
    }

    #[test]
    fn cancel_request_edit_removes_a_pending_reservation() {
        // request_edit then cancel: the first cancel removes it (true), a second is
        // a no-op (false). With the reservation gone, an agent submit proceeds
        // without being held back.
        let (_d, coord, _rec) = coord_with_sink(&["a.md"]);
        // Hold a running txn so the reservation does not immediately resolve; we
        // only want to prove cancel removes the standing reservation.
        let out = coord.request_edit("t", "a.md");
        assert!(out.queued);
        assert!(
            coord.cancel_request_edit("t", "a.md"),
            "the pending reservation is cancelled"
        );
        assert!(
            !coord.cancel_request_edit("t", "a.md"),
            "a second cancel is a no-op"
        );

        // With no reservation pending, an agent edit proceeds normally.
        coord
            .submit(
                TxnRequest::new(agent("s1"), article_locks("t", "a.md"), "after cancel"),
                |ctx| {
                    ctx.write_article("t", "a.md", "agent after cancel")?;
                    Ok("edit".to_string())
                },
            )
            .expect("agent proceeds after cancel");
        assert_eq!(
            coord
                .with_workspace(|ws| ws.read_article("t", "a.md"))
                .unwrap(),
            "agent after cancel"
        );
    }

    #[test]
    fn human_submit_consuming_a_reservation_does_not_double_announce_handoff() {
        // request_edit on an idle coordinator emits one handoff (up now). When the
        // human's actual edit then arrives and consumes that reservation, it must
        // not emit a SECOND handoff. Exactly one HandoffToHuman across both steps.
        let (_d, coord, rec) = coord_with_sink(&["a.md"]);
        coord.request_edit("t", "a.md");
        coord
            .submit(
                TxnRequest::new(WriterId::Human, article_locks("t", "a.md"), "human"),
                |ctx| {
                    ctx.write_article("t", "a.md", "human body")?;
                    Ok("edit human".to_string())
                },
            )
            .expect("human edit");

        let handoffs = rec
            .kinds()
            .into_iter()
            .filter(|k| *k == "HandoffToHuman")
            .count();
        assert_eq!(
            handoffs,
            1,
            "exactly one handoff, not doubled; {:?}",
            rec.kinds()
        );
    }

    #[test]
    fn undo_article_emits_lifecycle_and_routes_through_the_lock() {
        // undo_article holds the article lock for the revert and emits the same
        // acquire/release lifecycle as submit. Seed two versions so there is an
        // edit to undo.
        let (_d, coord, rec) = coord_with_sink(&["a.md"]);
        for body in ["v1", "v2"] {
            coord
                .submit(
                    TxnRequest::new(WriterId::Human, article_locks("t", "a.md"), "seed"),
                    |ctx| {
                        ctx.write_article("t", "a.md", body)?;
                        Ok(format!("edit {body}"))
                    },
                )
                .expect("seed");
        }
        // Drain the seed events so we only inspect the undo's.
        rec.0.lock().expect("not poisoned").clear();

        let undone = coord
            .undo_article(WriterId::Human, "t", "a.md")
            .expect("undo ok");
        assert!(undone.is_some(), "an edit was reverted");
        assert_eq!(rec.kinds(), ["TxnAcquired", "TxnReleased"]);
        // The revert restored the previous version's content.
        assert_eq!(
            coord
                .with_workspace(|ws| ws.read_article("t", "a.md"))
                .unwrap(),
            "v1"
        );
    }
}
