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
//! - [`Vcs::commit_paths`] stages **only** the named paths and creates **one**
//!   commit covering all of them — the primitive behind "one cognitive unit =
//!   one commit" (`docs/coordinator-design.md` §5): an article plus its theme
//!   index file are versioned together in a single atomic commit.
//!   [`Vcs::commit_file`] is the one-path convenience wrapper. Neither stages the
//!   whole tree (`add -A`), so concurrent edits to *other* articles never leak
//!   into an unrelated commit.
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

/// A single line's authorship, as returned by [`Vcs::blame`].
///
/// One entry per line of the file's current committed content, giving the
/// line-level attribution the kernel calls for (`docs/ai-write-kernel.html` §9:
/// "`git blame` gives line-level attribution"). The fields are flattened, owned,
/// and serializable so the WebUI layer can emit them directly as JSON.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlameLine {
    /// The 1-based line number within the file.
    pub line_no: usize,
    /// The author that last touched this line, rendered as `"<name> <email>"` —
    /// the same shape as [`CommitInfo::author`], so `"human <human@…>"` for a
    /// human edit and `"<model>/<label> <agent@…>"` for an agent edit.
    pub author: String,
    /// The abbreviated SHA (the first 10 hex characters) of the commit that last
    /// touched this line.
    pub short_sha: String,
}

/// A handle to the workspace's git repository.
///
/// Construct one with [`Vcs::open_or_init`]; it owns the [`git2::Repository`]
/// and exposes the commit / history / diff / restore / blame operations the tool
/// layer calls after a successful edit. See the [module docs](self) for the model.
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

    /// Stages every path in `paths` and records them as **one** commit authored
    /// by `author`, returning the abbreviated SHA.
    ///
    /// This is the atomic-commit primitive behind "one cognitive unit = one
    /// commit" (`docs/coordinator-design.md` §5): the caller hands in every path
    /// a single logical edit touched — an article body plus the theme index
    /// (`index.json`) it belongs to, say — and they all land in the same commit.
    /// Only the listed paths are added to the index; the rest of the work tree
    /// is left untouched, so a commit never sweeps in unrelated edits.
    ///
    /// A path that exists in the work tree is staged with `index.add_path`, which
    /// records its current content; a path that is **absent** from the work tree is
    /// staged as a removal with `index.remove_path`, so a transaction that deleted
    /// a file (a merge consuming its sources, say) records the deletion in the same
    /// commit. After all paths are staged the index is written once and a single
    /// tree/commit pair is produced. The commit's author and committer are both
    /// derived from `author` (see the [module docs](self)). When the repository
    /// already has commits, the new commit's parent is the current `HEAD`; the very
    /// first commit is a root commit with no parents.
    ///
    /// # Errors
    ///
    /// Returns [`VcsError::NoHistory`] if `paths` is empty (there is nothing to
    /// commit), [`VcsError::NonUtf8Path`] if any path is not valid UTF-8, or
    /// [`VcsError::Git`] if staging fails or writing the commit fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use ai_write::tool::workspace::WriterId;
    /// # use ai_write::vcs::Vcs;
    /// # fn main() -> Result<(), ai_write::vcs::VcsError> {
    /// let vcs = Vcs::open_or_init(Path::new("workspace"))?;
    /// // Version the article body and its theme index in a single commit.
    /// let sha = vcs.commit_paths(
    ///     &[Path::new("rust/intro.md"), Path::new("rust/index.json")],
    ///     &WriterId::Human,
    ///     "edit(rust/intro.md): human revision",
    /// )?;
    /// # let _ = sha;
    /// # Ok(())
    /// # }
    /// ```
    pub fn commit_paths(
        &self,
        paths: &[&Path],
        author: &WriterId,
        message: &str,
    ) -> Result<String, VcsError> {
        if paths.is_empty() {
            return Err(VcsError::NoHistory(
                "commit_paths requires at least one path".to_string(),
            ));
        }

        let workdir = self.repo.workdir();
        let mut index = self.repo.index()?;
        // Stage only the named paths; nothing else is touched. A path that still
        // exists on disk is staged with its current content; a path that has been
        // removed from the work tree is staged as a deletion, so a transaction that
        // deleted a file records the removal in this same commit.
        for rel in paths {
            let rel_str = path_str(rel)?;
            let rel_path = Path::new(rel_str);
            let exists = workdir.map(|w| w.join(rel_path).exists()).unwrap_or(false);
            if exists {
                index.add_path(rel_path)?;
            } else if index.get_path(rel_path, 0).is_some() {
                // Tracked but gone from disk: stage its removal. An untracked path
                // that was created and deleted within the same transaction is
                // simply not in the index, so there is nothing to stage.
                index.remove_path(rel_path)?;
            }
        }
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

    /// Stages the single file `rel` and records it as one commit authored by
    /// `author`, returning the abbreviated SHA.
    ///
    /// A convenience wrapper over [`Vcs::commit_paths`] with a one-element slice.
    /// Only `rel` is added to the index — the rest of the work tree is left
    /// untouched, so a commit never sweeps in unrelated edits. To version a
    /// theme index (`index.json`) **together** with the article in one atomic
    /// commit, use [`Vcs::commit_paths`] with both paths instead of two separate
    /// `commit_file` calls (`docs/coordinator-design.md` §5).
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
        self.commit_paths(&[rel], author, message)
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

    /// Returns the per-line authorship of `rel` as committed at `HEAD`, one
    /// [`BlameLine`] per line, in ascending line order.
    ///
    /// This is the line-level provenance the kernel describes
    /// (`docs/ai-write-kernel.html` §9): `git blame` resolves, for every line of
    /// the file's committed content, which commit (and therefore which
    /// [`WriterId`] — human vs a specific model snapshot) last changed it. The
    /// blame runs against the version of `rel` at `HEAD`, not the working tree, so
    /// uncommitted edits are not attributed; commit first to see them.
    ///
    /// libgit2 reports authorship in *hunks* (a run of consecutive lines sharing a
    /// final commit); this method expands each hunk into its individual lines so
    /// the caller gets a flat, line-indexed vector. Line numbers are 1-based. A
    /// file that has never been committed yields an empty vector — not an error.
    ///
    /// # Errors
    ///
    /// Returns [`VcsError::NonUtf8Path`] if `rel` is not valid UTF-8, or
    /// [`VcsError::Git`] if the blame fails for any reason other than the file
    /// being absent from history (for which an empty vector is returned). An empty
    /// repository (no commits at all) also yields an empty vector.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use ai_write::vcs::Vcs;
    /// # fn main() -> Result<(), ai_write::vcs::VcsError> {
    /// let vcs = Vcs::open_or_init(Path::new("workspace"))?;
    /// for line in vcs.blame(Path::new("rust/intro.md"))? {
    ///     println!("{:>4} {} {}", line.line_no, line.short_sha, line.author);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn blame(&self, rel: &Path) -> Result<Vec<BlameLine>, VcsError> {
        // Validate UTF-8 up front (libgit2 needs it) and short-circuit an empty
        // repository: there is nothing committed to attribute yet.
        let _ = path_str(rel)?;
        if self.repo.head().is_err() {
            return Ok(Vec::new());
        }

        let blame = match self.repo.blame_file(rel, None) {
            Ok(blame) => blame,
            // The file is not part of HEAD's history (never committed, or absent
            // at this revision): no attribution, not an error.
            Err(e) if e.code() == ErrorCode::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(VcsError::Git(e)),
        };

        let mut out = Vec::new();
        for hunk in blame.iter() {
            let author = hunk
                .final_signature()
                .map(|sig| signature_string(&sig))
                .unwrap_or_default();
            let short_sha = short_sha(hunk.final_commit_id());
            let start = hunk.final_start_line();
            // `final_start_line` is the 1-based number of the hunk's first line in
            // the final file; expand the run into one entry per line.
            for offset in 0..hunk.lines_in_hunk() {
                out.push(BlameLine {
                    line_no: start + offset,
                    author: author.clone(),
                    short_sha: short_sha.clone(),
                });
            }
        }
        // Hunks arrive in file order, but make the per-line ordering explicit and
        // robust regardless of iteration order.
        out.sort_by_key(|l| l.line_no);
        Ok(out)
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
    fn pinned_dated_model_id_round_trips_into_commit_author() {
        // Kernel §9: an exact dated snapshot id must survive into the git author,
        // so the commit names the precise model that produced the text.
        let (dir, vcs) = repo();
        let pinned = WriterId::Agent {
            model: "deepseek-v4-pro-2026-05-01".to_string(),
            label: "s1".to_string(),
        };
        write(&dir, "t/a.md", "snapshot-authored body");
        vcs.commit_file(Path::new("t/a.md"), &pinned, "edit(t/a.md): write")
            .expect("commit");

        let hist = vcs.history(Path::new("t/a.md")).unwrap();
        assert_eq!(hist.len(), 1);
        // The author line carries the pinned id verbatim, matching the writer's
        // provenance tag (`<model>/<label>`).
        assert_eq!(
            hist[0].author,
            "deepseek-v4-pro-2026-05-01/s1 <agent@ai-write.local>"
        );
        assert!(hist[0].author.contains(&pinned.provenance_tag()));
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
    fn commit_paths_commits_multiple_paths_as_one_commit() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "article body\n");
        write(&dir, "t/index.json", "{\"order\":[]}\n");

        let sha = vcs
            .commit_paths(
                &[Path::new("t/a.md"), Path::new("t/index.json")],
                &agent("s1"),
                "edit(t/a.md): body + index in one commit",
            )
            .expect("commit_paths");

        // Each path's history records exactly one touching commit...
        let hist_a = vcs.history(Path::new("t/a.md")).unwrap();
        let hist_idx = vcs.history(Path::new("t/index.json")).unwrap();
        assert_eq!(hist_a.len(), 1, "a.md touched by exactly one commit");
        assert_eq!(
            hist_idx.len(),
            1,
            "index.json touched by exactly one commit"
        );

        // ...and it is the *same* commit (one cognitive unit = one commit).
        assert_eq!(hist_a[0].id, sha);
        assert_eq!(hist_idx[0].id, sha);
        assert_eq!(hist_a[0].id, hist_idx[0].id);

        // The repository has exactly one commit in total.
        let mut walk = vcs.repo.revwalk().unwrap();
        walk.push_head().unwrap();
        assert_eq!(walk.count(), 1, "exactly one new commit was created");
    }

    #[test]
    fn commit_paths_empty_slice_is_error() {
        let (_dir, vcs) = repo();
        let err = vcs
            .commit_paths(&[], &agent("s1"), "nothing to commit")
            .unwrap_err();
        assert!(matches!(err, VcsError::NoHistory(_)), "got: {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn commit_paths_non_utf8_path_is_non_utf8_path() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let (_dir, vcs) = repo();
        // 0xFF is never valid UTF-8.
        let bad = Path::new(OsStr::from_bytes(b"t/\xff.md"));
        let err = vcs
            .commit_paths(&[bad], &agent("s1"), "bad path")
            .unwrap_err();
        assert!(matches!(err, VcsError::NonUtf8Path(_)), "got: {err:?}");
    }

    #[test]
    fn commit_file_delegates_to_commit_paths() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "solo\n");
        let sha = vcs
            .commit_file(Path::new("t/a.md"), &agent("s1"), "solo edit")
            .expect("commit_file");
        let hist = vcs.history(Path::new("t/a.md")).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].id, sha);
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

    // ----- blame: per-line authorship ---------------------------------------

    #[test]
    fn blame_of_uncommitted_file_is_empty_not_error() {
        let (_dir, vcs) = repo();
        // Nothing committed at all: blame yields an empty vector, never an error.
        assert!(vcs.blame(Path::new("t/a.md")).unwrap().is_empty());
    }

    #[test]
    fn blame_unknown_file_is_empty_not_error() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "x\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();
        // A path absent from HEAD's history attributes to nothing, not an error.
        assert!(vcs.blame(Path::new("t/ghost.md")).unwrap().is_empty());
    }

    #[test]
    fn blame_single_commit_attributes_every_line_to_its_author() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "line one\nline two\nline three\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();

        let blame = vcs.blame(Path::new("t/a.md")).unwrap();
        assert_eq!(blame.len(), 3, "one entry per line");
        // Line numbers are 1-based and contiguous.
        assert_eq!(
            blame.iter().map(|l| l.line_no).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        // Every line is attributed to the single agent author.
        for line in &blame {
            assert_eq!(line.author, "deepseek-v4-flash/s1 <agent@ai-write.local>");
            assert_eq!(line.short_sha.len(), SHORT_SHA_LEN);
        }
    }

    #[test]
    fn blame_maps_each_line_to_the_writer_that_last_touched_it() {
        // Kernel §9: line-level attribution must distinguish human from model.
        // Build a two-writer history in a temp repo and assert each line maps to
        // the expected author.
        let (dir, vcs) = repo();

        // v1: the agent writes a three-line draft. All three lines are the agent's.
        write(&dir, "t/a.md", "agent first\nagent second\nagent third\n");
        let sha_v1 = vcs
            .commit_file(Path::new("t/a.md"), &agent("slave-1"), "v1: agent draft")
            .unwrap();

        // v2: a human revises only the *middle* line, leaving lines 1 and 3 as the
        // agent's. Git blame must split the file: lines 1 & 3 → agent commit,
        // line 2 → the human commit.
        write(&dir, "t/a.md", "agent first\nhuman revised\nagent third\n");
        let sha_v2 = vcs
            .commit_file(Path::new("t/a.md"), &WriterId::Human, "v2: human edits l2")
            .unwrap();

        let blame = vcs.blame(Path::new("t/a.md")).unwrap();
        assert_eq!(blame.len(), 3);

        const AGENT: &str = "deepseek-v4-flash/slave-1 <agent@ai-write.local>";
        const HUMAN: &str = "human <human@ai-write.local>";

        // Line 1: untouched by the human → still the agent's v1 commit.
        assert_eq!(blame[0].line_no, 1);
        assert_eq!(blame[0].author, AGENT);
        assert_eq!(blame[0].short_sha, sha_v1);

        // Line 2: the human's v2 commit.
        assert_eq!(blame[1].line_no, 2);
        assert_eq!(blame[1].author, HUMAN);
        assert_eq!(blame[1].short_sha, sha_v2);

        // Line 3: untouched by the human → still the agent's v1 commit.
        assert_eq!(blame[2].line_no, 3);
        assert_eq!(blame[2].author, AGENT);
        assert_eq!(blame[2].short_sha, sha_v1);
    }

    #[test]
    fn blame_reflects_only_committed_content_not_the_work_tree() {
        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "committed line\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();
        // Dirty the work tree without committing; blame still reflects HEAD.
        write(&dir, "t/a.md", "committed line\ndirty uncommitted line\n");

        let blame = vcs.blame(Path::new("t/a.md")).unwrap();
        assert_eq!(blame.len(), 1, "only the committed line is attributed");
        assert_eq!(blame[0].line_no, 1);
        assert_eq!(
            blame[0].author,
            "deepseek-v4-flash/s1 <agent@ai-write.local>"
        );
    }

    #[cfg(unix)]
    #[test]
    fn blame_non_utf8_path_is_non_utf8_path() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let (dir, vcs) = repo();
        write(&dir, "t/a.md", "x\n");
        vcs.commit_file(Path::new("t/a.md"), &agent("s1"), "v1")
            .unwrap();
        let bad = Path::new(OsStr::from_bytes(b"t/\xff.md"));
        let err = vcs.blame(bad).unwrap_err();
        assert!(matches!(err, VcsError::NonUtf8Path(_)), "got: {err:?}");
    }
}
