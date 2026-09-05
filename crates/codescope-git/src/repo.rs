//! [`GitRepo`]: discovery plus the high-level read-only queries.

use crate::diff::{parse_unified_diff_with_sections, unmerged_change, ParsedUnifiedDiff};
use crate::error::{GitError, Result};
use crate::runner::GitCommand;
use crate::status::{parse_status_z, StatusSnapshot};
use camino::{Utf8Path, Utf8PathBuf};
use codescope_core::{
    BaseInfo, BaseSource, ChangeScope, ChangeSet, FileChange, FileStatus, Oid, RepoContext,
    Upstream,
};
use std::collections::{HashMap, HashSet};

/// Maximum number of commits returned by [`GitRepo::branch_commits`].
pub const MAX_BRANCH_COMMITS: usize = 200;

/// Maximum number of graph-ranked strict ancestors offered by the base picker.
pub const MAX_ANCESTOR_PICKER_ENTRIES: usize = 256;

/// Maximum commits examined after the first exact ancestor match while refining the
/// optional picker order. Inference itself has no pre-match bound and stops at its first
/// exact match.
pub const MAX_RANK_COMMITS_AFTER_FIRST: usize = 50_000;

/// Comparison-base picker data plus honesty metadata for a bounded graph scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseCandidates {
    /// Ordered, meaningful comparison bases.
    pub entries: Vec<BaseInfo>,
    /// `true` when more strict ancestors may exist beyond the picker scan bound.
    pub truncated: bool,
}

#[derive(Debug, Clone)]
struct BranchCandidate {
    /// Stable presentation identity. Local/remote twins share the local identity; two
    /// remote-only spellings remain separate even when they point at the same object.
    identity: String,
    ref_name: String,
    tip: Oid,
    aliases: Vec<String>,
    namespace_rank: u8,
}

impl BranchCandidate {
    fn matches_ref(&self, reference: &str) -> bool {
        self.ref_name == reference || self.aliases.iter().any(|alias| alias == reference)
    }

    fn as_base(&self, source: BaseSource) -> BaseInfo {
        BaseInfo {
            source,
            ref_name: self.ref_name.clone(),
            // A graph-ranked candidate is a strict ancestor, so its merge base is its tip.
            merge_base: self.tip.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum RankMode {
    Inference,
    Picker,
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

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
    /// The override must yield a meaningful merge base (one strictly before HEAD).
    /// Use [`Self::repo_context_for_scope`] for the more permissive combined scope.
    pub async fn repo_context_with_base(&self, base_override: Option<&str>) -> Result<RepoContext> {
        self.repo_context_for_scope(ChangeScope::Branch, base_override)
            .await
    }

    /// Repo context using the base semantics of a specific comparison scope.
    ///
    /// `BranchWorking` permits a ref whose merge base equals `HEAD`: although that ref produces no
    /// committed branch diff, it is still a meaningful base for dirty worktree changes.
    pub async fn repo_context_for_scope(
        &self,
        scope: ChangeScope,
        base_override: Option<&str>,
    ) -> Result<RepoContext> {
        let status = self.status_snapshot().await?;
        let allow_head_equivalent = scope == ChangeScope::BranchWorking;
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
            Some(reference) => match self.merge_base(reference).await? {
                Some(mb)
                    if status
                        .oid
                        .as_ref()
                        .is_some_and(|head| allow_head_equivalent || *head != mb) =>
                {
                    Some(BaseInfo {
                        source: BaseSource::Override,
                        ref_name: reference.to_string(),
                        merge_base: mb,
                    })
                }
                Some(_) | None => return Err(GitError::NoBase),
            },
            None => {
                self.infer_base_with_head_equivalent(&status, allow_head_equivalent)
                    .await?
            }
        };
        Ok(RepoContext {
            toplevel: self.toplevel.clone(),
            head,
            head_oid: status.oid,
            upstream,
            base,
        })
    }

    async fn infer_base_with_head_equivalent(
        &self,
        status: &StatusSnapshot,
        allow_head_equivalent: bool,
    ) -> Result<Option<BaseInfo>> {
        let Some(head) = status.oid.as_ref() else {
            return Ok(None);
        };
        let candidates = self
            .branch_ref_candidates(status, allow_head_equivalent)
            .await?;
        if let Some(upstream) = &status.upstream {
            if let Some(merge_base) = self.merge_base(upstream).await? {
                if allow_head_equivalent || merge_base != *head {
                    let (ref_name, _) = self.canonical_ref(upstream, &candidates);
                    return Ok(Some(BaseInfo {
                        source: BaseSource::Upstream,
                        ref_name,
                        merge_base,
                    }));
                }
            }
        }
        let (ranked, _) = self
            .rank_by_topology(head, candidates.clone(), RankMode::Inference)
            .await?;
        self.infer_base_from_candidates(status, &candidates, &ranked, allow_head_equivalent)
            .await
    }

    /// Meaningful comparison bases for the picker. The actual inferred base is first,
    /// followed by remaining graph-ranked strict ancestors and conventional integration
    /// branches. HEAD-equivalent refs never appear.
    pub async fn base_candidates(&self) -> Result<Vec<BaseInfo>> {
        Ok(self.base_candidates_with_metadata().await?.entries)
    }

    /// Every branch ref that can be selected in the interactive base picker.
    ///
    /// The inferred and graph-ranked meaningful bases stay first. Remaining local and
    /// remote-tracking branches are appended in deterministic namespace/name order so a
    /// user can filter to divergent branches without making the automatic inference walk
    /// rank every ref. Automatic inference still canonicalizes local/remote twins, while
    /// the picker exposes each short ref spelling. Refs equivalent to HEAD remain excluded
    /// because they cannot produce a comparison.
    pub async fn base_picker_refs(&self) -> Result<Vec<String>> {
        self.base_picker_refs_for_scope(ChangeScope::Branch).await
    }

    /// Every branch ref selectable for `scope`. `BranchWorking` includes HEAD-equivalent refs,
    /// since they still provide a meaningful base for uncommitted work.
    pub async fn base_picker_refs_for_scope(&self, scope: ChangeScope) -> Result<Vec<String>> {
        let allow_head_equivalent = scope == ChangeScope::BranchWorking;
        let meaningful = self
            .base_candidates_with_metadata_inner(allow_head_equivalent)
            .await?
            .entries;
        let status = self.status_snapshot().await?;
        let candidates = self
            .branch_ref_candidates(&status, allow_head_equivalent)
            .await?;

        let mut refs = Vec::with_capacity(candidates.len().max(meaningful.len()));
        let mut seen = HashSet::new();
        for base in meaningful {
            if seen.insert(base.ref_name.clone()) {
                refs.push(base.ref_name);
            }
        }
        for candidate in candidates {
            for ref_name in std::iter::once(candidate.ref_name).chain(
                candidate
                    .aliases
                    .into_iter()
                    .filter(|alias| !alias.starts_with("refs/")),
            ) {
                if seen.insert(ref_name.clone()) {
                    refs.push(ref_name);
                }
            }
        }
        Ok(refs)
    }

    /// Base picker entries with an explicit marker when the bounded ancestor scan stopped
    /// before exhausting the graph.
    pub async fn base_candidates_with_metadata(&self) -> Result<BaseCandidates> {
        self.base_candidates_with_metadata_inner(false).await
    }

    async fn base_candidates_with_metadata_inner(
        &self,
        allow_head_equivalent: bool,
    ) -> Result<BaseCandidates> {
        let status = self.status_snapshot().await?;
        let Some(head) = status.oid.as_ref() else {
            return Ok(BaseCandidates {
                entries: Vec::new(),
                truncated: false,
            });
        };
        let candidates = self
            .branch_ref_candidates(&status, allow_head_equivalent)
            .await?;
        let (ranked, truncated) = self
            .rank_by_topology(head, candidates.clone(), RankMode::Picker)
            .await?;
        let inferred = self
            .infer_base_from_candidates(&status, &candidates, &ranked, allow_head_equivalent)
            .await?;

        let mut out = Vec::new();
        let mut seen = HashSet::new();
        if let Some(base) = inferred {
            let key = self.base_identity(&base, &candidates);
            seen.insert(key);
            out.push(base);
        }
        for candidate in &ranked {
            let key = candidate.identity.clone();
            if seen.insert(key) {
                out.push(candidate.as_base(BaseSource::Ancestor));
            }
        }

        // Conventional integration branches can be divergent and therefore absent from
        // the strict-ancestor walk. Keep this tier small and deterministic.
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
            if status.branch.as_deref() == Some(guess) {
                continue;
            }
            if self.ref_exists(guess).await? {
                if let Some(mb) = self.merge_base(guess).await? {
                    if !allow_head_equivalent && mb == *head {
                        continue;
                    }
                    let (ref_name, key) = self.canonical_ref(guess, &candidates);
                    if seen.insert(key) {
                        out.push(BaseInfo {
                            source: BaseSource::Guess,
                            ref_name,
                            merge_base: mb,
                        });
                    }
                }
            }
        }
        Ok(BaseCandidates {
            entries: out,
            truncated,
        })
    }

    /// Enumerate local and remote-tracking refs once, exclude symbolic/HEAD-equivalent
    /// rows, and canonicalize presentation identities before any graph work.
    async fn branch_ref_candidates(
        &self,
        status: &StatusSnapshot,
        allow_head_equivalent: bool,
    ) -> Result<Vec<BranchCandidate>> {
        let Some(head) = status.oid.as_ref() else {
            return Ok(Vec::new());
        };
        let out = self
            .cmd(&[
                "for-each-ref",
                "--format=%(refname)%00%(refname:short)%00%(objectname)%00%(symref)",
                "refs/heads",
                "refs/remotes",
            ])
            .output()
            .await?;
        out.require_success()?;
        let stderr = out.stderr_trimmed();
        if !stderr.is_empty() {
            tracing::warn!(stderr, "git for-each-ref reported warnings");
        }
        let text = out.stdout_utf8("for-each-ref")?;

        #[derive(Debug)]
        struct RawRef {
            full: String,
            short: String,
            logical: String,
            tip: Oid,
            local: bool,
            namespace_rank: u8,
        }

        let mut grouped: HashMap<(String, String), Vec<RawRef>> = HashMap::new();
        for line in text.lines() {
            let fields: Vec<&str> = line.split('\0').collect();
            if fields.len() != 4 {
                tracing::warn!(record = ?line, "ignoring malformed for-each-ref record");
                continue;
            }
            let (full, short, tip, symref) = (fields[0], fields[1], fields[2], fields[3]);
            if full.is_empty() || short.is_empty() || !symref.is_empty() || full.ends_with("/HEAD")
            {
                continue;
            }
            let current_local_branch = status
                .branch
                .as_deref()
                .is_some_and(|branch| full.strip_prefix("refs/heads/") == Some(branch));
            if current_local_branch || (!allow_head_equivalent && tip == head.as_str()) {
                continue;
            }
            if !valid_object_id(tip) {
                tracing::warn!(
                    ref_name = full,
                    object = tip,
                    "ignoring ref with malformed object id"
                );
                continue;
            }
            let (logical, local, namespace_rank) =
                if let Some(path) = full.strip_prefix("refs/heads/") {
                    (path.to_string(), true, 0)
                } else if let Some(remote_path) = full.strip_prefix("refs/remotes/") {
                    let Some((remote, path)) = remote_path.split_once('/') else {
                        continue;
                    };
                    (
                        path.to_string(),
                        false,
                        if remote == "origin" { 1 } else { 2 },
                    )
                } else {
                    continue;
                };
            grouped
                .entry((logical.clone(), tip.to_string()))
                .or_default()
                .push(RawRef {
                    full: full.to_string(),
                    short: short.to_string(),
                    logical,
                    tip: Oid::new(tip),
                    local,
                    namespace_rank,
                });
        }

        let mut candidates = Vec::new();
        for ((_logical, _tip), mut refs) in grouped {
            refs.sort_by(|a, b| {
                a.namespace_rank
                    .cmp(&b.namespace_rank)
                    .then_with(|| a.short.cmp(&b.short))
            });
            if let Some(local) = refs.iter().find(|r| r.local) {
                let mut aliases = Vec::new();
                for raw in &refs {
                    aliases.push(raw.full.clone());
                    aliases.push(raw.short.clone());
                }
                aliases.sort();
                aliases.dedup();
                candidates.push(BranchCandidate {
                    identity: format!("local:{}:{}", local.logical, local.tip),
                    ref_name: local.short.clone(),
                    tip: local.tip.clone(),
                    aliases,
                    namespace_rank: 0,
                });
            } else {
                // Remote-only refs keep their remote-qualified presentation identities.
                for raw in refs {
                    candidates.push(BranchCandidate {
                        identity: format!("remote:{}:{}", raw.short, raw.tip),
                        ref_name: raw.short.clone(),
                        tip: raw.tip,
                        aliases: vec![raw.full, raw.short],
                        namespace_rank: raw.namespace_rank,
                    });
                }
            }
        }
        candidates.sort_by(|a, b| {
            a.namespace_rank
                .cmp(&b.namespace_rank)
                .then_with(|| a.ref_name.cmp(&b.ref_name))
        });
        Ok(candidates)
    }

    /// Rank possible ref tips with one exact topological graph walk. Missing-object,
    /// non-commit, sibling, and descendant refs never occur in the walk and are dropped.
    async fn rank_by_topology(
        &self,
        head: &Oid,
        candidates: Vec<BranchCandidate>,
        mode: RankMode,
    ) -> Result<(Vec<BranchCandidate>, bool)> {
        if candidates.is_empty() {
            return Ok((Vec::new(), false));
        }
        let mut by_tip: HashMap<String, Vec<BranchCandidate>> = HashMap::new();
        for candidate in candidates {
            by_tip
                .entry(candidate.tip.to_string())
                .or_default()
                .push(candidate);
        }
        for group in by_tip.values_mut() {
            group.sort_by(|a, b| {
                a.namespace_rank
                    .cmp(&b.namespace_rank)
                    .then_with(|| a.ref_name.cmp(&b.ref_name))
            });
        }

        let mut ranked = Vec::new();
        let mut found_first = false;
        let mut commits_after_first = 0usize;
        let mut truncated = false;
        self.cmd(&["rev-list", "--topo-order", head.as_str()])
            .stream_stdout_lines(|oid| {
                if found_first {
                    commits_after_first = commits_after_first.saturating_add(1);
                }
                if let Some(group) = by_tip.remove(oid) {
                    found_first = true;
                    let group_len = group.len();
                    let remaining = match mode {
                        RankMode::Inference => group_len,
                        RankMode::Picker => {
                            MAX_ANCESTOR_PICKER_ENTRIES.saturating_sub(ranked.len())
                        }
                    };
                    if matches!(mode, RankMode::Picker) && group_len > remaining {
                        truncated = true;
                    }
                    ranked.extend(group.into_iter().take(remaining));
                    if matches!(mode, RankMode::Inference) {
                        return false;
                    }
                }
                if by_tip.is_empty() {
                    return false;
                }
                if matches!(mode, RankMode::Picker)
                    && (ranked.len() >= MAX_ANCESTOR_PICKER_ENTRIES
                        || (found_first && commits_after_first >= MAX_RANK_COMMITS_AFTER_FIRST))
                {
                    truncated = !by_tip.is_empty();
                    return false;
                }
                true
            })
            .await?;
        if truncated {
            tracing::warn!(
                ranked = ranked.len(),
                remaining_tip_groups = by_tip.len(),
                "comparison-base ancestor picker was truncated"
            );
        }
        Ok((ranked, truncated))
    }

    async fn infer_base_from_candidates(
        &self,
        status: &StatusSnapshot,
        candidates: &[BranchCandidate],
        ranked: &[BranchCandidate],
        allow_head_equivalent: bool,
    ) -> Result<Option<BaseInfo>> {
        let Some(head) = status.oid.as_ref() else {
            return Ok(None);
        };
        if let Some(upstream) = &status.upstream {
            if let Some(mb) = self.merge_base(upstream).await? {
                if allow_head_equivalent || mb != *head {
                    let (ref_name, _) = self.canonical_ref(upstream, candidates);
                    return Ok(Some(BaseInfo {
                        source: BaseSource::Upstream,
                        ref_name,
                        merge_base: mb,
                    }));
                }
            }
        }
        if let Some(candidate) = ranked.first() {
            return Ok(Some(candidate.as_base(BaseSource::Ancestor)));
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
                if allow_head_equivalent || mb != *head {
                    let (ref_name, _) = self.canonical_ref(&name, candidates);
                    return Ok(Some(BaseInfo {
                        source: BaseSource::OriginHead,
                        ref_name,
                        merge_base: mb,
                    }));
                }
            }
        }

        for guess in ["origin/main", "origin/master"] {
            if self.ref_exists(guess).await? {
                if let Some(mb) = self.merge_base(guess).await? {
                    if allow_head_equivalent || mb != *head {
                        let (ref_name, _) = self.canonical_ref(guess, candidates);
                        return Ok(Some(BaseInfo {
                            source: BaseSource::Guess,
                            ref_name,
                            merge_base: mb,
                        }));
                    }
                }
            }
        }

        for local in ["main", "master"] {
            if status.branch.as_deref() == Some(local) {
                continue;
            }
            let out = self
                .cmd(&["merge-base", "--fork-point", local, "HEAD"])
                .output()
                .await?;
            if out.success() {
                let sha = out.stdout_trimmed("merge-base --fork-point")?;
                if !sha.is_empty() && (allow_head_equivalent || sha != head.as_str()) {
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

    fn canonical_ref(&self, reference: &str, candidates: &[BranchCandidate]) -> (String, String) {
        candidates
            .iter()
            .find(|candidate| candidate.matches_ref(reference))
            .map(|candidate| (candidate.ref_name.clone(), candidate.identity.clone()))
            .unwrap_or_else(|| (reference.to_string(), format!("ref:{reference}")))
    }

    fn base_identity(&self, base: &BaseInfo, candidates: &[BranchCandidate]) -> String {
        candidates
            .iter()
            .find(|candidate| {
                candidate.matches_ref(&base.ref_name)
                    && (candidate.tip == base.merge_base || base.source != BaseSource::Ancestor)
            })
            .map(|candidate| candidate.identity.clone())
            .unwrap_or_else(|| format!("ref:{}", base.ref_name))
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
    ///   [`GitError::NoBase`] when no meaningful base can be inferred.
    /// - [`ChangeScope::BranchWorking`]: `git diff -M -U3 <merge-base>` against the current
    ///   worktree, plus untracked files. Unlike `Branch`, a base at `HEAD` remains meaningful
    ///   because the worktree can differ from it.
    /// - [`ChangeScope::Staged`]: `git diff --cached -M -U3` (works on unborn HEAD too:
    ///   git diffs the index against the empty tree).
    /// - [`ChangeScope::Unstaged`]: `git diff -M -U3` plus untracked files from porcelain
    ///   status (`--untracked-files=all`), as [`FileStatus::Untracked`] with no hunks.
    ///
    /// No pathspec is ever passed: excluding a rename source silently breaks rename
    /// pairing (verified pitfall). Unmerged paths are marked, never hunk-parsed.
    #[tracing::instrument(skip(self), err)]
    pub async fn changeset(&self, scope: ChangeScope) -> Result<ChangeSet> {
        if matches!(scope, ChangeScope::Branch | ChangeScope::BranchWorking) {
            let status = self.status_snapshot().await?;
            let base = self
                .infer_base_with_head_equivalent(&status, scope == ChangeScope::BranchWorking)
                .await?
                .ok_or(GitError::NoBase)?;
            return match scope {
                ChangeScope::Branch => self.branch_changeset_from_base(&base).await,
                ChangeScope::BranchWorking => self.branch_working_changeset_from_base(&base).await,
                _ => unreachable!("branch-dependent scopes handled above"),
            };
        }
        let (mut files, mut diff_sections) = match scope {
            ChangeScope::Branch | ChangeScope::BranchWorking => unreachable!("returned above"),
            ChangeScope::Staged => {
                let mut args = vec!["diff", "--cached"];
                args.extend_from_slice(DIFF_FLAGS);
                let out = self.cmd(&args).run().await?;
                let text = String::from_utf8_lossy(out.stdout_bytes());
                let parsed = parse_unified_diff_with_sections(&text)?;
                let mut files = parsed.files;
                let status = self.status_snapshot().await?;
                merge_unmerged(&mut files, &status);
                (files, parsed.sections)
            }
            ChangeScope::Unstaged => {
                let mut args = vec!["diff"];
                args.extend_from_slice(DIFF_FLAGS);
                let out = self.cmd(&args).run().await?;
                let text = String::from_utf8_lossy(out.stdout_bytes());
                let parsed = parse_unified_diff_with_sections(&text)?;
                let mut files = parsed.files;
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
                (files, parsed.sections)
            }
            ChangeScope::Working => {
                let parsed = self.working_tree_diff().await?;
                (parsed.files, parsed.sections)
            }
        };
        files.sort_by(|a, b| a.path.cmp(&b.path));
        diff_sections.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(ChangeSet::new(scope, files).with_diff_sections(diff_sections))
    }

    /// All uncommitted changes as one list: `git diff HEAD` (staged + unstaged) plus
    /// untracked paths from porcelain status ([`FileStatus::Untracked`], no hunks), with
    /// unmerged paths marked. On an unborn HEAD, diffs against the empty tree instead
    /// (HEAD does not resolve).
    async fn working_tree_diff(&self) -> Result<ParsedUnifiedDiff> {
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
        self.worktree_diff_from(&target).await
    }

    /// Compare a committed tree directly with the current index/worktree and merge untracked and
    /// unmerged status metadata into the parsed diff.
    async fn worktree_diff_from(&self, target: &str) -> Result<ParsedUnifiedDiff> {
        let mut args = vec!["diff", target];
        args.extend_from_slice(DIFF_FLAGS);
        let out = self.cmd(&args).run().await?;
        let text = String::from_utf8_lossy(out.stdout_bytes());
        let mut parsed = parse_unified_diff_with_sections(&text)?;
        let status = self.status_snapshot().await?;
        merge_unmerged(&mut parsed.files, &status);
        for path in status.untracked_paths() {
            if !parsed.files.iter().any(|f| &f.path == path) {
                parsed.files.push(FileChange {
                    path: path.clone(),
                    old_path: None,
                    status: FileStatus::Untracked,
                    hunks: Vec::new(),
                    binary: false,
                });
            }
        }
        Ok(parsed)
    }

    /// Branch-scope changeset against an explicit base ref (a picker override).
    /// The ref must yield a meaningful merge base strictly before HEAD.
    pub async fn branch_changeset_with_base(&self, base_ref: &str) -> Result<ChangeSet> {
        let mb = self.merge_base(base_ref).await?.ok_or(GitError::NoBase)?;
        let status = self.status_snapshot().await?;
        if status.oid.as_ref().is_none_or(|head| *head == mb) {
            return Err(GitError::NoBase);
        }
        self.branch_changeset_from_base(&BaseInfo {
            source: BaseSource::Override,
            ref_name: base_ref.to_string(),
            merge_base: mb,
        })
        .await
    }

    /// Compute a branch change-set from an already-resolved base. This keeps the status
    /// label and the diff on the exact same merge-base even if refs move during refresh.
    pub async fn branch_changeset_from_base(&self, base: &BaseInfo) -> Result<ChangeSet> {
        let range = format!("{}...HEAD", base.merge_base);
        let mut args = vec!["diff"];
        args.extend_from_slice(DIFF_FLAGS);
        args.push(&range);
        let out = self.cmd(&args).run().await?;
        let text = String::from_utf8_lossy(out.stdout_bytes());
        let mut parsed = parse_unified_diff_with_sections(&text)?;
        let mut files = parsed.files;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        parsed.sections.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(ChangeSet::new(ChangeScope::Branch, files).with_diff_sections(parsed.sections))
    }

    /// Compute the combined committed-branch + dirty-worktree comparison from an already-resolved
    /// merge base. Unlike branch scope, the new side is the worktree rather than `HEAD`.
    pub async fn branch_working_changeset_from_base(&self, base: &BaseInfo) -> Result<ChangeSet> {
        let mut parsed = self.worktree_diff_from(base.merge_base.as_str()).await?;
        parsed.files.sort_by(|a, b| a.path.cmp(&b.path));
        parsed.sections.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(ChangeSet::new(ChangeScope::BranchWorking, parsed.files)
            .with_diff_sections(parsed.sections))
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

    /// A same-tip upstream is not meaningful for committed-only Branch scope, but it is the exact
    /// base needed by BranchWorking when the branch has dirty changes before its first commit.
    #[tokio::test]
    async fn branch_scope_rejects_head_equivalent_upstream() {
        let root = fully_pushed_repo();
        let repo_root = Utf8PathBuf::from_path_buf(root.clone()).expect("utf-8 temp path");
        let repo = GitRepo::discover(&repo_root)
            .await
            .expect("discover scratch repo");

        let ctx = repo.repo_context().await.expect("repo context");
        assert!(ctx.base.is_none(), "same-tip upstream must be discarded");

        // Dirty the worktree: one unstaged edit, one staged edit, one untracked file.
        std::fs::write(root.join("tracked.txt"), "two\n").expect("edit tracked.txt");
        std::fs::write(root.join("staged.txt"), "two\n").expect("edit staged.txt");
        git(&root, &["add", "staged.txt"]);
        std::fs::write(root.join("new.txt"), "brand new\n").expect("write new.txt");

        let err = repo.changeset(ChangeScope::Branch).await.unwrap_err();
        assert!(
            err.is_no_base(),
            "branch scope is honestly unavailable: {err}"
        );
        let combined_context = repo
            .repo_context_for_scope(ChangeScope::BranchWorking, None)
            .await
            .expect("combined context");
        assert!(
            combined_context.base.is_some(),
            "same-tip upstream is valid for branch + working"
        );
        assert!(
            repo.base_picker_refs_for_scope(ChangeScope::BranchWorking)
                .await
                .expect("combined base picker")
                .contains(&"upstream".to_string()),
            "the combined scope picker includes its same-tip base"
        );
        let combined = repo
            .changeset(ChangeScope::BranchWorking)
            .await
            .expect("branch + working changeset");
        let paths: Vec<_> = combined.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["new.txt", "staged.txt", "tracked.txt"]);
        let working = repo
            .changeset(ChangeScope::Working)
            .await
            .expect("working changeset");
        let paths: Vec<_> = working.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["new.txt", "staged.txt", "tracked.txt"]);

        // Once committed, HEAD advances while the upstream ref remains behind. It becomes
        // meaningful and normal base-to-HEAD branch comparison resumes.
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
