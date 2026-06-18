//! Rich-text DSL layer (v2): a custom line-oriented syntax that is parsed to /
//! serialized from [`content::Document`](crate::content::Document), plus a
//! semantic-HTML renderer.
//!
//! # The syntax
//!
//! A document is a sequence of **blocks**, one per logical line (code blocks
//! being the sole multi-line exception). Each block is introduced by a short,
//! unambiguous **sigil** at the very start of the line, so the block kind is
//! known from the first one or two characters — no look-ahead, no counting, and
//! no collision with prose:
//!
//! | Block | Syntax | Example |
//! |-------|--------|---------|
//! | [`Paragraph`](crate::content::Block::Paragraph) | `: ` + text | `: Hello world.` |
//! | [`Heading`](crate::content::Block::Heading) | `#` + level digit `1`–`6` + ` ` + text | `#2 A subsection` |
//! | [`ListItem`](crate::content::Block::ListItem) | `- ` + text | `- first point` |
//! | [`Quote`](crate::content::Block::Quote) | `> ` + text | `> to be or not` |
//! | [`CodeBlock`](crate::content::Block::CodeBlock) | ` ``` ` + optional lang, body lines, closing ` ``` ` | see below |
//!
//! A fenced code block (three backticks open the fence, an optional language
//! follows on the same line, the closing fence is three backticks alone):
//!
//! ````text
//! ```rust
//! fn main() {}
//! ```
//! ````
//!
//! Blocks are written one per line and joined by a single `\n`; the serializer
//! emits no blank separator lines, and the parser ignores blank lines between
//! blocks (so hand-written input may be spaced out freely).
//!
//! ## Why a custom syntax rather than Markdown
//!
//! Every block kind is tagged explicitly, so the mapping to
//! [`content::Block`](crate::content::Block) is total and reversible: there is exactly
//! one way to write each block and exactly one block each line denotes.
//! Heading levels are a literal digit (`#3 `) instead of a counted run of `#`,
//! which removes the "is `####` four hashes or a typo" ambiguity and lets the
//! full 1–6 range round-trip. Inline text is escaped (see below) so a sigil
//! appearing inside prose can never be mistaken for a block introducer.
//!
//! ## Inline escaping
//!
//! Prose ([`RichText`]) is flattened to its plain string for
//! the DSL form (v2 has no inline marks). Because blocks are line-oriented, the
//! two characters that would break that framing are escaped:
//!
//! - `\\` → `\\\\`
//! - newline (`\n`) → `\\n`
//!
//! Parsing reverses both. A lone `\\` before any other character is an error
//! ([`DslError::BadEscape`]). The plain text of a parsed block is attributed
//! entirely to the `author` passed to [`parse`], as a single
//! [`Run`]; authorship across multiple writers is the provenance
//! layer's concern, not the DSL's.
//!
//! # Round-trip guarantee
//!
//! [`parse`] and [`serialize`] are mutual inverses on the value level:
//! `parse(&serialize(d), a)` reconstructs `d` for any [`Document`] `d` whose
//! prose is single-author `a` and normalized (one run per block), and
//! `serialize(&parse(s, a)?)` reproduces the **canonical** form of `s` (blank
//! separator lines and authorship distinctions are the only information not
//! preserved, by design).
//!
//! # Examples
//!
//! ```
//! use ai_write::content::{AuthorId, Block, Document, RichText};
//! use ai_write::dsl;
//!
//! let src = "#1 Title\n: A paragraph.\n- one\n- two";
//! let doc = dsl::parse(src, AuthorId::Human).expect("valid DSL");
//! assert_eq!(doc.blocks.len(), 4);
//! assert_eq!(dsl::serialize(&doc), src);
//!
//! let html = dsl::render_html(&doc);
//! assert!(html.contains("<h1>"));
//! assert!(html.contains(r#"<span data-author="human">Title</span>"#));
//! ```

use crate::content::{AuthorId, Block, Document, RichText, Run};

/// The error returned when [`parse`] cannot interpret its input as a valid
/// document in the DSL syntax.
///
/// Marked `#[non_exhaustive]`: match arms must include a wildcard so new
/// variants can be added in a backward-compatible release.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum DslError {
    /// A line did not start with any known block sigil (`: `, `#`, `- `, `> `,
    /// or a code fence). The payload is the 1-based line number.
    #[error("line {0}: unrecognized block syntax (no known sigil)")]
    UnknownBlock(usize),

    /// A heading sigil `#` was not followed by a single level digit `1`–`6`
    /// and a space (for example `#7 ` or `#x ` or `# `). The payload is the
    /// 1-based line number.
    #[error("line {0}: heading level must be a digit 1-6 followed by a space")]
    BadHeadingLevel(usize),

    /// A code fence was opened with ` ``` ` but the input ended before a
    /// closing fence was found. The payload is the 1-based line number of the
    /// opening fence.
    #[error("line {0}: unterminated code block (missing closing ```)")]
    UnterminatedCode(usize),

    /// An inline backslash escape was malformed: a `\` was the last character of
    /// the text, or was followed by a character other than `\` or `n`. The
    /// payload is the 1-based line number.
    #[error("line {0}: invalid '\\' escape (expected '\\\\' or '\\n')")]
    BadEscape(usize),
}

/// Parses DSL `input` into a [`Document`], attributing all prose to `author`.
///
/// Every block's text becomes a single [`Run`] owned by `author`; the DSL form
/// carries no per-character authorship (that is the provenance layer's job), so
/// a parsed document is always single-author. Blank lines between blocks are
/// ignored, letting hand-written input breathe.
///
/// See the [module documentation](self) for the full grammar.
///
/// # Errors
///
/// Returns a [`DslError`] when a line cannot be interpreted:
/// - [`DslError::UnknownBlock`] — a non-blank line has no recognized sigil;
/// - [`DslError::BadHeadingLevel`] — `#` is not followed by a digit `1`–`6` and a space;
/// - [`DslError::UnterminatedCode`] — a code fence is opened but never closed;
/// - [`DslError::BadEscape`] — an inline `\` escape is malformed.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, Block};
/// use ai_write::dsl;
///
/// let doc = dsl::parse("#2 Heading\n: Body text.", AuthorId::Human).unwrap();
/// assert!(matches!(doc.blocks[0], Block::Heading { level: 2, .. }));
/// assert!(matches!(doc.blocks[1], Block::Paragraph(_)));
///
/// // An unknown sigil is rejected with the offending line number.
/// assert!(dsl::parse("plain text", AuthorId::Human).is_err());
/// ```
pub fn parse(input: &str, author: AuthorId) -> Result<Document, DslError> {
    let mut doc = Document::new();
    // 1-based line numbering for diagnostics; we step the iterator manually so
    // code blocks can consume their body lines.
    let lines: Vec<&str> = input.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let lineno = i + 1;

        // Blank lines act only as optional visual separators between blocks.
        if line.is_empty() {
            i += 1;
            continue;
        }

        if let Some(rest) = line.strip_prefix("```") {
            // Opening fence: `rest` is the optional language tag.
            let lang = parse_code_lang(rest);
            let (code, consumed) = parse_code_body(&lines, i + 1, lineno)?;
            doc.push(Block::CodeBlock { lang, code });
            // Skip the opening fence, the body, and the closing fence.
            i = consumed + 1;
            continue;
        }

        let block = parse_inline_block(line, lineno, &author)?;
        doc.push(block);
        i += 1;
    }
    Ok(doc)
}

/// Serializes a [`Document`] back into canonical DSL text.
///
/// The output is the canonical form: exactly one line per non-code block, code
/// blocks fenced with ` ``` `, no blank separator lines, and blocks joined by a
/// single `\n` with no trailing newline. Inline prose is escaped per the
/// [module documentation](self). This is the inverse of [`parse`] at the value
/// level (authorship and cosmetic blank lines are not represented in the DSL and
/// so are not preserved).
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, Block, Document, RichText};
/// use ai_write::dsl;
///
/// let mut doc = Document::new();
/// doc.push(Block::Quote(RichText::from_plain("stay hungry", AuthorId::Human)));
/// assert_eq!(dsl::serialize(&doc), "> stay hungry");
/// ```
pub fn serialize(doc: &Document) -> String {
    let mut out = String::new();
    for (idx, block) in doc.blocks.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        serialize_block(block, &mut out);
    }
    out
}

/// Renders a [`Document`] to semantic HTML.
///
/// Each block maps to its natural HTML element — `<p>`, `<h1>`…`<h6>`,
/// `<pre><code>`, `<li>`, `<blockquote>` — and the prose inside is emitted run
/// by run as `<span data-author="…">…</span>`, where the attribute is the run
/// author's [`tag`](AuthorId::tag). Preserving each [`Run`] as its own span is
/// what makes character-level authorship visualizable downstream. All text
/// (and the language class on code blocks) is HTML-escaped.
///
/// Output notes:
/// - Consecutive [`ListItem`](Block::ListItem) blocks are wrapped in a
///   single `<ul>` (v2 lists are flat).
/// - A code block's language becomes `<code class="language-…">` when present.
/// - The result has no surrounding document scaffold (no `<html>`/`<body>`); it
///   is a fragment meant to be embedded.
///
/// # Examples
///
/// ```
/// use ai_write::content::{AuthorId, Block, Document, RichText, Run};
/// use ai_write::dsl;
///
/// let mut doc = Document::new();
/// doc.push(Block::Paragraph(RichText {
///     runs: vec![
///         Run::new("Hi ", AuthorId::Human),
///         Run::new("<there>", AuthorId::Agent { model: "m".into(), label: "l".into() }),
///     ],
/// }));
/// let html = dsl::render_html(&doc);
/// assert_eq!(
///     html,
///     concat!(
///         "<p>",
///         r#"<span data-author="human">Hi </span>"#,
///         r#"<span data-author="m/l">&lt;there&gt;</span>"#,
///         "</p>",
///     )
/// );
/// ```
pub fn render_html(doc: &Document) -> String {
    let mut out = String::new();
    let mut in_list = false;
    for block in &doc.blocks {
        // Open/close the implicit `<ul>` that groups consecutive list items.
        let is_item = matches!(block, Block::ListItem(_));
        if is_item && !in_list {
            out.push_str("<ul>");
            in_list = true;
        } else if !is_item && in_list {
            out.push_str("</ul>");
            in_list = false;
        }
        render_block_html(block, &mut out);
    }
    if in_list {
        out.push_str("</ul>");
    }
    out
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Parses a single-line block (everything except code fences) from `line`.
fn parse_inline_block(line: &str, lineno: usize, author: &AuthorId) -> Result<Block, DslError> {
    if let Some(text) = line.strip_prefix(": ") {
        return Ok(Block::Paragraph(rich(text, lineno, author)?));
    }
    if let Some(text) = line.strip_prefix("- ") {
        return Ok(Block::ListItem(rich(text, lineno, author)?));
    }
    if let Some(text) = line.strip_prefix("> ") {
        return Ok(Block::Quote(rich(text, lineno, author)?));
    }
    if let Some(after_hash) = line.strip_prefix('#') {
        return parse_heading(after_hash, lineno, author);
    }
    // An empty paragraph is written as a bare ":" with no trailing space.
    if line == ":" {
        return Ok(Block::Paragraph(RichText::empty()));
    }
    // A bare "-" / ">" denote an empty list item / quote.
    if line == "-" {
        return Ok(Block::ListItem(RichText::empty()));
    }
    if line == ">" {
        return Ok(Block::Quote(RichText::empty()));
    }
    Err(DslError::UnknownBlock(lineno))
}

/// Parses a heading from the text following the leading `#`.
fn parse_heading(after_hash: &str, lineno: usize, author: &AuthorId) -> Result<Block, DslError> {
    let mut chars = after_hash.chars();
    let level_ch = chars.next().ok_or(DslError::BadHeadingLevel(lineno))?;
    let level = match level_ch {
        '1'..='6' => (level_ch as u8) - b'0',
        _ => return Err(DslError::BadHeadingLevel(lineno)),
    };
    let remainder = &after_hash[level_ch.len_utf8()..];
    // The level digit must be followed by a space (then the text), or be the
    // whole token (an empty heading).
    let text = match remainder.strip_prefix(' ') {
        Some(t) => t,
        None if remainder.is_empty() => "",
        None => return Err(DslError::BadHeadingLevel(lineno)),
    };
    Ok(Block::Heading {
        level,
        text: rich(text, lineno, author)?,
    })
}

/// Extracts the optional language tag from a code fence's trailing text.
///
/// Returns `None` for an empty/whitespace-only tag, otherwise the trimmed tag.
fn parse_code_lang(rest: &str) -> Option<String> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Collects code-block body lines starting at index `start`, until the closing
/// ` ``` ` fence.
///
/// Returns the verbatim body (lines joined by `\n`, no trailing newline) and the
/// index of the closing fence line.
///
/// # Errors
///
/// [`DslError::UnterminatedCode`] if no closing fence is found before EOF,
/// reported against `open_lineno` (the 1-based opening-fence line).
fn parse_code_body(
    lines: &[&str],
    start: usize,
    open_lineno: usize,
) -> Result<(String, usize), DslError> {
    let mut body: Vec<&str> = Vec::new();
    let mut j = start;
    while j < lines.len() {
        if lines[j] == "```" {
            return Ok((body.join("\n"), j));
        }
        body.push(lines[j]);
        j += 1;
    }
    Err(DslError::UnterminatedCode(open_lineno))
}

/// Builds a single-run [`RichText`] from escaped DSL `text`, owned by `author`.
fn rich(text: &str, lineno: usize, author: &AuthorId) -> Result<RichText, DslError> {
    let plain = unescape_inline(text, lineno)?;
    Ok(RichText::from_plain(plain, author.clone()))
}

/// Reverses inline escaping: `\\` → `\`, `\n` → newline. Any other escape, or a
/// trailing lone `\`, is a [`DslError::BadEscape`].
fn unescape_inline(text: &str, lineno: usize) -> Result<String, DslError> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                _ => return Err(DslError::BadEscape(lineno)),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

/// Appends the canonical DSL form of one block to `out`.
fn serialize_block(block: &Block, out: &mut String) {
    match block {
        Block::Paragraph(t) => write_prefixed(out, ":", t),
        Block::ListItem(t) => write_prefixed(out, "-", t),
        Block::Quote(t) => write_prefixed(out, ">", t),
        Block::Heading { level, text } => {
            // `level` is constrained to 1..=6 by `parse`; clamp defensively so a
            // hand-built out-of-range document still serializes to valid syntax.
            let lvl = (*level).clamp(1, 6);
            let prefix = format!("#{lvl}");
            write_prefixed(out, &prefix, text);
        }
        Block::CodeBlock { lang, code } => {
            out.push_str("```");
            if let Some(lang) = lang {
                out.push_str(lang);
            }
            out.push('\n');
            out.push_str(code);
            // Separate the body from the closing fence. A non-empty body needs a
            // newline; an empty body already sits on its own (empty) line.
            out.push('\n');
            out.push_str("```");
        }
    }
}

/// Writes `sigil` then the escaped prose, with a separating space only when the
/// prose is non-empty (so an empty block serializes to the bare sigil).
fn write_prefixed(out: &mut String, sigil: &str, text: &RichText) {
    out.push_str(sigil);
    let plain = text.plain_string();
    if !plain.is_empty() {
        out.push(' ');
        out.push_str(&escape_inline(&plain));
    }
}

/// Applies inline escaping: `\` → `\\`, newline → `\n`. The inverse of
/// [`unescape_inline`].
fn escape_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// HTML rendering helpers
// ---------------------------------------------------------------------------

/// Appends the HTML for one block to `out` (list grouping handled by caller).
fn render_block_html(block: &Block, out: &mut String) {
    match block {
        Block::Paragraph(t) => wrap_runs(out, "<p>", t, "</p>"),
        Block::Quote(t) => wrap_runs(out, "<blockquote>", t, "</blockquote>"),
        Block::ListItem(t) => wrap_runs(out, "<li>", t, "</li>"),
        Block::Heading { level, text } => {
            let lvl = (*level).clamp(1, 6);
            wrap_runs(out, &format!("<h{lvl}>"), text, &format!("</h{lvl}>"));
        }
        Block::CodeBlock { lang, code } => {
            out.push_str("<pre><code");
            if let Some(lang) = lang {
                out.push_str(" class=\"language-");
                out.push_str(&escape_html(lang));
                out.push('"');
            }
            out.push('>');
            out.push_str(&escape_html(code));
            out.push_str("</code></pre>");
        }
    }
}

/// Writes `open`, each run of `text` as an escaped `data-author` span, then
/// `close`.
fn wrap_runs(out: &mut String, open: &str, text: &RichText, close: &str) {
    out.push_str(open);
    for run in &text.runs {
        render_run_html(run, out);
    }
    out.push_str(close);
}

/// Appends one run as `<span data-author="…">escaped text</span>`.
fn render_run_html(run: &Run, out: &mut String) {
    out.push_str("<span data-author=\"");
    out.push_str(&escape_html(&run.author.tag()));
    out.push_str("\">");
    out.push_str(&escape_html(&run.text));
    out.push_str("</span>");
}

/// HTML-escapes the five characters that are unsafe in element text or in a
/// double-quoted attribute value (`&`, `<`, `>`, `"`, `'`).
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn human_doc(src: &str) -> Document {
        parse(src, AuthorId::Human).expect("valid DSL")
    }

    fn agent() -> AuthorId {
        AuthorId::Agent {
            model: "deepseek-v4-flash".into(),
            label: "slave-1".into(),
        }
    }

    // --- round-trip: every block kind -------------------------------------

    #[test]
    fn round_trip_paragraph() {
        let src = ": A simple paragraph.";
        assert_eq!(serialize(&human_doc(src)), src);
    }

    #[test]
    fn round_trip_all_heading_levels() {
        for level in 1..=6 {
            let src = format!("#{level} Heading {level}");
            let doc = human_doc(&src);
            assert!(matches!(doc.blocks[0], Block::Heading { level: l, .. } if l == level));
            assert_eq!(serialize(&doc), src);
        }
    }

    #[test]
    fn round_trip_list_and_quote() {
        let src = "- first\n- second\n> a quote";
        let doc = human_doc(src);
        assert_eq!(doc.blocks.len(), 3);
        assert_eq!(serialize(&doc), src);
    }

    #[test]
    fn round_trip_code_block_with_lang() {
        let src = "```rust\nfn main() {}\n```";
        let doc = human_doc(src);
        assert_eq!(
            doc.blocks[0],
            Block::CodeBlock {
                lang: Some("rust".into()),
                code: "fn main() {}".into(),
            }
        );
        assert_eq!(serialize(&doc), src);
    }

    #[test]
    fn round_trip_code_block_no_lang_multiline() {
        let src = "```\nline one\nline two\n```";
        let doc = human_doc(src);
        assert_eq!(
            doc.blocks[0],
            Block::CodeBlock {
                lang: None,
                code: "line one\nline two".into(),
            }
        );
        assert_eq!(serialize(&doc), src);
    }

    #[test]
    fn round_trip_empty_code_block() {
        let src = "```\n\n```";
        let doc = human_doc(src);
        assert_eq!(
            doc.blocks[0],
            Block::CodeBlock {
                lang: None,
                code: String::new(),
            }
        );
        assert_eq!(serialize(&doc), src);
    }

    #[test]
    fn round_trip_mixed_document() {
        let src = "#1 Title\n: Intro paragraph.\n- point one\n- point two\n> wisdom\n```py\nprint(1)\n```";
        let doc = human_doc(src);
        assert_eq!(doc.blocks.len(), 6);
        assert_eq!(serialize(&doc), src);
    }

    // --- empties ----------------------------------------------------------

    #[test]
    fn empty_document_round_trips() {
        let doc = human_doc("");
        assert!(doc.blocks.is_empty());
        assert_eq!(serialize(&doc), "");
        assert_eq!(render_html(&doc), "");
    }

    #[test]
    fn empty_blocks_serialize_to_bare_sigils() {
        let src = ":\n-\n>\n#3";
        let doc = human_doc(src);
        assert!(matches!(&doc.blocks[0], Block::Paragraph(t) if t.is_empty()));
        assert!(matches!(&doc.blocks[1], Block::ListItem(t) if t.is_empty()));
        assert!(matches!(&doc.blocks[2], Block::Quote(t) if t.is_empty()));
        assert!(matches!(&doc.blocks[3], Block::Heading { level: 3, text } if text.is_empty()));
        assert_eq!(serialize(&doc), src);
    }

    // --- blank-line tolerance + canonicalization --------------------------

    #[test]
    fn blank_lines_between_blocks_are_ignored() {
        let doc = human_doc(": one\n\n\n: two");
        assert_eq!(doc.blocks.len(), 2);
        // Serialization is canonical (no blank separators).
        assert_eq!(serialize(&doc), ": one\n: two");
    }

    // --- escaping (inline) ------------------------------------------------

    #[test]
    fn inline_backslash_and_newline_round_trip() {
        // A paragraph whose text contains a literal backslash and a newline.
        let mut doc = Document::new();
        doc.push(Block::Paragraph(RichText::from_plain(
            "a\\b\nc",
            AuthorId::Human,
        )));
        let dsl = serialize(&doc);
        assert_eq!(dsl, ": a\\\\b\\nc");
        assert_eq!(human_doc(&dsl), doc);
    }

    #[test]
    fn leading_sigil_chars_in_prose_survive() {
        // Prose that *looks* like a sigil is fine once it follows the block's
        // own sigil — it is plain text, escaped only for `\` and newline.
        let src = ": > not a quote, just text";
        let doc = human_doc(src);
        assert_eq!(
            doc.blocks[0],
            Block::Paragraph(RichText::from_plain(
                "> not a quote, just text",
                AuthorId::Human
            ))
        );
        assert_eq!(serialize(&doc), src);
    }

    // --- error paths ------------------------------------------------------

    #[test]
    fn unknown_block_is_rejected_with_line_number() {
        assert_eq!(
            parse("just some text", AuthorId::Human),
            Err(DslError::UnknownBlock(1))
        );
        assert_eq!(
            parse(": ok\nbad line", AuthorId::Human),
            Err(DslError::UnknownBlock(2))
        );
    }

    #[test]
    fn heading_level_bounds_enforced() {
        assert_eq!(
            parse("#7 too deep", AuthorId::Human),
            Err(DslError::BadHeadingLevel(1))
        );
        assert_eq!(
            parse("#0 too shallow", AuthorId::Human),
            Err(DslError::BadHeadingLevel(1))
        );
        assert_eq!(
            parse("#x not a digit", AuthorId::Human),
            Err(DslError::BadHeadingLevel(1))
        );
        // A digit must be followed by a space (or be the whole token).
        assert_eq!(
            parse("#2no-space", AuthorId::Human),
            Err(DslError::BadHeadingLevel(1))
        );
    }

    #[test]
    fn unterminated_code_block_is_rejected() {
        assert_eq!(
            parse("```rust\nfn main() {}", AuthorId::Human),
            Err(DslError::UnterminatedCode(1))
        );
    }

    #[test]
    fn bad_escape_is_rejected() {
        // A lone trailing backslash.
        assert_eq!(
            parse(": ends with \\", AuthorId::Human),
            Err(DslError::BadEscape(1))
        );
        // An unknown escape sequence.
        assert_eq!(
            parse(": bad \\t tab", AuthorId::Human),
            Err(DslError::BadEscape(1))
        );
    }

    // --- authorship attribution on parse ----------------------------------

    #[test]
    fn parsed_prose_is_attributed_to_the_passed_author() {
        let doc = parse(": written by an agent", agent()).unwrap();
        match &doc.blocks[0] {
            Block::Paragraph(rt) => {
                assert_eq!(rt.runs.len(), 1);
                assert_eq!(rt.runs[0].author, agent());
            }
            _ => panic!("expected paragraph"),
        }
    }

    // --- HTML rendering ---------------------------------------------------

    #[test]
    fn render_html_all_block_kinds() {
        let doc = human_doc("#2 Head\n: Para\n- item\n> Quote\n```rust\ncode\n```");
        let html = render_html(&doc);
        assert!(html.contains("<h2><span data-author=\"human\">Head</span></h2>"));
        assert!(html.contains("<p><span data-author=\"human\">Para</span></p>"));
        assert!(html.contains("<ul><li><span data-author=\"human\">item</span></li></ul>"));
        assert!(html.contains("<blockquote><span data-author=\"human\">Quote</span></blockquote>"));
        assert!(html.contains("<pre><code class=\"language-rust\">code</code></pre>"));
    }

    #[test]
    fn render_html_groups_consecutive_list_items_in_one_ul() {
        let doc = human_doc("- a\n- b\n: not a list\n- c");
        let html = render_html(&doc);
        // Two separate lists, one of two items, one of a single item.
        assert_eq!(html.matches("<ul>").count(), 2);
        assert_eq!(html.matches("</ul>").count(), 2);
        assert_eq!(html.matches("<li>").count(), 3);
    }

    #[test]
    fn render_html_multi_run_emits_one_span_per_run() {
        let mut doc = Document::new();
        doc.push(Block::Paragraph(RichText {
            runs: vec![
                Run::new("human bit ", AuthorId::Human),
                Run::new("agent bit", agent()),
            ],
        }));
        let html = render_html(&doc);
        assert_eq!(
            html,
            concat!(
                "<p>",
                "<span data-author=\"human\">human bit </span>",
                "<span data-author=\"deepseek-v4-flash/slave-1\">agent bit</span>",
                "</p>",
            )
        );
    }

    #[test]
    fn render_html_escapes_text_and_attributes() {
        // Author label and text both carry HTML metacharacters.
        let author = AuthorId::Agent {
            model: "a&b".into(),
            label: "\"x\"".into(),
        };
        let mut doc = Document::new();
        doc.push(Block::Paragraph(RichText::from_plain(
            "1 < 2 && 3 > 2 'q'",
            author,
        )));
        let html = render_html(&doc);
        assert!(html.contains("data-author=\"a&amp;b/&quot;x&quot;\""));
        assert!(html.contains("1 &lt; 2 &amp;&amp; 3 &gt; 2 &#39;q&#39;"));
        // No raw metacharacters leaked into element text or attribute.
        assert!(!html.contains("1 < 2"));
    }

    #[test]
    fn render_html_escapes_code_block_lang_and_body() {
        let mut doc = Document::new();
        doc.push(Block::CodeBlock {
            lang: Some("ht\"ml".into()),
            code: "<script>alert(1)</script>".into(),
        });
        let html = render_html(&doc);
        assert!(html.contains("class=\"language-ht&quot;ml\""));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn empty_paragraph_renders_empty_tag() {
        let doc = human_doc(":");
        assert_eq!(render_html(&doc), "<p></p>");
    }

    // --- serialize -> parse direction on a hand-built document -------------

    #[test]
    fn serialize_then_parse_reproduces_value() {
        let mut doc = Document::new();
        doc.push(Block::Heading {
            level: 4,
            text: RichText::from_plain("Section", AuthorId::Human),
        })
        .push(Block::Paragraph(RichText::from_plain(
            "Body with a tab\tand unicode → ✓.",
            AuthorId::Human,
        )))
        .push(Block::CodeBlock {
            lang: Some("json".into()),
            code: "{\n  \"k\": 1\n}".into(),
        });
        let dsl = serialize(&doc);
        assert_eq!(parse(&dsl, AuthorId::Human).unwrap(), doc);
    }
}
