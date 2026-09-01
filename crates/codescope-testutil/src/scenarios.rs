//! Reproducible git-repository scenarios for testing codescope's git + analysis layers.
//!
//! Each [`Scenario`] builds a scratch repo in a temp dir with a fixed identity and fixed
//! dates (deterministic), applies steps in order, and records expected git-facing facts.
//! The integration test in `crates/codescope/tests/scenarios.rs` drives each one.
//!
//! No TUI, no network, no language server required — these assert the git layer and the
//! git-only analysis path.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use codescope_core::Utf8PathBuf;
use tempfile::TempDir;

/// One deterministic scratch-repo scenario.
pub struct Scenario {
    /// Human-readable name (test case label).
    pub name: &'static str,
    /// Setup steps, applied in order.
    pub steps: Vec<Step>,
    /// Expected facts after setup.
    pub expect: Expect,
}

/// A setup step.
#[derive(Debug, Clone)]
pub enum Step {
    /// Write `content` to `path` (creating parent dirs).
    Write {
        /// Repo-relative path.
        path: &'static str,
        /// File content.
        content: &'static str,
    },
    /// Append `content` to `path`.
    Append {
        /// Repo-relative path.
        path: &'static str,
        /// Content to append.
        content: &'static str,
    },
    /// `git add <path>` (stage).
    Add {
        /// Repo-relative path.
        path: &'static str,
    },
    /// `git add -A` (stage everything).
    AddAll,
    /// `git commit -m <msg>` with the scenario's fixed date.
    Commit {
        /// Commit message.
        msg: &'static str,
    },
    /// `git checkout -b <name>`.
    Branch {
        /// New branch name.
        name: &'static str,
    },
    /// `git checkout <name>`.
    Checkout {
        /// Branch to switch to.
        name: &'static str,
    },
    /// Detach HEAD at the current commit.
    Detach,
    /// `git mv <from> <to>` (staged rename).
    Rename {
        /// Source path.
        from: &'static str,
        /// Destination path.
        to: &'static str,
    },
    /// `git rm <path>` (staged delete).
    Remove {
        /// Repo-relative path.
        path: &'static str,
    },
    /// Delete `path` from the worktree without staging (unstaged delete).
    DeleteWorktree {
        /// Repo-relative path.
        path: &'static str,
    },
    /// `git branch --set-upstream-to <to>` for the current branch.
    SetUpstream {
        /// The ref to track.
        to: &'static str,
    },
    /// Create a bare remote and push the current branch.
    AddRemoteAndPush {
        /// Remote name.
        remote: &'static str,
    },
}

/// Expected facts a scenario asserts. `Option` fields are only checked when `Some`.
#[derive(Debug, Default)]
pub struct Expect {
    /// Expected current branch name, or `"(detached)"` / `"(no commits)"`.
    pub branch: Option<&'static str>,
    /// Expected base source for the Branch scope (None = don't assert).
    pub base_source: Option<&'static str>,
    /// Whether a base is expected at all.
    pub has_base: Option<bool>,
    /// Changed-file counts per scope: (branch, staged, unstaged, working).
    pub scope_counts: Option<(usize, usize, usize, usize)>,
    /// Statuses that must appear somewhere across all scopes.
    pub must_have_status: Vec<&'static str>,
    /// Whether the fingerprint must change after a follow-up edit (the test edits).
    pub fingerprint_changes_on_edit: bool,
    /// Whether `discover` should fail (non-git dir).
    pub discover_fails: bool,
}

/// A built scenario. `_tmp` keeps the dir alive.
pub struct Built {
    _tmp: TempDir,
    /// Repo root (or plain dir for `non_git_dir`).
    pub root: std::path::PathBuf,
    /// Whether this is a git repo.
    pub git: bool,
}

impl Built {
    /// The repo root as a UTF-8 path.
    pub fn root_utf8(&self) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(self.root.clone()).expect("utf8 path")
    }

    /// Make a follow-up edit (for fingerprint-change assertions).
    pub fn edit(&self, rel: &str, content: &str) {
        write(&self.root, rel, content);
    }
}

/// Build the scenario's repo. Returns the [`Built`] handle (keep alive).
pub fn build(s: &Scenario) -> Result<Built> {
    let tmp = TempDir::new().context("tempdir")?;
    let root = tmp.path().to_path_buf();

    if s.name == "non_git_dir" {
        std::fs::write(root.join("lonely.txt"), "not a repo\n")?;
        return Ok(Built {
            _tmp: tmp,
            root,
            git: false,
        });
    }

    git(&root, &["init", "-q", "-b", "main"])?;
    git(&root, &["config", "user.name", "codescope"])?;
    git(&root, &["config", "user.email", "codescope@example.com"])?;
    git(&root, &["config", "commit.gpgsign", "false"])?;
    git(&root, &["config", "core.autocrlf", "false"])?;

    for step in &s.steps {
        apply(&root, step)?;
    }
    Ok(Built {
        _tmp: tmp,
        root,
        git: true,
    })
}

/// The library of named scenarios.
pub fn all() -> Vec<Scenario> {
    vec![
        single_commit_repo(),
        dirty_worktree(),
        staged_only(),
        mixed_staged_unstaged_untracked(),
        detached_head(),
        unborn_branch(),
        renamed_file(),
        deleted_file(),
        merge_conflict(),
        branch_fully_pushed(),
        stacked_branches(),
        deep_nesting(),
        binary_change(),
        special_char_filename(),
        crlf_file(),
        non_git_dir(),
    ]
}

fn base_commit() -> Vec<Step> {
    vec![
        Step::Write {
            path: "main.go",
            content: "package main\n\nfunc main() {}\n",
        },
        Step::Write {
            path: "util.go",
            content: "package main\n\nfunc Helper() int { return 1 }\n",
        },
        Step::Write {
            path: "README.md",
            content: "# scratch\n",
        },
        Step::AddAll,
        Step::Commit { msg: "initial" },
    ]
}

fn single_commit_repo() -> Scenario {
    Scenario {
        name: "single_commit_repo",
        steps: base_commit(),
        expect: Expect {
            branch: Some("main"),
            has_base: Some(false),
            scope_counts: Some((0, 0, 0, 0)),
            ..Default::default()
        },
    }
}

fn dirty_worktree() -> Scenario {
    let mut steps = base_commit();
    steps.push(Step::Append {
        path: "util.go",
        content: "\n// unstaged edit\n",
    });
    Scenario {
        name: "dirty_worktree",
        steps,
        expect: Expect {
            branch: Some("main"),
            scope_counts: Some((0, 0, 1, 1)),
            fingerprint_changes_on_edit: true,
            ..Default::default()
        },
    }
}

fn staged_only() -> Scenario {
    let mut steps = base_commit();
    steps.push(Step::Append {
        path: "util.go",
        content: "\n// staged edit\n",
    });
    steps.push(Step::Add { path: "util.go" });
    Scenario {
        name: "staged_only",
        steps,
        expect: Expect {
            branch: Some("main"),
            scope_counts: Some((0, 1, 0, 1)),
            ..Default::default()
        },
    }
}

fn mixed_staged_unstaged_untracked() -> Scenario {
    let mut steps = base_commit();
    steps.push(Step::Append {
        path: "util.go",
        content: "\n// staged\n",
    });
    steps.push(Step::Add { path: "util.go" });
    steps.push(Step::Append {
        path: "main.go",
        content: "\n// unstaged\n",
    });
    steps.push(Step::Write {
        path: "untracked.go",
        content: "package main\n",
    });
    Scenario {
        name: "mixed_staged_unstaged_untracked",
        steps,
        expect: Expect {
            branch: Some("main"),
            scope_counts: Some((0, 1, 2, 3)),
            ..Default::default()
        },
    }
}

fn detached_head() -> Scenario {
    let mut steps = base_commit();
    steps.push(Step::Write {
        path: "x.go",
        content: "package main\n",
    });
    steps.push(Step::AddAll);
    steps.push(Step::Commit { msg: "second" });
    steps.push(Step::Detach);
    Scenario {
        name: "detached_head",
        steps,
        expect: Expect {
            branch: Some("(detached)"),
            scope_counts: Some((0, 0, 0, 0)),
            ..Default::default()
        },
    }
}

fn unborn_branch() -> Scenario {
    Scenario {
        name: "unborn_branch",
        steps: vec![Step::Write {
            path: "new.go",
            content: "package main\n",
        }],
        expect: Expect {
            branch: Some("(no commits)"),
            has_base: Some(false),
            scope_counts: Some((0, 0, 1, 1)),
            ..Default::default()
        },
    }
}

fn renamed_file() -> Scenario {
    let mut steps = base_commit();
    steps.push(Step::Rename {
        from: "util.go",
        to: "helper.go",
    });
    Scenario {
        name: "renamed_file",
        steps,
        expect: Expect {
            branch: Some("main"),
            // git mv is staged: staged=1, unstaged=0, working=1 (HEAD vs worktree).
            scope_counts: Some((0, 1, 0, 1)),
            must_have_status: vec!["renamed"],
            ..Default::default()
        },
    }
}

fn deleted_file() -> Scenario {
    let mut steps = base_commit();
    steps.push(Step::Remove { path: "util.go" });
    Scenario {
        name: "deleted_file",
        steps,
        expect: Expect {
            branch: Some("main"),
            // git rm is staged: staged=1, unstaged=0, working=1.
            scope_counts: Some((0, 1, 0, 1)),
            must_have_status: vec!["deleted"],
            ..Default::default()
        },
    }
}

fn merge_conflict() -> Scenario {
    // main and other diverge from the initial commit; merging main into other conflicts.
    let mut steps = base_commit();
    steps.push(Step::Branch { name: "other" }); // create + switch to `other` (at initial)
    steps.push(Step::Append {
        path: "util.go",
        content: "\n// other side\n",
    });
    steps.push(Step::AddAll);
    steps.push(Step::Commit { msg: "other edit" }); // on `other`
    steps.push(Step::Checkout { name: "main" }); // back to main (at initial)
    steps.push(Step::Append {
        path: "util.go",
        content: "\n// main side\n",
    });
    steps.push(Step::AddAll);
    steps.push(Step::Commit { msg: "main edit" }); // on `main`
    steps.push(Step::Checkout { name: "other" }); // back to `other`
    steps.push(Step::Commit { msg: "__merge__" }); // sentinel: merge main into other
    Scenario {
        name: "merge_conflict",
        steps,
        expect: Expect {
            branch: Some("other"),
            must_have_status: vec!["unmerged"],
            ..Default::default()
        },
    }
}

fn branch_fully_pushed() -> Scenario {
    // merge-base(upstream, HEAD) == HEAD: the branch is at its upstream tip, nothing committed.
    // This is not a meaningful comparison base; other worktree scopes remain available.
    let mut steps = base_commit();
    steps.push(Step::Branch { name: "feature" }); // feature == HEAD == main tip
    steps.push(Step::SetUpstream { to: "main" });
    steps.push(Step::Append {
        path: "util.go",
        content: "\n// dirty\n",
    });
    Scenario {
        name: "branch_fully_pushed",
        steps,
        expect: Expect {
            branch: Some("feature"),
            has_base: Some(false),
            scope_counts: Some((0, 0, 1, 1)),
            ..Default::default()
        },
    }
}

fn stacked_branches() -> Scenario {
    let mut steps = base_commit();
    steps.push(Step::Branch { name: "a" });
    steps.push(Step::Write {
        path: "a.go",
        content: "package main\n",
    });
    steps.push(Step::AddAll);
    steps.push(Step::Commit { msg: "a1" });
    steps.push(Step::Branch { name: "b" });
    steps.push(Step::Write {
        path: "b.go",
        content: "package main\n",
    });
    steps.push(Step::AddAll);
    steps.push(Step::Commit { msg: "b1" });
    Scenario {
        name: "stacked_branches",
        steps,
        expect: Expect {
            branch: Some("b"),
            base_source: Some("ancestor"),
            has_base: Some(true),
            scope_counts: Some((1, 0, 0, 0)), // b.go is new on b vs a
            ..Default::default()
        },
    }
}

fn deep_nesting() -> Scenario {
    let mut steps = base_commit();
    steps.push(Step::Write {
        path: "a/b/c/d/e/deep.go",
        content: "package e\n",
    });
    Scenario {
        name: "deep_nesting",
        steps,
        expect: Expect {
            branch: Some("main"),
            scope_counts: Some((0, 0, 1, 1)),
            ..Default::default()
        },
    }
}

fn binary_change() -> Scenario {
    let mut steps = base_commit();
    steps.push(Step::Write {
        path: "bin.dat",
        content: "\u{0}\u{1}\u{2}\u{ff}",
    });
    steps.push(Step::AddAll);
    steps.push(Step::Commit { msg: "add binary" });
    steps.push(Step::Write {
        path: "bin.dat",
        content: "\u{0}\u{1}\u{3}\u{fe}\u{fd}",
    });
    Scenario {
        name: "binary_change",
        steps,
        expect: Expect {
            branch: Some("main"),
            scope_counts: Some((0, 0, 1, 1)),
            ..Default::default()
        },
    }
}

fn special_char_filename() -> Scenario {
    let mut steps = base_commit();
    steps.push(Step::Write {
        path: "sp ace.go",
        content: "package main\n",
    });
    Scenario {
        name: "special_char_filename",
        steps,
        expect: Expect {
            branch: Some("main"),
            scope_counts: Some((0, 0, 1, 1)),
            ..Default::default()
        },
    }
}

fn crlf_file() -> Scenario {
    let mut steps = base_commit();
    steps.push(Step::Write {
        path: "crlf.go",
        content: "package main\r\n\r\nfunc C() {}\r\n",
    });
    steps.push(Step::AddAll);
    steps.push(Step::Commit { msg: "crlf" });
    steps.push(Step::Append {
        path: "crlf.go",
        content: "\r\n// edit\r\n",
    });
    Scenario {
        name: "crlf_file",
        steps,
        expect: Expect {
            branch: Some("main"),
            scope_counts: Some((0, 0, 1, 1)),
            ..Default::default()
        },
    }
}

fn non_git_dir() -> Scenario {
    Scenario {
        name: "non_git_dir",
        steps: vec![],
        expect: Expect {
            discover_fails: true,
            ..Default::default()
        },
    }
}

// --- engine ------------------------------------------------------------------

fn apply(root: &Path, step: &Step) -> Result<()> {
    match step {
        Step::Write { path, content } => write(root, path, content),
        Step::Append { path, content } => {
            let p = root.join(path);
            let mut existing = std::fs::read_to_string(&p).unwrap_or_default();
            existing.push_str(content);
            std::fs::write(&p, existing)?;
        }
        Step::Add { path } => {
            git(root, &["add", path])?;
        }
        Step::AddAll => {
            git(root, &["add", "-A"])?;
        }
        Step::Commit { msg } => {
            if *msg == "__merge__" {
                // Merge "main" into the current branch, expecting a conflict; do not commit.
                let _ = Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(["merge", "--no-commit", "--no-ff", "main"])
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_SYSTEM", "/dev/null")
                    .output()
                    .context("spawn git merge")?;
            } else {
                git(root, &["commit", "-q", "-m", msg])?;
            }
        }
        Step::Branch { name } => {
            git(root, &["checkout", "-q", "-b", name])?;
        }
        Step::Checkout { name } => {
            git(root, &["checkout", "-q", name])?;
        }
        Step::Detach => {
            git(root, &["checkout", "-q", "--detach"])?;
        }
        Step::Rename { from, to } => {
            git(root, &["mv", from, to])?;
        }
        Step::Remove { path } => {
            git(root, &["rm", "-q", path])?;
        }
        Step::DeleteWorktree { path } => {
            std::fs::remove_file(root.join(path))?;
        }
        Step::SetUpstream { to } => {
            git(root, &["branch", "--set-upstream-to", to])?;
        }
        Step::AddRemoteAndPush { remote } => {
            // Keep the bare remote INSIDE the scenario's tempdir (no cross-test collision,
            // cleaned up with the rest). Create it first; init requires an existing dir.
            let bare = root.join(format!("_{remote}.git"));
            std::fs::create_dir_all(&bare)?;
            git(&bare, &["init", "-q", "--bare"])?;
            git(root, &["remote", "add", remote, bare.to_str().unwrap()])?;
            git(root, &["push", "-q", "-u", remote, "HEAD"])?;
        }
    }
    Ok(())
}

fn write(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, content).expect("write");
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .context("spawn git")?;
    anyhow::ensure!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout).expect("utf8 stdout"))
}
