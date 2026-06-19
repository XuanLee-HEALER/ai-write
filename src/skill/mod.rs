//! Writing **skills**: named system-prompt presets loaded from a directory.
//!
//! A skill is a reusable writing persona — a role, a voice, and a set of
//! refusals — that a dispatched writer ([slave](crate::engine)) runs under. It is
//! authored as a plain Markdown file with optional front matter:
//!
//! ```text
//! ---
//! name: Functional writing
//! description: Plain, argument-driven functional prose
//! ---
//! You are a focused writing agent. Write functional prose that …
//! ```
//!
//! The file's body is the system-prompt text; the front matter only supplies a
//! human-readable `name` and `description` for a picker UI. A skill governs
//! **voice**, never tool mechanics: the engine composes the chosen skill with a
//! fixed operational preamble (the lock discipline and the obligation to call
//! `report`) so a skill can never make a writer skip those — see
//! [`compose_slave_prompt`](crate::engine::compose_slave_prompt).
//!
//! In development the skills live in `./skills/*.md` (the WebUI's default
//! directory); the file stem is the skill `id`. A missing directory simply means
//! "no skills available" rather than an error, so the feature is opt-in.
//!
//! # Activating more than one skill (kernel §10)
//!
//! Several skills may be active at once. They form an **ordered stack** and are
//! combined into a single voice block by [`compose_stack`], which the engine then
//! pairs with the fixed operational rules. The stack's contract is a single,
//! documented rule: **when two skills give conflicting directives, the skill
//! later in the stack wins.** The stack therefore reads as "a base persona, with
//! each later layer refining or overriding the ones before it" — the same
//! last-wins cascade as CSS rules, mixin order, or layered `git config`. Because
//! free-text personas cannot be mechanically diffed for conflicts, the composed
//! prompt states this precedence directive explicitly so the *model* resolves
//! conflicts deterministically in the documented direction. A single-skill stack
//! composes to exactly that one skill's body, so the multi-skill path is a strict
//! superset of the single-skill one.

use std::path::Path;

/// A loaded writing skill: a named system-prompt preset.
///
/// The [`body`](Skill::body) is the prompt text a writer is configured with;
/// [`name`](Skill::name) and [`description`](Skill::description) come from the
/// file's front matter (falling back to the [`id`](Skill::id) when absent) and
/// are for display only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The skill identifier — the source file's stem (e.g. `functional-writing`
    /// for `functional-writing.md`).
    pub id: String,
    /// A human-readable name from the front matter, or the `id` if none.
    pub name: String,
    /// A one-line description from the front matter, or empty if none.
    pub description: String,
    /// The system-prompt body (the file content after the front matter, trimmed).
    pub body: String,
}

/// An error loading a skill from disk.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SkillError {
    /// No skill with the requested id exists (or the id was malformed).
    #[error("skill `{0}` not found")]
    NotFound(String),
    /// An I/O failure reading the skills directory or a skill file.
    #[error("io error reading skills: {0}")]
    Io(String),
}

/// Splits a skill file into its optional front matter and its body.
///
/// Front matter is a block delimited by a `---` line at the very start of the
/// file and a closing `---` line. Returns `(Some(front), body)` when both
/// delimiters are present, or `(None, whole_file)` otherwise (an unterminated or
/// absent block is treated as "no front matter", so the whole file is the body).
fn split_front_matter(content: &str) -> (Option<String>, String) {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, content.to_string());
    }
    let mut front: Vec<&str> = Vec::new();
    let mut body: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines {
        if !closed && line.trim() == "---" {
            closed = true;
            continue;
        }
        if closed {
            body.push(line);
        } else {
            front.push(line);
        }
    }
    if !closed {
        // No closing delimiter: there is no real front matter block.
        return (None, content.to_string());
    }
    (Some(front.join("\n")), body.join("\n"))
}

/// Parses a skill's file `content` under the given `id`.
///
/// The front matter (if any) supplies `name` and `description`; the body is the
/// trimmed remainder. A `name` is never empty — it falls back to `id`.
fn parse_skill(id: &str, content: &str) -> Skill {
    let (front, body) = split_front_matter(content);
    let mut name = String::new();
    let mut description = String::new();
    if let Some(front) = front {
        for line in front.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("name:") {
                name = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("description:") {
                description = v.trim().to_string();
            }
        }
    }
    if name.is_empty() {
        name = id.to_string();
    }
    Skill {
        id: id.to_string(),
        name,
        description,
        body: body.trim().to_string(),
    }
}

/// Returns `true` if `id` is a safe single-segment skill id (no path separators
/// or traversal), so it cannot escape the skills directory.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains('\0')
}

/// Loads every skill (`*.md`) from `dir`, sorted by id.
///
/// A non-existent `dir` is **not** an error: it yields an empty list, so the
/// skills feature is opt-in (a project without a `./skills` directory simply has
/// no skills). Files without a `.md` extension are ignored.
///
/// # Errors
///
/// Returns [`SkillError::Io`] if the directory exists but cannot be read, or a
/// skill file cannot be read.
///
/// # Examples
///
/// ```no_run
/// use ai_write::skill::load_skills;
///
/// let skills = load_skills("skills")?;
/// for s in &skills {
///     println!("{} — {}", s.id, s.name);
/// }
/// # Ok::<(), ai_write::skill::SkillError>(())
/// ```
pub fn load_skills(dir: impl AsRef<Path>) -> Result<Vec<Skill>, SkillError> {
    let dir = dir.as_ref();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| SkillError::Io(e.to_string()))?;
    let mut skills = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| SkillError::Io(e.to_string()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let content = std::fs::read_to_string(&path).map_err(|e| SkillError::Io(e.to_string()))?;
        skills.push(parse_skill(id, &content));
    }
    skills.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(skills)
}

/// Loads a single skill by `id` from `dir`.
///
/// # Errors
///
/// Returns [`SkillError::NotFound`] if `id` is malformed (empty, or containing a
/// path separator or traversal) or no `<id>.md` file exists in `dir`.
///
/// # Examples
///
/// ```no_run
/// use ai_write::skill::load_skill;
///
/// let skill = load_skill("skills", "functional-writing")?;
/// assert_eq!(skill.id, "functional-writing");
/// # Ok::<(), ai_write::skill::SkillError>(())
/// ```
pub fn load_skill(dir: impl AsRef<Path>, id: &str) -> Result<Skill, SkillError> {
    if !is_safe_id(id) {
        return Err(SkillError::NotFound(id.to_string()));
    }
    let path = dir.as_ref().join(format!("{id}.md"));
    let content =
        std::fs::read_to_string(&path).map_err(|_| SkillError::NotFound(id.to_string()))?;
    Ok(parse_skill(id, &content))
}

/// Loads an ordered stack of skills from `dir`, preserving the order of `ids`.
///
/// This is the multi-skill counterpart of [`load_skill`]: it loads each id in
/// turn so the returned [`Skill`]s line up with the precedence order the caller
/// intends (earliest first, latest-wins). Duplicate ids are loaded as given —
/// de-duplication, if wanted, is the caller's choice — and an empty `ids` yields
/// an empty stack.
///
/// # Errors
///
/// Returns [`SkillError::NotFound`] for the **first** id that is malformed or has
/// no `<id>.md` file in `dir`, so a bad selection fails fast and cleanly.
///
/// # Examples
///
/// ```no_run
/// use ai_write::skill::load_skills_ordered;
///
/// let stack = load_skills_ordered("skills", &["functional-writing", "concise"])?;
/// assert_eq!(stack.len(), 2);
/// assert_eq!(stack[0].id, "functional-writing");
/// # Ok::<(), ai_write::skill::SkillError>(())
/// ```
pub fn load_skills_ordered(
    dir: impl AsRef<Path>,
    ids: &[impl AsRef<str>],
) -> Result<Vec<Skill>, SkillError> {
    let dir = dir.as_ref();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(load_skill(dir, id.as_ref())?);
    }
    Ok(out)
}

/// The directive prepended to a multi-skill stack stating the precedence rule.
///
/// It is only emitted when two or more skills are stacked (a single skill needs
/// no conflict rule). The wording fixes the documented semantics in the prompt
/// itself so the model resolves conflicting directives deterministically.
const STACK_PRECEDENCE_DIRECTIVE: &str = "\
You are configured with multiple writing skills, applied as an ordered stack \
below. Follow all of them. Where two skills give conflicting instructions, the \
skill that appears LATER in the stack overrides the earlier one.";

/// Composes an ordered stack of skill `bodies` into a single voice block.
///
/// The bodies are laid out **in order** (earliest first), each under a
/// `## Skill N` heading, so the boundaries between stacked skills are explicit to
/// the model. When two or more bodies are present, the block is prefixed with a
/// precedence directive documenting that a **later** skill overrides an earlier
/// one on conflict (kernel §10). Edge cases collapse cleanly:
///
/// - an empty stack composes to an empty string;
/// - a single non-empty body composes to exactly that body (no heading, no
///   directive), so single-skill behaviour is byte-identical to before;
/// - blank bodies are skipped so they cannot introduce empty sections.
///
/// The result is the *voice* layer only; the engine appends the fixed operational
/// rules afterwards (see
/// [`compose_slave_prompt`](crate::engine::compose_slave_prompt)), which always
/// win over anything a skill stack says.
///
/// # Examples
///
/// ```
/// use ai_write::skill::compose_stack;
///
/// // A single skill composes to just its body.
/// assert_eq!(compose_stack(&["Write tersely."]), "Write tersely.");
///
/// // A stack of two carries the precedence directive and both, in order.
/// let stacked = compose_stack(&["Write tersely.", "Use a warm tone."]);
/// assert!(stacked.contains("LATER in the stack overrides"));
/// let terse = stacked.find("Write tersely.").unwrap();
/// let warm = stacked.find("Use a warm tone.").unwrap();
/// assert!(terse < warm, "earlier skill appears first");
/// ```
pub fn compose_stack(bodies: &[impl AsRef<str>]) -> String {
    let trimmed: Vec<&str> = bodies
        .iter()
        .map(|b| b.as_ref().trim())
        .filter(|b| !b.is_empty())
        .collect();
    match trimmed.as_slice() {
        [] => String::new(),
        [only] => (*only).to_string(),
        many => {
            let mut out = String::from(STACK_PRECEDENCE_DIRECTIVE);
            for (i, body) in many.iter().enumerate() {
                out.push_str(&format!("\n\n## Skill {}\n{body}", i + 1));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_with_front_matter() {
        let content = "---\nname: Functional writing\ndescription: plain prose\n---\nYou are a writer.\nWrite well.\n";
        let skill = parse_skill("functional-writing", content);
        assert_eq!(skill.id, "functional-writing");
        assert_eq!(skill.name, "Functional writing");
        assert_eq!(skill.description, "plain prose");
        assert_eq!(skill.body, "You are a writer.\nWrite well.");
    }

    #[test]
    fn parse_skill_without_front_matter_uses_id_as_name() {
        let skill = parse_skill("plain", "Just a body, no front matter.");
        assert_eq!(skill.name, "plain");
        assert_eq!(skill.description, "");
        assert_eq!(skill.body, "Just a body, no front matter.");
    }

    #[test]
    fn unterminated_front_matter_is_treated_as_body() {
        let content = "---\nname: oops\nno closing delimiter here";
        let skill = parse_skill("x", content);
        // Whole file is the body; name falls back to id.
        assert_eq!(skill.name, "x");
        assert!(skill.body.contains("no closing delimiter"));
    }

    #[test]
    fn load_skills_reads_md_sorted_and_ignores_others() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.md"), "---\nname: Bee\n---\nbody b").unwrap();
        std::fs::write(dir.path().join("a.md"), "body a").unwrap();
        std::fs::write(dir.path().join("note.txt"), "ignored").unwrap();

        let skills = load_skills(dir.path()).unwrap();
        let ids: Vec<&str> = skills.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(skills[1].name, "Bee");
    }

    #[test]
    fn load_skills_missing_dir_is_empty_not_error() {
        let skills = load_skills("definitely/not/a/real/dir").unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn load_skill_found_and_not_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("one.md"), "---\nname: One\n---\nb").unwrap();
        let s = load_skill(dir.path(), "one").unwrap();
        assert_eq!(s.name, "One");
        assert!(matches!(
            load_skill(dir.path(), "missing"),
            Err(SkillError::NotFound(_))
        ));
    }

    #[test]
    fn load_skill_rejects_traversal_ids() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["", "..", "a/b", "a\\b"] {
            assert!(
                matches!(load_skill(dir.path(), bad), Err(SkillError::NotFound(_))),
                "id {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn compose_stack_empty_is_empty() {
        let bodies: &[&str] = &[];
        assert_eq!(compose_stack(bodies), "");
        // Whitespace-only bodies are skipped, collapsing to empty too.
        assert_eq!(compose_stack(&["   ", "\n\t"]), "");
    }

    #[test]
    fn compose_stack_single_is_just_the_body() {
        // A single-skill stack must be byte-identical to the lone body so the
        // multi-skill path is a strict superset of the single-skill one.
        assert_eq!(compose_stack(&["Write tersely."]), "Write tersely.");
        // A single non-blank body among blanks still collapses to just that body.
        assert_eq!(
            compose_stack(&["", "Write tersely.", "  "]),
            "Write tersely."
        );
    }

    #[test]
    fn compose_stack_orders_bodies_and_documents_precedence() {
        let out = compose_stack(&["EARLIER voice.", "LATER voice."]);
        // The precedence directive is present and names the later-wins rule.
        assert!(out.contains("LATER in the stack overrides"));
        // Both bodies appear, earliest first (precedence order is positional).
        let earlier = out.find("EARLIER voice.").expect("earlier body present");
        let later = out.find("LATER voice.").expect("later body present");
        assert!(
            earlier < later,
            "earlier skill must appear before later one"
        );
        // Each stacked skill is under its own heading.
        assert!(out.contains("## Skill 1"));
        assert!(out.contains("## Skill 2"));
    }

    #[test]
    fn load_skills_ordered_preserves_order_and_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "body a").unwrap();
        std::fs::write(dir.path().join("b.md"), "body b").unwrap();

        // Reverse-alphabetical request order is preserved (not re-sorted).
        let stack = load_skills_ordered(dir.path(), &["b", "a"]).unwrap();
        let ids: Vec<&str> = stack.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);

        // A missing id anywhere in the stack is a clean NotFound.
        assert!(matches!(
            load_skills_ordered(dir.path(), &["a", "missing"]),
            Err(SkillError::NotFound(_))
        ));

        // An empty selection yields an empty stack, not an error.
        let empty: &[&str] = &[];
        assert!(load_skills_ordered(dir.path(), empty).unwrap().is_empty());
    }
}
