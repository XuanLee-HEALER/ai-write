//! The on-disk workspace model: themes, articles, the index, and the
//! single-writer article lock.
//!
//! The workspace is a two-level tree rooted at a directory: a **theme** is a
//! subdirectory, and an **article** is a plain-text file inside a theme. An
//! [`Index`] manifest records the reading order of articles within a theme and
//! is persisted with `serde`. Each article carries a [`LockState`] enforcing the
//! single-writer invariant: only the [`WriterId`] holding the lock may mutate the
//! article.
//!
//! All filesystem access goes through the path sandbox: a relative path is
//! resolved against the workspace root and rejected (with
//! [`ToolError::SandboxViolation`]) if
//! it is absolute, escapes the root via `..`, or points at a system path. v0
//! articles are **plain text**; provenance is tracked only at the file level via
//! the model id recorded in [`ArticleMeta`].
//!
//! # Locks are process-local
//!
//! The single-writer lock lives in memory on the [`Workspace`] handle, keyed by
//! `(theme, article)`. It coordinates writers *within one process* (the slaves
//! and master sharing a workspace through the orchestration layer); it is **not**
//! an OS-level file lock and does not coordinate across processes. v0's
//! concurrency model is in-process `std::thread`, so this is sufficient.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::content::{AuthorId, Document, RichText};
use crate::tool::ToolError;

/// The file name of the per-theme [`Index`] manifest, stored inside the theme
/// directory.
const INDEX_FILE: &str = "index.json";

/// The suffix appended to an article file name to form its sidecar asset
/// directory (kernel §7).
///
/// An article `topic/name.md` may carry associated binary or rich-media assets
/// (images, figures destined for a PDF render) under
/// `topic/name.md.assets/...`. The suffix is appended to the **whole** article
/// file name — including its extension — so `a.md` and `a.txt` never collide on
/// the same assets directory, and so the convention needs no extension parsing.
const ASSETS_DIR_SUFFIX: &str = ".assets";

/// The suffix appended to an article file name to form its character-level
/// **provenance sidecar** (the deferred B2 integration layer,
/// `docs/impl-v2-results.md` §5).
///
/// The plain-text article file stays the source of truth for reads, git history,
/// blame and diff (so the file-level provenance layer is untouched), while the
/// sidecar `topic/name.md.prov.json` carries the per-character authorship as a
/// serialized [`Document`](crate::content::Document). The two are kept in lock-step
/// by the authorship-aware write path
/// ([`Workspace::write_article_authored`]): every body change re-attributes only
/// the edited span and rewrites the sidecar. An article without a sidecar (created
/// before B2, or never edited through the authored path) degrades gracefully — its
/// rich view is a single run attributed to its last recorded contributor.
const PROVENANCE_SIDECAR_SUFFIX: &str = ".prov.json";

/// The largest article (in bytes) [`Workspace::read_article`] will return before
/// refusing with [`ToolError::Unsupported`]. v0 articles are short plain-text
/// files; anything larger is treated as out of scope and bounced back to the
/// model so it can adapt (chunked reads, a summary, a different tool).
const MAX_ARTICLE_BYTES: u64 = 1024 * 1024;

/// The identity of an entity that may hold an article lock and author edits.
///
/// In v0 a writer is either a human or a model-backed agent identified by its
/// model id; the id is recorded in [`ArticleMeta`] for file-level provenance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum WriterId {
    /// A human writer (the person intervening directly).
    Human,
    /// A model-backed agent, identified by its model id (e.g.
    /// `"deepseek-v4-pro"`) and an opaque agent label distinguishing concurrent
    /// slaves.
    Agent {
        /// The model id the agent runs on.
        model: String,
        /// A label distinguishing this agent from other concurrent writers.
        label: String,
    },
}

impl WriterId {
    /// Renders this writer as a stable provenance tag recorded in
    /// [`ArticleMeta::contributors`].
    ///
    /// A human is `"human"`; an agent is `"<model>/<label>"` (e.g.
    /// `"deepseek-v4-pro/slave-1"`). This is the canonical file-level provenance
    /// identity: it is what the workspace records in
    /// [`ArticleMeta::contributors`], what the [`vcs`](crate::vcs) layer uses as
    /// the git author name, and what the [`observe`](crate::observe) layer reports
    /// as the author of an [`Event::EditCommitted`](crate::observe::Event::EditCommitted),
    /// so a commit lines up one-to-one across all three.
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::tool::workspace::WriterId;
    ///
    /// assert_eq!(WriterId::Human.provenance_tag(), "human");
    /// let agent = WriterId::Agent {
    ///     model: "deepseek-v4-pro".to_string(),
    ///     label: "slave-1".to_string(),
    /// };
    /// assert_eq!(agent.provenance_tag(), "deepseek-v4-pro/slave-1");
    /// ```
    pub fn provenance_tag(&self) -> String {
        match self {
            WriterId::Human => "human".to_string(),
            WriterId::Agent { model, label } => format!("{model}/{label}"),
        }
    }
}

/// The lock state of an [`Article`]: idle, or held by exactly one writer.
///
/// This is the whole article state machine in v0 — there is no "settled" state;
/// finishing an edit simply releases the lock back to [`LockState::Idle`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LockState {
    /// No writer holds the article; edits are not permitted.
    #[default]
    Idle,
    /// The article is being edited; `holder` is the sole permitted writer.
    Editing {
        /// The writer currently holding the lock.
        holder: WriterId,
    },
}

/// File-level metadata for an [`Article`], stored alongside its text.
///
/// v0 provenance is file-level only: this records the set of model ids that have
/// edited the article rather than per-character authorship.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArticleMeta {
    /// Human-readable title of the article.
    pub title: String,
    /// The model ids that have contributed edits to this article, in first-seen
    /// order.
    pub contributors: Vec<String>,
    /// Free-form notes (e.g. the originating task), persisted with the index.
    #[serde(default)]
    pub notes: Option<String>,
    /// The file name of this article's parent in the theme's logical hierarchy,
    /// or `None` for a top-level article.
    ///
    /// The hierarchy is expressed by parent pointers; the linear reading order
    /// lives in [`Index::order`]. An older index without this field deserializes
    /// to `None` (every article top-level), so the change is backward compatible.
    #[serde(default)]
    pub parent: Option<String>,
}

/// Theme-level configuration, persisted inside the theme's [`Index`].
///
/// This is the per-theme global config the WebUI reads and writes: a description
/// of the theme's goal, the default writing skill(s) dispatched writers run
/// under, and the model slaves write with. All fields default to empty / `None`,
/// so an older index without a `config` block deserializes to
/// [`ThemeConfig::default`].
///
/// Both a single [`default_skill`](ThemeConfig::default_skill) and an ordered
/// [`default_skill_ids`](ThemeConfig::default_skill_ids) stack are carried: the
/// stack is the kernel §10 multi-skill form (later overrides earlier on a
/// conflicting directive), and the single field is retained for back-compat. When
/// the stack is non-empty it is the authoritative selection; the single field is
/// the fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeConfig {
    /// A free-form description or goal for the theme.
    #[serde(default)]
    pub description: String,
    /// The id of the default writing skill (a `./skills/<id>.md` file) dispatched
    /// writers use, or `None` to use the engine's built-in default.
    ///
    /// Back-compat single-skill field. Prefer
    /// [`default_skill_ids`](ThemeConfig::default_skill_ids) for an ordered
    /// multi-skill stack; when both are set, the stack is authoritative and this
    /// is the fallback.
    #[serde(default)]
    pub default_skill: Option<String>,
    /// The ordered stack of default writing-skill ids dispatched writers run under
    /// (kernel §10), earliest first; on a conflicting directive a **later** id
    /// overrides an earlier one. Empty (the default) falls back to
    /// [`default_skill`](ThemeConfig::default_skill).
    ///
    /// Serialized only when non-empty, so an older index round-trips unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_skill_ids: Vec<String>,
    /// The model id slaves dispatched for this theme write under (e.g.
    /// `"deepseek-v4-pro"`), or `None` to use the engine default.
    #[serde(default)]
    pub slave_model: Option<String>,
}

/// A single plain-text article within a [`Theme`].
///
/// The article owns its on-disk relative path (under the theme), its metadata,
/// and its current [`LockState`]. The text body itself is read from / written to
/// disk on demand rather than held resident.
#[derive(Debug, Clone)]
pub struct Article {
    /// The article's path relative to the workspace root.
    pub path: PathBuf,
    /// File-level metadata and provenance.
    pub meta: ArticleMeta,
    /// The current lock state.
    pub lock: LockState,
}

impl Article {
    /// Returns `true` if `writer` may currently mutate this article (i.e. it
    /// holds the lock).
    pub fn is_writable_by(&self, writer: &WriterId) -> bool {
        matches!(&self.lock, LockState::Editing { holder } if holder == writer)
    }
}

/// A theme: a named directory containing articles and an [`Index`].
#[derive(Debug, Clone)]
pub struct Theme {
    /// The theme name (also its directory name under the workspace root).
    pub name: String,
    /// The theme directory relative to the workspace root.
    pub path: PathBuf,
}

/// The per-theme manifest recording article reading order and metadata.
///
/// Persisted as a `serde`-serialized file inside the theme directory; it is the
/// source of truth for the order in which articles are presented.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Index {
    /// Article file names (relative to the theme directory), in reading order.
    pub order: Vec<String>,
    /// Per-article metadata, keyed by the article file name.
    #[serde(default)]
    pub articles: BTreeMap<String, ArticleMeta>,
    /// Theme-level configuration (description, default skill, slave model).
    #[serde(default)]
    pub config: ThemeConfig,
}

impl Index {
    /// Appends an article to the end of the reading order, recording its
    /// metadata. A no-op for the order if the name is already present (the
    /// metadata is still updated).
    pub fn insert(&mut self, file_name: impl Into<String>, meta: ArticleMeta) {
        let file_name = file_name.into();
        if !self.order.iter().any(|n| n == &file_name) {
            self.order.push(file_name.clone());
        }
        self.articles.insert(file_name, meta);
    }

    /// Removes an article from the index by file name, returning its metadata if
    /// it was present.
    ///
    /// Any article whose `parent` pointed at the removed file is re-parented to
    /// the removed file's own parent (its children are lifted up one level),
    /// keeping the hierarchy free of dangling parent pointers.
    pub fn remove(&mut self, file_name: &str) -> Option<ArticleMeta> {
        self.order.retain(|n| n != file_name);
        let removed = self.articles.remove(file_name);
        if removed.is_some() {
            let grandparent = removed.as_ref().and_then(|m| m.parent.clone());
            for meta in self.articles.values_mut() {
                if meta.parent.as_deref() == Some(file_name) {
                    meta.parent = grandparent.clone();
                }
            }
        }
        removed
    }
}

/// One entry of a theme's article hierarchy, in reading order.
///
/// Produced by [`Workspace::article_outline`]: it pairs each article's file name
/// and title with its parent pointer and its computed `depth` (the number of
/// ancestors), so a UI can render the logical tree by indenting on `depth` while
/// honouring the linear reading order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArticleOutline {
    /// The article file name.
    pub file: String,
    /// The article's human-readable title (falls back to the file name).
    pub title: String,
    /// The parent article's file name, or `None` for a top-level article.
    pub parent: Option<String>,
    /// The depth in the hierarchy: `0` for a top-level article, `1` for its
    /// children, and so on.
    pub depth: usize,
}

/// Computes an article's depth (ancestor count) by walking parent pointers.
///
/// Bounded by the number of articles so a malformed index (a parent cycle that
/// slipped past [`Workspace::set_parent`]'s guard) cannot loop forever.
fn outline_depth(index: &Index, file: &str) -> usize {
    let mut depth = 0usize;
    let mut current = file.to_string();
    let mut guard = 0usize;
    loop {
        let parent = index.articles.get(&current).and_then(|m| m.parent.clone());
        match parent {
            Some(p) if index.articles.contains_key(&p) => {
                depth += 1;
                current = p;
                guard += 1;
                if guard > index.order.len() {
                    break;
                }
            }
            _ => break,
        }
    }
    depth
}

/// Parses a provenance tag back into an [`AuthorId`], the inverse of
/// [`AuthorId::tag`] / [`WriterId::provenance_tag`].
///
/// `"human"` maps to [`AuthorId::Human`]; any other tag is read as
/// `"<model>/<label>"` and split on the **last** `'/'` so a model id that itself
/// contains slashes is preserved. A tag with no `'/'` (an unexpected shape) is
/// treated as the whole model id with an empty label, which round-trips back to
/// the same tag. This is used only to reconstruct a legacy article's single author
/// from its recorded contributor tag.
fn author_from_tag(tag: &str) -> AuthorId {
    if tag == "human" {
        return AuthorId::Human;
    }
    match tag.rsplit_once('/') {
        Some((model, label)) => AuthorId::Agent {
            model: model.to_string(),
            label: label.to_string(),
        },
        None => AuthorId::Agent {
            model: tag.to_string(),
            label: String::new(),
        },
    }
}

/// The root of a writing workspace and the entry point for all sandboxed
/// filesystem operations.
///
/// A workspace owns a root directory; themes are its subdirectories and articles
/// are plain-text files within them. Every path passed to a workspace method is
/// resolved through [`Workspace::resolve`], which enforces the sandbox.
pub struct Workspace {
    /// The canonical absolute root directory containing all themes.
    root: PathBuf,
    /// In-memory, process-local article locks keyed by `(theme, file_name)`.
    /// Absent entries are [`LockState::Idle`].
    locks: BTreeMap<(String, String), WriterId>,
}

impl Workspace {
    /// Opens (or adopts) a workspace rooted at `root`, creating the directory if
    /// it does not yet exist.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Io`] if the root cannot be created or canonicalized.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ai_write::tool::workspace::Workspace;
    /// let dir = tempfile::tempdir().unwrap();
    /// let ws = Workspace::open(dir.path()).unwrap();
    /// assert!(ws.root().is_absolute());
    /// ```
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let root = root.as_ref();
        std::fs::create_dir_all(root)
            .map_err(|e| ToolError::Io(format!("cannot create workspace root: {e}")))?;
        let root = std::fs::canonicalize(root)
            .map_err(|e| ToolError::Io(format!("cannot canonicalize workspace root: {e}")))?;
        Ok(Workspace {
            root,
            locks: BTreeMap::new(),
        })
    }

    /// The workspace root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a workspace-relative path against the root, enforcing the
    /// sandbox.
    ///
    /// The check is performed in two layers. First, lexical: the path must be
    /// relative and contain no `..` or root components, so it cannot *name* a
    /// location outside the workspace. Second, symbolic: for every ancestor of
    /// the resolved path that already exists on disk, the canonical form is
    /// verified to stay within the canonical root, so an existing symlink cannot
    /// redirect the path outside the sandbox.
    ///
    /// The path need not exist; resolving a not-yet-created article is how
    /// `create_article` and `write_article` obtain their destination.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::SandboxViolation`] if `relative` is absolute, contains
    /// a `..` (or other non-normal) component, or resolves — via an existing
    /// symlink — outside the workspace root.
    pub fn resolve(&self, relative: impl AsRef<Path>) -> Result<PathBuf, ToolError> {
        let relative = relative.as_ref();

        // Layer 1 — lexical: only plain, forward path components are allowed.
        for component in relative.components() {
            match component {
                Component::Normal(_) => {}
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(ToolError::SandboxViolation(format!(
                        "`..` traversal is not allowed: {}",
                        relative.display()
                    )));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(ToolError::SandboxViolation(format!(
                        "absolute paths are not allowed: {}",
                        relative.display()
                    )));
                }
            }
        }

        let candidate = self.root.join(relative);

        // Layer 2 — symbolic: canonicalize the deepest existing ancestor and
        // confirm it is still inside the root. This catches a pre-existing
        // symlink whose target escapes the sandbox.
        let mut existing = candidate.as_path();
        loop {
            match std::fs::canonicalize(existing) {
                Ok(canonical) => {
                    if !canonical.starts_with(&self.root) {
                        return Err(ToolError::SandboxViolation(format!(
                            "path resolves outside the workspace: {}",
                            relative.display()
                        )));
                    }
                    break;
                }
                Err(_) => match existing.parent() {
                    // Walk up to the nearest ancestor that exists on disk.
                    Some(parent) if parent.starts_with(&self.root) => existing = parent,
                    // Reached (or passed) the root without finding an existing
                    // ancestor; the lexical check already proved the path stays
                    // under the root, so accept the lexical candidate.
                    _ => break,
                },
            }
        }

        Ok(candidate)
    }

    /// Validates a single theme or article name component.
    ///
    /// Names must be a single non-empty path segment: no separators, no `.`/`..`,
    /// and no NUL. This keeps a name from smuggling a traversal in through the
    /// `theme`/`file_name` arguments.
    fn validate_name(kind: &str, name: &str) -> Result<(), ToolError> {
        if name.is_empty() {
            return Err(ToolError::InvalidArgs(format!("{kind} name is empty")));
        }
        if name == "." || name == ".." {
            return Err(ToolError::SandboxViolation(format!(
                "{kind} name `{name}` is not allowed"
            )));
        }
        if name.contains('/') || name.contains('\\') || name.contains('\0') {
            return Err(ToolError::SandboxViolation(format!(
                "{kind} name `{name}` contains a path separator"
            )));
        }
        Ok(())
    }

    /// Resolves a theme directory, validating the name and the sandbox.
    fn theme_dir(&self, theme: &str) -> Result<PathBuf, ToolError> {
        Self::validate_name("theme", theme)?;
        self.resolve(theme)
    }

    /// Resolves an article file path, validating both name components.
    fn article_path(&self, theme: &str, file_name: &str) -> Result<PathBuf, ToolError> {
        Self::validate_name("theme", theme)?;
        Self::validate_name("article", file_name)?;
        self.resolve(Path::new(theme).join(file_name))
    }

    /// Creates a new theme directory and an empty [`Index`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::SandboxViolation`] / [`ToolError::InvalidArgs`] for an
    /// illegal name, [`ToolError::Lock`] if the theme already exists, or
    /// [`ToolError::Io`] on a filesystem failure.
    pub fn create_theme(&mut self, name: &str) -> Result<Theme, ToolError> {
        let dir = self.theme_dir(name)?;
        if dir.exists() {
            return Err(ToolError::Lock(format!("theme `{name}` already exists")));
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| ToolError::Io(format!("cannot create theme `{name}`: {e}")))?;
        self.save_index(name, &Index::default())?;
        Ok(Theme {
            name: name.to_string(),
            path: PathBuf::from(name),
        })
    }

    /// Deletes a theme directory and everything inside it.
    ///
    /// In-memory locks for articles under the theme are dropped. (v0 treats this
    /// as a hard delete; the product spec's "soft delete" is deferred.)
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the theme does not exist, or
    /// [`ToolError::Io`] on a filesystem failure.
    pub fn delete_theme(&mut self, name: &str) -> Result<(), ToolError> {
        let dir = self.theme_dir(name)?;
        if !dir.exists() {
            return Err(ToolError::NotFound(format!("theme `{name}`")));
        }
        std::fs::remove_dir_all(&dir)
            .map_err(|e| ToolError::Io(format!("cannot delete theme `{name}`: {e}")))?;
        self.locks.retain(|(theme, _), _| theme != name);
        Ok(())
    }

    /// Loads a theme's [`Index`] from disk.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the theme or its index is missing, or
    /// [`ToolError::Io`] on a read/parse failure.
    pub fn load_index(&self, theme: &str) -> Result<Index, ToolError> {
        let dir = self.theme_dir(theme)?;
        if !dir.exists() {
            return Err(ToolError::NotFound(format!("theme `{theme}`")));
        }
        let index_path = dir.join(INDEX_FILE);
        let bytes = match std::fs::read(&index_path) {
            Ok(bytes) => bytes,
            // A theme with no manifest yet behaves as an empty index.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Index::default()),
            Err(e) => {
                return Err(ToolError::Io(format!(
                    "cannot read index for `{theme}`: {e}"
                )));
            }
        };
        serde_json::from_slice(&bytes)
            .map_err(|e| ToolError::Io(format!("cannot parse index for `{theme}`: {e}")))
    }

    /// Persists a theme's [`Index`] to disk.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the theme directory does not exist, or
    /// [`ToolError::Io`] on a write/serialize failure.
    pub fn save_index(&mut self, theme: &str, index: &Index) -> Result<(), ToolError> {
        let dir = self.theme_dir(theme)?;
        if !dir.exists() {
            return Err(ToolError::NotFound(format!("theme `{theme}`")));
        }
        let json = serde_json::to_vec_pretty(index)
            .map_err(|e| ToolError::Io(format!("cannot serialize index for `{theme}`: {e}")))?;
        std::fs::write(dir.join(INDEX_FILE), json)
            .map_err(|e| ToolError::Io(format!("cannot write index for `{theme}`: {e}")))
    }

    /// Reads an article's full text.
    ///
    /// The file is refused if it exceeds the article size limit (1 MiB) or is not
    /// valid UTF-8 (treated as binary), so the model can adapt rather than receive
    /// a truncated or garbled body.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the article is missing,
    /// [`ToolError::Unsupported`] if it is too large or binary, or
    /// [`ToolError::Io`] on a read failure.
    pub fn read_article(&self, theme: &str, file_name: &str) -> Result<String, ToolError> {
        let path = self.article_path(theme, file_name)?;
        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::NotFound(format!(
                    "article `{theme}/{file_name}`"
                )));
            }
            Err(e) => {
                return Err(ToolError::Io(format!(
                    "cannot stat `{theme}/{file_name}`: {e}"
                )));
            }
        };
        if metadata.len() > MAX_ARTICLE_BYTES {
            return Err(ToolError::Unsupported(format!(
                "article `{theme}/{file_name}` is {} bytes (limit {MAX_ARTICLE_BYTES})",
                metadata.len()
            )));
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| ToolError::Io(format!("cannot read `{theme}/{file_name}`: {e}")))?;
        String::from_utf8(bytes).map_err(|_| {
            ToolError::Unsupported(format!(
                "article `{theme}/{file_name}` is not valid UTF-8 (binary)"
            ))
        })
    }

    /// Returns the writer currently holding the lock on `(theme, file_name)`, if
    /// any.
    fn lock_holder(&self, theme: &str, file_name: &str) -> Option<&WriterId> {
        self.locks.get(&(theme.to_string(), file_name.to_string()))
    }

    /// Confirms `writer` holds the lock on the article, erroring otherwise.
    fn require_lock(
        &self,
        theme: &str,
        file_name: &str,
        writer: &WriterId,
    ) -> Result<(), ToolError> {
        match self.lock_holder(theme, file_name) {
            Some(holder) if holder == writer => Ok(()),
            Some(_) => Err(ToolError::Lock(format!(
                "article `{theme}/{file_name}` is locked by another writer"
            ))),
            None => Err(ToolError::Lock(format!(
                "article `{theme}/{file_name}` is not locked by the caller; acquire the lock first"
            ))),
        }
    }

    /// Records `writer` as a contributor to the article in the theme index and
    /// persists it. Idempotent per writer tag.
    fn record_contribution(
        &mut self,
        theme: &str,
        file_name: &str,
        writer: &WriterId,
    ) -> Result<(), ToolError> {
        let tag = writer.provenance_tag();
        let mut index = self.load_index(theme)?;
        let meta = index.articles.entry(file_name.to_string()).or_default();
        if !meta.contributors.iter().any(|c| c == &tag) {
            meta.contributors.push(tag);
        }
        if !index.order.iter().any(|n| n == file_name) {
            index.order.push(file_name.to_string());
        }
        self.save_index(theme, &index)
    }

    /// Overwrites an article's full text, requiring `writer` to hold the lock.
    ///
    /// The writer is recorded as a contributor in the theme index. The article
    /// file must already exist (create it with `create_article`).
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Lock`] if `writer` does not hold the article lock,
    /// [`ToolError::NotFound`] if the article is missing, [`ToolError::Unsupported`]
    /// if the text exceeds the size limit, or [`ToolError::Io`] on a write failure.
    ///
    /// # Character-level authorship (B2)
    ///
    /// As well as overwriting the plain-text body, the write routes the change
    /// through the character-level provenance layer
    /// ([`provenance::reauthor`](crate::provenance::reauthor)): the prior
    /// authorship is loaded from the article's
    /// [provenance sidecar](Workspace::read_document), only the edited span is
    /// re-attributed to `writer`, and the updated authorship is persisted back to
    /// the sidecar as a [`Document`]. So an edit by a second writer keeps the
    /// untouched text attributed to whoever originally wrote it. The plain file
    /// remains the source of truth for reads, git history, blame and diff; the
    /// sidecar is the additional char-level layer the rich article view reads.
    pub fn write_article(
        &mut self,
        theme: &str,
        file_name: &str,
        text: &str,
        writer: &WriterId,
    ) -> Result<(), ToolError> {
        let path = self.article_path(theme, file_name)?;
        self.require_lock(theme, file_name, writer)?;
        if !path.exists() {
            return Err(ToolError::NotFound(format!(
                "article `{theme}/{file_name}`"
            )));
        }
        if text.len() as u64 > MAX_ARTICLE_BYTES {
            return Err(ToolError::Unsupported(format!(
                "new content is {} bytes (limit {MAX_ARTICLE_BYTES})",
                text.len()
            )));
        }
        std::fs::write(&path, text)
            .map_err(|e| ToolError::Io(format!("cannot write `{theme}/{file_name}`: {e}")))?;
        // Persist the character-level authorship layer alongside the plain body.
        self.reauthor_sidecar(theme, file_name, text, writer)?;
        self.record_contribution(theme, file_name, writer)
    }

    // ----- Character-level provenance sidecar (B2) --------------------------

    /// Resolves an article's provenance sidecar to an absolute, sandboxed path.
    fn sidecar_path(&self, theme: &str, file_name: &str) -> Result<PathBuf, ToolError> {
        Self::validate_name("theme", theme)?;
        Self::validate_name("article", file_name)?;
        self.resolve(Path::new(theme).join(format!("{file_name}{PROVENANCE_SIDECAR_SUFFIX}")))
    }

    /// Reads an article's character-level authorship as a [`Document`] (B2).
    ///
    /// When the article has a provenance sidecar (written by every authored edit),
    /// its stored [`Document`] is returned verbatim, so every run carries the
    /// author who wrote it. When no sidecar exists — an article created before B2,
    /// or one never edited through the authored write path — the current plain body
    /// is returned as a single run attributed to the article's **last recorded
    /// contributor** (falling back to [`AuthorId::Human`] when the index records
    /// none): the most honest single-author reconstruction available without
    /// per-character history.
    ///
    /// The returned document's [`Document::to_plain_string`](crate::content::Document::to_plain_string)
    /// equals the article's plain body, so the rich and plain views agree.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the article is missing,
    /// [`ToolError::Unsupported`] if the body is oversized or binary, or
    /// [`ToolError::Io`] on a read / parse failure.
    pub fn read_document(&self, theme: &str, file_name: &str) -> Result<Document, ToolError> {
        let sidecar = self.sidecar_path(theme, file_name)?;
        match std::fs::read(&sidecar) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                ToolError::Io(format!(
                    "cannot parse provenance sidecar for `{theme}/{file_name}`: {e}"
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No sidecar yet: reconstruct a single-author document from the
                // plain body, attributed to the last recorded contributor.
                let text = self.read_article(theme, file_name)?;
                let author = self.last_contributor(theme, file_name);
                Ok(crate::provenance::paragraph_document(
                    &RichText::from_plain(text, author),
                ))
            }
            Err(e) => Err(ToolError::Io(format!(
                "cannot read provenance sidecar for `{theme}/{file_name}`: {e}"
            ))),
        }
    }

    /// Returns the [`AuthorId`] of the article's most recently recorded
    /// contributor (the last entry in [`ArticleMeta::contributors`]), or
    /// [`AuthorId::Human`] when the index records none.
    ///
    /// Used only to attribute a legacy article (one with no provenance sidecar)
    /// when first reconstructing its [`Document`].
    fn last_contributor(&self, theme: &str, file_name: &str) -> AuthorId {
        let Ok(index) = self.load_index(theme) else {
            return AuthorId::Human;
        };
        let tag = index
            .articles
            .get(file_name)
            .and_then(|m| m.contributors.last().cloned());
        match tag {
            Some(tag) => author_from_tag(&tag),
            None => AuthorId::Human,
        }
    }

    /// Re-attributes only the changed span of an article's body to `writer` and
    /// persists the updated authorship to the provenance sidecar.
    ///
    /// Loads the prior [`Document`], flattens it to an authored body, replaces just
    /// the edited region via [`provenance::reauthor`](crate::provenance::reauthor)
    /// (so untouched text keeps its original author), splits the result back into a
    /// paragraph [`Document`], and writes it as the sidecar JSON.
    fn reauthor_sidecar(
        &self,
        theme: &str,
        file_name: &str,
        new_text: &str,
        writer: &WriterId,
    ) -> Result<(), ToolError> {
        let author = AuthorId::from(writer);
        // Flatten the prior authorship (or a fresh single-author reconstruction
        // for a legacy article), then re-attribute only the edited span.
        let prior = self.read_document(theme, file_name)?;
        let mut body = crate::provenance::flatten_body(&prior, &author);
        crate::provenance::reauthor(&mut body, new_text, &author)
            .map_err(|e| ToolError::Io(format!("cannot re-author `{theme}/{file_name}`: {e}")))?;
        let doc = crate::provenance::paragraph_document(&body);
        self.save_document(theme, file_name, &doc)
    }

    /// Persists an article's character-level authorship [`Document`] to its
    /// provenance sidecar.
    fn save_document(&self, theme: &str, file_name: &str, doc: &Document) -> Result<(), ToolError> {
        let sidecar = self.sidecar_path(theme, file_name)?;
        let json = serde_json::to_vec(doc).map_err(|e| {
            ToolError::Io(format!(
                "cannot serialize provenance sidecar for `{theme}/{file_name}`: {e}"
            ))
        })?;
        std::fs::write(&sidecar, json).map_err(|e| {
            ToolError::Io(format!(
                "cannot write provenance sidecar for `{theme}/{file_name}`: {e}"
            ))
        })
    }

    /// Acquires the single-writer lock on an article for `writer`.
    ///
    /// Re-acquiring a lock already held by the same writer is a no-op success.
    ///
    /// This is **coordinator-only** (`pub(crate)`): with the
    /// [`coordinator`](crate::coordinator) owning the operation-level lock state
    /// (kernel §6), the per-article in-memory lock is an implementation detail the
    /// coordinator takes transiently while writing. The model has no
    /// `acquire_lock` tool any more; locking is implicit per edit.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Lock`] if another writer already holds the lock, or
    /// [`ToolError::NotFound`] if the article is missing.
    pub(crate) fn acquire_lock(
        &mut self,
        theme: &str,
        file_name: &str,
        writer: &WriterId,
    ) -> Result<(), ToolError> {
        let path = self.article_path(theme, file_name)?;
        if !path.exists() {
            return Err(ToolError::NotFound(format!(
                "article `{theme}/{file_name}`"
            )));
        }
        match self.lock_holder(theme, file_name) {
            Some(holder) if holder == writer => Ok(()),
            Some(_) => Err(ToolError::Lock(format!(
                "article `{theme}/{file_name}` is already locked by another writer"
            ))),
            None => {
                self.locks
                    .insert((theme.to_string(), file_name.to_string()), writer.clone());
                Ok(())
            }
        }
    }

    /// Releases the lock on an article previously acquired by `writer`.
    ///
    /// **Coordinator-only** (`pub(crate)`), the counterpart to
    /// [`Workspace::acquire_lock`]: the per-article lock is transient state the
    /// coordinator manages, not a model-facing operation.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Lock`] if `writer` does not hold the lock.
    pub(crate) fn release_lock(
        &mut self,
        theme: &str,
        file_name: &str,
        writer: &WriterId,
    ) -> Result<(), ToolError> {
        Self::validate_name("theme", theme)?;
        Self::validate_name("article", file_name)?;
        match self.lock_holder(theme, file_name) {
            Some(holder) if holder == writer => {
                self.locks
                    .remove(&(theme.to_string(), file_name.to_string()));
                Ok(())
            }
            Some(_) => Err(ToolError::Lock(format!(
                "article `{theme}/{file_name}` is held by another writer"
            ))),
            None => Err(ToolError::Lock(format!(
                "article `{theme}/{file_name}` is not locked"
            ))),
        }
    }

    // ----- Article lifecycle (used by the native tools) ---------------------

    /// Creates a new, empty article file inside a theme and records it in the
    /// index.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the theme does not exist,
    /// [`ToolError::Lock`] if the article already exists, or [`ToolError::Io`] on a
    /// filesystem failure.
    pub fn create_article(
        &mut self,
        theme: &str,
        file_name: &str,
        title: &str,
        notes: Option<String>,
    ) -> Result<(), ToolError> {
        let dir = self.theme_dir(theme)?;
        if !dir.exists() {
            return Err(ToolError::NotFound(format!("theme `{theme}`")));
        }
        let path = self.article_path(theme, file_name)?;
        if path.exists() {
            return Err(ToolError::Lock(format!(
                "article `{theme}/{file_name}` already exists"
            )));
        }
        std::fs::write(&path, "")
            .map_err(|e| ToolError::Io(format!("cannot create `{theme}/{file_name}`: {e}")))?;
        let mut index = self.load_index(theme)?;
        index.insert(
            file_name,
            ArticleMeta {
                title: title.to_string(),
                contributors: Vec::new(),
                notes,
                parent: None,
            },
        );
        self.save_index(theme, &index)
    }

    /// Creates a new article with `content`, a `title`, and an optional `parent`,
    /// attributing it to `writer`, in one structural step.
    ///
    /// This is the **coordinator-only** primitive the cross-file split/merge
    /// transactions build new articles with (kernel §6): it writes the file body,
    /// appends the article to the reading order, records `writer` as its sole
    /// contributor, and sets its parent pointer — all by rewriting the theme index
    /// once. Unlike [`Workspace::create_article`] it does **not** require a
    /// separate `write_article` (and therefore no in-memory article lock), because
    /// the coordinator's operation-level lock already provides exclusion.
    ///
    /// The article must not already exist; `parent`, when given, must name an
    /// existing article in the theme.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the theme (or the named `parent`) does
    /// not exist, [`ToolError::Lock`] if the article already exists,
    /// [`ToolError::Unsupported`] if `content` exceeds the size limit, or
    /// [`ToolError::Io`] on a filesystem failure.
    pub(crate) fn create_article_with_content(
        &mut self,
        theme: &str,
        file_name: &str,
        content: &str,
        title: &str,
        parent: Option<&str>,
        writer: &WriterId,
    ) -> Result<(), ToolError> {
        let dir = self.theme_dir(theme)?;
        if !dir.exists() {
            return Err(ToolError::NotFound(format!("theme `{theme}`")));
        }
        let path = self.article_path(theme, file_name)?;
        if path.exists() {
            return Err(ToolError::Lock(format!(
                "article `{theme}/{file_name}` already exists"
            )));
        }
        if content.len() as u64 > MAX_ARTICLE_BYTES {
            return Err(ToolError::Unsupported(format!(
                "new content is {} bytes (limit {MAX_ARTICLE_BYTES})",
                content.len()
            )));
        }
        let mut index = self.load_index(theme)?;
        if let Some(p) = parent
            && !index.order.iter().any(|n| n == p)
        {
            return Err(ToolError::NotFound(format!("parent article `{theme}/{p}`")));
        }
        std::fs::write(&path, content)
            .map_err(|e| ToolError::Io(format!("cannot create `{theme}/{file_name}`: {e}")))?;
        // Seed the character-level authorship sidecar: the whole initial body is
        // attributed to its creating writer (B2).
        let body = RichText::from_plain(content.to_string(), AuthorId::from(writer));
        self.save_document(
            theme,
            file_name,
            &crate::provenance::paragraph_document(&body),
        )?;
        index.insert(
            file_name,
            ArticleMeta {
                title: title.to_string(),
                contributors: vec![writer.provenance_tag()],
                notes: None,
                parent: parent.map(|s| s.to_string()),
            },
        );
        self.save_index(theme, &index)
    }

    /// Deletes an article file and removes it from the index.
    ///
    /// The article must not be locked by another writer. Any lock held on it is
    /// dropped.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the article is missing,
    /// [`ToolError::Lock`] if another writer holds the lock, or [`ToolError::Io`] on
    /// a filesystem failure.
    pub fn delete_article(&mut self, theme: &str, file_name: &str) -> Result<(), ToolError> {
        let path = self.article_path(theme, file_name)?;
        if !path.exists() {
            return Err(ToolError::NotFound(format!(
                "article `{theme}/{file_name}`"
            )));
        }
        if self.lock_holder(theme, file_name).is_some() {
            return Err(ToolError::Lock(format!(
                "article `{theme}/{file_name}` is locked; release it before deleting"
            )));
        }
        std::fs::remove_file(&path)
            .map_err(|e| ToolError::Io(format!("cannot delete `{theme}/{file_name}`: {e}")))?;
        // Drop the character-level provenance sidecar alongside the body (B2). A
        // missing sidecar (legacy article) is not an error.
        if let Ok(sidecar) = self.sidecar_path(theme, file_name) {
            let _ = std::fs::remove_file(sidecar);
        }
        let mut index = self.load_index(theme)?;
        index.remove(file_name);
        self.save_index(theme, &index)
    }

    /// Lists the article file names in a theme, in index (reading) order.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the theme does not exist, or
    /// [`ToolError::Io`] on a read failure.
    pub fn list_articles(&self, theme: &str) -> Result<Vec<String>, ToolError> {
        let index = self.load_index(theme)?;
        Ok(index.order)
    }

    /// Returns a theme's article hierarchy as a flat list of [`ArticleOutline`]
    /// entries, in reading order, each carrying its parent and computed depth.
    ///
    /// This is the read model the WebUI renders the logical article tree from:
    /// the order is the linear reading order, and `depth` lets it indent each
    /// article under its parent.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the theme does not exist, or
    /// [`ToolError::Io`] on a read failure.
    pub fn article_outline(&self, theme: &str) -> Result<Vec<ArticleOutline>, ToolError> {
        let index = self.load_index(theme)?;
        let mut out = Vec::with_capacity(index.order.len());
        for file in &index.order {
            let meta = index.articles.get(file);
            let title = meta
                .map(|m| m.title.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| file.clone());
            let parent = meta.and_then(|m| m.parent.clone());
            let depth = outline_depth(&index, file);
            out.push(ArticleOutline {
                file: file.clone(),
                title,
                parent,
                depth,
            });
        }
        Ok(out)
    }

    /// Sets (or clears, with `parent = None`) an article's parent in the theme's
    /// logical hierarchy.
    ///
    /// The reading order ([`Index::order`]) is left untouched; only the parent
    /// pointer changes. Both the article and the proposed parent must already
    /// exist in the theme, an article may not be its own parent, and the new
    /// parent must not be a descendant of the article (which would form a cycle).
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the article or the proposed parent does
    /// not exist, [`ToolError::InvalidArgs`] if the parent is the article itself
    /// or would create a cycle, or [`ToolError::Io`] on a write failure.
    pub fn set_parent(
        &mut self,
        theme: &str,
        file_name: &str,
        parent: Option<&str>,
    ) -> Result<(), ToolError> {
        Self::validate_name("theme", theme)?;
        Self::validate_name("article", file_name)?;
        let mut index = self.load_index(theme)?;
        if !index.order.iter().any(|n| n == file_name) {
            return Err(ToolError::NotFound(format!(
                "article `{theme}/{file_name}`"
            )));
        }
        if let Some(p) = parent {
            if p == file_name {
                return Err(ToolError::InvalidArgs(format!(
                    "article `{file_name}` cannot be its own parent"
                )));
            }
            if !index.order.iter().any(|n| n == p) {
                return Err(ToolError::NotFound(format!("parent article `{theme}/{p}`")));
            }
            // Cycle guard: climb from the proposed parent via existing parent
            // pointers; reaching `file_name` means it is already an ancestor of
            // `p`, so re-parenting `file_name` under `p` would close a loop.
            let mut cursor = Some(p.to_string());
            let mut guard = 0usize;
            while let Some(cur) = cursor {
                if cur == file_name {
                    return Err(ToolError::InvalidArgs(format!(
                        "setting parent of `{file_name}` to `{p}` would create a cycle"
                    )));
                }
                cursor = index.articles.get(&cur).and_then(|m| m.parent.clone());
                guard += 1;
                if guard > index.order.len() {
                    break;
                }
            }
        }
        index
            .articles
            .entry(file_name.to_string())
            .or_default()
            .parent = parent.map(|s| s.to_string());
        self.save_index(theme, &index)
    }

    /// Replaces a theme's reading order with `new_order`.
    ///
    /// `new_order` must be a permutation of the theme's current articles — every
    /// existing article present exactly once, and no unknown names — so reordering
    /// can never silently drop or invent an article. Parent pointers are left
    /// untouched.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the theme does not exist,
    /// [`ToolError::InvalidArgs`] if `new_order` is not a permutation of the
    /// current articles, or [`ToolError::Io`] on a write failure.
    pub fn reorder(&mut self, theme: &str, new_order: Vec<String>) -> Result<(), ToolError> {
        Self::validate_name("theme", theme)?;
        let mut index = self.load_index(theme)?;
        let mut have = index.order.clone();
        let mut want = new_order.clone();
        have.sort();
        want.sort();
        if have != want {
            return Err(ToolError::InvalidArgs(format!(
                "reorder for `{theme}` must be a permutation of its current articles"
            )));
        }
        index.order = new_order;
        self.save_index(theme, &index)
    }

    /// Loads a theme's [`ThemeConfig`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the theme does not exist, or
    /// [`ToolError::Io`] on a read failure.
    pub fn load_config(&self, theme: &str) -> Result<ThemeConfig, ToolError> {
        Ok(self.load_index(theme)?.config)
    }

    /// Persists a theme's [`ThemeConfig`], leaving the article order and metadata
    /// untouched.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::NotFound`] if the theme does not exist, or
    /// [`ToolError::Io`] on a write failure.
    pub fn save_config(&mut self, theme: &str, config: ThemeConfig) -> Result<(), ToolError> {
        let mut index = self.load_index(theme)?;
        index.config = config;
        self.save_index(theme, &index)
    }

    // ----- Sidecar assets (kernel §7) ---------------------------------------

    /// Returns the workspace-relative sidecar asset directory for an article.
    ///
    /// The kernel's organisation model (§7) keeps an article as a single
    /// plain-text body file, but makes one honest concession: once rich media or
    /// figures enter (e.g. for a PDF render), the article becomes "one body file
    /// plus a documented sidecar convention". This is that convention: the assets
    /// for `theme/name.md` live under `theme/name.md.assets/` (the `.assets`
    /// suffix appended to the full file name). The directory is
    /// *not* created — this only computes the path.
    ///
    /// The returned path is theme-relative (e.g. `rust/intro.md.assets`), the
    /// same shape [`Theme::path`] uses, so it composes with [`Workspace::resolve`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::SandboxViolation`] / [`ToolError::InvalidArgs`] if
    /// `theme` or `file_name` is not a single legal path segment.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ai_write::tool::workspace::Workspace;
    /// let dir = tempfile::tempdir().unwrap();
    /// let ws = Workspace::open(dir.path()).unwrap();
    /// let assets = ws.asset_dir("rust", "intro.md").unwrap();
    /// assert_eq!(assets.to_str().unwrap(), "rust/intro.md.assets");
    /// ```
    pub fn asset_dir(&self, theme: &str, file_name: &str) -> Result<PathBuf, ToolError> {
        Self::validate_name("theme", theme)?;
        Self::validate_name("article", file_name)?;
        Ok(Path::new(theme).join(format!("{file_name}{ASSETS_DIR_SUFFIX}")))
    }

    /// Resolves a single sidecar asset of an article to an absolute, sandboxed
    /// path (kernel §7).
    ///
    /// The asset is named by a path *relative to the article's asset directory*
    /// ([`Workspace::asset_dir`]); subdirectories are allowed (e.g.
    /// `figures/fig1.png`). The combined path is run through the full
    /// [`Workspace::resolve`] sandbox, so a traversal attempt in `asset_path`
    /// (an absolute path, a `..` component, or an escaping symlink) is rejected
    /// rather than allowed to escape the article's asset directory.
    ///
    /// The file need not exist; this resolves the destination an asset *would*
    /// occupy, mirroring how [`Workspace::resolve`] resolves a not-yet-created
    /// article.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::SandboxViolation`] / [`ToolError::InvalidArgs`] if
    /// `theme` or `file_name` is not a single legal segment, if `asset_path` is
    /// empty, or if the resolved location would escape the workspace sandbox
    /// (absolute path, `..` traversal, or symlink escape).
    ///
    /// # Examples
    ///
    /// ```
    /// # use ai_write::tool::workspace::Workspace;
    /// let dir = tempfile::tempdir().unwrap();
    /// let ws = Workspace::open(dir.path()).unwrap();
    /// let p = ws.resolve_asset("rust", "intro.md", "figures/fig1.png").unwrap();
    /// assert!(p.starts_with(ws.root()));
    /// assert!(p.ends_with("rust/intro.md.assets/figures/fig1.png"));
    ///
    /// // A traversal attempt is rejected, not allowed to escape.
    /// assert!(ws.resolve_asset("rust", "intro.md", "../../etc/passwd").is_err());
    /// ```
    pub fn resolve_asset(
        &self,
        theme: &str,
        file_name: &str,
        asset_path: impl AsRef<Path>,
    ) -> Result<PathBuf, ToolError> {
        let asset_path = asset_path.as_ref();
        if asset_path.as_os_str().is_empty() {
            return Err(ToolError::InvalidArgs("asset path is empty".to_string()));
        }
        let relative = self.asset_dir(theme, file_name)?.join(asset_path);
        // `resolve` enforces both the lexical (no `..`, no absolute) and the
        // symbolic (no symlink escape) layers of the sandbox; an `asset_path`
        // carrying a `..` is rejected here.
        self.resolve(relative)
    }

    /// Lists an article's sidecar asset files, as paths relative to its asset
    /// directory, sorted (kernel §7).
    ///
    /// The walk is confined to the article's [`Workspace::asset_dir`] and
    /// descends into subdirectories, returning every regular file it finds (e.g.
    /// `["cover.png", "figures/fig1.png"]`). Directories themselves are not
    /// listed. If the asset directory does not exist yet, the result is empty —
    /// an article without assets is the common case and not an error.
    ///
    /// Returned paths use forward slashes only when the platform separator is
    /// `/`; they are plain [`PathBuf`]s relative to the asset directory and can
    /// be fed straight back into [`Workspace::resolve_asset`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::SandboxViolation`] / [`ToolError::InvalidArgs`] for an
    /// illegal `theme` / `file_name`, or [`ToolError::Io`] on a filesystem read
    /// failure while walking the directory.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use ai_write::tool::workspace::Workspace;
    /// let dir = tempfile::tempdir().unwrap();
    /// let ws = Workspace::open(dir.path()).unwrap();
    /// // With no assets directory yet, the list is empty.
    /// assert!(ws.list_assets("rust", "intro.md").unwrap().is_empty());
    /// ```
    pub fn list_assets(&self, theme: &str, file_name: &str) -> Result<Vec<PathBuf>, ToolError> {
        let dir_rel = self.asset_dir(theme, file_name)?;
        let dir_abs = self.resolve(&dir_rel)?;
        if !dir_abs.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        Self::walk_assets(&dir_abs, &dir_abs, &mut out)?;
        out.sort();
        Ok(out)
    }

    /// Recursively collects regular files under `dir`, pushing each as a path
    /// relative to `base`.
    fn walk_assets(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), ToolError> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            ToolError::Io(format!("cannot read asset dir `{}`: {e}", dir.display()))
        })?;
        for entry in entries {
            let entry =
                entry.map_err(|e| ToolError::Io(format!("cannot read asset dir entry: {e}")))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| ToolError::Io(format!("cannot stat `{}`: {e}", path.display())))?;
            if file_type.is_dir() {
                Self::walk_assets(base, &path, out)?;
            } else if file_type.is_file()
                && let Ok(rel) = path.strip_prefix(base)
            {
                out.push(rel.to_path_buf());
            }
            // Symlinks and other entry kinds are skipped: a sidecar asset is a
            // plain file, and a symlink could redirect outside the sandbox.
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::open(dir.path()).expect("open workspace");
        (dir, ws)
    }

    fn agent(label: &str) -> WriterId {
        WriterId::Agent {
            model: "deepseek-v4-pro".to_string(),
            label: label.to_string(),
        }
    }

    #[test]
    fn open_canonicalizes_and_creates_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a/b/c");
        let ws = Workspace::open(&nested).expect("open");
        assert!(ws.root().is_absolute());
        assert!(nested.exists());
    }

    #[test]
    fn writer_provenance_tags() {
        assert_eq!(WriterId::Human.provenance_tag(), "human");
        assert_eq!(agent("s1").provenance_tag(), "deepseek-v4-pro/s1");
    }

    #[test]
    fn article_is_writable_only_by_lock_holder() {
        let a = Article {
            path: PathBuf::from("t/x.md"),
            meta: ArticleMeta::default(),
            lock: LockState::Editing {
                holder: agent("s1"),
            },
        };
        assert!(a.is_writable_by(&agent("s1")));
        assert!(!a.is_writable_by(&agent("s2")));
        assert!(!a.is_writable_by(&WriterId::Human));

        let idle = Article {
            lock: LockState::Idle,
            ..a
        };
        assert!(!idle.is_writable_by(&agent("s1")));
    }

    #[test]
    fn index_insert_and_remove_maintain_order() {
        let mut idx = Index::default();
        idx.insert("a.md", ArticleMeta::default());
        idx.insert("b.md", ArticleMeta::default());
        idx.insert("a.md", ArticleMeta::default()); // duplicate: no reorder
        assert_eq!(idx.order, vec!["a.md", "b.md"]);
        assert_eq!(idx.articles.len(), 2);

        let removed = idx.remove("a.md");
        assert!(removed.is_some());
        assert_eq!(idx.order, vec!["b.md"]);
        assert!(!idx.articles.contains_key("a.md"));
        assert!(idx.remove("missing.md").is_none());
    }

    // ----- Sandbox ----------------------------------------------------------

    #[test]
    fn resolve_accepts_relative_paths() {
        let (_d, ws) = ws();
        let p = ws.resolve("theme/article.md").expect("relative ok");
        assert!(p.starts_with(ws.root()));
    }

    #[test]
    fn resolve_rejects_parent_traversal() {
        let (_d, ws) = ws();
        let err = ws.resolve("../escape.txt").unwrap_err();
        assert!(matches!(err, ToolError::SandboxViolation(_)));
        let err = ws.resolve("theme/../../escape.txt").unwrap_err();
        assert!(matches!(err, ToolError::SandboxViolation(_)));
    }

    #[test]
    fn resolve_rejects_absolute_paths() {
        let (_d, ws) = ws();
        let err = ws.resolve("/etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::SandboxViolation(_)));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let (_d, ws) = ws();
        // Create a symlink inside the workspace pointing outside it.
        let outside = tempfile::tempdir().expect("outside dir");
        let link = ws.root().join("escape");
        symlink(outside.path(), &link).expect("symlink");
        let err = ws.resolve("escape/secret.txt").unwrap_err();
        assert!(
            matches!(err, ToolError::SandboxViolation(_)),
            "symlink escape must be rejected, got {err:?}"
        );
    }

    #[test]
    fn validate_name_rejects_separators_and_dots() {
        assert!(Workspace::validate_name("theme", "ok").is_ok());
        assert!(matches!(
            Workspace::validate_name("theme", "a/b"),
            Err(ToolError::SandboxViolation(_))
        ));
        assert!(matches!(
            Workspace::validate_name("theme", ".."),
            Err(ToolError::SandboxViolation(_))
        ));
        assert!(matches!(
            Workspace::validate_name("theme", ""),
            Err(ToolError::InvalidArgs(_))
        ));
    }

    // ----- Theme / index lifecycle -----------------------------------------

    #[test]
    fn create_theme_writes_empty_index() {
        let (_d, mut ws) = ws();
        let theme = ws.create_theme("rust").expect("create theme");
        assert_eq!(theme.name, "rust");
        let index = ws.load_index("rust").expect("load index");
        assert!(index.order.is_empty());
        // Re-creating the same theme errors.
        assert!(matches!(ws.create_theme("rust"), Err(ToolError::Lock(_))));
    }

    #[test]
    fn delete_theme_removes_dir_and_locks() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        ws.acquire_lock("t", "a.md", &agent("s1")).unwrap();
        ws.delete_theme("t").expect("delete theme");
        assert!(matches!(ws.load_index("t"), Err(ToolError::NotFound(_))));
        assert!(ws.locks.is_empty());
        assert!(matches!(ws.delete_theme("t"), Err(ToolError::NotFound(_))));
    }

    // ----- Article lifecycle + index maintenance ----------------------------

    #[test]
    fn create_article_maintains_index() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "Title A", Some("task".into()))
            .unwrap();
        ws.create_article("t", "b.md", "Title B", None).unwrap();
        assert_eq!(ws.list_articles("t").unwrap(), vec!["a.md", "b.md"]);
        let index = ws.load_index("t").unwrap();
        assert_eq!(index.articles["a.md"].title, "Title A");
        assert_eq!(index.articles["a.md"].notes.as_deref(), Some("task"));

        // Duplicate create fails.
        assert!(matches!(
            ws.create_article("t", "a.md", "dup", None),
            Err(ToolError::Lock(_))
        ));
        // Create in missing theme fails.
        assert!(matches!(
            ws.create_article("nope", "x.md", "x", None),
            Err(ToolError::NotFound(_))
        ));
    }

    #[test]
    fn delete_article_maintains_index() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        ws.create_article("t", "b.md", "B", None).unwrap();
        ws.delete_article("t", "a.md").expect("delete");
        assert_eq!(ws.list_articles("t").unwrap(), vec!["b.md"]);
        assert!(matches!(
            ws.delete_article("t", "a.md"),
            Err(ToolError::NotFound(_))
        ));
    }

    #[test]
    fn delete_article_rejected_while_locked() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        ws.acquire_lock("t", "a.md", &agent("s1")).unwrap();
        assert!(matches!(
            ws.delete_article("t", "a.md"),
            Err(ToolError::Lock(_))
        ));
    }

    // ----- Read / write + size / binary -------------------------------------

    #[test]
    fn write_then_read_round_trips() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        ws.acquire_lock("t", "a.md", &agent("s1")).unwrap();
        ws.write_article("t", "a.md", "hello world", &agent("s1"))
            .expect("write");
        assert_eq!(ws.read_article("t", "a.md").unwrap(), "hello world");
        // Contribution recorded.
        let index = ws.load_index("t").unwrap();
        assert_eq!(
            index.articles["a.md"].contributors,
            vec!["deepseek-v4-pro/s1".to_string()]
        );
    }

    #[test]
    fn pinned_dated_model_id_round_trips_into_contributors() {
        // Kernel §9: a pinned dated snapshot id must be recorded as the article's
        // contributor tag, so file-level provenance names the exact model.
        let (_d, mut ws) = ws();
        let pinned = WriterId::Agent {
            model: "deepseek-v4-pro-2026-05-01".to_string(),
            label: "s1".to_string(),
        };
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        ws.acquire_lock("t", "a.md", &pinned).unwrap();
        ws.write_article("t", "a.md", "snapshot-authored", &pinned)
            .expect("write");

        let index = ws.load_index("t").unwrap();
        assert_eq!(
            index.articles["a.md"].contributors,
            vec!["deepseek-v4-pro-2026-05-01/s1".to_string()]
        );
        // The contributor tag is exactly the writer's provenance tag.
        assert_eq!(
            index.articles["a.md"].contributors[0],
            pinned.provenance_tag()
        );
    }

    #[test]
    fn read_missing_article_is_not_found() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        assert!(matches!(
            ws.read_article("t", "ghost.md"),
            Err(ToolError::NotFound(_))
        ));
    }

    #[test]
    fn read_binary_article_is_unsupported() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "bin.md", "B", None).unwrap();
        // Write raw invalid UTF-8 directly to disk.
        let path = ws.article_path("t", "bin.md").unwrap();
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        assert!(matches!(
            ws.read_article("t", "bin.md"),
            Err(ToolError::Unsupported(_))
        ));
    }

    #[test]
    fn read_oversized_article_is_unsupported() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "big.md", "B", None).unwrap();
        let path = ws.article_path("t", "big.md").unwrap();
        let big = vec![b'a'; (MAX_ARTICLE_BYTES + 1) as usize];
        std::fs::write(&path, big).unwrap();
        assert!(matches!(
            ws.read_article("t", "big.md"),
            Err(ToolError::Unsupported(_))
        ));
    }

    // ----- Locks ------------------------------------------------------------

    #[test]
    fn write_without_lock_is_rejected() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        assert!(matches!(
            ws.write_article("t", "a.md", "x", &agent("s1")),
            Err(ToolError::Lock(_))
        ));
    }

    #[test]
    fn single_writer_enforced() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        ws.acquire_lock("t", "a.md", &agent("s1")).unwrap();
        // Second writer cannot acquire.
        assert!(matches!(
            ws.acquire_lock("t", "a.md", &agent("s2")),
            Err(ToolError::Lock(_))
        ));
        // Second writer cannot write.
        assert!(matches!(
            ws.write_article("t", "a.md", "x", &agent("s2")),
            Err(ToolError::Lock(_))
        ));
        // Re-acquire by holder is fine.
        ws.acquire_lock("t", "a.md", &agent("s1")).unwrap();
    }

    #[test]
    fn release_lock_rules() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        // Releasing an unlocked article errors.
        assert!(matches!(
            ws.release_lock("t", "a.md", &agent("s1")),
            Err(ToolError::Lock(_))
        ));
        ws.acquire_lock("t", "a.md", &agent("s1")).unwrap();
        // Wrong writer cannot release.
        assert!(matches!(
            ws.release_lock("t", "a.md", &agent("s2")),
            Err(ToolError::Lock(_))
        ));
        // Holder releases, then can re-acquire.
        ws.release_lock("t", "a.md", &agent("s1")).unwrap();
        ws.acquire_lock("t", "a.md", &agent("s2")).unwrap();
    }

    #[test]
    fn acquire_lock_on_missing_article_is_not_found() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        assert!(matches!(
            ws.acquire_lock("t", "ghost.md", &agent("s1")),
            Err(ToolError::NotFound(_))
        ));
    }

    // ----- Hierarchy: parent pointers, reorder, outline ---------------------

    /// Creates a theme with `files` articles (top-level, in order).
    fn themed(files: &[&str]) -> (tempfile::TempDir, Workspace) {
        let (d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        for f in files {
            ws.create_article("t", f, f, None).unwrap();
        }
        (d, ws)
    }

    #[test]
    fn set_parent_builds_hierarchy_and_outline_depths() {
        let (_d, mut ws) = themed(&["a.md", "b.md", "c.md"]);
        ws.set_parent("t", "b.md", Some("a.md")).unwrap();
        ws.set_parent("t", "c.md", Some("b.md")).unwrap();

        let outline = ws.article_outline("t").unwrap();
        let depths: Vec<(String, usize)> =
            outline.iter().map(|o| (o.file.clone(), o.depth)).collect();
        assert_eq!(
            depths,
            vec![
                ("a.md".to_string(), 0),
                ("b.md".to_string(), 1),
                ("c.md".to_string(), 2),
            ]
        );
        assert_eq!(outline[1].parent.as_deref(), Some("a.md"));

        // Clearing a parent lifts the article back to the top level.
        ws.set_parent("t", "b.md", None).unwrap();
        let outline = ws.article_outline("t").unwrap();
        assert_eq!(outline[1].depth, 0);
        // c.md's parent (b.md) is now top-level, so c.md is depth 1.
        assert_eq!(outline[2].depth, 1);
    }

    #[test]
    fn set_parent_rejects_self_cycle_and_missing() {
        let (_d, mut ws) = themed(&["a.md", "b.md"]);
        // Self-parent.
        assert!(matches!(
            ws.set_parent("t", "a.md", Some("a.md")),
            Err(ToolError::InvalidArgs(_))
        ));
        // Missing parent.
        assert!(matches!(
            ws.set_parent("t", "a.md", Some("ghost.md")),
            Err(ToolError::NotFound(_))
        ));
        // Missing article.
        assert!(matches!(
            ws.set_parent("t", "ghost.md", None),
            Err(ToolError::NotFound(_))
        ));
        // Cycle: a -> b, then b -> a is rejected.
        ws.set_parent("t", "b.md", Some("a.md")).unwrap();
        assert!(matches!(
            ws.set_parent("t", "a.md", Some("b.md")),
            Err(ToolError::InvalidArgs(_))
        ));
    }

    #[test]
    fn delete_article_reparents_children_to_grandparent() {
        let (_d, mut ws) = themed(&["a.md", "b.md", "c.md"]);
        ws.set_parent("t", "b.md", Some("a.md")).unwrap();
        ws.set_parent("t", "c.md", Some("b.md")).unwrap();
        // Deleting the middle node lifts c.md under a.md.
        ws.delete_article("t", "b.md").unwrap();
        let outline = ws.article_outline("t").unwrap();
        let c = outline.iter().find(|o| o.file == "c.md").unwrap();
        assert_eq!(c.parent.as_deref(), Some("a.md"));
        assert_eq!(c.depth, 1);
    }

    #[test]
    fn reorder_requires_a_permutation() {
        let (_d, mut ws) = themed(&["a.md", "b.md", "c.md"]);
        ws.reorder("t", vec!["c.md".into(), "a.md".into(), "b.md".into()])
            .unwrap();
        assert_eq!(ws.list_articles("t").unwrap(), vec!["c.md", "a.md", "b.md"]);

        // Dropping an article is rejected.
        assert!(matches!(
            ws.reorder("t", vec!["a.md".into(), "b.md".into()]),
            Err(ToolError::InvalidArgs(_))
        ));
        // Inventing an article is rejected.
        assert!(matches!(
            ws.reorder(
                "t",
                vec!["a.md".into(), "b.md".into(), "c.md".into(), "x.md".into()]
            ),
            Err(ToolError::InvalidArgs(_))
        ));
    }

    #[test]
    fn theme_config_round_trips_through_index() {
        let (_d, mut ws) = themed(&["a.md"]);
        // Default is empty.
        assert_eq!(ws.load_config("t").unwrap(), ThemeConfig::default());

        let cfg = ThemeConfig {
            description: "a guide".into(),
            default_skill: Some("functional-writing".into()),
            default_skill_ids: vec!["functional-writing".into(), "concise".into()],
            slave_model: Some("deepseek-v4-pro".into()),
        };
        ws.save_config("t", cfg.clone()).unwrap();
        // The full config (including the multi-skill stack) round-trips through
        // the on-disk index.
        assert_eq!(ws.load_config("t").unwrap(), cfg);

        // Saving config leaves the article order intact.
        assert_eq!(ws.list_articles("t").unwrap(), vec!["a.md"]);
    }

    #[test]
    fn old_index_without_parent_or_config_deserializes() {
        // A legacy index.json with neither `parent` on articles nor a `config`
        // block must load (serde defaults), proving backward compatibility.
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        let legacy = r#"{
            "order": ["a.md"],
            "articles": { "a.md": { "title": "A", "contributors": [] } }
        }"#;
        let dir = ws.theme_dir("t").unwrap();
        std::fs::write(dir.join(INDEX_FILE), legacy).unwrap();

        let index = ws.load_index("t").unwrap();
        assert!(index.articles["a.md"].parent.is_none());
        assert_eq!(index.config, ThemeConfig::default());
        let outline = ws.article_outline("t").unwrap();
        assert_eq!(outline[0].depth, 0);
    }

    // ----- Sidecar assets (kernel §7) ---------------------------------------

    #[test]
    fn asset_dir_appends_suffix_to_full_file_name() {
        let (_d, ws) = ws();
        let dir = ws.asset_dir("rust", "intro.md").unwrap();
        assert_eq!(dir, PathBuf::from("rust/intro.md.assets"));
        // `a.md` and `a.txt` never collide on the same assets directory.
        let md = ws.asset_dir("t", "a.md").unwrap();
        let txt = ws.asset_dir("t", "a.txt").unwrap();
        assert_ne!(md, txt);
    }

    #[test]
    fn asset_dir_rejects_illegal_names() {
        let (_d, ws) = ws();
        assert!(matches!(
            ws.asset_dir("a/b", "x.md"),
            Err(ToolError::SandboxViolation(_))
        ));
        assert!(matches!(
            ws.asset_dir("t", ".."),
            Err(ToolError::SandboxViolation(_))
        ));
        assert!(matches!(
            ws.asset_dir("t", ""),
            Err(ToolError::InvalidArgs(_))
        ));
    }

    #[test]
    fn resolve_asset_stays_inside_sandbox() {
        let (_d, ws) = ws();
        let p = ws
            .resolve_asset("rust", "intro.md", "figures/fig1.png")
            .unwrap();
        assert!(p.starts_with(ws.root()));
        assert!(p.ends_with("rust/intro.md.assets/figures/fig1.png"));
    }

    #[test]
    fn resolve_asset_rejects_traversal() {
        let (_d, ws) = ws();
        // `..` inside the asset path must not escape the asset directory.
        assert!(matches!(
            ws.resolve_asset("t", "a.md", "../../etc/passwd"),
            Err(ToolError::SandboxViolation(_))
        ));
        assert!(matches!(
            ws.resolve_asset("t", "a.md", "../sibling.png"),
            Err(ToolError::SandboxViolation(_))
        ));
        // An absolute asset path is rejected.
        assert!(matches!(
            ws.resolve_asset("t", "a.md", "/etc/passwd"),
            Err(ToolError::SandboxViolation(_))
        ));
        // An empty asset path is rejected.
        assert!(matches!(
            ws.resolve_asset("t", "a.md", ""),
            Err(ToolError::InvalidArgs(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_asset_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        // Create the asset dir and a symlink inside it pointing outside the ws.
        let assets_abs = ws.resolve(ws.asset_dir("t", "a.md").unwrap()).unwrap();
        std::fs::create_dir_all(&assets_abs).unwrap();
        let outside = tempfile::tempdir().expect("outside dir");
        symlink(outside.path(), assets_abs.join("escape")).expect("symlink");
        assert!(matches!(
            ws.resolve_asset("t", "a.md", "escape/secret.png"),
            Err(ToolError::SandboxViolation(_))
        ));
    }

    #[test]
    fn list_assets_empty_when_no_dir() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        assert!(ws.list_assets("t", "a.md").unwrap().is_empty());
    }

    #[test]
    fn list_assets_returns_nested_files_sorted() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();

        // Write a few assets, including a nested one, via the resolver.
        for rel in ["cover.png", "figures/fig1.png", "figures/fig2.png"] {
            let abs = ws.resolve_asset("t", "a.md", rel).unwrap();
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(&abs, b"binary").unwrap();
        }

        let listed = ws.list_assets("t", "a.md").unwrap();
        assert_eq!(
            listed,
            vec![
                PathBuf::from("cover.png"),
                PathBuf::from("figures/fig1.png"),
                PathBuf::from("figures/fig2.png"),
            ]
        );
        // Each listed path round-trips back through resolve_asset.
        for rel in &listed {
            let abs = ws.resolve_asset("t", "a.md", rel).unwrap();
            assert!(abs.exists());
        }
    }

    // ----- B2: character-level provenance sidecar ---------------------------

    /// The `(text, author-tag)` view of a document body, flattened across blocks.
    fn doc_shape(doc: &Document) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for block in &doc.blocks {
            let runs = match block {
                crate::content::Block::Paragraph(t)
                | crate::content::Block::ListItem(t)
                | crate::content::Block::Quote(t) => &t.runs,
                crate::content::Block::Heading { text, .. } => &text.runs,
                crate::content::Block::CodeBlock { .. } => continue,
            };
            for run in runs {
                out.push((run.text.clone(), run.author.tag()));
            }
        }
        out
    }

    #[test]
    fn author_from_tag_round_trips() {
        assert_eq!(author_from_tag("human"), AuthorId::Human);
        assert_eq!(
            author_from_tag("deepseek-v4-pro/slave-1"),
            AuthorId::Agent {
                model: "deepseek-v4-pro".into(),
                label: "slave-1".into()
            }
        );
        // The split is on the LAST slash, so a model id is preserved verbatim and
        // the tag round-trips.
        let a = author_from_tag("deepseek-v4-pro/slave-1");
        assert_eq!(a.tag(), "deepseek-v4-pro/slave-1");
    }

    #[test]
    fn write_article_persists_char_level_authorship() {
        // Human writes the whole body; an agent rewrites just one word. The
        // sidecar must keep the untouched text the human's and the edited word the
        // agent's, while the plain read still returns the joined text.
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();

        ws.acquire_lock("t", "a.md", &WriterId::Human).unwrap();
        ws.write_article("t", "a.md", "the quick brown fox", &WriterId::Human)
            .unwrap();
        ws.release_lock("t", "a.md", &WriterId::Human).unwrap();

        ws.acquire_lock("t", "a.md", &agent("s1")).unwrap();
        ws.write_article("t", "a.md", "the quick red fox", &agent("s1"))
            .unwrap();
        ws.release_lock("t", "a.md", &agent("s1")).unwrap();

        // Plain read still returns the joined text (back-compatible).
        assert_eq!(ws.read_article("t", "a.md").unwrap(), "the quick red fox");

        // The rich document attributes each run to whoever wrote it.
        let doc = ws.read_document("t", "a.md").unwrap();
        assert_eq!(doc.to_plain_string(), "the quick red fox");
        assert_eq!(
            doc_shape(&doc),
            vec![
                ("the quick ".to_string(), "human".to_string()),
                ("red".to_string(), "deepseek-v4-pro/s1".to_string()),
                (" fox".to_string(), "human".to_string()),
            ]
        );
    }

    #[test]
    fn three_writer_edits_each_persist_their_authorship() {
        // human -> agent edits one word -> human edits another word. Each
        // contribution survives the others' later edits.
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();

        let writes = [
            (WriterId::Human, "one two three four"),
            (agent("s1"), "one TWO three four"),
            (WriterId::Human, "one TWO three FOUR"),
        ];
        for (writer, text) in &writes {
            ws.acquire_lock("t", "a.md", writer).unwrap();
            ws.write_article("t", "a.md", text, writer).unwrap();
            ws.release_lock("t", "a.md", writer).unwrap();
        }

        let doc = ws.read_document("t", "a.md").unwrap();
        // "TWO" stays the agent's even after a later human edit.
        let shape = doc_shape(&doc);
        let two = shape.iter().find(|(t, _)| t.contains("TWO")).unwrap();
        assert_eq!(two.1, "deepseek-v4-pro/s1");
        // The human owns the rest, including the later "FOUR".
        assert!(
            shape
                .iter()
                .any(|(t, a)| t.contains("FOUR") && a == "human")
        );
    }

    #[test]
    fn read_document_for_legacy_article_uses_last_contributor() {
        // An article whose plain body exists but whose sidecar was removed (a
        // pre-B2 article) reconstructs a single-author document from its last
        // recorded contributor.
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        ws.acquire_lock("t", "a.md", &agent("s1")).unwrap();
        ws.write_article("t", "a.md", "legacy body", &agent("s1"))
            .unwrap();
        ws.release_lock("t", "a.md", &agent("s1")).unwrap();

        // Simulate a pre-B2 article by deleting just the sidecar.
        let sidecar = ws.sidecar_path("t", "a.md").unwrap();
        std::fs::remove_file(&sidecar).unwrap();

        let doc = ws.read_document("t", "a.md").unwrap();
        assert_eq!(doc.to_plain_string(), "legacy body");
        // The whole body is attributed to the last recorded contributor.
        assert_eq!(
            doc_shape(&doc),
            vec![("legacy body".to_string(), "deepseek-v4-pro/s1".to_string())]
        );
    }

    #[test]
    fn create_article_with_content_seeds_sidecar_authored_by_writer() {
        // The coordinator's split/merge new-article primitive attributes the whole
        // initial body to its creating writer.
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article_with_content("t", "n.md", "fresh body", "N", None, &agent("s2"))
            .unwrap();
        let doc = ws.read_document("t", "n.md").unwrap();
        assert_eq!(
            doc_shape(&doc),
            vec![("fresh body".to_string(), "deepseek-v4-pro/s2".to_string())]
        );
    }

    #[test]
    fn multi_paragraph_body_round_trips_through_sidecar() {
        // A body with blank-line-separated paragraphs must round-trip exactly
        // through the flatten -> reauthor -> paragraph_document path, and an edit
        // confined to the second paragraph must not disturb the first's author.
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();

        ws.acquire_lock("t", "a.md", &WriterId::Human).unwrap();
        ws.write_article("t", "a.md", "first para\n\nsecond para", &WriterId::Human)
            .unwrap();
        ws.release_lock("t", "a.md", &WriterId::Human).unwrap();

        ws.acquire_lock("t", "a.md", &agent("s1")).unwrap();
        ws.write_article("t", "a.md", "first para\n\nsecond PARA", &agent("s1"))
            .unwrap();
        ws.release_lock("t", "a.md", &agent("s1")).unwrap();

        let doc = ws.read_document("t", "a.md").unwrap();
        // Exact plain round-trip.
        assert_eq!(doc.to_plain_string(), "first para\n\nsecond PARA");
        assert_eq!(doc.blocks.len(), 2);
        // First paragraph is wholly the human's; the agent only owns its edit.
        let shape = doc_shape(&doc);
        assert!(shape.iter().any(|(t, a)| t == "first para" && a == "human"));
        assert!(
            shape
                .iter()
                .any(|(t, a)| t.contains("PARA") && a == "deepseek-v4-pro/s1")
        );
    }

    #[test]
    fn delete_article_removes_sidecar() {
        let (_d, mut ws) = ws();
        ws.create_theme("t").unwrap();
        ws.create_article("t", "a.md", "A", None).unwrap();
        ws.acquire_lock("t", "a.md", &agent("s1")).unwrap();
        ws.write_article("t", "a.md", "body", &agent("s1")).unwrap();
        ws.release_lock("t", "a.md", &agent("s1")).unwrap();
        let sidecar = ws.sidecar_path("t", "a.md").unwrap();
        assert!(sidecar.exists(), "sidecar written");
        ws.delete_article("t", "a.md").unwrap();
        assert!(!sidecar.exists(), "sidecar removed with the article");
    }
}
