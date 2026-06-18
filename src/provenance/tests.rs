//! Tests for the character-level provenance layer.
//!
//! Coverage: run split/merge, insert/delete/replace authorship, range queries,
//! per-text and document-level aggregation, author-attributed diff, UTF-8
//! boundary rejection, normalization, and edits at the start/end/empty cases.

use super::*;
use crate::content::{AuthorId, Block, Document, RichText, Run};

fn human() -> AuthorId {
    AuthorId::Human
}

fn agent(label: &str) -> AuthorId {
    AuthorId::Agent {
        model: "m".into(),
        label: label.into(),
    }
}

/// Builds a `RichText` from `(text, author)` pairs without normalizing, so tests
/// can set up deliberately un-coalesced inputs.
fn rt(parts: &[(&str, AuthorId)]) -> RichText {
    RichText {
        runs: parts.iter().map(|(t, a)| Run::new(*t, a.clone())).collect(),
    }
}

/// Returns the `(text, author-tag)` view of a `RichText`'s runs.
fn shape(text: &RichText) -> Vec<(String, String)> {
    text.runs
        .iter()
        .map(|r| (r.text.clone(), r.author.tag()))
        .collect()
}

// --- normalization -----------------------------------------------------------

#[test]
fn normalize_drops_empty_runs() {
    let mut t = rt(&[("ab", human()), ("", human()), ("cd", human())]);
    normalize(&mut t);
    assert_eq!(t.runs.len(), 1);
    assert_eq!(t.plain_string(), "abcd");
}

#[test]
fn normalize_coalesces_adjacent_same_author() {
    let mut t = rt(&[("a", human()), ("b", human()), ("c", agent("x"))]);
    normalize(&mut t);
    assert_eq!(
        shape(&t),
        vec![
            ("ab".to_string(), "human".to_string()),
            ("c".to_string(), "m/x".to_string()),
        ]
    );
}

#[test]
fn normalize_keeps_distinct_authors_separate() {
    let mut t = rt(&[("a", human()), ("b", agent("x")), ("c", human())]);
    normalize(&mut t);
    assert_eq!(t.runs.len(), 3);
}

#[test]
fn normalize_empty_is_empty() {
    let mut t = rt(&[("", human()), ("", agent("x"))]);
    normalize(&mut t);
    assert!(t.runs.is_empty());
}

// --- insert ------------------------------------------------------------------

#[test]
fn insert_in_middle_splits_run_and_tags_new_text() {
    let mut t = RichText::from_plain("abcdef", human());
    apply_edit(&mut t, Edit::insert(3, "XYZ"), &agent("w")).unwrap();
    assert_eq!(t.plain_string(), "abcXYZdef");
    assert_eq!(
        shape(&t),
        vec![
            ("abc".to_string(), "human".to_string()),
            ("XYZ".to_string(), "m/w".to_string()),
            ("def".to_string(), "human".to_string()),
        ]
    );
}

#[test]
fn insert_at_start() {
    let mut t = RichText::from_plain("abc", human());
    apply_edit(&mut t, Edit::insert(0, "Z"), &agent("w")).unwrap();
    assert_eq!(t.plain_string(), "Zabc");
    assert_eq!(
        shape(&t),
        vec![
            ("Z".to_string(), "m/w".to_string()),
            ("abc".to_string(), "human".to_string()),
        ]
    );
}

#[test]
fn insert_at_end() {
    let mut t = RichText::from_plain("abc", human());
    apply_edit(&mut t, Edit::insert(3, "Z"), &agent("w")).unwrap();
    assert_eq!(t.plain_string(), "abcZ");
    assert_eq!(
        shape(&t),
        vec![
            ("abc".to_string(), "human".to_string()),
            ("Z".to_string(), "m/w".to_string()),
        ]
    );
}

#[test]
fn insert_into_empty() {
    let mut t = RichText::empty();
    apply_edit(&mut t, Edit::insert(0, "hello"), &human()).unwrap();
    assert_eq!(t.plain_string(), "hello");
    assert_eq!(t.runs.len(), 1);
    assert_eq!(t.runs[0].author, human());
}

#[test]
fn insert_same_author_coalesces() {
    let mut t = RichText::from_plain("abc", human());
    apply_edit(&mut t, Edit::insert(1, "XY"), &human()).unwrap();
    // All human -> one coalesced run.
    assert_eq!(t.plain_string(), "aXYbc");
    assert_eq!(t.runs.len(), 1);
}

#[test]
fn insert_empty_string_is_noop() {
    let mut t = RichText::from_plain("abc", human());
    apply_edit(&mut t, Edit::insert(1, ""), &agent("w")).unwrap();
    assert_eq!(t.plain_string(), "abc");
    assert_eq!(t.runs.len(), 1);
    assert_eq!(t.runs[0].author, human());
}

// --- delete ------------------------------------------------------------------

#[test]
fn delete_middle_keeps_surrounding_authorship() {
    let mut t = rt(&[("abc", human()), ("def", agent("w"))]);
    // Delete "cd" (bytes 2..4) spanning the run boundary.
    apply_edit(&mut t, Edit::delete(2..4), &human()).unwrap();
    assert_eq!(t.plain_string(), "abef");
    assert_eq!(
        shape(&t),
        vec![
            ("ab".to_string(), "human".to_string()),
            ("ef".to_string(), "m/w".to_string()),
        ]
    );
}

#[test]
fn delete_whole_run_then_coalesce() {
    let mut t = rt(&[("ab", human()), ("XY", agent("w")), ("cd", human())]);
    // Delete the agent run entirely (bytes 2..4); the two human runs coalesce.
    apply_edit(&mut t, Edit::delete(2..4), &human()).unwrap();
    assert_eq!(t.plain_string(), "abcd");
    assert_eq!(t.runs.len(), 1);
    assert_eq!(t.runs[0].author, human());
}

#[test]
fn delete_at_start() {
    let mut t = RichText::from_plain("abc", human());
    apply_edit(&mut t, Edit::delete(0..1), &human()).unwrap();
    assert_eq!(t.plain_string(), "bc");
}

#[test]
fn delete_at_end() {
    let mut t = RichText::from_plain("abc", human());
    apply_edit(&mut t, Edit::delete(2..3), &human()).unwrap();
    assert_eq!(t.plain_string(), "ab");
}

#[test]
fn delete_everything_yields_empty() {
    let mut t = rt(&[("ab", human()), ("cd", agent("w"))]);
    apply_edit(&mut t, Edit::delete(0..4), &human()).unwrap();
    assert!(t.is_empty());
    assert!(t.runs.is_empty());
}

#[test]
fn delete_empty_range_is_noop() {
    let mut t = RichText::from_plain("abc", human());
    apply_edit(&mut t, Edit::delete(1..1), &agent("w")).unwrap();
    assert_eq!(t.plain_string(), "abc");
    assert_eq!(t.runs.len(), 1);
}

// --- replace -----------------------------------------------------------------

#[test]
fn replace_middle_tags_new_text() {
    let mut t = RichText::from_plain("hello world", human());
    apply_edit(&mut t, Edit::replace(6..11, "there"), &agent("w")).unwrap();
    assert_eq!(t.plain_string(), "hello there");
    assert_eq!(
        shape(&t),
        vec![
            ("hello ".to_string(), "human".to_string()),
            ("there".to_string(), "m/w".to_string()),
        ]
    );
}

#[test]
fn replace_spanning_runs() {
    let mut t = rt(&[("abc", human()), ("def", agent("w"))]);
    // Replace "cd" (2..4) with "Z" by a third author.
    apply_edit(&mut t, Edit::replace(2..4, "Z"), &agent("z")).unwrap();
    assert_eq!(t.plain_string(), "abZef");
    assert_eq!(
        shape(&t),
        vec![
            ("ab".to_string(), "human".to_string()),
            ("Z".to_string(), "m/z".to_string()),
            ("ef".to_string(), "m/w".to_string()),
        ]
    );
}

#[test]
fn replace_with_empty_is_delete() {
    let mut t = RichText::from_plain("abcdef", human());
    apply_edit(&mut t, Edit::replace(2..4, ""), &agent("w")).unwrap();
    assert_eq!(t.plain_string(), "abef");
    assert_eq!(t.runs.len(), 1);
    assert_eq!(t.runs[0].author, human());
}

#[test]
fn replace_whole_text() {
    let mut t = RichText::from_plain("old", human());
    apply_edit(&mut t, Edit::replace(0..3, "new"), &agent("w")).unwrap();
    assert_eq!(t.plain_string(), "new");
    assert_eq!(t.runs.len(), 1);
    assert_eq!(t.runs[0].author, agent("w"));
}

// --- UTF-8 boundary & bounds rejection ---------------------------------------

#[test]
fn insert_off_char_boundary_is_rejected() {
    // "é" is 2 bytes; offset 1 splits it.
    let mut t = RichText::from_plain("é", human());
    let before = t.clone();
    let err = apply_edit(&mut t, Edit::insert(1, "x"), &agent("w")).unwrap_err();
    assert_eq!(err, ProvenanceError::NotCharBoundary { offset: 1 });
    // Text untouched on error.
    assert_eq!(t, before);
}

#[test]
fn delete_off_char_boundary_is_rejected() {
    let mut t = RichText::from_plain("aé", human()); // bytes: a(1) é(2) => len 3
    let err = apply_edit(&mut t, Edit::delete(1..2), &human()).unwrap_err();
    assert_eq!(err, ProvenanceError::NotCharBoundary { offset: 2 });
}

#[test]
fn boundary_across_run_split_is_rejected() {
    // Two runs; the second run starts mid-codepoint would be impossible, but a
    // multibyte char fully inside the second run must still reject interior
    // offsets relative to the whole text.
    let t = rt(&[("ab", human()), ("☃", agent("w"))]); // ☃ is 3 bytes (2..5)
    // offset 3 splits the snowman.
    let err = author_at(&t, 3).unwrap_err();
    assert_eq!(err, ProvenanceError::NotCharBoundary { offset: 3 });
    // offset 2 (run boundary, char boundary) is fine.
    assert_eq!(author_at(&t, 2).unwrap(), Some(&agent("w")));
}

#[test]
fn offset_out_of_bounds_is_rejected() {
    let mut t = RichText::from_plain("abc", human());
    let err = apply_edit(&mut t, Edit::insert(4, "x"), &human()).unwrap_err();
    assert_eq!(
        err,
        ProvenanceError::OffsetOutOfBounds { offset: 4, len: 3 }
    );
}

#[test]
fn inverted_range_is_rejected() {
    let mut t = RichText::from_plain("abc", human());
    let err = apply_edit(&mut t, Edit::delete(2..1), &human()).unwrap_err();
    assert_eq!(err, ProvenanceError::InvalidRange { start: 2, end: 1 });
}

#[test]
fn multibyte_edit_succeeds_on_boundaries() {
    let mut t = RichText::from_plain("café", human()); // c a f é => bytes 0..5
    // Replace "é" (bytes 3..5) with "e".
    apply_edit(&mut t, Edit::replace(3..5, "e"), &agent("w")).unwrap();
    assert_eq!(t.plain_string(), "cafe");
    assert_eq!(
        shape(&t),
        vec![
            ("caf".to_string(), "human".to_string()),
            ("e".to_string(), "m/w".to_string()),
        ]
    );
}

// --- author_at ---------------------------------------------------------------

#[test]
fn author_at_reports_correct_author() {
    let t = rt(&[("abc", human()), ("def", agent("w"))]);
    assert_eq!(author_at(&t, 0).unwrap(), Some(&human()));
    assert_eq!(author_at(&t, 2).unwrap(), Some(&human()));
    assert_eq!(author_at(&t, 3).unwrap(), Some(&agent("w")));
    assert_eq!(author_at(&t, 5).unwrap(), Some(&agent("w")));
    assert_eq!(author_at(&t, 6).unwrap(), None); // end of text
}

#[test]
fn author_at_empty_text_end() {
    let t = RichText::empty();
    assert_eq!(author_at(&t, 0).unwrap(), None);
}

// --- authors_in_range --------------------------------------------------------

#[test]
fn authors_in_range_tiles_exactly() {
    let t = rt(&[("abc", human()), ("def", agent("w"))]);
    let spans = authors_in_range(&t, 2..5).unwrap();
    assert_eq!(spans, vec![(2..3, &human()), (3..5, &agent("w"))]);
}

#[test]
fn authors_in_range_single_run() {
    let t = rt(&[("abc", human()), ("def", agent("w"))]);
    let spans = authors_in_range(&t, 0..2).unwrap();
    assert_eq!(spans, vec![(0..2, &human())]);
}

#[test]
fn authors_in_range_empty_range_is_empty() {
    let t = RichText::from_plain("abc", human());
    assert!(authors_in_range(&t, 1..1).unwrap().is_empty());
}

#[test]
fn authors_in_range_full() {
    let t = rt(&[("ab", human()), ("cd", agent("w"))]);
    let spans = authors_in_range(&t, 0..4).unwrap();
    assert_eq!(spans, vec![(0..2, &human()), (2..4, &agent("w"))]);
}

// --- contributors_of (per-RichText) ------------------------------------------

#[test]
fn contributors_of_first_seen_order() {
    let t = rt(&[
        ("a", agent("x")),
        ("b", human()),
        ("c", agent("x")),
        ("d", agent("y")),
    ]);
    assert_eq!(
        contributors_of(&t),
        vec![&agent("x"), &human(), &agent("y")]
    );
}

#[test]
fn contributors_of_empty() {
    let t = RichText::empty();
    assert!(contributors_of(&t).is_empty());
}

// --- contributors (Document aggregation) -------------------------------------

#[test]
fn document_contributors_first_seen_across_blocks() {
    let mut doc = Document::new();
    doc.push(Block::Heading {
        level: 1,
        text: RichText::from_plain("Title", human()),
    })
    .push(Block::Paragraph(rt(&[
        ("hi ", agent("x")),
        ("there", human()),
    ])))
    .push(Block::CodeBlock {
        lang: Some("rust".into()),
        code: "fn main() {}".into(),
    })
    .push(Block::ListItem(RichText::from_plain("item", agent("y"))))
    .push(Block::Quote(RichText::from_plain("q", agent("x"))));
    // human (Title) -> x (paragraph) -> y (list); code block contributes none.
    assert_eq!(
        contributors(&doc),
        vec!["human".to_string(), "m/x".to_string(), "m/y".to_string()]
    );
}

#[test]
fn document_contributors_empty() {
    assert!(contributors(&Document::new()).is_empty());
}

#[test]
fn document_contributors_code_only_is_empty() {
    let mut doc = Document::new();
    doc.push(Block::CodeBlock {
        lang: None,
        code: "x".into(),
    });
    assert!(contributors(&doc).is_empty());
}

// --- diff --------------------------------------------------------------------

#[test]
fn diff_insertion_in_middle() {
    let old = RichText::from_plain("cat", human());
    let new = RichText::from_plain("cart", agent("w"));
    let ops = diff(&old, &new);
    // "ca" equal, "r" inserted by agent, "t" equal.
    assert!(
        ops.iter()
            .any(|op| matches!(op, DiffOp::Insert { text, author }
        if text == "r" && *author == agent("w")))
    );
    // Reconstruct new from Equal+Insert.
    let rebuilt_new: String = ops
        .iter()
        .filter_map(|op| match op {
            DiffOp::Equal { text, .. } | DiffOp::Insert { text, .. } => Some(text.as_str()),
            DiffOp::Delete { .. } => None,
        })
        .collect();
    assert_eq!(rebuilt_new, "cart");
}

#[test]
fn diff_deletion() {
    let old = RichText::from_plain("hello", human());
    let new = RichText::from_plain("hlo", human());
    let ops = diff(&old, &new);
    let deleted: String = ops
        .iter()
        .filter_map(|op| match op {
            DiffOp::Delete { text, author } => {
                assert_eq!(*author, human());
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(deleted, "el");
    // Reconstruct old from Equal+Delete.
    let rebuilt_old: String = ops
        .iter()
        .filter_map(|op| match op {
            DiffOp::Equal { text, .. } | DiffOp::Delete { text, .. } => Some(text.as_str()),
            DiffOp::Insert { .. } => None,
        })
        .collect();
    assert_eq!(rebuilt_old, "hello");
}

#[test]
fn diff_replacement_attributes_old_and_new() {
    // Human wrote "hello world"; agent replaced "world" with "there".
    let old = RichText::from_plain("hello world", human());
    let mut new = old.clone();
    apply_edit(&mut new, Edit::replace(6..11, "there"), &agent("w")).unwrap();

    let ops = diff(&old, &new);
    // Deleted text is the human's; inserted text is the agent's.
    let has_human_delete = ops
        .iter()
        .any(|op| matches!(op, DiffOp::Delete { author, .. } if *author == human()));
    let has_agent_insert = ops
        .iter()
        .any(|op| matches!(op, DiffOp::Insert { author, .. } if *author == agent("w")));
    assert!(has_human_delete);
    assert!(has_agent_insert);

    // Equal portion "hello " stays attributed to the human.
    assert!(
        ops.iter()
            .any(|op| matches!(op, DiffOp::Equal { text, author }
        if text == "hello " && *author == human()))
    );
}

#[test]
fn diff_identical_is_all_equal() {
    let a = RichText::from_plain("same", human());
    let b = RichText::from_plain("same", human());
    let ops = diff(&a, &b);
    assert_eq!(
        ops,
        vec![DiffOp::Equal {
            text: "same".into(),
            author: human(),
        }]
    );
}

#[test]
fn diff_from_empty_is_all_insert() {
    let a = RichText::empty();
    let b = RichText::from_plain("new", agent("w"));
    let ops = diff(&a, &b);
    assert_eq!(
        ops,
        vec![DiffOp::Insert {
            text: "new".into(),
            author: agent("w"),
        }]
    );
}

#[test]
fn diff_to_empty_is_all_delete() {
    let a = RichText::from_plain("old", human());
    let b = RichText::empty();
    let ops = diff(&a, &b);
    assert_eq!(
        ops,
        vec![DiffOp::Delete {
            text: "old".into(),
            author: human(),
        }]
    );
}

#[test]
fn diff_unicode() {
    let old = RichText::from_plain("café", human());
    let new = RichText::from_plain("cafe", agent("w"));
    let ops = diff(&old, &new);
    // "é" deleted (human), "e" inserted (agent); "caf" equal.
    assert!(
        ops.iter()
            .any(|op| matches!(op, DiffOp::Delete { text, author }
        if text == "é" && *author == human()))
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, DiffOp::Insert { text, author }
        if text == "e" && *author == agent("w")))
    );
}

// --- end-to-end authorship walk ---------------------------------------------

#[test]
fn sequential_edits_preserve_per_char_authorship() {
    // Human writes a sentence; two agents revise different spans.
    let mut t = RichText::from_plain("The quick fox", human());
    // agent x inserts "brown " before "fox" (byte 10).
    apply_edit(&mut t, Edit::insert(10, "brown "), &agent("x")).unwrap();
    assert_eq!(t.plain_string(), "The quick brown fox");
    // agent y replaces "quick" (bytes 4..9) with "slow".
    apply_edit(&mut t, Edit::replace(4..9, "slow"), &agent("y")).unwrap();
    assert_eq!(t.plain_string(), "The slow brown fox");

    // "The " human, "slow" y, " " human, "brown " x, "fox" human.
    assert_eq!(
        shape(&t),
        vec![
            ("The ".to_string(), "human".to_string()),
            ("slow".to_string(), "m/y".to_string()),
            (" ".to_string(), "human".to_string()),
            ("brown ".to_string(), "m/x".to_string()),
            ("fox".to_string(), "human".to_string()),
        ]
    );
    assert_eq!(
        contributors_of(&t),
        vec![&human(), &agent("y"), &agent("x")]
    );
}
