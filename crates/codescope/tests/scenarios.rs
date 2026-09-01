//! Table-driven integration tests over the codescope-testutil scenario library.
//!
//! Each scenario builds a deterministic scratch repo; we drive `GitRepo` + the git-only
//! analysis path and assert the backend facts. No TUI, no network, no language server.

use codescope_core::{ChangeScope, FileStatus, HeadState};
use codescope_git::GitRepo;
use codescope_testutil::scenarios::{self, Scenario};

fn status_name(s: &FileStatus) -> &'static str {
    match s {
        FileStatus::Added => "added",
        FileStatus::Modified => "modified",
        FileStatus::Deleted => "deleted",
        FileStatus::Renamed { .. } => "renamed",
        FileStatus::Copied { .. } => "copied",
        FileStatus::TypeChanged => "type_changed",
        FileStatus::Unmerged => "unmerged",
        FileStatus::Untracked => "untracked",
        FileStatus::Gitlink => "gitlink",
    }
}

fn branch_label(h: &HeadState) -> String {
    match h {
        HeadState::Branch(b) => b.clone(),
        HeadState::Detached(_) => "(detached)".to_string(),
        HeadState::Unborn => "(no commits)".to_string(),
    }
}

async fn check(s: &Scenario) {
    let built = scenarios::build(s).expect("build scenario");

    // discover
    let discovered = GitRepo::discover(built.root_utf8()).await;
    if s.expect.discover_fails {
        assert!(
            discovered.is_err(),
            "{}: discover must fail for a non-git dir",
            s.name
        );
        return;
    }
    let repo = discovered.unwrap_or_else(|e| panic!("{}: discover failed: {e}", s.name));
    assert!(repo.toplevel().is_absolute());

    // repo_context
    let ctx = repo.repo_context().await.expect("repo_context");
    if let Some(want) = s.expect.branch {
        assert_eq!(branch_label(&ctx.head), want, "{}: branch", s.name);
    }
    if let Some(want) = s.expect.has_base {
        assert_eq!(ctx.base.is_some(), want, "{}: has_base", s.name);
    }
    if let Some(want) = s.expect.base_source {
        let src = ctx
            .base
            .as_ref()
            .map(|b| format!("{:?}", b.source).to_lowercase());
        assert!(
            src.as_deref().map(|s| s.contains(want)).unwrap_or(false),
            "{}: base_source {:?} must contain {:?}",
            s.name,
            src,
            want
        );
    }

    // scope counts: (branch, staged, unstaged, working)
    if let Some((b, st, u, w)) = s.expect.scope_counts {
        let branch = repo.changeset(ChangeScope::Branch).await.map(|c| c.len());
        let staged = repo.changeset(ChangeScope::Staged).await.map(|c| c.len());
        let unstaged = repo.changeset(ChangeScope::Unstaged).await.map(|c| c.len());
        let working = repo.changeset(ChangeScope::Working).await.map(|c| c.len());
        let branch_n = match branch {
            Ok(n) => n,
            // Only GitError::NoBase is a tolerated absence (review 11 F3).
            Err(e) if e.is_no_base() => 0,
            Err(e) => panic!("{}: branch scope failed: {e}", s.name),
        };
        assert_eq!(branch_n, b, "{}: branch scope count", s.name);
        assert_eq!(
            staged.expect("staged"),
            st,
            "{}: staged scope count",
            s.name
        );
        assert_eq!(
            unstaged.expect("unstaged"),
            u,
            "{}: unstaged scope count",
            s.name
        );
        assert_eq!(
            working.expect("working"),
            w,
            "{}: working scope count",
            s.name
        );
    }

    // must_have_status: somewhere across staged+unstaged+working.
    if !s.expect.must_have_status.is_empty() {
        let mut all = Vec::new();
        for scope in [
            ChangeScope::Staged,
            ChangeScope::Unstaged,
            ChangeScope::Working,
        ] {
            if let Ok(cs) = repo.changeset(scope).await {
                all.extend(cs.files.iter().map(status_name_from));
            }
        }
        for want in &s.expect.must_have_status {
            assert!(
                all.iter().any(|s| s == want),
                "{}: expected a {want:?} status somewhere; got {all:?}",
                s.name
            );
        }
    }

    // fingerprint: stable, then changes on a follow-up edit.
    if s.expect.fingerprint_changes_on_edit {
        let f1 = repo.fingerprint().await.expect("fp1");
        built.edit(
            "util.go",
            "package main\n\nfunc Helper() int { return 2 }\n",
        );
        let f2 = repo.fingerprint().await.expect("fp2");
        assert_ne!(f1, f2, "{}: fingerprint must change after an edit", s.name);
    }
}

fn status_name_from(f: &codescope_core::FileChange) -> &'static str {
    status_name(&f.status)
}

#[tokio::test(flavor = "multi_thread")]
async fn scenarios_behave_as_expected() {
    for s in scenarios::all() {
        check(&s).await;
    }
}

// One test per scenario name so a failure points at the exact shape.
macro_rules! per_scenario {
    ($($name:ident),*) => {$(
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let s = scenarios::all().into_iter().find(|s| s.name == stringify!($name))
                .expect("scenario exists");
            check(&s).await;
        }
    )*};
}

per_scenario!(
    single_commit_repo,
    dirty_worktree,
    staged_only,
    mixed_staged_unstaged_untracked,
    detached_head,
    unborn_branch,
    renamed_file,
    deleted_file,
    merge_conflict,
    branch_fully_pushed,
    stacked_branches,
    deep_nesting,
    binary_change,
    special_char_filename,
    crlf_file,
    non_git_dir
);

/// Review 26: a same-tip upstream is discarded instead of manufacturing an empty branch
/// comparison. The explicit working scope still carries the dirty file.
#[tokio::test(flavor = "multi_thread")]
async fn branch_fully_pushed_has_no_base_but_working_remains_available() {
    let s = scenarios::all()
        .into_iter()
        .find(|s| s.name == "branch_fully_pushed")
        .expect("scenario exists");
    let built = scenarios::build(&s).expect("build");
    let repo = GitRepo::discover(built.root_utf8())
        .await
        .expect("discover");
    let ctx = repo.repo_context().await.expect("context");
    assert!(ctx.base.is_none(), "same-tip upstream is not a base");
    let err = repo.changeset(ChangeScope::Branch).await.unwrap_err();
    assert!(err.is_no_base(), "branch scope reports no base: {err}");
    let cs = repo
        .changeset(ChangeScope::Working)
        .await
        .expect("working changeset");
    assert!(
        cs.files.iter().any(|f| f.path.as_str() == "util.go"),
        "the dirty worktree file remains available explicitly: {:?}",
        cs.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>()
    );
}
