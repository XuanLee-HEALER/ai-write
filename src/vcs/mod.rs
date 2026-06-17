//! Version control for the workspace, backed by libgit2 (the `git2` crate).
//!
//! The workspace root is a git repository. Each successful edit to an article is
//! recorded as a commit, so the workspace gains a per-file history, unified
//! diffs between any two versions, and an article-level undo. This is the
//! file-level provenance backbone described in `docs/impl-v1.md` §1: the git
//! author of every commit is the [`WriterId`] that made the edit, mapped to a
//! synthetic `"<name> <email>"` signature.
//!
//! # Model
//!
//! - [`Vcs::open_or_init`] adopts an existing repository at the workspace root,
//!   or runs the equivalent of `git init` if there is no `.git` directory yet.
//! - [`Vcs::commit_file`] stages **only** the named file (plus, when relevant,
//!   the theme index file the caller passes in a separate call) and creates one
//!   commit. It never stages the whole tree (`add -A`), so concurrent edits to
//!   *other* articles never leak into an unrelated commit.
//! - [`Vcs::history`] lists the commits that touched one file, newest first.
//! - [`Vcs::diff`] renders a unified patch for one file between two revisions
//!   (or against the working tree / the empty tree, when an endpoint is
//!   omitted).
//! - [`Vcs::restore`] and [`Vcs::undo_last`] implement article-level undo by
//!   **restoring then re-committing** — history is never rewritten with `reset`,
//!   so every revert is itself an auditable commit (`docs/impl-v1.md` §1,
//!   decision V2).
//!
//! # Author / committer identity
//!
//! A [`WriterId`] is rendered to a git [`git2::Signature`] as
//! `"<name> <agent@ai-write.local>"`, where the name is `"human"` for
//! [`WriterId::Human`] and `"<model>/<label>"` for an agent (matching the
//! file-level provenance tag recorded in the theme index). Humans get
//! `human@ai-write.local`; agents get `agent@ai-write.local`. The email is
//! synthetic and stable — it only has to be a syntactically valid address for
//! libgit2; it is never delivered to.
//!
//! # Concurrency
//!
//! [`Vcs`] is **not** `Sync`: it wraps a single [`git2::Repository`] handle,
//! which is not thread-safe. v0/v1 hold one workspace (and thus one `Vcs`)
//! behind the orchestration layer, and the workspace's single-writer article
//! lock already serializes edits to any one file, so commits never race.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//! use ai_write::tool::workspace::WriterId;
//! use ai_write::vcs::Vcs;
//!
//! # fn main() -> Result<(), ai_write::vcs::VcsError> {
//! let vcs = Vcs::open_or_init(Path::new("workspace"))?;
//! let author = WriterId::Agent {
//!     model: "deepseek-v4-flash".to_string(),
//!     label: "slave-1".to_string(),
//! };
//! let sha = vcs.commit_file(
//!     Path::new("rust/article.md"),
//!     &author,
//!     "edit(rust/article.md): write full draft",
//! )?;
//! println!("committed {sha}");
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};

use git2::{DiffFormat, DiffOptions, ErrorCode, ObjectType, Oid, Repository, Signature};

use crate::tool::workspace::WriterId;

/// The synthetic email domain used for every git signature this module creates.
///
/// The author/committer email has to be a syntactically valid address for
/// libgit2, but it is never used to deliver anything — it just disambiguates the
/// two writer kinds at the email level (`human@…` vs `agent@…`).
const EMAIL_DOMAIN: &str = "ai-write.local";

/// The number of hex characters in an abbreviated ("short") commit SHA returned
/// by [`Vcs::commit_file`] / [`Vcs::restore`] / [`Vcs::undo_last`].
const SHORT_SHA_LEN: usize = 10;

/// An error produced by a [`Vcs`] operation.
///
/// Wraps the underlying [`git2::Error`] for any libgit2 failure, and adds a few
/// semantic variants for conditions this module detects itself (a path that is
/// not valid UTF-8, or a revision/file with no recorded history). It is
/// `#[non_exhaustive]`: callers matching on it must include a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VcsError {
    /// A libgit2 operation failed. The inner [`git2::Error`] carries the class
    /// and message.
    #[error("git error: {0}")]
    Git(#[from] git2::Error),

    /// A relative path could not be represented as UTF-8, which libgit2 requires
    /// for index entries and pathspecs.
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),

    /// The requested file or revision has no history to operate on — for example,
    /// [`Vcs::undo_last`] was asked to revert a file with fewer than two commits,
    /// or [`Vcs::history`] / [`Vcs::diff`] referenced a commit that does not
    /// exist.
    #[error("no such history: {0}")]
    NoHistory(String),
}

/// A single commit in an article's history, as returned by [`Vcs::history`].
///
/// The fields are flattened, owned, and serializable so the WebUI layer can emit
/// them directly as JSON without reaching back into libgit2.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommitInfo {
    /// The abbreviated commit SHA (the first 10 hex characters).
    pub id: String,
    /// The commit author, rendered as `"<name> <email>"`.
    pub author: String,
    /// The commit message (first line plus any body, verbatim).
    pub message: String,
    /// The author time, as a Unix timestamp in seconds.
    pub time: i64,
}

/// A handle to the workspace's git repository.
///
/// Construct one with [`Vcs::open_or_init`]; it owns the [`git2::Repository`]
/// and exposes the commit / history / diff / restore operations the tool layer
/// calls after a successful edit. See the [module docs](self) for the model.
pub struct Vcs {
    /// The underlying libgit2 repository handle, rooted at the workspace root.
    repo: Repository,
}

impl Vcs {
    /// Opens the git repository at `root`, initializing a fresh one if `root` is
    /// not yet a repository.
    ///
    /// This is the version-control counterpart to
    /// [`Workspace::open`](crate::tool::workspace::Workspace::open): the same
    /// directory that holds themes and articles is the git work tree. An existing
    /// repository is adopted as-is; an empty or plain directory is initialized
    /// with a default `.git` (no initial commit is created — the first commit is
    /// produced by the first [`Vcs::commit_file`]).
    ///
    /// # Errors
    ///
    /// Returns [`VcsError::Git`] if `root` exists but cannot be opened as a
    /// repository, or if initialization fails (for example, the directory is not
    /// writable).
    ///
    /// # Examples
    ///
    /// ```
    /// # use ai_write::vcs::Vcs;
    /// let dir = tempfile::tempdir().unwrap();
    /// let vcs = Vcs::open_or_init(dir.path()).unwrap();
    /// # let _ = vcs;
    /// ```
    pub fn open_or_init(root: &Path) -> Result<Self, VcsError> {
        let repo = match Repository::open(root) {
            Ok(repo) => repo,
            Err(e) if e.code() == ErrorCode::NotFound => Repository::init(root)?,
            Err(e) => return Err(VcsError::Git(e)),
        };
        Ok(Vcs { repo })
    }

    /// Stages the single file `rel` and records it as one commit authored by
    /// `author`, returning the abbreviated SHA.
    ///
    /// Only `rel` is added to the index — the rest of the work tree is left
    /// untouched, so a commit never sweeps in unrelated edits. To also version a
    /// theme index (`index.json`) alongside the article, call `commit_file` again
    /// for that path; each call is its own commit, matching the "one edit, one
    /// commit" granularity (`docs/impl-v1.md` §1, decision V1).
    ///
    /// The commit's author and committer are both derived from `author` (see the
    /// [module docs](self)). When the repository already has commits, the new
    /// commit's parent is the current `HEAD`; the very first commit is a root
    /// commit with no parents.
    ///
    /// # Errors
    ///
    /// Returns [`VcsError::NonUtf8Path`] if `rel` is not valid UTF-8, or
    /// [`VcsError::Git`] if the file is missing from the work tree, staging
    /// fails, or writing the commit fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use ai_write::tool::workspace::WriterId;
    /// # use ai_write::vcs::Vcs;
    /// # fn main() -> Result<(), ai_write::vcs::VcsError> {
    /// let vcs = Vcs::open_or_init(Path::new("workspace"))?;
    /// let sha = vcs.commit_file(
    ///     Path::new("rust/intro.md"),
    ///     &WriterId::Human,
    ///     "edit(rust/intro.md): human revision",
    /// )?;
    /// # let _ = sha;
    /// # Ok(())
    /// # }
    /// ```
    pub fn commit_file(
        &self,
        rel: &Path,
        author: &WriterId,
        message: &str,
    ) -> Result<String, VcsError> {
        let rel_str = path_str(rel)?;
        let mut index = self.repo.index()?;
        // Stage only the named path. `add_path` records the current work-tree
        // content of exactly this file; nothing else is touched.
        index.add_path(Path::new(rel_str))?;
        index.write()?;
        let tree_oid = index.write_tree()?;
        let tree = self.repo.find_tree(tree_oid)?;

        let sig = signature(author)?;
        let parents = self.head_commit_vec()?;
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)?;
        Ok(short_sha(oid))
    }

    /// Returns the commits that touched `rel`, newest first.
    ///
    /// The history is computed by walking from `HEAD` in topological + time
    /// order and keeping only the commits whose change set includes `rel` (a
    /// commit is included when the file's blob differs from at least one parent,
    /// or when it first appears in a root commit). A file that has never been
    /// committed yields an empty vector — not an error.
    ///
    /// # Errors
    ///
    /// Returns [`VcsError::NonUtf8Path`] if `rel` is not valid UTF-8, or
    /// [`VcsError::Git`] if the revision walk or a tree lookup fails. An empty
    /// repository (no commits at all) yields an empty vector.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use ai_write::vcs::Vcs;
    /// # fn main() -> Result<(), ai_write::vcs::VcsError> {
    /// let vcs = Vcs::open_or_init(Path::new("workspace"))?;
    /// for c in vcs.history(Path::new("rust/intro.md"))? {
    ///     println!("{} {} {}", c.id, c.author, c.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn history(&self, rel: &Path) -> Result<Vec<CommitInfo>, VcsError> {
        let rel_str = path_str(rel)?.to_string();
        // An empty repository has no HEAD; that is simply "no history yet".
        if self.repo.head().is_err() {
            return Ok(Vec::new());
        }

        let mut walk = self.repo.revwalk()?;
        walk.push_head()?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)?;

        let mut out = Vec::new();
        for oid in walk {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            if self.commit_touches(&commit, &rel_str)? {
                let author = commit.author();
                out.push(CommitInfo {
                    id: short_sha(oid),
                    author: signature_string(&author),
                    message: commit.message().unwrap_or("").to_string(),
                    time: author.when().seconds(),
                });
            }
        }
        Ok(out)
    }

    /// Renders a unified diff for `rel` between two revisions.
    ///
    /// `from` and `to` are revision specifiers resolvable by
    /// [`Repository::revparse_single`] (a full or abbreviated SHA, `HEAD`,
    /// `HEAD~1`, …). The endpoints are interpreted as:
    ///
    /// - `from = Some(a)`, `to = Some(b)` — the patch turning the file at `a`
    ///   into the file at `b`.
    /// - `from = None` — the empty tree (so the patch shows the file's full
    ///   content at `to` as additions).
    /// - `to = None` — the current **work tree** (so the patch shows uncommitted
    ///   changes to the file relative to `from`).
    ///
    /// The result is a standard unified patch limited to `rel`; an empty string
    /// means the file is identical between the two endpoints.
    ///
    /// # Errors
    ///
    /// Returns [`VcsError::NonUtf8Path`] if `rel` is not valid UTF-8,
    /// [`VcsError::NoHistory`] if a named revision cannot be resolved, or
    /// [`VcsError::Git`] if computing the diff fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use ai_write::vcs::Vcs;
    /// # fn main() -> Result<(), ai_write::vcs::VcsError> {
    /// let vcs = Vcs::open_or_init(Path::new("workspace"))?;
    /// let patch = vcs.diff(Path::new("rust/intro.md"), Some("HEAD~1"), Some("HEAD"))?;
    /// print!("{patch}");
    /// # Ok(())
    /// # }
    /// ```
    pub fn diff(
        &self,
        rel: &Path,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<String, VcsError> {
        let rel_str = path_str(rel)?;

        let mut opts = DiffOptions::new();
        opts.pathspec(rel_str);

        let from_tree = match from {
            Some(rev) => Some(self.revspec_tree(rev)?),
            None => None,
        };

        let diff = match to {
            Some(rev) => {
                let to_tree = self.revspec_tree(rev)?;
                self.repo
                    .diff_tree_to_tree(from_tree.as_ref(), Some(&to_tree), Some(&mut opts))?
            }
            None => self
                .repo
                .diff_tree_to_workdir_with_index(from_tree.as_ref(), Some(&mut opts))?,
        };

        let mut patch = String::new();
        diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
            // The origin character ('+', '-', ' ', or a header marker) precedes
            // the line content; headers ('F', 'H', 'B') carry their own prefix.
            match line.origin() {
                '+' | '-' | ' ' => patch.push(line.origin()),
                _ => {}
            }
            patch.push_str(&String::from_utf8_lossy(line.content()));
            true
        })?;
        Ok(patch)
    }

    /// Restores `rel` to its content at `commit` and records that restoration as
    /// a new commit, returning the new abbreviated SHA.
    ///
    /// This is the building block of article-level undo (`docs/impl-v1.md` §1,
    /// decision V2): rather than rewriting history with `reset`, the file's blob
    /// at `commit` is written back into the work tree and committed afresh, so
    /// the revert is itself an auditable entry authored by `author`. If `rel`
    /// already matches `commit`, a new (empty-change) commit is still created so
    /// the action is always recorded.
    ///
    /// # Errors
    ///
    /// Returns [`VcsError::NonUtf8Path`] if `rel` is not valid UTF-8,
    /// [`VcsError::NoHistory`] if `commit` cannot be resolved or does not contain
    /// `rel`, or [`VcsError::Git`] if writing the file or the commit fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use ai_write::tool::workspace::WriterId;
    /// # use ai_write::vcs::Vcs;
    /// # fn main() -> Result<(), ai_write::vcs::VcsError> {
    /// let vcs = Vcs::open_or_init(Path::new("workspace"))?;
    /// let sha = vcs.restore(Path::new("rust/intro.md"), "HEAD~2", &WriterId::Human)?;
    /// # let _ = sha;
    /// # Ok(())
    /// # }
    /// ```
    pub fn restore(&self, rel: &Path, commit: &str, author: &WriterId) -> Result<String, VcsError> {
        let rel_str = path_str(rel)?.to_string();
        let tree = self.revspec_tree(commit)?;
        let entry = tree.get_path(Path::new(&rel_str)).map_err(|_| {
            VcsError::NoHistory(format!("`{rel_str}` not present at revision `{commit}`"))
        })?;
        let blob = self.repo.find_blob(entry.id())?;

        // Write the historical content back into the work tree, then commit it as
        // a brand-new edit. History is preserved (no reset).
        let abs = self
            .repo
            .workdir()
            .ok_or_else(|| VcsError::Git(git2::Error::from_str("repository has no work tree")))?;
        std::fs::write(abs.join(&rel_str), blob.content())
            .map_err(|e| VcsError::Git(git2::Error::from_str(&format!("write failed: {e}"))))?;

        let message = format!(
            "restore({rel_str}): revert to {}",
            short_sha(tree_commit_oid(&self.repo, commit)?)
        );
        self.commit_file(Path::new(&rel_str), author, &message)
    }

    /// Reverts `rel` to its previous committed version, recording the revert as a
    /// new commit, and returns the new abbreviated SHA — or `None` if there is no
    /// previous version to revert to.
    ///
    /// "Previous" is the second-most-recent commit that touched `rel`: the file's
    /// editor-style undo. The revert reuses [`Vcs::restore`], so it preserves
    /// history rather than rewriting it. When `rel` has only one (or zero)
    /// commits in its history there is nothing to undo and `Ok(None)` is
    /// returned.
    ///
    /// # Errors
    ///
    /// Returns [`VcsError::NonUtf8Path`] if `rel` is not valid UTF-8, or
    /// [`VcsError::Git`] if walking the history or writing the revert commit
    /// fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use ai_write::tool::workspace::WriterId;
    /// # use ai_write::vcs::Vcs;
    /// # fn main() -> Result<(), ai_write::vcs::VcsError> {
    /// let vcs = Vcs::open_or_init(Path::new("workspace"))?;
    /// match vcs.undo_last(Path::new("rust/intro.md"), &WriterId::Human)? {
    ///     Some(sha) => println!("reverted, new commit {sha}"),
    ///     None => println!("nothing to undo"),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn undo_last(&self, rel: &Path, author: &WriterId) -> Result<Option<String>, VcsError> {
        let history = self.history(rel)?;
        // Need at least a current version and a prior version to undo to.
        let Some(prev) = history.get(1) else {
            return Ok(None);
        };
        let sha = self.restore(rel, &prev.id, author)?;
        Ok(Some(sha))
    }

    // ----- Internal helpers -------------------------------------------------

    /// Returns the current `HEAD` commit as a single-element vector, or an empty
    /// vector when the repository has no commits yet (an unborn `HEAD`).
    ///
    /// This shape feeds [`Repository::commit`] directly as its `parents` slice.
    fn head_commit_vec(&self) -> Result<Vec<git2::Commit<'_>>, VcsError> {
        match self.repo.head() {
            Ok(head) => {
                let commit = head.peel_to_commit()?;
                Ok(vec![commit])
            }
            // Unborn branch (no commits yet): the next commit is the root.
            Err(e) if e.code() == ErrorCode::UnbornBranch || e.code() == ErrorCode::NotFound => {
                Ok(Vec::new())
            }
            Err(e) => Err(VcsError::Git(e)),
        }
    }

    /// Resolves a revision specifier to its [`git2::Tree`].
    ///
    /// Maps a resolution failure to [`VcsError::NoHistory`] (the caller named a
    /// revision that does not exist) rather than a raw libgit2 error.
    fn revspec_tree(&self, rev: &str) -> Result<git2::Tree<'_>, VcsError> {
        let obj = self
            .repo
            .revparse_single(rev)
            .map_err(|_| VcsError::NoHistory(format!("cannot resolve revision `{rev}`")))?;
        let tree = obj.peel(ObjectType::Tree)?.peel_to_tree()?;
        Ok(tree)
    }

    /// Returns `true` if `commit` changed `rel` relative to its first parent (or
    /// introduced `rel` in a root commit).
    fn commit_touches(&self, commit: &git2::Commit, rel: &str) -> Result<bool, VcsError> {
        let tree = commit.tree()?;
        let this_entry = tree.get_path(Path::new(rel)).ok().map(|e| e.id());

        if commit.parent_count() == 0 {
            // Root commit: the file is "touched" iff it exists in this tree.
            return Ok(this_entry.is_some());
        }
        // Touched iff the blob differs from any parent's version of the file.
        for i in 0..commit.parent_count() {
            let parent = commit.parent(i)?;
            let parent_entry = parent.tree()?.get_path(Path::new(rel)).ok().map(|e| e.id());
            if parent_entry != this_entry {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Resolves a revision specifier to the [`Oid`] of the commit it names.
///
/// Used to render a short SHA inside a restore commit message.
fn tree_commit_oid(repo: &Repository, rev: &str) -> Result<Oid, VcsError> {
    let obj = repo
        .revparse_single(rev)
        .map_err(|_| VcsError::NoHistory(format!("cannot resolve revision `{rev}`")))?;
    let commit = obj
        .peel(ObjectType::Commit)
        .map_err(|_| VcsError::NoHistory(format!("`{rev}` is not a commit")))?;
    Ok(commit.id())
}

/// Renders a workspace-relative path as the UTF-8 `str` libgit2 wants, or
/// [`VcsError::NonUtf8Path`].
fn path_str(rel: &Path) -> Result<&str, VcsError> {
    rel.to_str()
        .ok_or_else(|| VcsError::NonUtf8Path(rel.to_path_buf()))
}

/// Truncates a full [`Oid`] to a short, human-facing SHA of [`SHORT_SHA_LEN`]
/// hex characters.
fn short_sha(oid: Oid) -> String {
    let full = oid.to_string();
    full.chars().take(SHORT_SHA_LEN).collect()
}

/// Derives the git author/committer name from a [`WriterId`].
///
/// `"human"` for a human, `"<model>/<label>"` for an agent — the same shape as
/// the file-level provenance tag recorded in the theme index, so a commit's
/// author lines up with `ArticleMeta::contributors`.
fn author_name(writer: &WriterId) -> String {
    match writer {
        WriterId::Human => "human".to_string(),
        WriterId::Agent { model, label } => format!("{model}/{label}"),
    }
}

/// Derives the synthetic email for a [`WriterId`]: `human@…` or `agent@…` under
/// [`EMAIL_DOMAIN`].
fn author_email(writer: &WriterId) -> String {
    let local = match writer {
        WriterId::Human => "human",
        WriterId::Agent { .. } => "agent",
    };
    format!("{local}@{EMAIL_DOMAIN}")
}

/// Builds a libgit2 [`Signature`] (with the current time) for a [`WriterId`].
fn signature(writer: &WriterId) -> Result<Signature<'static>, VcsError> {
    let name = author_name(writer);
    let email = author_email(writer);
    Ok(Signature::now(&name, &email)?)
}

/// Renders a libgit2 [`Signature`] back to the `"<name> <email>"` form stored in
/// [`CommitInfo::author`].
fn signature_string(sig: &Signature<'_>) -> String {
    let name = sig.name().unwrap_or("");
    let email = sig.email().unwrap_or("");
    format!("{name} <{email}>")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a temp-dir-backed [`Vcs`] plus a [`Workspace`]-like work tree, and
    /// returns the temp dir (kept alive by the caller) and the `Vcs`.
    fn repo() -> (tempfile::TempDir, Vcs) {
        let dir = tempfile::tempdir().expect("tempdir");
        let vcs = Vcs::open_or_init(dir.path()).expect("open_or_init");
        (dir, vcs)
    }

    fn agent(label: &str) -> WriterId {
        WriterId::Agent {
            model: "deepseek-v4-flash".to_string(),
            label: label.to_string(),
        }
    }

    /// Writes `content` to `rel` under the repo's work tree, creating parent
    /// directories as needed.
    fn write(dir: &tempfile::TempDir, rel: &str, content: &str) {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn read(dir: &tempfile::TempDir, rel: &str) -> String {
        std::fs::read_to_string(dir.path().join(rel)).unwrap()
    }

    #[test]
    fn open_or_init_creates_then_reopens_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!dir.path().join(".git").exists());
        let _vcs = Vcs::open_or_init(dir.path()).expect("init");
        assert!(dir.path().join(".git").exists());
        // Re-opening adopts the existing repo without error.
        let _again = Vcs::open_or_init(dir.path()).expect("reopen");
    }

    #[test]
    fn commit_file_returns_a_short_sha() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "hello");
        let sha = vcs
            .commit_file(Path::new("t/a.md"), &agent("s1"), "edit(t/a.md): first")
            .expect("commit");
        assert_eq!(sha.len(), SHORT_SHA_LEN);
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn commit_only_stages_the_named_file() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "A");
        write(&dir, "t/b.md", "B");
        // Commit only a.md; b.md must stay untracked / uncommitted.
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "edit a")
            .expect("commit a");
        let hist_a = vcs.history(Path::new("t/a.md")).unwrap();
        let hist_b = vcs.history(Path::new("t/b.md")).unwrap();
        assert_eq!(hist_a.len(), 1);
        assert!(hist_b.is_empty(), "b.md must not have been committed");
    }

    #[test]
    fn history_lists_commits_newest_first() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "v1");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();
        write(&dir, "t/a.md", "v2");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v2")
            .unwrap();
        write(&dir, "t/a.md", "v3");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v3")
            .unwrap();

        let hist = vcs.history(Path::new("t/a.md")).unwrap();
        let messages: Vec<&str> = hist.iter().map(|c| c.message.as_str()).collect();
        assert_eq!(messages, vec!["v3", "v2", "v1"]);
    }

    #[test]
    fn history_records_author_identity() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "x");
        vcs.commit_file(Path::new("t/a.md"), &agent("slave-7"), "edit")
            .unwrap();
        let human_dir = dir; // keep alive
        write(&human_dir, "t/a.md", "y");
        vcs.commit_file(Path::new("t/a.md"), &WriterId::Human, "human edit")
            .unwrap();

        let hist = vcs.history(Path::new("t/a.md")).unwrap();
        assert_eq!(hist[0].author, "human <human@ai-write.local>");
        assert_eq!(
            hist[1].author,
            "deepseek-v4-flash/slave-7 <agent@ai-write.local>"
        );
    }

    #[test]
    fn history_of_unknown_file_is_empty_not_error() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "x");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "edit")
            .unwrap();
        assert!(vcs.history(Path::new("t/ghost.md")).unwrap().is_empty());
    }

    #[test]
    fn diff_between_two_versions_shows_change() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "line one\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();
        write(&dir, "t/a.md", "line two\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v2")
            .unwrap();

        let patch = vcs
            .diff(Path::new("t/a.md"), Some("HEAD~1"), Some("HEAD"))
            .unwrap();
        assert!(patch.contains("-line one"), "patch was: {patch}");
        assert!(patch.contains("+line two"), "patch was: {patch}");
    }

    #[test]
    fn diff_from_none_shows_full_addition() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "alpha\nbeta\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();
        let patch = vcs.diff(Path::new("t/a.md"), None, Some("HEAD")).unwrap();
        assert!(patch.contains("+alpha"));
        assert!(patch.contains("+beta"));
    }

    #[test]
    fn diff_unresolvable_revision_is_no_history() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "x");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();
        let err = vcs
            .diff(Path::new("t/a.md"), Some("deadbeef"), Some("HEAD"))
            .unwrap_err();
        assert!(matches!(err, VcsError::NoHistory(_)));
    }

    #[test]
    fn undo_last_reverts_content_and_adds_a_commit() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "original\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();
        write(&dir, "t/a.md", "changed\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v2")
            .unwrap();
        assert_eq!(read(&dir, "t/a.md"), "changed\n");

        let before = vcs.history(Path::new("t/a.md")).unwrap().len();
        let sha = vcs
            .undo_last(Path::new("t/a.md"), &WriterId::Human)
            .unwrap()
            .expect("there is a prior version");
        assert_eq!(sha.len(), SHORT_SHA_LEN);

        // Content is reverted to the previous version on disk.
        assert_eq!(read(&dir, "t/a.md"), "original\n");
        // A new commit was added (history grows; nothing was rewritten).
        let after = vcs.history(Path::new("t/a.md")).unwrap();
        assert_eq!(after.len(), before + 1);
        assert_eq!(after[0].id, sha);
        assert_eq!(after[0].author, "human <human@ai-write.local>");
    }

    #[test]
    fn undo_last_with_single_commit_returns_none() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "only\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();
        assert!(
            vcs.undo_last(Path::new("t/a.md"), &WriterId::Human)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn undo_last_on_untracked_file_returns_none() {
        let (_dir, vcs) = repo();
        assert!(
            vcs.undo_last(Path::new("t/never.md"), &WriterId::Human)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn restore_to_specific_commit_round_trips_content() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "one\n");
        let _v1 = vcs
            .commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();
        write(&dir, "t/a.md", "two\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v2")
            .unwrap();
        write(&dir, "t/a.md", "three\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v3")
            .unwrap();

        // Restore to the v1 commit (HEAD~2) and confirm disk + a new commit.
        vcs.restore(Path::new("t/a.md"), "HEAD~2", &agent("s2"))
            .unwrap();
        assert_eq!(read(&dir, "t/a.md"), "one\n");
        let hist = vcs.history(Path::new("t/a.md")).unwrap();
        assert_eq!(hist.len(), 4, "restore adds a commit, never rewrites");
    }

    #[test]
    fn restore_missing_path_at_revision_is_no_history() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "x");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();
        let err = vcs
            .restore(Path::new("t/other.md"), "HEAD", &agent("s1"))
            .unwrap_err();
        assert!(matches!(err, VcsError::NoHistory(_)));
    }

    #[test]
    fn first_commit_is_a_root_commit() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "x");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "root")
            .unwrap();
        let head = vcs.repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.parent_count(), 0);
    }
}
