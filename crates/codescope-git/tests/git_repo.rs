//! Integration tests: real `git` against scratch repositories in temp dirs.
//!
//! Repositories are created with `std::process::Command` using fixed author/committer
//! dates and isolated global/system config, so runs are deterministic.

use camino::{Utf8Path, Utf8PathBuf};
use codescope_core::{BaseSource, ChangeScope, FileStatus, HeadState};
use codescope_git::{GitError, GitRepo};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Run a git command for test setup (not through the crate under test).
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_DATE", "2024-01-02T03:04:05Z")
        .env("GIT_COMMITTER_DATE", "2024-01-02T03:04:05Z")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn write(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, content).expect("write");
}

/// New repo on branch `main` with deterministic identity and one initial commit.
fn scratch_repo() -> (TempDir, Utf8PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "codescope"]);
    git(dir, &["config", "user.email", "codescope@example.com"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    git(dir, &["config", "core.autocrlf", "false"]);
    write(
        dir,
        "a.go",
        "package main\n\nimport \"fmt\"\n\n// A returns a constant used by tests.\nfunc A() int { return 1 }\n\nfunc helperOne() string { return \"one\" }\n\nfunc helperTwo() string { return \"two\" }\n\nfunc main() { fmt.Println(A()) }\n",
    );
    write(
        dir,
        "b.txt",
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n",
    );
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "initial"]);
    let toplevel = Utf8PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"]).trim());
    (tmp, toplevel)
}

/// Unborn repo (init only, no commits).
fn unborn_repo() -> (TempDir, Utf8PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "codescope"]);
    git(dir, &["config", "user.email", "codescope@example.com"]);
    let toplevel = Utf8PathBuf::from(
        git(dir, &["rev-parse", "--show-toplevel"])
            .trim()
            .to_string(),
    );
    (tmp, toplevel)
}

async fn open_repo(toplevel: &Utf8Path) -> GitRepo {
    GitRepo::discover(toplevel).await.expect("discover")
}

#[tokio::test]
async fn discover_from_toplevel_and_subdir() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;
    assert_eq!(repo.toplevel(), top);
    assert!(
        repo.git_dir().as_str().ends_with(".git"),
        "{}",
        repo.git_dir()
    );
    assert_eq!(repo.git_dir(), repo.common_dir());

    // From a subdirectory.
    std::fs::create_dir_all(top.join("sub/inner")).unwrap();
    let repo2 = GitRepo::discover(top.join("sub/inner"))
        .await
        .expect("discover subdir");
    assert_eq!(repo2.toplevel(), top);
}

#[tokio::test]
async fn discover_rejects_non_repo() {
    let tmp = TempDir::new().unwrap();
    let path = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let err = GitRepo::discover(&path).await.unwrap_err();
    assert!(matches!(err, GitError::NotARepo { .. }), "{err}");
}

#[tokio::test]
async fn repo_context_on_branch_without_remote() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;
    let ctx = repo.repo_context().await.expect("context");
    assert_eq!(ctx.toplevel, top);
    assert_eq!(ctx.head, HeadState::Branch("main".into()));
    assert!(ctx.upstream.is_none());
    assert!(
        ctx.base.is_none(),
        "main with no remote has no base: {:?}",
        ctx.base
    );
}

#[tokio::test]
async fn repo_context_detached() {
    let (_tmp, top) = scratch_repo();
    git(top.as_std_path(), &["checkout", "-q", "--detach"]);
    let repo = open_repo(&top).await;
    let ctx = repo.repo_context().await.expect("context");
    match ctx.head {
        HeadState::Detached(oid) => assert!(!oid.as_str().is_empty()),
        other => panic!("expected detached, got {other:?}"),
    }
}

#[tokio::test]
async fn repo_context_unborn() {
    let (_tmp, top) = unborn_repo();
    let repo = open_repo(&top).await;
    let ctx = repo.repo_context().await.expect("context");
    assert_eq!(ctx.head, HeadState::Unborn);
    assert!(ctx.base.is_none());

    write(top.as_std_path(), "new.go", "package main\n");
    let cs = repo
        .changeset(ChangeScope::Unstaged)
        .await
        .expect("unstaged");
    assert_eq!(cs.files.len(), 1);
    assert_eq!(cs.files[0].status, FileStatus::Untracked);

    let err = repo.changeset(ChangeScope::Branch).await.unwrap_err();
    assert!(err.is_no_base(), "{err}");
}

#[tokio::test]
async fn staged_vs_unstaged_modification() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;

    // Staged edit to a.go, unstaged edit to b.txt.
    write(top.as_std_path(), "a.go", "package main\n\nimport \"fmt\"\n\n// A returns a constant used by tests.\nfunc A() int { return 42 }\n\nfunc helperOne() string { return \"one\" }\n\nfunc helperTwo() string { return \"two\" }\n\nfunc main() { fmt.Println(A()) }\n");
    git(top.as_std_path(), &["add", "a.go"]);
    write(
        top.as_std_path(),
        "b.txt",
        "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\n",
    );

    let staged = repo.changeset(ChangeScope::Staged).await.expect("staged");
    assert_eq!(staged.files.len(), 1);
    let f = &staged.files[0];
    assert_eq!(f.path, "a.go");
    assert_eq!(f.status, FileStatus::Modified);
    assert_eq!(f.hunks.len(), 1);
    assert_eq!(f.hunks[0].count_added(), 1);
    assert_eq!(f.hunks[0].count_deleted(), 1);

    let unstaged = repo
        .changeset(ChangeScope::Unstaged)
        .await
        .expect("unstaged");
    assert_eq!(unstaged.files.len(), 1);
    let f = &unstaged.files[0];
    assert_eq!(f.path, "b.txt");
    assert_eq!(f.status, FileStatus::Modified);
    // Change on line 5 with U3 context: old lines 2..=8.
    assert_eq!(f.hunks[0].old_start, 2);
    assert_eq!(f.hunks[0].old_len, 7);
}

#[tokio::test]
async fn untracked_files_reported_per_file() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;
    write(top.as_std_path(), "newdir/deep/x.go", "package deep\n");
    write(top.as_std_path(), "newdir/y.go", "package newdir\n");

    let cs = repo
        .changeset(ChangeScope::Unstaged)
        .await
        .expect("unstaged");
    let paths: Vec<_> = cs.files.iter().map(|f| f.path.as_str()).collect();
    // --untracked-files=all: individual files, not a collapsed "newdir/" entry.
    assert_eq!(paths, vec!["newdir/deep/x.go", "newdir/y.go"]);
    assert!(cs.files.iter().all(|f| f.status == FileStatus::Untracked));
    assert!(cs.files.iter().all(|f| f.hunks.is_empty()));
}

#[tokio::test]
async fn staged_rename_pure_and_with_edit() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;

    // Pure rename via git mv (index-only rename).
    git(top.as_std_path(), &["mv", "a.go", "renamed.go"]);
    let staged = repo.changeset(ChangeScope::Staged).await.expect("staged");
    assert_eq!(staged.files.len(), 1);
    let f = &staged.files[0];
    assert_eq!(f.status, FileStatus::Renamed { score: 100 });
    assert_eq!(f.path, "renamed.go");
    assert_eq!(f.old_path.as_deref().map(Utf8Path::as_str), Some("a.go"));
    assert!(f.hunks.is_empty(), "pure rename has no hunks");

    // Rename + edit: score < 100, hunks present, pairing intact (no pathspec).
    write(
        top.as_std_path(),
        "renamed.go",
        "package main\n\nimport \"fmt\"\n\n// A returns a constant used by tests.\nfunc A() int { return 2 }\n\nfunc helperOne() string { return \"one\" }\n\nfunc helperTwo() string { return \"two\" }\n\nfunc main() { fmt.Println(A()) }\n",
    );
    git(top.as_std_path(), &["add", "renamed.go"]);
    let staged = repo.changeset(ChangeScope::Staged).await.expect("staged");
    let f = &staged.files[0];
    match f.status {
        FileStatus::Renamed { score } => assert!(score < 100, "edited rename score {score}"),
        other => panic!("expected rename, got {other:?}"),
    }
    assert_eq!(f.old_path.as_deref().map(Utf8Path::as_str), Some("a.go"));
    assert_eq!(f.hunks.len(), 1);
}

#[tokio::test]
async fn deleted_staged_and_unstaged() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;

    git(top.as_std_path(), &["rm", "-q", "a.go"]);
    let staged = repo.changeset(ChangeScope::Staged).await.expect("staged");
    let f = staged.find_file(Utf8Path::new("a.go")).expect("a.go");
    assert_eq!(f.status, FileStatus::Deleted);
    assert!(f.hunks[0].is_pure_deletion());
    assert_eq!(f.hunks[0].new_len, 0);

    std::fs::remove_file(top.join("b.txt")).unwrap();
    let unstaged = repo
        .changeset(ChangeScope::Unstaged)
        .await
        .expect("unstaged");
    let f = unstaged.find_file(Utf8Path::new("b.txt")).expect("b.txt");
    assert_eq!(f.status, FileStatus::Deleted);
    assert!(f.hunks[0].is_pure_deletion());
}

#[tokio::test]
async fn pure_add_zero_old_len_and_comma_one_omission() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;

    // Commit a single-line file first, then dirty it: header omits ,1 (@@ -1 +1 @@).
    write(top.as_std_path(), "one.txt", "only\n");
    git(top.as_std_path(), &["add", "one.txt"]);
    git(
        top.as_std_path(),
        &["commit", "-q", "-m", "setup one-liner"],
    );
    write(top.as_std_path(), "one.txt", "changed\n");
    // New staged file: @@ -0,0 +1,N @@ (len 0 on the old side).
    write(
        top.as_std_path(),
        "added.go",
        "package main\n\nfunc B() {}\n",
    );
    git(top.as_std_path(), &["add", "added.go"]);

    let staged = repo.changeset(ChangeScope::Staged).await.expect("staged");
    let f = staged
        .find_file(Utf8Path::new("added.go"))
        .expect("added.go");
    assert_eq!(f.status, FileStatus::Added);
    let h = &f.hunks[0];
    assert!(h.is_pure_addition());
    assert_eq!(
        (h.old_start, h.old_len, h.new_start, h.new_len),
        (0, 0, 1, 3)
    );

    let unstaged = repo
        .changeset(ChangeScope::Unstaged)
        .await
        .expect("unstaged");
    let f = unstaged
        .find_file(Utf8Path::new("one.txt"))
        .expect("one.txt");
    let h = &f.hunks[0];
    assert_eq!(
        (h.old_start, h.old_len, h.new_start, h.new_len),
        (1, 1, 1, 1)
    );
    assert_eq!(h.lines.len(), 2);
}

#[tokio::test]
async fn binary_file_staged() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;
    std::fs::write(top.join("blob.bin"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
    git(top.as_std_path(), &["add", "blob.bin"]);

    let staged = repo.changeset(ChangeScope::Staged).await.expect("staged");
    let f = staged
        .find_file(Utf8Path::new("blob.bin"))
        .expect("blob.bin");
    assert!(f.binary);
    assert_eq!(f.status, FileStatus::Added);
    assert!(f.hunks.is_empty());
}

#[tokio::test]
async fn gitlink_staged_without_submodule_machinery() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;
    // Stage a gitlink entry directly (no network, no .gitmodules needed).
    let head = git(top.as_std_path(), &["rev-parse", "HEAD"]);
    git(
        top.as_std_path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},vendor/sub", head.trim()),
        ],
    );
    let staged = repo.changeset(ChangeScope::Staged).await.expect("staged");
    let f = staged
        .find_file(Utf8Path::new("vendor/sub"))
        .expect("gitlink");
    assert_eq!(f.status, FileStatus::Gitlink);
    assert!(f.hunks.is_empty());
}

#[tokio::test]
async fn branch_scope_via_fork_point() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;
    git(top.as_std_path(), &["checkout", "-q", "-b", "feature"]);
    write(top.as_std_path(), "a.go", "package main\n\nimport \"fmt\"\n\n// A returns a constant used by tests.\nfunc A() int { return 7 }\n\nfunc helperOne() string { return \"one\" }\n\nfunc helperTwo() string { return \"two\" }\n\nfunc main() { fmt.Println(A()) }\n");
    git(top.as_std_path(), &["commit", "-q", "-am", "feature edit"]);
    write(top.as_std_path(), "c.go", "package main\n\nfunc C() {}\n");
    git(top.as_std_path(), &["add", "c.go"]);
    git(top.as_std_path(), &["commit", "-q", "-m", "add c"]);

    let ctx = repo.repo_context().await.expect("context");
    let base = ctx.base.expect("base inferred");
    assert_eq!(base.source, BaseSource::ForkPoint);
    assert_eq!(base.ref_name, "main");
    let main_sha = git(top.as_std_path(), &["rev-parse", "main"]);
    assert_eq!(base.merge_base.as_str(), main_sha.trim());

    let cs = repo
        .changeset(ChangeScope::Branch)
        .await
        .expect("branch scope");
    let paths: Vec<_> = cs.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["a.go", "c.go"]);
    assert_eq!(cs.files[0].status, FileStatus::Modified);
    assert_eq!(cs.files[1].status, FileStatus::Added);

    let commits = repo.branch_commits(&base.merge_base).await.expect("log");
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0].subject, "add c"); // newest first
    assert_eq!(commits[1].subject, "feature edit");
}

#[tokio::test]
async fn base_inference_upstream_originhead_guess() {
    let (_tmp, top) = scratch_repo();
    let remote_tmp = TempDir::new().unwrap();
    git(remote_tmp.path(), &["init", "-q", "--bare", "-b", "main"]);
    let remote_path = remote_tmp.path().to_str().unwrap().to_string();
    git(
        top.as_std_path(),
        &["remote", "add", "origin", &remote_path],
    );
    git(top.as_std_path(), &["push", "-q", "origin", "main"]);

    let repo = open_repo(&top).await;

    // 3) Guess: origin/main exists, but no upstream and no origin/HEAD.
    git(top.as_std_path(), &["checkout", "-q", "-b", "feature"]);
    write(top.as_std_path(), "a.go", "package main\n\nimport \"fmt\"\n\n// A returns a constant used by tests.\nfunc A() int { return 9 }\n\nfunc helperOne() string { return \"one\" }\n\nfunc helperTwo() string { return \"two\" }\n\nfunc main() { fmt.Println(A()) }\n");
    git(top.as_std_path(), &["commit", "-q", "-am", "edit"]);
    let ctx = repo.repo_context().await.expect("context");
    let base = ctx.base.expect("base");
    assert_eq!(base.source, BaseSource::Guess);
    assert_eq!(base.ref_name, "origin/main");

    // 2) OriginHead: set refs/remotes/origin/HEAD.
    git(top.as_std_path(), &["remote", "set-head", "origin", "main"]);
    let base = repo.repo_context().await.unwrap().base.expect("base");
    assert_eq!(base.source, BaseSource::OriginHead);
    assert_eq!(base.ref_name, "origin/main");

    // 1) Upstream wins over everything.
    git(
        top.as_std_path(),
        &["push", "-q", "-u", "origin", "feature"],
    );
    write(top.as_std_path(), "a.go", "package main\n\nimport \"fmt\"\n\n// A returns a constant used by tests.\nfunc A() int { return 10 }\n\nfunc helperOne() string { return \"one\" }\n\nfunc helperTwo() string { return \"two\" }\n\nfunc main() { fmt.Println(A()) }\n");
    git(top.as_std_path(), &["commit", "-q", "-am", "ahead"]);
    let ctx = repo.repo_context().await.expect("context");
    let up = ctx.upstream.expect("upstream");
    assert_eq!(up.name, "origin/feature");
    assert_eq!(up.ahead, 1);
    assert_eq!(up.behind, 0);
    let base = ctx.base.expect("base");
    assert_eq!(base.source, BaseSource::Upstream);
    assert_eq!(base.ref_name, "origin/feature");
}

#[tokio::test]
async fn unmerged_conflict_marked_no_hunks() {
    let (_tmp, top) = scratch_repo();
    git(top.as_std_path(), &["checkout", "-q", "-b", "side"]);
    write(top.as_std_path(), "b.txt", "side version\n");
    git(top.as_std_path(), &["commit", "-q", "-am", "side edit"]);
    git(top.as_std_path(), &["checkout", "-q", "main"]);
    write(top.as_std_path(), "b.txt", "main version\n");
    git(top.as_std_path(), &["commit", "-q", "-am", "main edit"]);
    // Merge conflicts; git merge exits non-zero, so run it raw.
    let _ = Command::new("git")
        .arg("-C")
        .arg(top.as_std_path())
        .args(["merge", "side", "-q"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn merge");

    let repo = open_repo(&top).await;
    let unstaged = repo
        .changeset(ChangeScope::Unstaged)
        .await
        .expect("unstaged");
    let f = unstaged.find_file(Utf8Path::new("b.txt")).expect("b.txt");
    assert_eq!(f.status, FileStatus::Unmerged);
    assert!(f.hunks.is_empty());

    let staged = repo.changeset(ChangeScope::Staged).await.expect("staged");
    let f = staged.find_file(Utf8Path::new("b.txt")).expect("b.txt");
    assert_eq!(f.status, FileStatus::Unmerged);
    assert!(f.hunks.is_empty());
}

#[tokio::test]
async fn base_file_content_present_and_absent() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;
    write(top.as_std_path(), "a.go", "package main\n\nimport \"fmt\"\n\n// A returns a constant used by tests.\nfunc A() int { return 99 }\n\nfunc helperOne() string { return \"one\" }\n\nfunc helperTwo() string { return \"two\" }\n\nfunc main() { fmt.Println(A()) }\n");

    let content = repo
        .base_file_content("HEAD", Utf8Path::new("a.go"))
        .await
        .expect("show");
    assert_eq!(
        content.as_deref(),
        Some(
            "package main\n\nimport \"fmt\"\n\n// A returns a constant used by tests.\nfunc A() int { return 1 }\n\nfunc helperOne() string { return \"one\" }\n\nfunc helperTwo() string { return \"two\" }\n\nfunc main() { fmt.Println(A()) }\n"
        ),
        "must be the committed version, not the worktree edit"
    );

    let missing = repo
        .base_file_content("HEAD", Utf8Path::new("nope.go"))
        .await
        .expect("show missing");
    assert_eq!(missing, None);

    let bad_rev = repo
        .base_file_content("no-such-rev", Utf8Path::new("a.go"))
        .await;
    assert!(bad_rev.is_err(), "invalid revision must error");
}

#[tokio::test]
async fn fingerprint_stable_and_sensitive() {
    let (_tmp, top) = scratch_repo();
    let repo = open_repo(&top).await;

    let f1 = repo.fingerprint().await.expect("fp1");
    let f2 = repo.fingerprint().await.expect("fp2");
    assert_eq!(f1, f2, "fingerprint must be stable with no changes");
    assert_eq!(f1.len(), 32);

    write(top.as_std_path(), "a.go", "package main\n\nimport \"fmt\"\n\n// A returns a constant used by tests.\nfunc A() int { return 5 }\n\nfunc helperOne() string { return \"one\" }\n\nfunc helperTwo() string { return \"two\" }\n\nfunc main() { fmt.Println(A()) }\n");
    let f3 = repo.fingerprint().await.expect("fp3");
    assert_ne!(f1, f3, "worktree change must change the fingerprint");

    // Review 03 finding 5: a second edit to the already-modified file must also change the
    // fingerprint (porcelain v2 carries no worktree content hash).
    std::thread::sleep(std::time::Duration::from_millis(5));
    write(top.as_std_path(), "a.go", "package main\n\nimport \"fmt\"\n\nfunc A() int { return 6 }\n\nfunc helperOne() string { return \"one\" }\n\nfunc helperTwo() string { return \"two\" }\n\nfunc main() { fmt.Println(A()) }\n");
    let f3b = repo.fingerprint().await.expect("fp3b");
    assert_ne!(f3, f3b, "repeat edit of a modified file must change the fingerprint");

    git(top.as_std_path(), &["add", "a.go"]);
    let f4 = repo.fingerprint().await.expect("fp4");
    assert_ne!(f3, f4, "staging must change the fingerprint");

    git(top.as_std_path(), &["commit", "-q", "-m", "edit"]);
    let f5 = repo.fingerprint().await.expect("fp5");
    assert_ne!(f4, f5, "commit must change the fingerprint");

    // Unborn repos fingerprint too.
    let (_tmp2, top2) = unborn_repo();
    let repo2 = open_repo(&top2).await;
    assert_eq!(repo2.fingerprint().await.expect("fp unborn").len(), 32);
}

#[tokio::test]
async fn read_only_guarantee_smoke() {
    // Snapshot .git mtimes, run every query, verify nothing under .git changed shape.
    let (_tmp, top) = scratch_repo();
    write(top.as_std_path(), "a.go", "package main\n\nimport \"fmt\"\n\n// A returns a constant used by tests.\nfunc A() int { return 3 }\n\nfunc helperOne() string { return \"one\" }\n\nfunc helperTwo() string { return \"two\" }\n\nfunc main() { fmt.Println(A()) }\n");
    let repo = open_repo(&top).await;

    let index_before = std::fs::read(top.join(".git/index")).expect("read index");
    let _ = repo.repo_context().await.expect("ctx");
    let _ = repo.changeset(ChangeScope::Staged).await.expect("staged");
    let _ = repo
        .changeset(ChangeScope::Unstaged)
        .await
        .expect("unstaged");
    let _ = repo.fingerprint().await.expect("fp");
    let index_after = std::fs::read(top.join(".git/index")).expect("read index");
    assert_eq!(
        index_before, index_after,
        "the index must never be rewritten"
    );
}
