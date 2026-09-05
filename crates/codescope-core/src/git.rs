//! Git domain model: repo context, change scopes, file changes, hunks, diff lines.
//!
//! Mirrors the verified `git` CLI formats in research 02. All types are plain data; the
//! `codescope-git` crate produces them from porcelain v2 / unified-diff output.
//!
//! **Line-number convention:** [`Hunk`] stores line numbers exactly as git emits them in
//! `@@` headers: 1-based, with length 0 on the empty side of a pure addition/deletion.
//! Convert before comparing with zero-based [`LineRange`](crate::LineRange) values.

use camino::{Utf8Path, Utf8PathBuf};
use std::fmt;

/// A git object id (full or abbreviated SHA), as emitted by git.
#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Oid(pub String);

impl Oid {
    /// Wrap a SHA string.
    #[must_use]
    pub fn new(sha: impl Into<String>) -> Self {
        Oid(sha.into())
    }

    /// The SHA string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What `HEAD` points at.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadState {
    /// On a local branch (`# branch.head <name>`).
    Branch(String),
    /// Detached at a commit (`# branch.head (detached)`).
    Detached(Oid),
    /// No commits yet (`# branch.oid (initial)`); every file is untracked.
    Unborn,
}

impl fmt::Display for HeadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeadState::Branch(name) => f.write_str(name),
            HeadState::Detached(oid) => write!(f, "detached@{oid}"),
            HeadState::Unborn => f.write_str("(unborn)"),
        }
    }
}

/// Upstream tracking branch, when one is configured.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Upstream {
    /// Upstream ref name, e.g. `origin/main` (`# branch.upstream`).
    pub name: String,
    /// Commits ahead of upstream (`# branch.ab +A`).
    pub ahead: u32,
    /// Commits behind upstream (`# branch.ab -B`).
    pub behind: u32,
}

impl Upstream {
    /// `true` when the branch has diverged from its upstream.
    #[must_use]
    pub fn is_diverged(&self) -> bool {
        self.ahead > 0 && self.behind > 0
    }
}

/// How the base ref was chosen (research 02, fallback chain order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseSource {
    /// `@{upstream}` of the current branch.
    Upstream,
    /// `refs/remotes/origin/HEAD` symbolic ref.
    OriginHead,
    /// Guessed `origin/main` / `origin/master`.
    Guess,
    /// `git merge-base --fork-point` (needs reflog).
    ForkPoint,
    /// The nearest ancestor branch (a branch whose merge-base with HEAD is the most
    /// recent common commit). This is the default when no upstream is configured.
    Ancestor,
    /// The user picked this base explicitly (overrides inference).
    Override,
}

/// The base ref branch changes are compared against.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct BaseInfo {
    /// Which fallback-chain step selected this base.
    pub source: BaseSource,
    /// Base ref name, e.g. `origin/main`.
    pub ref_name: String,
    /// `git merge-base <base> HEAD`.
    pub merge_base: Oid,
}

/// Static repository context shown in the status bar; refreshed on repo changes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RepoContext {
    /// Absolute repo root (`git rev-parse --show-toplevel`); all other paths are relative to it.
    pub toplevel: Utf8PathBuf,
    /// HEAD state.
    pub head: HeadState,
    /// Resolved commit object currently named by HEAD; `None` for an unborn repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<Oid>,
    /// Upstream tracking info, if configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<Upstream>,
    /// Inferred base for a branch-based change scope; `None` when no base could be inferred
    /// (e.g. unborn HEAD or no other branch ref).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<BaseInfo>,
}

/// Which change-set a diff describes (research 02: scopes stay distinct, never merged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeScope {
    /// Committed on this branch: `merge-base...HEAD`.
    Branch,
    /// Committed branch changes plus the current index/worktree: `merge-base` vs worktree.
    BranchWorking,
    /// Index vs HEAD (`git diff --cached`).
    Staged,
    /// Worktree vs index (`git diff`); untracked files live in this set.
    Unstaged,
    /// All uncommitted changes: HEAD vs worktree (staged + unstaged, incl. untracked).
    Working,
}

/// One independently computed set of file changes for a scope.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChangeSet {
    /// Which scope this set describes.
    pub scope: ChangeScope,
    /// Changed files, in git output order (sorted by path).
    #[serde(default)]
    pub files: Vec<FileChange>,
    /// `true` when this scope's committed diff was empty and the set was filled from the
    /// working tree instead (a branch fully pushed to its base with a dirty tree). Consumers
    /// must not misattribute these as committed branch changes (review 11 F2).
    #[serde(default)]
    pub fallback: bool,
    /// Exact per-file unified-diff sections captured by the Git command that produced `files`.
    /// This sidecar is intentionally omitted from general `ChangeSet` serialization; it exists so
    /// trusted local consumers can reconstruct the complete comparison without running Git again.
    #[serde(skip)]
    pub diff_sections: Option<Vec<UnifiedDiffSection>>,
}

/// One complete canonical section from the unified patch that produced a [`FileChange`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedDiffSection {
    /// Current/new repo-relative path used to join this section back to `ChangeSet::files`.
    pub path: Utf8PathBuf,
    /// Complete section text, including extended headers, hunks, no-newline markers, and a final
    /// newline. It is retained without line or byte truncation.
    pub text: String,
}

impl ChangeSet {
    /// Create a change-set for `scope`.
    #[must_use]
    pub fn new(scope: ChangeScope, files: Vec<FileChange>) -> Self {
        ChangeSet {
            scope,
            files,
            fallback: false,
            diff_sections: None,
        }
    }

    /// Attach the exact unified sections returned by the same Git invocation parsed into `files`.
    #[must_use]
    pub fn with_diff_sections(mut self, sections: Vec<UnifiedDiffSection>) -> Self {
        self.diff_sections = Some(sections);
        self
    }

    /// Mark this set as a working-tree fallback for an empty committed diff.
    #[must_use]
    pub fn into_fallback(mut self) -> Self {
        self.fallback = true;
        self
    }

    /// Number of changed files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// `true` when no files changed in this scope.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Find a file change by repo-relative path (current/new path).
    #[must_use]
    pub fn find_file(&self, path: &Utf8Path) -> Option<&FileChange> {
        self.files.iter().find(|f| f.path == path)
    }

    /// Iterate over file changes.
    pub fn iter(&self) -> impl Iterator<Item = &FileChange> {
        self.files.iter()
    }
}

/// What happened to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// New in the compared tree.
    Added,
    /// Content (or mode) changed.
    Modified,
    /// Removed from the compared tree.
    Deleted,
    /// Renamed; `score` is the similarity percentage (0–100).
    Renamed {
        /// Similarity score 0–100 (git `-M`).
        score: u8,
    },
    /// Copied; `score` is the similarity percentage (0–100).
    Copied {
        /// Similarity score 0–100 (git `-C`).
        score: u8,
    },
    /// Type changed (file ↔ symlink, etc.).
    TypeChanged,
    /// Unmerged (conflicted); hunks are not parsed (combined `--cc` diff).
    Unmerged,
    /// Not tracked by git (only in the `Unstaged` scope, content read from disk).
    Untracked,
    /// Submodule pointer change (mode 160000); never hunk-parsed.
    Gitlink,
}

/// One file's change within a [`ChangeSet`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileChange {
    /// Repo-relative current path.
    pub path: Utf8PathBuf,
    /// Repo-relative original path for renames/copies; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<Utf8PathBuf>,
    /// What happened to the file.
    pub status: FileStatus,
    /// Parsed hunks; empty for binary files, gitlinks, unmerged paths, and untracked files
    /// not yet diffed.
    #[serde(default)]
    pub hunks: Vec<Hunk>,
    /// `true` for binary files ("Binary files … differ"); never hunk-parsed.
    #[serde(default)]
    pub binary: bool,
}

impl FileChange {
    /// `true` for renames and copies (status carries an `old_path`).
    #[must_use]
    pub fn is_rename_or_copy(&self) -> bool {
        matches!(
            self.status,
            FileStatus::Renamed { .. } | FileStatus::Copied { .. }
        )
    }

    /// `true` when the change carries no line hunks (binary, gitlink, unmerged, untracked).
    #[must_use]
    pub fn has_hunks(&self) -> bool {
        !self.hunks.is_empty()
    }
}

/// One `@@` hunk of a unified diff.
///
/// Line numbers are **git-native**: 1-based; a `len` of 0 means the side is empty and
/// `start` is the line *after which* the content was added/removed. The `,1` count omitted
/// by git is normalized to `1` by the parser.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Hunk {
    /// 1-based first line on the old side.
    pub old_start: u32,
    /// Number of old-side lines (0 for pure additions).
    pub old_len: u32,
    /// 1-based first line on the new side.
    pub new_start: u32,
    /// Number of new-side lines (0 for pure deletions).
    pub new_len: u32,
    /// Section context from the `@@ … @@ <section>` header (e.g. enclosing function, via the
    /// userdiff driver). Crude label hint only — never a semantic fact (research 03).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// Diff body lines, in file order.
    #[serde(default)]
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// Old-side line span `(start, end_exclusive)`, git 1-based. Empty span at `old_start`
    /// when `old_len == 0` (insertion point on the old side).
    #[must_use]
    pub fn old_span(&self) -> (u32, u32) {
        (self.old_start, self.old_start + self.old_len)
    }

    /// New-side line span `(start, end_exclusive)`, git 1-based. Empty span at `new_start`
    /// when `new_len == 0` (pure deletion).
    #[must_use]
    pub fn new_span(&self) -> (u32, u32) {
        (self.new_start, self.new_start + self.new_len)
    }

    /// `true` when the hunk only deletes lines (`new_len == 0`).
    ///
    /// Pure deletions can only be mapped against the *base* revision's symbol tree, or
    /// approximately to the nearest surviving symbol (research 03).
    #[must_use]
    pub fn is_pure_deletion(&self) -> bool {
        self.new_len == 0
    }

    /// `true` when the hunk only adds lines (`old_len == 0`).
    #[must_use]
    pub fn is_pure_addition(&self) -> bool {
        self.old_len == 0
    }

    /// Zero-based new-side line index at which deleted content would be re-inserted,
    /// meaningful for pure deletions.
    ///
    /// For `@@ -15,5 +14,0 @@` the deletion happened after new-side line 14 (1-based),
    /// i.e. before zero-based line 14 — which equals `new_start`.
    #[must_use]
    pub fn insertion_point_zero_based(&self) -> u32 {
        self.new_start
    }

    /// Number of `+` lines in the body.
    #[must_use]
    pub fn count_added(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Add)
            .count()
    }

    /// Number of `-` lines in the body.
    #[must_use]
    pub fn count_deleted(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Del)
            .count()
    }
}

/// Kind of one line inside a [`Hunk`] body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    /// `+` line: present only on the new side.
    Add,
    /// `-` line: present only on the old side.
    Del,
    /// Context line: present on both sides.
    Context,
}

/// One line inside a [`Hunk`] body, without its leading `+`/`-`/space marker.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct DiffLine {
    /// Add / delete / context.
    pub kind: DiffLineKind,
    /// 1-based line number on the old side; `None` for added lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_ln: Option<u32>,
    /// 1-based line number on the new side; `None` for deleted lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_ln: Option<u32>,
    /// Line content without the leading diff marker and without the trailing newline.
    pub text: String,
}

impl DiffLine {
    /// An added line (`old_ln == None`).
    #[must_use]
    pub fn add(new_ln: u32, text: impl Into<String>) -> Self {
        DiffLine {
            kind: DiffLineKind::Add,
            old_ln: None,
            new_ln: Some(new_ln),
            text: text.into(),
        }
    }

    /// A deleted line (`new_ln == None`).
    #[must_use]
    pub fn del(old_ln: u32, text: impl Into<String>) -> Self {
        DiffLine {
            kind: DiffLineKind::Del,
            old_ln: Some(old_ln),
            new_ln: None,
            text: text.into(),
        }
    }

    /// A context line present on both sides.
    #[must_use]
    pub fn context(old_ln: u32, new_ln: u32, text: impl Into<String>) -> Self {
        DiffLine {
            kind: DiffLineKind::Context,
            old_ln: Some(old_ln),
            new_ln: Some(new_ln),
            text: text.into(),
        }
    }
}

/// Stable reference to one hunk: `(file, hunk_index)` in the file's diff order.
///
/// AI evidence and node code references use this identity to re-check cited changed rows against
/// repository-owned diff facts; the model never supplies authoritative diff text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct HunkId {
    /// Repo-relative file path (the change's current path).
    pub file: Utf8PathBuf,
    /// Zero-based index into [`FileChange::hunks`].
    pub index: u32,
}

impl fmt::Display for HunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#h{}", self.file, self.index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_spans_and_flags() {
        let del = Hunk {
            old_start: 15,
            old_len: 5,
            new_start: 14,
            new_len: 0,
            section: None,
            lines: vec![],
        };
        assert!(del.is_pure_deletion());
        assert!(!del.is_pure_addition());
        assert_eq!(del.old_span(), (15, 20));
        assert_eq!(del.new_span(), (14, 14));
        assert_eq!(del.insertion_point_zero_based(), 14);

        let add = Hunk {
            old_start: 3,
            old_len: 0,
            new_start: 4,
            new_len: 2,
            ..del.clone()
        };
        assert!(add.is_pure_addition());
        assert!(!add.is_pure_deletion());
        assert_eq!(add.new_span(), (4, 6));
    }

    #[test]
    fn hunk_line_counts() {
        let h = Hunk {
            old_start: 1,
            old_len: 2,
            new_start: 1,
            new_len: 3,
            section: Some("func main()".to_string()),
            lines: vec![
                DiffLine::context(1, 1, "package main"),
                DiffLine::del(2, "old()"),
                DiffLine::add(2, "new()"),
                DiffLine::add(3, "newer()"),
            ],
        };
        assert_eq!(h.count_added(), 2);
        assert_eq!(h.count_deleted(), 1);
    }

    #[test]
    fn diff_line_side_numbers() {
        let a = DiffLine::add(9, "x");
        assert_eq!(a.old_ln, None);
        assert_eq!(a.new_ln, Some(9));
        let d = DiffLine::del(9, "x");
        assert_eq!(d.old_ln, Some(9));
        assert_eq!(d.new_ln, None);
        let c = DiffLine::context(9, 9, "x");
        assert_eq!(c.kind, DiffLineKind::Context);
    }

    #[test]
    fn change_set_helpers() {
        let fc = FileChange {
            path: Utf8PathBuf::from("a.go"),
            old_path: None,
            status: FileStatus::Modified,
            hunks: vec![],
            binary: false,
        };
        let cs = ChangeSet::new(ChangeScope::Unstaged, vec![fc]);
        assert_eq!(cs.len(), 1);
        assert!(!cs.is_empty());
        assert!(cs.find_file(Utf8Path::new("a.go")).is_some());
        assert!(cs.find_file(Utf8Path::new("b.go")).is_none());
        assert_eq!(cs.iter().count(), 1);
        let empty = ChangeSet::new(ChangeScope::Branch, vec![]);
        assert!(empty.is_empty());
    }

    #[test]
    fn head_state_display() {
        assert_eq!(HeadState::Branch("main".into()).to_string(), "main");
        assert_eq!(
            HeadState::Detached(Oid::new("abc123")).to_string(),
            "detached@abc123"
        );
        assert_eq!(HeadState::Unborn.to_string(), "(unborn)");
    }

    #[test]
    fn upstream_divergence() {
        assert!(
            Upstream {
                name: "origin/main".into(),
                ahead: 1,
                behind: 2
            }
            .is_diverged()
        );
        assert!(
            !Upstream {
                name: "origin/main".into(),
                ahead: 1,
                behind: 0
            }
            .is_diverged()
        );
    }

    #[test]
    fn rename_flags() {
        let ren = FileChange {
            path: Utf8PathBuf::from("new.go"),
            old_path: Some(Utf8PathBuf::from("old.go")),
            status: FileStatus::Renamed { score: 96 },
            hunks: vec![],
            binary: false,
        };
        assert!(ren.is_rename_or_copy());
        assert!(!ren.has_hunks());
    }

    #[test]
    fn hunk_id_display() {
        let id = HunkId {
            file: Utf8PathBuf::from("pkg/a.go"),
            index: 2,
        };
        assert_eq!(id.to_string(), "pkg/a.go#h2");
    }
}
