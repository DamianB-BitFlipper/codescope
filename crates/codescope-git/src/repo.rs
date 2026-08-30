//! [`GitRepo`]: discovery plus the high-level read-only queries.

use crate::diff::{parse_unified_diff, unmerged_change};
use crate::error::{GitError, Result};
use crate::runner::GitCommand;
use crate::status::{parse_status_z, StatusSnapshot};
use camino::{Utf8Path, Utf8PathBuf};
use codescope_core::{
    BaseInfo, BaseSource, ChangeScope, ChangeSet, FileChange, FileStatus, Oid, RepoContext,
    Upstream,
};
use std::collections::HashSet;

/// Maximum number of commits returned by [`GitRepo::branch_commits`].
pub const MAX_BRANCH_COMMITS: usize = 200;

/// Shared flags for every patch-producing `diff` invocation.
///
/// `--no-ext-diff` / `--no-textconv` keep user diff drivers from replacing the unified
/// format; explicit `--src-prefix`/`--dst-prefix` defeat `diff.noprefix` /
/// `diff.mnemonicPrefix` config, which would break `---`/`+++` path parsing.
const DIFF_FLAGS: &[&str] = &[
    "-M",
    "-U3",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    // Pin the submodule section format: `diff.submodule=log|diff` in user config would
    // otherwise change the output shape the parser expects (review 03 finding 3).
    "--submodule=short",
];

/// One `git log --oneline` entry on the current branch since the merge base.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitSummary {
    /// Abbreviated commit id, as printed by `--oneline`.
    pub oid: Oid,
    /// Commit subject line.
    pub subject: String,
}

/// A discovered git repository, anchored at its worktree toplevel.
///
/// All queries are read-only: every subprocess passes `--no-optional-locks` and runs with a
/// hardened environment (see the crate docs). Commands run from [`GitRepo::toplevel`], so
/// every path in the produced [`ChangeSet`]s is repo-root-relative — exactly as git reports
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRepo {
    toplevel: Utf8PathBuf,
    git_dir: Utf8PathBuf,
    common_dir: Utf8PathBuf,
}

impl GitRepo {
    /// Discover the repository containing `path` (any directory inside the worktree).
    ///
    /// Runs `git rev-parse --show-toplevel` / `--absolute-git-dir` / `--git-common-dir`;
    /// a linked worktree's `.git` *file* is resolved by git itself, so `git_dir` points at
    /// `<main>/.git/worktrees/<name>` and `common_dir` at the shared `<main>/.git`.
    ///
    /// A relative `path` is resolved against the current process directory. Bare
    /// repositories are rejected (codescope needs a worktree).
    #[tracing::instrument(err)]
    pub async fn discover(path: impl AsRef<Utf8Path> + std::fmt::Debug) -> Result<GitRepo> {
        let path = path.as_ref();
        let out = GitCommand::new(None, &["-C", path.as_str(), "rev-parse", "--show-toplevel"])
            .output()
            .await?;
        if !out.success() {
            let stderr = out.stderr_trimmed();
            if stderr.contains("not a git repository")
                || stderr.contains("must be run in a work tree")
            {
                return Err(GitError::NotARepo {
                    path: path.to_owned(),
                    stderr,
                });
            }
            out.require_success()?;
        }
        let toplevel = Utf8PathBuf::from(out.stdout_trimmed("rev-parse --show-toplevel")?);
        if toplevel.as_str().is_empty() {
            return Err(GitError::NotARepo {
                path: path.to_owned(),
                stderr: "empty toplevel (bare repository?)".to_string(),
            });
        }

        let out = GitCommand::new(
            Some(&toplevel),
            &["rev-parse", "--absolute-git-dir", "--git-common-dir"],
        )
        .run()
        .await?;
        let text = out.stdout_utf8("rev-parse --git-dir")?;
        let mut lines = text.lines();
        let git_dir = Utf8PathBuf::from(lines.next().unwrap_or_default().trim());
        let common_raw = Utf8Path::new(lines.next().unwrap_or_default().trim());
        let common_dir = if common_raw.is_absolute() {
            common_raw.to_owned()
        } else {
            toplevel.join(common_raw)
        };
        if git_dir.as_str().is_empty() || common_dir.as_str().is_empty() {
            return Err(GitError::ParseStatus {
                detail: format!("unexpected rev-parse output: {text:?}"),
            });
        }
        Ok(GitRepo {
            toplevel,
            git_dir,
            common_dir,
        })
    }

    /// Absolute worktree root; every repo-relative path is anchored here.
    #[must_use]
    pub fn toplevel(&self) -> &Utf8Path {
        &self.toplevel
    }

    /// Absolute `.git` directory of this worktree (per-worktree state: HEAD, index).
    #[must_use]
    pub fn git_dir(&self) -> &Utf8Path {
        &self.git_dir
    }

    /// Absolute common git directory (shared object store / refs across linked worktrees).
    #[must_use]
    pub fn common_dir(&self) -> &Utf8Path {
        &self.common_dir
    }

    fn cmd(&self, args: &[&str]) -> GitCommand {
        GitCommand::new(Some(&self.toplevel), args)
    }

    async fn status_snapshot(&self) -> Result<StatusSnapshot> {
        let out = self
            .cmd(&[
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=all",
            ])
            .run()
            .await?;
        parse_status_z(out.stdout_bytes())
    }

    /// Repository context for the status bar: HEAD state, upstream tracking, inferred base.
    #[tracing::instrument(skip(self), err)]
    pub async fn repo_context(&self) -> Result<RepoContext> {
        self.repo_context_with_base(None).await
    }

    /// Repo context with an explicit base override (`Some(ref)`) instead of inference.
    /// The override ref must yield a merge base with HEAD, else it is rejected.
    pub async fn repo_context_with_base(&self, base_override: Option<&str>) -> Result<RepoContext> {
        let status = self.status_snapshot().await?;
        let head = status.head_state();
        let upstream = status.upstream.clone().map(|name| {
            let (ahead, behind) = status.ahead_behind.unwrap_or((0, 0));
            Upstream {
                name,
                ahead,
                behind,
            }
        });
        let base = match base_override {
            Some(reference) => {
                // The user picked a base explicitly; it must share a merge base with HEAD.
                match self.merge_base(reference).await? {
                    Some(mb) => Some(BaseInfo {
                        source: BaseSource::Override,
                        ref_name: reference.to_string(),
                        merge_base: mb,
                    }),
                    None => return Err(GitError::NoBase),
                }
            }
            None => self.infer_base(&status).await?,
        };
        Ok(RepoContext {
            toplevel: self.toplevel.clone(),
            head,
            upstream,
            base,
        })
    }

    /// Infer the base ref for branch comparisons (research 02 fallback chain):
    /// `@{upstream}` → `origin/HEAD` → `origin/main`|`origin/master` → fork-point against a
    /// local `main`/`master`. Each candidate must also yield a merge base with `HEAD`.
    async fn infer_base(&self, status: &StatusSnapshot) -> Result<Option<BaseInfo>> {
        if status.oid.is_none() {
            return Ok(None); // unborn HEAD: nothing to compare against
        }

        if let Some(upstream) = &status.upstream {
            if let Some(mb) = self.merge_base(upstream).await? {
                return Ok(Some(BaseInfo {
                    source: BaseSource::Upstream,
                    ref_name: upstream.clone(),
                    merge_base: mb,
                }));
            }
        }

        // Nearest ancestor branch: a branch whose tip shares the most recent common commit
        // with HEAD. This is preferred over the repo's default branch — for X <- A <- B, the
        // base of B is A, not X.
        if let Some(base) = self.nearest_ancestor(status).await? {
            return Ok(Some(base));
        }

        let origin_head = self
            .cmd(&[
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ])
            .output()
            .await?;
        if origin_head.success() {
            let name = origin_head.stdout_trimmed("symbolic-ref origin/HEAD")?;
            if let Some(mb) = self.merge_base(&name).await? {
                return Ok(Some(BaseInfo {
                    source: BaseSource::OriginHead,
                    ref_name: name,
                    merge_base: mb,
                }));
            }
        }

        for guess in ["origin/main", "origin/master"] {
            if self.ref_exists(guess).await? {
                if let Some(mb) = self.merge_base(guess).await? {
                    return Ok(Some(BaseInfo {
                        source: BaseSource::Guess,
                        ref_name: guess.to_string(),
                        merge_base: mb,
                    }));
                }
            }
        }

        for local in ["main", "master"] {
            if status.branch.as_deref() == Some(local) {
                continue; // a branch is not its own base
            }
            let out = self
                .cmd(&["merge-base", "--fork-point", local, "HEAD"])
                .output()
                .await?;
            if out.success() {
                let sha = out.stdout_trimmed("merge-base --fork-point")?;
                if !sha.is_empty() {
                    return Ok(Some(BaseInfo {
                        source: BaseSource::ForkPoint,
                        ref_name: local.to_string(),
                        merge_base: Oid::new(sha),
                    }));
                }
            }
        }

        Ok(None)
    }

    /// All plausible base branches for the picker: upstream first, then ancestor branches
    /// (most recent common commit first), then the conventional default branches. The current
    /// branch is excluded.
    pub async fn base_candidates(&self) -> Result<Vec<BaseInfo>> {
        let status = self.status_snapshot().await?;
        let mut out: Vec<BaseInfo> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(up) = &status.upstream {
            if let Some(mb) = self.merge_base(up).await? {
                seen.insert(up.clone());
                out.push(BaseInfo {
                    source: BaseSource::Upstream,
                    ref_name: up.clone(),
                    merge_base: mb,
                });
            }
        }
        // ancestor_branches is nearest-first (--sort=-committerdate); the picker gets the
        // nearest ancestor first.
        for b in self.ancestor_branches(&status).await? {
            if seen.insert(b.ref_name.clone()) {
                out.push(b);
            }
        }
        // Advanced default branches (develop/trunk/release) are NOT strict ancestors of HEAD
        // (they contain commits HEAD lacks), so `--merged` hides them; offer them explicitly.
        for guess in [
            "origin/main",
            "origin/master",
            "main",
            "master",
            "origin/develop",
            "develop",
            "origin/trunk",
            "trunk",
            "origin/release",
            "release",
        ] {
            if status.branch.as_deref() == Some(guess) || seen.contains(guess) {
                continue;
            }
            if self.ref_exists(guess).await? {
                if let Some(mb) = self.merge_base(guess).await? {
                    seen.insert(guess.to_string());
                    out.push(BaseInfo {
                        source: BaseSource::Guess,
                        ref_name: guess.to_string(),
                        merge_base: mb,
                    });
                }
            }
        }
        Ok(out)
    }

    /// The nearest ancestor branch, if any: the branch (not the current one) whose
    /// merge-base with HEAD is the most recent common commit.
    async fn nearest_ancestor(&self, status: &StatusSnapshot) -> Result<Option<BaseInfo>> {
        // --sort=-committerdate returns nearest-first; take the first (review 10 F1: the old
        // ascending order + pop() selected the farthest ancestor).
        Ok(self.ancestor_branches(status).await?.into_iter().next().map(|mut b| {
            b.source = BaseSource::Ancestor;
            b
        }))
    }

    /// Branches whose tip is an ancestor of HEAD, NEAREST (newest tip commit) first.
    /// Excludes the current branch and symbolic remote HEADs.
    /// Branches whose tip is an ancestor of HEAD, nearest first. One `for-each-ref --merged
    /// HEAD --sort=-committerdate` call — no per-ref subprocesses (review: a 5k-ref repo made
    /// the old per-ref merge-base+show scan pin the CPU for minutes).
    ///
    /// A merged ref's merge-base with HEAD is the ref's own tip, never HEAD itself, so the
    /// empty-diff guard (merge_base == HEAD) is inherent. Symbolic remote HEADs are dropped.
    async fn ancestor_branches(&self, status: &StatusSnapshot) -> Result<Vec<BaseInfo>> {
        if status.oid.is_none() {
            return Ok(Vec::new());
        }
        let head = status.oid.clone().expect("checked above");
        let out = self
            .cmd(&[
                "for-each-ref",
                "--merged",
                "HEAD",
                "--sort=-committerdate",
                "--format=%(refname) %(refname:short) %(objectname)",
                "refs/heads",
                "refs/remotes",
            ])
            .run()
            .await?;
        let text = out.stdout_trimmed("for-each-ref")?;
        let current = status.branch.clone();
        let mut candidates = Vec::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let (Some(full), Some(name), Some(tip)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if full.ends_with("/HEAD") || Some(name) == current.as_deref() {
                continue;
            }
            // For a merged ref the merge-base with HEAD is the tip itself. Skip the pathological
            // case where a tip somehow equals HEAD (empty diff guard).
            if tip == head.as_str() {
                continue;
            }
            candidates.push(BaseInfo {
                source: BaseSource::Ancestor,
                ref_name: name.to_string(),
                merge_base: Oid::new(tip),
            });
        }
        Ok(candidates)
    }

    async fn ref_exists(&self, name: &str) -> Result<bool> {
        let commitish = format!("{name}^{{commit}}");
        let out = self
            .cmd(&["rev-parse", "--verify", "--quiet", &commitish])
            .output()
            .await?;
        Ok(out.success())
    }

    async fn merge_base(&self, base_ref: &str) -> Result<Option<Oid>> {
        let out = self.cmd(&["merge-base", base_ref, "HEAD"]).output().await?;
        if !out.success() {
            // Exit 1: no common ancestor; 128: unknown ref — either way this candidate
            // cannot serve as a base, fall through to the next one.
            return Ok(None);
        }
        let sha = out.stdout_trimmed("merge-base")?;
        Ok((!sha.is_empty()).then(|| Oid::new(sha)))
    }

    /// Commits on the current branch since `merge_base`, newest first
    /// (`git log --oneline`, capped at [`MAX_BRANCH_COMMITS`]).
    #[tracing::instrument(skip(self), err)]
    pub async fn branch_commits(&self, merge_base: &Oid) -> Result<Vec<CommitSummary>> {
        let cap = MAX_BRANCH_COMMITS.to_string();
        let range = format!("{merge_base}..HEAD");
        let out = self
            .cmd(&[
                "log",
                "--oneline",
                "--no-decorate",
                "--no-show-signature",
                "-n",
                &cap,
                &range,
            ])
            .run()
            .await?;
        let text = out.stdout_utf8("log --oneline")?;
        Ok(text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let (oid, subject) = line.split_once(' ').unwrap_or((line, ""));
                CommitSummary {
                    oid: Oid::new(oid),
                    subject: subject.to_string(),
                }
            })
            .collect())
    }

    /// Compute the [`ChangeSet`] for one scope (research 02: scopes stay independent).
    ///
    /// - [`ChangeScope::Branch`]: `git diff -M -U3 <merge-base>...HEAD`; errors with
    ///   [`GitError::NoBase`] when no base can be inferred. When that committed diff is
    ///   empty but the worktree is dirty (e.g. the branch is fully pushed: merge-base ==
    ///   HEAD), the uncommitted changes are merged in so the branch view is not
    ///   misleadingly empty.
    /// - [`ChangeScope::Staged`]: `git diff --cached -M -U3` (works on unborn HEAD too:
    ///   git diffs the index against the empty tree).
    /// - [`ChangeScope::Unstaged`]: `git diff -M -U3` plus untracked files from porcelain
    ///   status (`--untracked-files=all`), as [`FileStatus::Untracked`] with no hunks.
    ///
    /// No pathspec is ever passed: excluding a rename source silently breaks rename
    /// pairing (verified pitfall). Unmerged paths are marked, never hunk-parsed.
    #[tracing::instrument(skip(self), err)]
    pub async fn changeset(&self, scope: ChangeScope) -> Result<ChangeSet> {
        let mut fell_back = false;
        let mut files = match scope {
            ChangeScope::Branch => {
                let status = self.status_snapshot().await?;
                let base = self.infer_base(&status).await?.ok_or(GitError::NoBase)?;
                let range = format!("{}...HEAD", base.merge_base);
                let mut args = vec!["diff"];
                args.extend_from_slice(DIFF_FLAGS);
                args.push(&range);
                let out = self.cmd(&args).run().await?;
                let text = String::from_utf8_lossy(out.stdout_bytes());
                let mut files = parse_unified_diff(&text)?;
                if files.is_empty() {
                    // A fully pushed branch has merge-base == HEAD, so the committed diff
                    // is empty even when the worktree is dirty. Surface the uncommitted
                    // changes rather than a misleading "no changes" pane.
                    files = self.working_tree_files().await?;
                    fell_back = true;
                }
                files
            }
            ChangeScope::Staged => {
                let mut args = vec!["diff", "--cached"];
                args.extend_from_slice(DIFF_FLAGS);
                let out = self.cmd(&args).run().await?;
                let text = String::from_utf8_lossy(out.stdout_bytes());
                let mut files = parse_unified_diff(&text)?;
                let status = self.status_snapshot().await?;
                merge_unmerged(&mut files, &status);
                files
            }
            ChangeScope::Unstaged => {
                let mut args = vec!["diff"];
                args.extend_from_slice(DIFF_FLAGS);
                let out = self.cmd(&args).run().await?;
                let text = String::from_utf8_lossy(out.stdout_bytes());
                let mut files = parse_unified_diff(&text)?;
                let status = self.status_snapshot().await?;
                merge_unmerged(&mut files, &status);
                for path in status.untracked_paths() {
                    if !files.iter().any(|f| &f.path == path) {
                        files.push(FileChange {
                            path: path.clone(),
                            old_path: None,
                            status: FileStatus::Untracked,
                            hunks: Vec::new(),
                            binary: false,
                        });
                    }
                }
                files
            }
            ChangeScope::Working => self.working_tree_files().await?,
        };
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let set = ChangeSet::new(scope, files);
        Ok(if fell_back { set.into_fallback() } else { set })
    }

    /// All uncommitted changes as one list: `git diff HEAD` (staged + unstaged) plus
    /// untracked paths from porcelain status ([`FileStatus::Untracked`], no hunks), with
    /// unmerged paths marked. On an unborn HEAD, diffs against the empty tree instead
    /// (HEAD does not resolve).
    async fn working_tree_files(&self) -> Result<Vec<FileChange>> {
        // All uncommitted changes: worktree vs HEAD (staged + unstaged), plus untracked.
        // On an unborn HEAD, diff against the empty tree instead (HEAD doesn't resolve).
        let target = if self
            .cmd(&["rev-parse", "--verify", "--quiet", "HEAD"])
            .output()
            .await?
            .success()
        {
            "HEAD".to_string()
        } else {
            // The well-known empty tree object id.
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
        };
        let mut args = vec!["diff", &target];
        args.extend_from_slice(DIFF_FLAGS);
        let out = self.cmd(&args).run().await?;
        let text = String::from_utf8_lossy(out.stdout_bytes());
        let mut files = parse_unified_diff(&text)?;
        let status = self.status_snapshot().await?;
        merge_unmerged(&mut files, &status);
        for path in status.untracked_paths() {
            if !files.iter().any(|f| &f.path == path) {
                files.push(FileChange {
                    path: path.clone(),
                    old_path: None,
                    status: FileStatus::Untracked,
                    hunks: Vec::new(),
                    binary: false,
                });
            }
        }
        Ok(files)
    }

    /// Branch-scope changeset against an explicit base ref (a picker override). The ref
    /// must yield a merge base with HEAD. As with [`GitRepo::changeset`], an empty
    /// committed diff merges in the uncommitted (working-tree) changes.
    pub async fn branch_changeset_with_base(&self, base_ref: &str) -> Result<ChangeSet> {
        let mb = self.merge_base(base_ref).await?.ok_or(GitError::NoBase)?;
        let range = format!("{}...HEAD", mb);
        let mut args = vec!["diff"];
        args.extend_from_slice(DIFF_FLAGS);
        args.push(&range);
        let out = self.cmd(&args).run().await?;
        let text = String::from_utf8_lossy(out.stdout_bytes());
        let mut files = parse_unified_diff(&text)?;
        let fell_back = files.is_empty();
        if fell_back {
            files = self.working_tree_files().await?;
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let set = ChangeSet::new(ChangeScope::Branch, files);
        Ok(if fell_back { set.into_fallback() } else { set })
    }

    /// Content of `path` at revision `base` (`git show <base>:<path>`).
    ///
    /// `Ok(None)` when the path does not exist in that revision; `Err` for an invalid
    /// revision or non-UTF-8 (binary) content.
    #[tracing::instrument(skip(self), err)]
    pub async fn base_file_content(&self, base: &str, path: &Utf8Path) -> Result<Option<String>> {
        let spec = format!("{base}:{path}");
        let out = self.cmd(&["show", &spec]).output().await?;
        if !out.success() {
            let stderr = out.stderr_trimmed();
            if stderr.contains("does not exist in") || stderr.contains("exists on disk, but not in")
            {
                return Ok(None);
            }
            out.require_success()?;
        }
        Ok(Some(out.stdout_utf8("show <base>:<path>")?.to_string()))
    }

    /// Stable fingerprint of the repo state: HEAD sha + `ls-files --stage` + porcelain
    /// status bytes, hashed with xxh3-128 (hex). Changes whenever HEAD, the index, or the
    /// worktree status changes — the dispatcher uses it to detect repo-state generations.
    #[tracing::instrument(skip(self), err)]
    pub async fn fingerprint(&self) -> Result<String> {
        let head = self.cmd(&["rev-parse", "HEAD"]).output().await?;
        let head_id = if head.success() {
            head.stdout_trimmed("rev-parse HEAD")?
        } else {
            "unborn".to_string()
        };
        let ls_files = self.cmd(&["ls-files", "--stage", "-z"]).run().await?;
        let status = self
            .cmd(&[
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=all",
            ])
            .run()
            .await?;

        let mut bytes = Vec::with_capacity(
            head_id.len() + ls_files.stdout_bytes().len() + status.stdout_bytes().len() + 2,
        );
        bytes.extend_from_slice(head_id.as_bytes());
        bytes.push(0xff);
        bytes.extend_from_slice(ls_files.stdout_bytes());
        bytes.push(0xff);
        bytes.extend_from_slice(status.stdout_bytes());
        // Worktree evidence (review 03 finding 5): porcelain carries no worktree content hash,
        // so a repeat edit of an already-modified file would otherwise leave the fingerprint
        // unchanged. Fold in size+mtime of every changed/untracked path (two stats per file).
        for path in changed_paths(&status) {
            let abs = self.toplevel().join(&path);
            if let Ok(md) = std::fs::symlink_metadata(abs.as_std_path()) {
                bytes.extend_from_slice(path.as_bytes());
                bytes.extend_from_slice(&md.len().to_le_bytes());
                if let Ok(m) = md.modified() {
                    if let Ok(d) = m.duration_since(std::time::UNIX_EPOCH) {
                        bytes.extend_from_slice(&d.as_nanos().to_le_bytes());
                    }
                }
            }
        }
        Ok(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&bytes)))
    }
}

/// Extract the changed/untracked paths from a porcelain v2 `-z` status output (best effort:
/// every non-header record contributes its path; rename records contribute the new path).
fn changed_paths(status: &crate::runner::GitOutput) -> Vec<String> {
    let mut out = Vec::new();
    for record in status.stdout_bytes().split(|b| *b == 0) {
        if record.len() < 2 || record[0] == b'#' {
            continue;
        }
        // Skip the fixed field prefix up to the first NUL-separated path. The path begins
        // after the last space of the metadata section for ordinary (`1`/`2`) records.
        if let Some(pos) = record.iter().rposition(|b| *b == b' ') {
            let path = &record[pos + 1..];
            if let Ok(s) = std::str::from_utf8(path) {
                if !s.is_empty() {
                    out.push(s.to_string());
                }
            }
        }
    }
    out
}

/// Force `Unmerged` status (and drop hunks) for every path porcelain reports as `u`.
/// The diff parser usually catches these via `diff --cc` / `* Unmerged path`, but the
/// porcelain record is authoritative.
fn merge_unmerged(files: &mut Vec<FileChange>, status: &StatusSnapshot) {
    for path in status.unmerged_paths() {
        if let Some(existing) = files.iter_mut().find(|f| &f.path == path) {
            existing.status = FileStatus::Unmerged;
            existing.hunks.clear();
            existing.old_path = None;
        } else {
            files.push(unmerged_change(path.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Run a git command for test setup (not through the crate under test); returns stdout.
    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@test.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@test.invalid")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8 stdout")
    }

    /// Build a throwaway repo on `main` with two tracked files, plus a local `upstream`
    /// ref at the same commit that `main` tracks — i.e. the branch is fully pushed
    /// (merge-base(upstream, HEAD) == HEAD, ahead/behind 0/0).
    fn fully_pushed_repo() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "codescope-fully-pushed-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        git(&dir, &["init", "--quiet", "-b", "main"]);
        std::fs::write(dir.join("tracked.txt"), "one\n").expect("write tracked.txt");
        std::fs::write(dir.join("staged.txt"), "one\n").expect("write staged.txt");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "--quiet", "--no-verify", "-m", "base"]);
        // A local 'upstream' ref at HEAD, tracked by main: fully pushed, 0/0.
        git(&dir, &["branch", "upstream"]);
        git(&dir, &["branch", "--set-upstream-to=upstream"]);
        dir
    }

    /// Regression: a fully pushed branch diffs empty against its base even when the
    /// worktree is dirty; the Branch scope must surface the uncommitted changes instead
    /// of a misleading "no changes" pane.
    #[tokio::test]
    async fn branch_scope_includes_dirty_worktree_when_fully_pushed() {
        let root = fully_pushed_repo();
        let repo_root = Utf8PathBuf::from_path_buf(root.clone()).expect("utf-8 temp path");
        let repo = GitRepo::discover(&repo_root)
            .await
            .expect("discover scratch repo");

        // Setup sanity: the base is the upstream ref, and its merge-base IS HEAD
        // (fully pushed — the committed branch diff is empty).
        let ctx = repo.repo_context().await.expect("repo context");
        let base = ctx.base.expect("base inferred from upstream");
        assert_eq!(base.source, BaseSource::Upstream);
        assert_eq!(base.ref_name, "upstream");
        let head = git(&root, &["rev-parse", "HEAD"]);
        assert_eq!(
            base.merge_base.as_str(),
            head.trim(),
            "fully pushed: merge-base == HEAD"
        );
        let commits = repo.branch_commits(&base.merge_base).await.expect("log");
        assert!(commits.is_empty(), "no commits ahead of the upstream");

        // Dirty the worktree: one unstaged edit, one staged edit, one untracked file.
        std::fs::write(root.join("tracked.txt"), "two\n").expect("edit tracked.txt");
        std::fs::write(root.join("staged.txt"), "two\n").expect("edit staged.txt");
        git(&root, &["add", "staged.txt"]);
        std::fs::write(root.join("new.txt"), "brand new\n").expect("write new.txt");

        let cs = repo
            .changeset(ChangeScope::Branch)
            .await
            .expect("branch changeset");
        assert_eq!(cs.scope, ChangeScope::Branch);
        let paths: Vec<_> = cs.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["new.txt", "staged.txt", "tracked.txt"],
            "dirty worktree files must surface in the (otherwise empty) branch scope"
        );
        assert_eq!(cs.files[0].status, FileStatus::Untracked);
        assert!(cs.files[0].hunks.is_empty(), "untracked files carry no hunks");
        assert_eq!(cs.files[1].status, FileStatus::Modified);
        assert!(!cs.files[1].hunks.is_empty(), "staged edit diffs against HEAD");
        assert_eq!(cs.files[2].status, FileStatus::Modified);
        assert!(!cs.files[2].hunks.is_empty(), "unstaged edit diffs against HEAD");

        // Once the dirty files are committed, the branch is ahead again and the normal
        // committed diff takes over (the fallback only fires on an empty branch diff).
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "--no-verify", "-m", "work"]);
        let cs = repo
            .changeset(ChangeScope::Branch)
            .await
            .expect("branch changeset after commit");
        let paths: Vec<_> = cs.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["new.txt", "staged.txt", "tracked.txt"]);
        assert!(
            cs.files.iter().all(|f| f.status != FileStatus::Untracked),
            "committed files are real diff entries"
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
