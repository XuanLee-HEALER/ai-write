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

use crate::tool::ToolError;

/// The file name of the per-theme [`Index`] manifest, stored inside the theme
/// directory.
const INDEX_FILE: &str = "index.json";

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
    /// A human is `"human"`; an agent is `"<model>/<label>"`.
    fn provenance_tag(&self) -> String {
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
    pub fn remove(&mut self, file_name: &str) -> Option<ArticleMeta> {
        self.order.retain(|n| n != file_name);
        self.articles.remove(file_name)
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
        self.record_contribution(theme, file_name, writer)
    }

    /// Acquires the single-writer lock on an article for `writer`.
    ///
    /// Re-acquiring a lock already held by the same writer is a no-op success.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Lock`] if another writer already holds the lock, or
    /// [`ToolError::NotFound`] if the article is missing.
    pub fn acquire_lock(
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
    /// # Errors
    ///
    /// Returns [`ToolError::Lock`] if `writer` does not hold the lock.
    pub fn release_lock(
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
}
