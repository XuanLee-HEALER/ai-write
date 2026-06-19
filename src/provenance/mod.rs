//! Character-level provenance over [`content::RichText`](crate::content::RichText):
//! the author-aware edit primitive, authorship queries, contributor aggregation,
//! and author-attributed diff.
//!
//! Implemented in the `feat/provenance` worktree; see `docs/impl-v2.md` §3.
//!
//! # The one edit primitive
//!
//! All mutation flows through a single function, [`apply_edit`]: it takes the
//! text to change, an [`Edit`] describing *what* changes (insert / delete /
//! replace, in **byte offsets**), and the [`AuthorId`] of *who* is making the
//! change. The primitive splits and re-tags [`Run`]s so
//! that newly written text carries `author` while untouched text keeps the
//! author who originally wrote it, then [normalizes](RichText) the result (no
//! empty runs; adjacent same-author runs coalesced). Both human and agent edits
//! go through this same path, so authorship stays self-consistent regardless of
//! who is writing.
//!
//! # Offsets are bytes
//!
//! Every offset in this module is a **byte** index into the run-concatenated
//! plain text ([`RichText::plain_string`]). Byte offsets are what the content
//! model already exposes ([`RichText::len`] is a byte count) and what callers
//! such as the editing tools speak. An offset that does not fall on a UTF-8
//! character boundary, or that runs past the end of the text, is rejected with a
//! [`ProvenanceError`] rather than silently truncated — this is the same
//! invariant `str` slicing enforces, surfaced as a typed error.
//!
//! # Examples
//!
//! ```
//! use ai_write::content::{AuthorId, RichText};
//! use ai_write::provenance::{apply_edit, contributors_of, Edit};
//!
//! let agent = AuthorId::Agent { model: "m".into(), label: "a".into() };
//! let mut text = RichText::from_plain("hello world", AuthorId::Human);
//!
//! // The agent replaces "world" with "there".
//! apply_edit(&mut text, Edit::replace(6..11, "there"), &agent).unwrap();
//! assert_eq!(text.plain_string(), "hello there");
//!
//! // "hello " is still the human's; "there" is the agent's.
//! let who: Vec<_> = contributors_of(&text).into_iter().cloned().collect();
//! assert_eq!(who, vec![AuthorId::Human, agent]);
//! ```

use std::ops::Range;

use crate::content::{AuthorId, Block, Document, RichText, Run};

/// Errors raised by the provenance layer.
///
/// The layer performs no IO, so every error is an *input* error: an offset that
/// is out of range or not on a UTF-8 character boundary, or a range whose ends
/// are inverted. Each variant carries the offending offsets to make the failure
/// actionable for the caller.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProvenanceError {
    /// A byte offset pointed past the end of the text.
    ///
    /// `offset` is the requested index and `len` is the text's byte length;
    /// valid offsets are `0..=len`.
    #[error("byte offset {offset} is out of bounds (text is {len} bytes)")]
    OffsetOutOfBounds {
        /// The requested byte offset.
        offset: usize,
        /// The text's total length in bytes.
        len: usize,
    },

    /// A byte offset landed inside a multi-byte UTF-8 character.
    ///
    /// Splitting a run here would corrupt the encoding, so the edit is rejected.
    #[error("byte offset {offset} is not on a UTF-8 character boundary")]
    NotCharBoundary {
        /// The offending byte offset.
        offset: usize,
    },

    /// A range had `start > end`.
    #[error("invalid range: start {start} is greater than end {end}")]
    InvalidRange {
        /// The range start.
        start: usize,
        /// The range end.
        end: usize,
    },
}

/// Convenience alias for `Result<T, `[`ProvenanceError`]`>`.
pub type Result<T> = std::result::Result<T, ProvenanceError>;

/// A single author-attributed edit, expressed in **byte offsets** into the
/// run-concatenated text.
///
/// This is the input to the one edit primitive, [`apply_edit`]. The three shapes
/// cover every text mutation:
///
/// - [`Edit::Insert`] adds `text` at a point, shifting everything after it.
/// - [`Edit::Delete`] removes a byte range, keeping the surrounding text and its
///   authorship.
/// - [`Edit::Replace`] is a delete followed by an insert at the same point; it
///   exists as its own variant so a "rewrite this span" operation is one edit
///   (and one normalization) rather than two.
///
/// Use the constructors ([`Edit::insert`], [`Edit::delete`], [`Edit::replace`])
/// for brevity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Insert `text` at byte offset `at`.
    Insert {
        /// Byte offset at which to insert; must be `0..=len` and on a char
        /// boundary.
        at: usize,
        /// The text to insert, attributed to the edit's author.
        text: String,
    },
    /// Delete the bytes in `range`.
    Delete {
        /// Half-open byte range to remove; both ends must be on char
        /// boundaries and within `0..=len`.
        range: Range<usize>,
    },
    /// Replace the bytes in `range` with `new_text`.
    Replace {
        /// Half-open byte range to overwrite.
        range: Range<usize>,
        /// The replacement text, attributed to the edit's author.
        new_text: String,
    },
}

impl Edit {
    /// An [`Edit::Insert`] of `text` at byte offset `at`.
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Edit::Insert {
            at,
            text: text.into(),
        }
    }

    /// An [`Edit::Delete`] of `range`.
    pub fn delete(range: Range<usize>) -> Self {
        Edit::Delete { range }
    }

    /// An [`Edit::Replace`] of `range` with `new_text`.
    pub fn replace(range: Range<usize>, new_text: impl Into<String>) -> Self {
        Edit::Replace {
            range,
            new_text: new_text.into(),
        }
    }
}

/// Applies `edit` to `text`, attributing all newly written characters to
/// `author` while leaving the authorship of untouched characters intact.
///
/// This is the **single edit primitive** of the provenance layer: insert,
/// delete and replace all flow through it, so character-level authorship stays
/// consistent no matter who edits. The runs are split at the edit's boundaries,
/// the affected span is removed and/or replaced with a run owned by `author`,
/// and the result is normalized ([`normalize`]): no empty runs survive, and
/// adjacent runs with the same author are coalesced.
///
/// On success `text` is updated in place. On error `text` is left **unchanged**,
/// so a rejected edit never leaves the value half-mutated.
///
/// # Errors
///
/// Returns [`ProvenanceError::OffsetOutOfBounds`] if an offset exceeds the text
/// length, [`ProvenanceError::NotCharBoundary`] if an offset splits a multi-byte
/// character, or [`ProvenanceError::InvalidRange`] if a range has `start > end`.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, RichText};
/// use ai_write::provenance::{apply_edit, Edit};
///
/// let agent = AuthorId::Agent { model: "m".into(), label: "a".into() };
/// let mut text = RichText::from_plain("abcdef", AuthorId::Human);
///
/// apply_edit(&mut text, Edit::insert(3, "XYZ"), &agent).unwrap();
/// assert_eq!(text.plain_string(), "abcXYZdef");
/// assert_eq!(text.runs.len(), 3); // human | agent | human
/// ```
pub fn apply_edit(text: &mut RichText, edit: Edit, author: &AuthorId) -> Result<()> {
    let len = text.len();
    // Validate first so a rejected edit leaves `text` untouched.
    let new_runs = match edit {
        Edit::Insert { at, text: ins } => {
            check_boundary(text, at, len)?;
            splice(text, at..at, ins, author)
        }
        Edit::Delete { range } => {
            check_range(text, &range, len)?;
            splice(text, range, String::new(), author)
        }
        Edit::Replace { range, new_text } => {
            check_range(text, &range, len)?;
            splice(text, range, new_text, author)
        }
    };
    text.runs = new_runs;
    normalize(text);
    Ok(())
}

/// Re-authors `text` so its plain string becomes `new_text`, attributing **only
/// the changed span** to `author` and preserving the original authorship of every
/// character that is unchanged.
///
/// This is the authorship-preserving "the editor handed me a whole new body"
/// primitive the workspace's [`apply_edit`]-routed write path
/// (`docs/impl-v2-results.md` §5) builds on: the three byte-level editing tools
/// (`write_article` / `edit_article` / `apply_edits`) and the human `PUT` all
/// compute a full new body, but most of it is usually unchanged. Rather than
/// blaming the entire body on the latest writer, this trims the longest common
/// prefix and suffix (on UTF-8 character boundaries) shared by the old and new
/// text and replaces just the differing middle through the single edit primitive
/// [`apply_edit`]. The untouched prefix and suffix keep whoever wrote them; the
/// rewritten middle is attributed to `author`. When `new_text` equals the current
/// plain string the call is a no-op (no authorship changes).
///
/// The reconciliation is a contiguous-span replace, not a full longest-common-
/// subsequence realignment: a single edit appears as one changed region, which is
/// exactly how a human or agent edit looks and keeps the cost linear. (Character-
/// level realignment across scattered edits is [`diff`]'s job, for visualization,
/// not persistence.)
///
/// On success `text` is updated in place to carry the new content and its blended
/// authorship.
///
/// # Errors
///
/// Returns a [`ProvenanceError`] only if the internal span replace does — which,
/// because the prefix/suffix are computed on character boundaries of both texts,
/// does not happen for valid UTF-8 inputs; the signature returns [`Result`] so the
/// guarantee is enforced rather than assumed.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, RichText};
/// use ai_write::provenance::{contributors_of, reauthor};
///
/// let agent = AuthorId::Agent { model: "m".into(), label: "a".into() };
/// // The human wrote the whole sentence.
/// let mut body = RichText::from_plain("the quick brown fox", AuthorId::Human);
///
/// // The agent rewrites just the middle word.
/// reauthor(&mut body, "the quick red fox", &agent).unwrap();
/// assert_eq!(body.plain_string(), "the quick red fox");
///
/// // "the quick " and " fox" stay the human's; "red" is the agent's.
/// let who: Vec<_> = contributors_of(&body).into_iter().cloned().collect();
/// assert_eq!(who, vec![AuthorId::Human, agent]);
/// ```
pub fn reauthor(text: &mut RichText, new_text: &str, author: &AuthorId) -> Result<()> {
    let old: String = text.plain_string();
    if old == new_text {
        return Ok(());
    }
    let old_bytes = old.as_bytes();
    let new_bytes = new_text.as_bytes();

    // Longest common byte prefix, then backed off to a char boundary of both.
    let max_prefix = old_bytes.len().min(new_bytes.len());
    let mut prefix = 0;
    while prefix < max_prefix && old_bytes[prefix] == new_bytes[prefix] {
        prefix += 1;
    }
    while prefix > 0 && (!old.is_char_boundary(prefix) || !new_text.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    // Longest common byte suffix that does not overlap the prefix, backed off to a
    // char boundary of both texts.
    let max_suffix = (old_bytes.len() - prefix).min(new_bytes.len() - prefix);
    let mut suffix = 0;
    while suffix < max_suffix
        && old_bytes[old_bytes.len() - 1 - suffix] == new_bytes[new_bytes.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut old_suffix_start = old_bytes.len() - suffix;
    let mut new_suffix_start = new_bytes.len() - suffix;
    while suffix > 0
        && (!old.is_char_boundary(old_suffix_start) || !new_text.is_char_boundary(new_suffix_start))
    {
        suffix -= 1;
        old_suffix_start = old_bytes.len() - suffix;
        new_suffix_start = new_bytes.len() - suffix;
    }

    // Replace the differing middle [prefix, old_len - suffix) of the old text with
    // the differing middle of the new text, attributed to `author`.
    let replacement = new_text[prefix..new_suffix_start].to_string();
    apply_edit(
        text,
        Edit::replace(prefix..old_suffix_start, replacement),
        author,
    )
}

/// Splices `replacement` (authored by `author`) into the byte `range` of `text`,
/// returning the new (not-yet-normalized) run list.
///
/// Runs entirely before `range.start` and entirely after `range.end` are copied
/// verbatim; a run straddling either boundary is split so the kept side retains
/// its original author. `replacement` becomes a single run owned by `author`
/// (empty replacement adds no run — that is the delete case).
fn splice(
    text: &RichText,
    range: Range<usize>,
    replacement: String,
    author: &AuthorId,
) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::with_capacity(text.runs.len() + 2);
    let mut cursor = 0usize; // byte offset of the current run's start

    for run in &text.runs {
        let run_start = cursor;
        let run_end = cursor + run.text.len();
        cursor = run_end;

        // The portion of this run before the deleted range (kept, same author).
        if run_start < range.start {
            let keep_end = run_end.min(range.start);
            if keep_end > run_start {
                let lo = 0;
                let hi = keep_end - run_start;
                out.push(Run::new(run.text[lo..hi].to_string(), run.author.clone()));
            }
        }

        // The portion of this run after the deleted range (kept, same author).
        if run_end > range.end {
            let keep_start = run_start.max(range.end);
            if keep_start < run_end {
                let lo = keep_start - run_start;
                let hi = run.text.len();
                out.push(Run::new(run.text[lo..hi].to_string(), run.author.clone()));
            }
        }
    }

    // Insert the replacement at the point where the range began. We find that
    // point by re-walking: everything emitted so far whose cumulative length is
    // < range.start belongs before the insertion. Simpler: rebuild with the
    // replacement spliced in at range.start.
    if replacement.is_empty() {
        return out;
    }
    splice_in_replacement(out, range.start, replacement, author)
}

/// Inserts a single `author`-owned run carrying `replacement` at byte offset
/// `at` within the already-split run list `runs`.
///
/// `at` is guaranteed (by the caller) to fall on a run boundary in `runs`,
/// because [`splice`] already cut every run at `range.start`.
fn splice_in_replacement(
    runs: Vec<Run>,
    at: usize,
    replacement: String,
    author: &AuthorId,
) -> Vec<Run> {
    let mut out = Vec::with_capacity(runs.len() + 1);
    let mut cursor = 0usize;
    let mut inserted = false;
    for run in runs {
        if !inserted && cursor >= at {
            out.push(Run::new(replacement.clone(), author.clone()));
            inserted = true;
        }
        cursor += run.text.len();
        out.push(run);
    }
    if !inserted {
        // `at` is at or past the end: append.
        out.push(Run::new(replacement, author.clone()));
    }
    out
}

/// Normalizes `text` in place: drops empty runs and coalesces adjacent runs that
/// share an author into a single run.
///
/// A normalized [`RichText`] is the canonical form the content model documents:
/// it contains no empty runs and no two neighbouring runs with equal authors.
/// [`apply_edit`] calls this after every edit, so callers rarely need it
/// directly; it is public for tests and for callers assembling a [`RichText`] by
/// hand.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, RichText, Run};
/// use ai_write::provenance::normalize;
///
/// let mut text = RichText {
///     runs: vec![
///         Run::new("ab", AuthorId::Human),
///         Run::new("", AuthorId::Human),
///         Run::new("cd", AuthorId::Human),
///     ],
/// };
/// normalize(&mut text);
/// assert_eq!(text.runs.len(), 1);
/// assert_eq!(text.plain_string(), "abcd");
/// ```
pub fn normalize(text: &mut RichText) {
    let mut out: Vec<Run> = Vec::with_capacity(text.runs.len());
    for run in text.runs.drain(..) {
        if run.text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.author == run.author => last.text.push_str(&run.text),
            _ => out.push(run),
        }
    }
    text.runs = out;
}

/// Returns the [`AuthorId`] of the character at byte offset `at`, or `None` if
/// `at` is at the very end of the text (no character there).
///
/// # Errors
///
/// Returns [`ProvenanceError::OffsetOutOfBounds`] if `at` exceeds the text
/// length, or [`ProvenanceError::NotCharBoundary`] if `at` is not on a UTF-8
/// boundary.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, RichText};
/// use ai_write::provenance::author_at;
///
/// let text = RichText::from_plain("hi", AuthorId::Human);
/// assert_eq!(author_at(&text, 0).unwrap(), Some(&AuthorId::Human));
/// assert_eq!(author_at(&text, 2).unwrap(), None); // end of text
/// ```
pub fn author_at(text: &RichText, at: usize) -> Result<Option<&AuthorId>> {
    let len = text.len();
    check_boundary(text, at, len)?;
    if at == len {
        return Ok(None);
    }
    let mut cursor = 0usize;
    for run in &text.runs {
        let run_end = cursor + run.text.len();
        if at < run_end {
            return Ok(Some(&run.author));
        }
        cursor = run_end;
    }
    // Unreachable for `at < len`, but stay total.
    Ok(None)
}

/// Returns the authors covering byte `range`, in reading order, each paired with
/// the sub-range of `range` they cover.
///
/// The returned spans tile `range` exactly: they are contiguous, non-empty, and
/// their union is `range`. Adjacent spans always have distinct authors (the
/// query reads a normalized run list). An empty `range` yields an empty vector.
///
/// # Errors
///
/// Returns [`ProvenanceError::InvalidRange`] if `start > end`,
/// [`ProvenanceError::OffsetOutOfBounds`] if an end exceeds the text length, or
/// [`ProvenanceError::NotCharBoundary`] if an end splits a character.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, RichText, Run};
/// use ai_write::provenance::authors_in_range;
///
/// let agent = AuthorId::Agent { model: "m".into(), label: "a".into() };
/// let text = RichText {
///     runs: vec![Run::new("abc", AuthorId::Human), Run::new("def", agent.clone())],
/// };
/// let spans = authors_in_range(&text, 2..5).unwrap();
/// assert_eq!(spans, vec![(2..3, &AuthorId::Human), (3..5, &agent)]);
/// ```
pub fn authors_in_range(
    text: &RichText,
    range: Range<usize>,
) -> Result<Vec<(Range<usize>, &AuthorId)>> {
    let len = text.len();
    check_range(text, &range, len)?;
    let mut spans = Vec::new();
    if range.is_empty() {
        return Ok(spans);
    }
    let mut cursor = 0usize;
    for run in &text.runs {
        let run_start = cursor;
        let run_end = cursor + run.text.len();
        cursor = run_end;
        let lo = run_start.max(range.start);
        let hi = run_end.min(range.end);
        if lo < hi {
            spans.push((lo..hi, &run.author));
        }
        if run_end >= range.end {
            break;
        }
    }
    Ok(spans)
}

/// Returns the distinct authors of `text`, in first-seen (reading) order.
///
/// This is the per-[`RichText`] contributor set. References point into `text`'s
/// runs. The order is the order in which each author's first run appears, which
/// is stable and intuitive for display.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, RichText, Run};
/// use ai_write::provenance::contributors_of;
///
/// let agent = AuthorId::Agent { model: "m".into(), label: "a".into() };
/// let text = RichText {
///     runs: vec![
///         Run::new("a", agent.clone()),
///         Run::new("b", AuthorId::Human),
///         Run::new("c", agent.clone()),
///     ],
/// };
/// // `agent` seen first, then `human`; the second `agent` run is not a new entry.
/// assert_eq!(contributors_of(&text), vec![&agent, &AuthorId::Human]);
/// ```
pub fn contributors_of(text: &RichText) -> Vec<&AuthorId> {
    let mut seen: Vec<&AuthorId> = Vec::new();
    for run in &text.runs {
        if !seen.contains(&&run.author) {
            seen.push(&run.author);
        }
    }
    seen
}

/// Aggregates every contributor across a whole [`Document`], rendered as
/// provenance tags via [`AuthorId::tag`], in first-seen reading order.
///
/// Blocks are visited top to bottom; within each block its [`RichText`] runs are
/// visited left to right. A [`Block::CodeBlock`] carries no per-character
/// authorship (its body is a plain string in the content model), so it
/// contributes no authors here — code-block attribution lives at the block level
/// in the surrounding metadata, outside this text-only layer.
///
/// The result feeds file-level provenance (for example `ArticleMeta::contributors`
/// or a git author list), which is why it returns owned [`String`] tags rather
/// than [`AuthorId`] references.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, Block, Document, RichText};
/// use ai_write::provenance::contributors;
///
/// let agent = AuthorId::Agent { model: "m".into(), label: "a".into() };
/// let mut doc = Document::new();
/// doc.push(Block::Paragraph(RichText::from_plain("hi", AuthorId::Human)))
///     .push(Block::Paragraph(RichText::from_plain("yo", agent)));
/// assert_eq!(contributors(&doc), vec!["human".to_string(), "m/a".to_string()]);
/// ```
pub fn contributors(doc: &Document) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut push = |author: &AuthorId| {
        let tag = author.tag();
        if !seen.contains(&tag) {
            seen.push(tag);
        }
    };
    for block in &doc.blocks {
        match block {
            Block::Paragraph(t) | Block::ListItem(t) | Block::Quote(t) => {
                for run in &t.runs {
                    push(&run.author);
                }
            }
            Block::Heading { text, .. } => {
                for run in &text.runs {
                    push(&run.author);
                }
            }
            // Code blocks carry no per-character authorship.
            Block::CodeBlock { .. } => {}
        }
    }
    seen
}

/// Flattens an authored [`Document`] body into a single [`RichText`], the
/// run-preserving inverse of [`paragraph_document`].
///
/// Block prose is concatenated in reading order with a blank line (`"\n\n"`)
/// between blocks, mirroring [`Document::to_plain_string`], while every run keeps
/// its author — so the result is the whole article body as one authored string.
/// The blank-line separators are attributed to the author of the block they
/// follow. This is how the workspace's authorship layer reconciles a full new
/// body against the stored authorship: flatten, [`reauthor`], then rebuild with
/// [`paragraph_document`].
///
/// A [`Block::CodeBlock`] (no per-character authorship in the content model)
/// contributes its code attributed to `code_author`, which the caller supplies
/// because the model itself does not record one.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, Block, Document, RichText};
/// use ai_write::provenance::flatten_body;
///
/// let mut doc = Document::new();
/// doc.push(Block::Paragraph(RichText::from_plain("one", AuthorId::Human)))
///     .push(Block::Paragraph(RichText::from_plain("two", AuthorId::Human)));
/// let body = flatten_body(&doc, &AuthorId::Human);
/// assert_eq!(body.plain_string(), "one\n\ntwo");
/// ```
pub fn flatten_body(doc: &Document, code_author: &AuthorId) -> RichText {
    let mut out = RichText::empty();
    for (i, block) in doc.blocks.iter().enumerate() {
        let (runs, sep_author): (&[Run], AuthorId) = match block {
            Block::Paragraph(t) | Block::ListItem(t) | Block::Quote(t) => (
                &t.runs,
                t.runs
                    .first()
                    .map(|r| r.author.clone())
                    .unwrap_or_else(|| code_author.clone()),
            ),
            Block::Heading { text, .. } => (
                &text.runs,
                text.runs
                    .first()
                    .map(|r| r.author.clone())
                    .unwrap_or_else(|| code_author.clone()),
            ),
            Block::CodeBlock { code, .. } => {
                if i > 0 {
                    out.runs.push(Run::new("\n\n", code_author.clone()));
                }
                if !code.is_empty() {
                    out.runs.push(Run::new(code.clone(), code_author.clone()));
                }
                continue;
            }
        };
        if i > 0 {
            out.runs.push(Run::new("\n\n", sep_author));
        }
        for run in runs {
            out.runs.push(run.clone());
        }
    }
    normalize(&mut out);
    out
}

/// Splits an authored body [`RichText`] into a paragraph [`Document`], the inverse
/// of [`flatten_body`].
///
/// The body's plain string is split on blank lines (`"\n\n"`) into paragraphs, and
/// each paragraph keeps the run authorship of exactly the characters it spans — so
/// re-flattening with [`flatten_body`] reproduces the input. The resulting blocks
/// are all [`Block::Paragraph`]: the workspace body is free-form prose, so it is
/// modelled as paragraphs rather than parsed through the strict DSL grammar (which
/// would reject ordinary text that does not start with a block sigil). An empty
/// body yields an empty [`Document`].
///
/// This is the shape the rich article view is built from: each paragraph block, in
/// reading order, exposes its authored runs so the front-end can colour each run
/// by its author tag.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, Block, RichText, Run};
/// use ai_write::provenance::paragraph_document;
///
/// let agent = AuthorId::Agent { model: "m".into(), label: "a".into() };
/// let body = RichText {
///     runs: vec![
///         Run::new("hello ", AuthorId::Human),
///         Run::new("world\n\nbye", agent.clone()),
///     ],
/// };
/// let doc = paragraph_document(&body);
/// assert_eq!(doc.blocks.len(), 2);
/// // The first paragraph spans two authors; the second is all the agent's.
/// assert!(matches!(&doc.blocks[0], Block::Paragraph(t) if t.runs.len() == 2));
/// ```
pub fn paragraph_document(body: &RichText) -> Document {
    let plain = body.plain_string();
    let mut doc = Document::new();
    if plain.is_empty() {
        return doc;
    }
    // Byte offsets at which each paragraph starts and ends, derived from the blank
    // line separators in the plain string.
    let mut cursor = 0usize;
    let mut para_start = 0usize;
    let sep = "\n\n";
    let bytes = plain.as_bytes();
    while cursor + sep.len() <= bytes.len() {
        if &bytes[cursor..cursor + sep.len()] == sep.as_bytes() {
            doc.push(Block::Paragraph(slice_runs(body, para_start, cursor)));
            cursor += sep.len();
            para_start = cursor;
        } else {
            cursor += 1;
        }
    }
    doc.push(Block::Paragraph(slice_runs(body, para_start, plain.len())));
    doc
}

/// Returns the sub-[`RichText`] of `body` covering byte range `start..end`, with
/// run authorship preserved and split at the boundaries.
fn slice_runs(body: &RichText, start: usize, end: usize) -> RichText {
    let mut out = RichText::empty();
    let mut cursor = 0usize;
    for run in &body.runs {
        let run_start = cursor;
        let run_end = cursor + run.text.len();
        cursor = run_end;
        let lo = run_start.max(start);
        let hi = run_end.min(end);
        if lo < hi {
            out.runs.push(Run::new(
                run.text[lo - run_start..hi - run_start].to_string(),
                run.author.clone(),
            ));
        }
        if run_end >= end {
            break;
        }
    }
    normalize(&mut out);
    out
}

/// One step of an author-attributed diff between two [`RichText`] values.
///
/// A diff is a sequence of these ops that, read left to right, transforms the
/// *old* text into the *new* text:
///
/// - [`DiffOp::Equal`] — text present in both, unchanged. Carries the author it
///   had in the **new** text (which, for unchanged text, equals the old author).
/// - [`DiffOp::Delete`] — text present only in the old version, removed. Carries
///   the author who had written it (the **old** author).
/// - [`DiffOp::Insert`] — text present only in the new version, added. Carries
///   the **new** author who wrote it.
///
/// "Who added what / who deleted what" is therefore read directly off the op
/// kind plus its `author`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffOp {
    /// Text unchanged between the two versions, with its (new == old) author.
    Equal {
        /// The unchanged text.
        text: String,
        /// The author of this text.
        author: AuthorId,
    },
    /// Text removed in the new version, with the author who had written it.
    Delete {
        /// The removed text.
        text: String,
        /// The author who originally wrote the removed text.
        author: AuthorId,
    },
    /// Text added in the new version, with the author who wrote it.
    Insert {
        /// The added text.
        text: String,
        /// The author who wrote the added text.
        author: AuthorId,
    },
}

/// Computes an author-attributed diff transforming `old` into `new`.
///
/// The two texts are compared at the **character** level using a longest-common-
/// subsequence (LCS) alignment; the common subsequence becomes [`DiffOp::Equal`]
/// runs, characters only in `old` become [`DiffOp::Delete`], and characters only
/// in `new` become [`DiffOp::Insert`]. Each op is then tagged with the relevant
/// author (old author for deletes, new author for inserts and equals) by mapping
/// character positions back onto the run lists, and consecutive same-kind /
/// same-author characters are coalesced into one op.
///
/// Concatenating the `text` of the [`DiffOp::Equal`] and [`DiffOp::Insert`] ops
/// reproduces `new.plain_string()`; concatenating the [`DiffOp::Equal`] and
/// [`DiffOp::Delete`] ops reproduces `old.plain_string()`.
///
/// The LCS table is `O(n * m)` in time and space over the character counts of
/// the two texts, which is appropriate for the paragraph- to article-sized
/// inputs this layer handles; see `docs/impl-v2-provenance.md` for the rationale
/// and the large-input caveat.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, RichText};
/// use ai_write::provenance::{diff, DiffOp};
///
/// let agent = AuthorId::Agent { model: "m".into(), label: "a".into() };
/// let old = RichText::from_plain("cat", AuthorId::Human);
/// let new = RichText::from_plain("cart", agent.clone());
///
/// let ops = diff(&old, &new);
/// // "ca" is common, "r" is inserted by the agent, "t" is common.
/// assert!(ops.iter().any(|op| matches!(op, DiffOp::Insert { text, author }
///     if text == "r" && *author == agent)));
/// ```
pub fn diff(old: &RichText, new: &RichText) -> Vec<DiffOp> {
    // Flatten each side to a per-character (char, author) sequence. Authorship
    // of a deleted char comes from `old`; of an inserted/equal char from `new`.
    let a = chars_with_authors(old);
    let b = chars_with_authors(new);
    let n = a.len();
    let m = b.len();

    // LCS dynamic-programming table over characters (authorship is ignored for
    // alignment — only the character identity matters for "same text").
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i].0 == b[j].0 {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    // Walk the table, emitting one Raw item per character, then coalesce.
    enum Raw {
        Equal(char, AuthorId),
        Delete(char, AuthorId),
        Insert(char, AuthorId),
    }
    let mut raw: Vec<Raw> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i].0 == b[j].0 {
            // Equal text: attribute to the new author (== old author for
            // genuinely unchanged characters).
            raw.push(Raw::Equal(b[j].0, b[j].1.clone()));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            raw.push(Raw::Delete(a[i].0, a[i].1.clone()));
            i += 1;
        } else {
            raw.push(Raw::Insert(b[j].0, b[j].1.clone()));
            j += 1;
        }
    }
    while i < n {
        raw.push(Raw::Delete(a[i].0, a[i].1.clone()));
        i += 1;
    }
    while j < m {
        raw.push(Raw::Insert(b[j].0, b[j].1.clone()));
        j += 1;
    }

    // Coalesce consecutive raw items with the same kind and author.
    let mut ops: Vec<DiffOp> = Vec::new();
    for item in raw {
        let (kind, ch, author) = match item {
            Raw::Equal(c, au) => (0u8, c, au),
            Raw::Delete(c, au) => (1u8, c, au),
            Raw::Insert(c, au) => (2u8, c, au),
        };
        let merged = ops.last_mut().is_some_and(|last| match (kind, last) {
            (0, DiffOp::Equal { text, author: a }) if *a == author => {
                text.push(ch);
                true
            }
            (1, DiffOp::Delete { text, author: a }) if *a == author => {
                text.push(ch);
                true
            }
            (2, DiffOp::Insert { text, author: a }) if *a == author => {
                text.push(ch);
                true
            }
            _ => false,
        });
        if !merged {
            let mut text = String::new();
            text.push(ch);
            ops.push(match kind {
                0 => DiffOp::Equal { text, author },
                1 => DiffOp::Delete { text, author },
                _ => DiffOp::Insert { text, author },
            });
        }
    }
    ops
}

/// Flattens a [`RichText`] into a per-character `(char, author)` sequence.
fn chars_with_authors(text: &RichText) -> Vec<(char, AuthorId)> {
    let mut out = Vec::with_capacity(text.len());
    for run in &text.runs {
        for ch in run.text.chars() {
            out.push((ch, run.author.clone()));
        }
    }
    out
}

// --- offset validation helpers ------------------------------------------------

/// Validates that `offset` is within `0..=len` and on a UTF-8 char boundary.
fn check_boundary(text: &RichText, offset: usize, len: usize) -> Result<()> {
    if offset > len {
        return Err(ProvenanceError::OffsetOutOfBounds { offset, len });
    }
    if !is_char_boundary(text, offset) {
        return Err(ProvenanceError::NotCharBoundary { offset });
    }
    Ok(())
}

/// Validates a range: ordered, in bounds, and both ends on char boundaries.
fn check_range(text: &RichText, range: &Range<usize>, len: usize) -> Result<()> {
    if range.start > range.end {
        return Err(ProvenanceError::InvalidRange {
            start: range.start,
            end: range.end,
        });
    }
    check_boundary(text, range.start, len)?;
    check_boundary(text, range.end, len)?;
    Ok(())
}

/// Returns whether `offset` falls on a UTF-8 character boundary of the
/// run-concatenated text, without materializing the whole string.
///
/// `0` and `len` are always boundaries. For an interior offset, we find the run
/// containing it and defer to [`str::is_char_boundary`] on that run's text.
fn is_char_boundary(text: &RichText, offset: usize) -> bool {
    let mut cursor = 0usize;
    for run in &text.runs {
        let run_start = cursor;
        let run_end = cursor + run.text.len();
        if offset == run_start {
            return true;
        }
        if offset < run_end {
            return run.text.is_char_boundary(offset - run_start);
        }
        cursor = run_end;
    }
    // offset == len (end of text) or text empty with offset 0.
    offset == cursor
}

#[cfg(test)]
mod tests;
