//! Integration tests for the JSON backend subcommands
//! (`codescope scan|changeset|analyze|digest|bases`).
//!
//! Each subcommand runs against a scratch git repo and the testutil Go fixture; the
//! assertions cover JSON shape, exit codes, the non-repo error contract, and the
//! determinism rule (repo-relative paths only, stable bytes across runs).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_codescope");

/// Run the codescope binary, isolated from any ambient git environment.
fn codescope(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("spawn codescope")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("utf8 stdout")
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("utf8 stderr")
}

/// Parse stdout as JSON, requiring exit code 0 first.
fn json_stdout(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "expected exit 0, got {:?}; stderr: {}",
        out.status.code(),
        stderr(out)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON: {e}: {}", stdout(out)))
}

/// Parse stderr as a `{"error": ...}` object, requiring exit code 1 first.
fn json_stderr_error(out: &Output) -> Value {
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1; stdout: {}",
        stdout(out)
    );
    assert!(
        out.stdout.is_empty(),
        "errors must not write to stdout: {}",
        stdout(out)
    );
    let value: Value = serde_json::from_slice(&out.stderr)
        .unwrap_or_else(|e| panic!("stderr is not JSON: {e}: {}", stderr(out)));
    assert!(
        value["error"].is_string(),
        "error object needs an \"error\" string: {value}"
    );
    value
}

/// The determinism rule: the repo root (as given and canonicalized) never appears.
fn assert_repo_relative(stdout: &str, root: &Path) {
    let canonical = std::fs::canonicalize(root).expect("canonicalize root");
    assert!(
        !stdout.contains(&*root.to_string_lossy()),
        "absolute repo root {} leaked into output:\n{stdout}",
        root.display()
    );
    assert!(
        !stdout.contains(&*canonical.to_string_lossy()),
        "canonical repo root {} leaked into output:\n{stdout}",
        canonical.display()
    );
}

// ---------------------------------------------------------------------------
// Repo builders
// ---------------------------------------------------------------------------

/// Run a git setup command (mirrors the codescope-git test helper).
fn git(dir: &Path, args: &[&str]) {
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
}

fn write(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, content).expect("write");
}

/// Two-branch repo: `main` (initial commit), `feature` (one commit on top, then a dirty
/// worktree edit). Not a supported-language project (a lone `.py` file), so no language
/// server applies.
fn scratch_repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).expect("mkdir");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "codescope"]);
    git(&dir, &["config", "user.email", "codescope@example.com"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);
    write(&dir, "src/notes.md", "hello\n");
    write(&dir, "app.py", "print(\"hi\")\n");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);
    git(&dir, &["checkout", "-qb", "feature"]);
    write(&dir, "src/notes.md", "hello world\n");
    git(&dir, &["commit", "-qam", "change"]);
    write(&dir, "app.py", "print(\"hi\")\nprint(\"dirty\")\n");
    (tmp, dir)
}

/// Single-branch repo (`main` only): no inferable base for the branch scope.
fn single_branch_repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).expect("mkdir");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "codescope"]);
    git(&dir, &["config", "user.email", "codescope@example.com"]);
    write(&dir, "readme.md", "hi\n");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);
    (tmp, dir)
}

/// A fresh copy of the testutil Go fixture (branch `feature/api-changes` over `main`,
/// staged edit + staged rename + unstaged edit + untracked file).
fn go_fixture() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("fx");
    let info = codescope_testutil::copy_fixture_into(&root).expect("copy fixture");
    (tmp, info.root)
}

// ---------------------------------------------------------------------------
// scan
// ---------------------------------------------------------------------------

#[test]
fn scan_fixture_reports_context_scopes_and_languages() {
    let (_tmp, root) = go_fixture();
    let out = codescope(&["scan", &root.to_string_lossy()]);
    let json = json_stdout(&out);

    // Repo context: branch + inferred base (ahead/behind ride on `upstream` when set).
    assert_eq!(json["repo"]["head"]["branch"], "feature/api-changes");
    assert_eq!(json["repo"]["base"]["ref_name"], "main");
    assert!(
        json["repo"].get("toplevel").is_none(),
        "absolute toplevel must be stripped: {json}"
    );

    // Per-scope change counts (fixture: staged edit+rename, unstaged edit+untracked).
    assert_eq!(json["scopes"]["staged"], 2);
    assert_eq!(json["scopes"]["unstaged"], 2);
    assert_eq!(json["scopes"]["working"], 3);
    assert!(json["scopes"]["branch"].as_u64().unwrap() >= 1);

    // Language detection + server availability probe.
    assert_eq!(json["languages"], serde_json::json!(["Go"]));
    assert_eq!(json["language_server"]["language"], "Go");
    assert!(json["language_server"]["available"].is_boolean());

    assert_repo_relative(&stdout(&out), &root);
}

#[test]
fn scan_scratch_repo_has_no_language_server() {
    let (_tmp, root) = scratch_repo();
    let out = codescope(&["scan", &root.to_string_lossy()]);
    let json = json_stdout(&out);
    assert_eq!(json["repo"]["head"]["branch"], "feature");
    assert_eq!(json["repo"]["base"]["ref_name"], "main");
    assert_eq!(json["scopes"]["branch"], 1);
    assert_eq!(json["scopes"]["working"], 1);
    assert_eq!(json["languages"], serde_json::json!(["Python"]));
    // Python is detected but has no adapter.
    assert!(json["language_server"].is_null());
    assert_repo_relative(&stdout(&out), &root);
}

#[test]
fn scan_tolerates_branch_scope_without_base() {
    let (_tmp, root) = single_branch_repo();
    let out = codescope(&["scan", &root.to_string_lossy()]);
    let json = json_stdout(&out);
    assert!(json["scopes"]["branch"].is_null());
    assert_eq!(json["scopes"]["working"], 0);
    let notes = json["notes"]
        .as_array()
        .expect("unavailable scope is explained in notes");
    assert!(
        notes.iter().any(|n| n.as_str().unwrap_or_default().contains("branch")),
        "note should name the branch scope: {json}"
    );
}

// ---------------------------------------------------------------------------
// changeset
// ---------------------------------------------------------------------------

#[test]
fn changeset_fixture_all_scopes() {
    let (_tmp, root) = go_fixture();
    let root = root.to_string_lossy().to_string();

    for scope in ["branch", "staged", "unstaged", "working"] {
        let out = codescope(&["changeset", &root, "--scope", scope]);
        let json = json_stdout(&out);
        assert_eq!(json["scope"], scope);
        assert!(json["files"].is_array(), "files array: {json}");
        assert_repo_relative(&stdout(&out), Path::new(&root));
    }

    // Default scope is branch.
    let out = codescope(&["changeset", &root]);
    let json = json_stdout(&out);
    assert_eq!(json["scope"], "branch");

    // Staged: the service edit and the rename (with its pre-rename path).
    let out = codescope(&["changeset", &root, "--scope", "staged"]);
    let json = json_stdout(&out);
    let files = json["files"].as_array().unwrap();
    let service = files
        .iter()
        .find(|f| f["path"] == "internal/service/service.go")
        .expect("service.go in staged set");
    assert_eq!(service["status"], "modified");
    assert!(
        !service["hunks"].as_array().unwrap().is_empty(),
        "hunks carry line numbers: {service}"
    );
    assert!(service["hunks"][0]["old_start"].is_u64());
    let renamed = files
        .iter()
        .find(|f| f["path"] == "internal/store/memstore.go")
        .expect("memstore.go in staged set");
    assert_eq!(renamed["status"]["renamed"]["score"], 100);
    assert_eq!(renamed["old_path"], "internal/store/memory.go");

    // Unstaged: the memstore edit plus the untracked file (no hunks for untracked).
    let out = codescope(&["changeset", &root, "--scope", "unstaged"]);
    let json = json_stdout(&out);
    let files = json["files"].as_array().unwrap();
    let health = files
        .iter()
        .find(|f| f["path"] == "internal/api/health.go")
        .expect("health.go in unstaged set");
    assert_eq!(health["status"], "untracked");
}

#[test]
fn changeset_scratch_repo_paths_are_relative() {
    let (_tmp, root) = scratch_repo();
    let out = codescope(&["changeset", &root.to_string_lossy(), "--scope", "working"]);
    let json = json_stdout(&out);
    let files = json["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "app.py");
    assert_eq!(files[0]["status"], "modified");
    assert_repo_relative(&stdout(&out), &root);
}

#[test]
fn changeset_branch_without_base_is_a_json_error() {
    let (_tmp, root) = single_branch_repo();
    let out = codescope(&["changeset", &root.to_string_lossy(), "--scope", "branch"]);
    let err = json_stderr_error(&out);
    assert!(
        err["error"].as_str().unwrap().contains("branch"),
        "error names the scope: {err}"
    );
}

// ---------------------------------------------------------------------------
// analyze
// ---------------------------------------------------------------------------

#[test]
fn analyze_fixture_full_snapshot() {
    let (_tmp, root) = go_fixture();
    let out = codescope(&["analyze", &root.to_string_lossy(), "--scope", "unstaged"]);
    let json = json_stdout(&out);

    for key in [
        "lsp",
        "epoch",
        "repo",
        "changeset",
        "files",
        "changed",
        "graph",
        "diagnostics",
        "digest",
    ] {
        assert!(json.get(key).is_some(), "missing top-level key {key}: {json}");
    }
    assert_eq!(json["changeset"]["scope"], "unstaged");

    if codescope_testutil::require_gopls().is_some() {
        assert_eq!(json["lsp"]["language"], "Go");
        let changed = json["changed"].as_array().unwrap();
        // The unstaged nil-guard edit lands on the MemoryRepo.Get method; the untracked
        // health.go contributes an added symbol.
        assert!(
            changed.iter().any(|c| c["name"]
                .as_str()
                .is_some_and(|n| n.contains("MemoryRepo") && n.contains("Get"))
                && c["record"]["change_kind"] == "modified"),
            "changed symbols: {changed:?}"
        );
        assert!(
            json["graph"]["value"]["edges"]
                .as_array()
                .is_some_and(|e| !e.is_empty()),
            "impact graph has callers of the changed method"
        );
        assert!(
            json["digest"]["changed_symbols"]
                .as_array()
                .is_some_and(|s| !s.is_empty()),
            "digest tier 1 populated"
        );
    } else {
        // No gopls in this environment: git-only mode still succeeds.
        assert!(json["lsp"].is_null());
    }

    // Per-file degradation notes never fail the run.
    let files = json["files"].as_array().unwrap();
    assert_eq!(files.len(), json["changeset"]["files"].as_array().unwrap().len());

    assert_repo_relative(&stdout(&out), &root);
}

#[test]
fn analyze_git_only_when_server_binary_missing() {
    let (_tmp, root) = go_fixture();
    let out = Command::new(BIN)
        .args(["analyze", &root.to_string_lossy(), "--scope", "unstaged"])
        .env("CODESCOPE_GOPLS", "/nonexistent/gopls")
        .output()
        .expect("spawn codescope");
    let json = json_stdout(&out);

    assert!(json["lsp"].is_null(), "git-only mode reports lsp: null: {json}");
    let notes = json["notes"].as_array().expect("git-only notes");
    assert!(
        notes.iter().any(|n| n.as_str().unwrap_or_default().contains("git-only")),
        "notes explain the degradation: {notes:?}"
    );
    // Files carry per-file notes and no semantic trees, but hunks survive in the digest.
    let files = json["files"].as_array().unwrap();
    assert!(files.iter().all(|f| !f["notes"].as_array().unwrap().is_empty()));
    assert!(files.iter().all(|f| f["worktree"].is_null()));
    assert!(json["changed"].as_array().unwrap().is_empty());
    assert!(
        !json["digest"]["hunks"].as_array().unwrap().is_empty(),
        "digest hunks are git-derived and must survive git-only mode"
    );
}

#[test]
fn analyze_scratch_repo_runs_git_only() {
    let (_tmp, root) = scratch_repo();
    let out = codescope(&["analyze", &root.to_string_lossy(), "--scope", "working"]);
    let json = json_stdout(&out);
    assert!(json["lsp"].is_null());
    assert_eq!(json["changeset"]["files"][0]["path"], "app.py");
    assert_repo_relative(&stdout(&out), &root);
}

// ---------------------------------------------------------------------------
// digest
// ---------------------------------------------------------------------------

#[test]
fn digest_fixture_json_and_text() {
    let (_tmp, root) = go_fixture();
    let root = root.to_string_lossy().to_string();

    let out = codescope(&["digest", &root, "--scope", "unstaged"]);
    let json = json_stdout(&out);
    for key in [
        "scope",
        "changed_symbols",
        "diagnostics",
        "hunks",
        "relations",
        "repo",
    ] {
        assert!(json.get(key).is_some(), "missing digest key {key}: {json}");
    }
    assert_eq!(json["scope"], "unstaged");
    assert!(
        !json["hunks"].as_array().unwrap().is_empty(),
        "digest hunks come from the change-set: {json}"
    );
    assert_repo_relative(&stdout(&out), Path::new(&root));

    // --text renders the prompt payload instead of JSON.
    let out = codescope(&["digest", &root, "--scope", "unstaged", "--text"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("# change digest\n"), "rendered digest: {text:?}");
    assert!(serde_json::from_str::<Value>(&text).is_err(), "--text is not JSON");
    assert_repo_relative(&text, Path::new(&root));
}

#[test]
fn digest_scratch_repo_git_only_notes() {
    let (_tmp, root) = scratch_repo();
    let out = codescope(&["digest", &root.to_string_lossy(), "--scope", "working"]);
    let json = json_stdout(&out);
    assert_eq!(json["scope"], "working");
    assert!(
        json["notes"].as_array().is_some_and(|notes| notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("git-only"))),
        "git-only digest carries a note: {json}"
    );
}

// ---------------------------------------------------------------------------
// bases
// ---------------------------------------------------------------------------

#[test]
fn bases_lists_candidates_for_fixture_and_scratch() {
    let (_tmp, root) = go_fixture();
    let out = codescope(&["bases", &root.to_string_lossy()]);
    let json = json_stdout(&out);
    let bases = json["bases"].as_array().expect("bases array");
    assert!(
        bases.iter().any(|b| b["ref_name"] == "main"),
        "fixture base candidates include main: {bases:?}"
    );
    assert_repo_relative(&stdout(&out), &root);

    let (_tmp2, root2) = scratch_repo();
    let out = codescope(&["bases", &root2.to_string_lossy()]);
    let json = json_stdout(&out);
    assert!(
        json["bases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["ref_name"] == "main"),
        "scratch base candidates include main"
    );

    let (_tmp3, root3) = single_branch_repo();
    let out = codescope(&["bases", &root3.to_string_lossy()]);
    let json = json_stdout(&out);
    assert_eq!(json["bases"], serde_json::json!([]));
}

// ---------------------------------------------------------------------------
// Error contract + determinism
// ---------------------------------------------------------------------------

#[test]
fn non_repo_path_errors_with_json_and_exit_1() {
    let tmp = TempDir::new().expect("tempdir"); // a plain directory, not a repo
    let path = tmp.path().to_string_lossy().to_string();
    for sub in ["scan", "changeset", "analyze", "digest", "bases"] {
        let out = codescope(&[sub, &path]);
        let err = json_stderr_error(&out);
        assert!(
            err["error"].as_str().unwrap().contains("not a git repository"),
            "{sub}: clear non-repo error: {err}"
        );
    }
}

#[test]
fn compact_is_single_line_and_output_is_deterministic() {
    let (_tmp, root) = go_fixture();
    let root = root.to_string_lossy().to_string();

    let pretty = codescope(&["changeset", &root, "--scope", "staged"]);
    let pretty_text = stdout(&pretty);
    assert!(
        pretty_text.lines().count() > 1,
        "pretty output spans lines: {pretty_text:?}"
    );

    let compact = codescope(&["changeset", &root, "--scope", "staged", "--compact"]);
    let compact_text = stdout(&compact);
    assert_eq!(
        compact_text.trim_end_matches('\n').lines().count(),
        1,
        "compact output is one line: {compact_text:?}"
    );

    // Same value, different formatting.
    let pretty_json: Value = serde_json::from_str(&pretty_text).unwrap();
    let compact_json: Value = serde_json::from_str(&compact_text).unwrap();
    assert_eq!(pretty_json, compact_json);

    // Byte-identical across runs.
    let again = codescope(&["changeset", &root, "--scope", "staged"]);
    assert_eq!(pretty.stdout, again.stdout, "deterministic bytes");

    let scan1 = codescope(&["scan", &root]);
    let scan2 = codescope(&["scan", &root]);
    assert_eq!(scan1.stdout, scan2.stdout, "scan deterministic bytes");
}
