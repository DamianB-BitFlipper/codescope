//! Integration tests for the deterministic Go fixture (research 08 §1).
//!
//! These run real `git` (always available per workspace toolchain) and skip gracefully on
//! machines without a Go toolchain.

use codescope_testutil::go_fixture::{
    self, FIXTURE_BASE, FIXTURE_BRANCH, HEAD_PREFIX_LEN, RENAMED_FROM, RENAMED_TO,
    STAGED_MODIFIED_FILE, UNTRACKED_FILE, build_fixture,
};
use codescope_testutil::helpers::{canonical_fixture, copy_fixture_into, require_go};
use std::path::Path;
use std::process::Command;

/// `git status --porcelain=v2 --branch -uall` in `root` (the exact invocation the git
/// layer standardizes on).
fn porcelain_v2(root: &Path) -> String {
    let out = Command::new("git")
        .args(["status", "--porcelain=v2", "--branch", "-uall"])
        .current_dir(root)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git status failed: {out:?}");
    String::from_utf8(out.stdout).expect("porcelain v2 is utf-8")
}

#[track_caller]
fn assert_porcelain_shape(status: &str, head_prefix: &str) {
    let lines: Vec<&str> = status.lines().collect();

    // Branch headers: on the feature branch, oid = the deterministic HEAD.
    assert!(
        lines
            .iter()
            .any(|l| *l == format!("# branch.head {FIXTURE_BRANCH}")),
        "missing branch.head header in:\n{status}"
    );
    let oid_line = lines
        .iter()
        .find(|l| l.starts_with("# branch.oid "))
        .expect("branch.oid header");
    assert!(
        oid_line["# branch.oid ".len()..].starts_with(head_prefix),
        "branch.oid does not start with head_prefix {head_prefix}: {oid_line}"
    );

    // Staged modification: `1 M. … internal/service/service.go`.
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("1 M. ") && l.ends_with(STAGED_MODIFIED_FILE)),
        "missing staged-M entry for {STAGED_MODIFIED_FILE} in:\n{status}"
    );

    // Staged rename carrying the unstaged modification: one `2 RM` entry,
    // `R100 <new>\t<old>` (TAB-separated, new path first).
    let rename_tail = format!("R100 {RENAMED_TO}\t{RENAMED_FROM}");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("2 RM ") && l.ends_with(&rename_tail)),
        "missing staged-R + unstaged-M rename entry ({rename_tail:?}) in:\n{status}"
    );

    // Untracked file.
    assert!(
        lines.iter().any(|l| *l == format!("? {UNTRACKED_FILE}")),
        "missing untracked entry for {UNTRACKED_FILE} in:\n{status}"
    );

    // Exactly three non-header entries: no stray dirt.
    let entries = lines.iter().filter(|l| !l.starts_with('#')).count();
    assert_eq!(entries, 3, "unexpected extra status entries in:\n{status}");
}

#[test]
fn fixture_head_is_stable_across_rebuilds() {
    let tmp = tempfile::tempdir().unwrap();
    let a = build_fixture(tmp.path().join("a")).unwrap();
    let b = build_fixture(tmp.path().join("b")).unwrap();

    assert_eq!(a.head_prefix, b.head_prefix, "OIDs must be deterministic");
    assert_eq!(a.head_prefix.as_str().len(), HEAD_PREFIX_LEN);
    assert!(
        a.head_prefix
            .as_str()
            .chars()
            .all(|c| c.is_ascii_hexdigit())
    );
    assert_eq!(a.branch, FIXTURE_BRANCH);
    assert_eq!(a.base, FIXTURE_BASE);
    assert_eq!(a.root, tmp.path().join("a"));
}

#[test]
fn fixture_rebuild_over_existing_dir_resets_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("fx");
    let first = build_fixture(&dir).unwrap();
    // Drop a stray file, then rebuild: it must be gone and the HEAD unchanged.
    std::fs::write(dir.join("stray.txt"), "junk").unwrap();
    let second = build_fixture(&dir).unwrap();
    assert_eq!(first.head_prefix, second.head_prefix);
    assert!(!dir.join("stray.txt").exists());
}

#[test]
fn fixture_porcelain_v2_shows_expected_states() {
    let tmp = tempfile::tempdir().unwrap();
    let info = build_fixture(tmp.path().join("fx")).unwrap();
    let status = porcelain_v2(&info.root);
    assert_porcelain_shape(&status, info.head_prefix.as_str());
}

#[test]
fn fixture_branch_diverges_from_main_by_two_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let info = build_fixture(tmp.path().join("fx")).unwrap();
    let out = Command::new("git")
        .args(["rev-list", "--count", "main..HEAD"])
        .current_dir(&info.root)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "2");
}

#[test]
fn copy_fixture_into_preserves_git_state() {
    let tmp = tempfile::tempdir().unwrap();
    let copy = copy_fixture_into(tmp.path().join("copy")).unwrap();
    let canonical = canonical_fixture().unwrap();

    assert_eq!(copy.head_prefix, canonical.head_prefix);
    assert_ne!(copy.root, canonical.root);
    let status = porcelain_v2(&copy.root);
    assert_porcelain_shape(&status, copy.head_prefix.as_str());
}

#[test]
fn fixture_typechecks_formats_and_tests_clean() {
    if require_go().is_none() {
        eprintln!("SKIP: go toolchain not found on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let info = build_fixture(tmp.path().join("fx")).unwrap();

    // The dirty worktree must still typecheck (gopls needs this, research 08 §1.4).
    go_fixture::go_build(&info.root).unwrap();

    // Everything gofmt-clean, including the staged/unstaged edited variants.
    let unformatted = go_fixture::gofmt_unformatted(&info.root).unwrap();
    assert!(unformatted.is_empty(), "gofmt -l reported: {unformatted:?}");

    // The two store tests pass.
    go_fixture::go_test(&info.root).unwrap();
}
