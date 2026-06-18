//! The shared rich-text content model with character-level authorship.
//!
//! This module is a **frozen data contract** consumed by the higher layers built
//! in parallel: the DSL layer (`dsl`) parses/serializes and renders a
//! [`Document`], while the provenance layer (`provenance`) implements the
//! authorship-aware edit primitive and queries over a [`RichText`]. To keep those
//! layers mergeable
//! without conflict, the types here carry **no heavy behavior** — only the data
//! shapes and trivial constructors both sides agree on.
//!
//! # Character-level authorship: the run model
//!
//! Text is stored not as a flat string but as a sequence of **runs** ([`Run`]):
//! each run is a maximal stretch of contiguous text written by one [`AuthorId`].
//! An edit splits and re-tags runs so that every character keeps the identity of
//! whoever wrote it, while neighbouring same-author runs stay coalesced. This is
//! the standard efficient representation for per-character attribution.
//!
//! v2 deliberately models only block structure plus authored text; inline marks
//! (bold / italic / links) are a later refinement and are intentionally absent.

use serde::{Deserialize, Serialize};

/// The identity of whoever authored a stretch of text.
///
/// This mirrors `WriterId` (from the `tool` module) but is defined here so the
/// content model stays self-contained (`tool` is feature gated; `content` is
/// always available). A `From<WriterId>` adapter is supplied by the integration
/// layer when the two meet.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AuthorId {
    /// A human author.
    Human,
    /// A model-backed agent, identified by its model id and an agent label.
    Agent {
        /// The model id the agent runs on (e.g. `"deepseek-v4-flash"`).
        model: String,
        /// A label distinguishing this agent from other concurrent writers.
        label: String,
    },
}

impl AuthorId {
    /// Renders this author as a stable provenance tag: `"human"` for a human, or
    /// `"<model>/<label>"` for an agent.
    ///
    /// The tag matches the workspace's file-level provenance and the git commit
    /// author, so all three line up.
    pub fn tag(&self) -> String {
        match self {
            AuthorId::Human => "human".to_string(),
            AuthorId::Agent { model, label } => format!("{model}/{label}"),
        }
    }
}

/// A maximal run of contiguous text written by a single [`AuthorId`].
///
/// Runs are the unit of character-level authorship: splitting a run at an offset
/// preserves the authorship of both halves, and adjacent runs with equal authors
/// are coalesced by the provenance layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// The run's text. Never empty in a normalized [`RichText`].
    pub text: String,
    /// Who wrote this text.
    pub author: AuthorId,
}

impl Run {
    /// Creates a run of `text` authored by `author`.
    pub fn new(text: impl Into<String>, author: AuthorId) -> Self {
        Run {
            text: text.into(),
            author,
        }
    }
}

/// Authored text: an ordered sequence of [`Run`]s carrying per-character
/// authorship.
///
/// The plain string is the concatenation of the runs' text; the authorship is
/// the per-run `author`. A normalized `RichText` has no empty runs and no two
/// adjacent runs with the same author (normalization is the provenance layer's
/// responsibility).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichText {
    /// The runs, in reading order.
    pub runs: Vec<Run>,
}

impl RichText {
    /// An empty [`RichText`] (no runs).
    pub fn empty() -> Self {
        RichText::default()
    }

    /// Builds a single-run [`RichText`] from `text` authored entirely by
    /// `author`. An empty `text` yields an empty (run-less) value.
    pub fn from_plain(text: impl Into<String>, author: AuthorId) -> Self {
        let text = text.into();
        if text.is_empty() {
            RichText::empty()
        } else {
            RichText {
                runs: vec![Run::new(text, author)],
            }
        }
    }

    /// Returns the concatenated plain text, dropping authorship.
    pub fn plain_string(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }

    /// The total number of bytes of text across all runs.
    pub fn len(&self) -> usize {
        self.runs.iter().map(|r| r.text.len()).sum()
    }

    /// Returns `true` if there is no text.
    pub fn is_empty(&self) -> bool {
        self.runs.iter().all(|r| r.text.is_empty())
    }
}

/// One block-level element of a [`Document`].
///
/// Blocks that hold prose carry [`RichText`] (and thus character-level
/// authorship); a [`Block::CodeBlock`] keeps its body as a plain string (code is
/// attributed at the block level, not per character).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Block {
    /// A paragraph of authored text.
    Paragraph(RichText),
    /// A heading of the given level (1–6) and authored text.
    Heading {
        /// Heading level, 1 (top) through 6.
        level: u8,
        /// The heading text.
        text: RichText,
    },
    /// A fenced code block with an optional language tag.
    CodeBlock {
        /// The language hint (e.g. `"rust"`), if any.
        lang: Option<String>,
        /// The verbatim code body.
        code: String,
    },
    /// A single list item of authored text. (v2 keeps lists flat: a run of
    /// `ListItem` blocks is one list.)
    ListItem(RichText),
    /// A block quote of authored text.
    Quote(RichText),
}

/// A rich-text document: an ordered sequence of [`Block`]s.
///
/// This is the in-memory model the DSL parses to / serializes from and renders to
/// HTML, and over whose text the provenance layer tracks authorship.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    /// The document's blocks, in reading order.
    pub blocks: Vec<Block>,
}

impl Document {
    /// An empty document.
    pub fn new() -> Self {
        Document::default()
    }

    /// Appends a block, returning `&mut self` for chaining.
    pub fn push(&mut self, block: Block) -> &mut Self {
        self.blocks.push(block);
        self
    }

    /// Renders the document to plain text (blocks joined by blank lines),
    /// dropping structure and authorship. Useful for length checks and for
    /// feeding a plain-text view to a model.
    pub fn to_plain_string(&self) -> String {
        let parts: Vec<String> = self
            .blocks
            .iter()
            .map(|b| match b {
                Block::Paragraph(t) | Block::ListItem(t) | Block::Quote(t) => t.plain_string(),
                Block::Heading { text, .. } => text.plain_string(),
                Block::CodeBlock { code, .. } => code.clone(),
            })
            .collect();
        parts.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> AuthorId {
        AuthorId::Agent {
            model: "deepseek-v4-flash".into(),
            label: "slave-1".into(),
        }
    }

    #[test]
    fn author_tag_matches_provenance_convention() {
        assert_eq!(AuthorId::Human.tag(), "human");
        assert_eq!(agent().tag(), "deepseek-v4-flash/slave-1");
    }

    #[test]
    fn from_plain_and_plain_string_round_trip() {
        let rt = RichText::from_plain("hello world", agent());
        assert_eq!(rt.runs.len(), 1);
        assert_eq!(rt.plain_string(), "hello world");
        assert_eq!(rt.len(), 11);
        assert!(!rt.is_empty());

        assert!(RichText::from_plain("", AuthorId::Human).is_empty());
        assert_eq!(RichText::empty().plain_string(), "");
    }

    #[test]
    fn document_plain_string_joins_blocks() {
        let mut doc = Document::new();
        doc.push(Block::Heading {
            level: 1,
            text: RichText::from_plain("Title", agent()),
        })
        .push(Block::Paragraph(RichText::from_plain("Body.", agent())));
        assert_eq!(doc.to_plain_string(), "Title\n\nBody.");
    }

    #[test]
    fn content_model_serde_round_trips() {
        let mut doc = Document::new();
        doc.push(Block::Paragraph(RichText {
            runs: vec![Run::new("hi ", AuthorId::Human), Run::new("there", agent())],
        }));
        let json = serde_json::to_string(&doc).expect("serialize");
        let back: Document = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(doc, back);
    }
}
