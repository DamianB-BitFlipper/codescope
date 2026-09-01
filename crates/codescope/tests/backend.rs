//! Integration tests for the JSON backend subcommands
//! (`codescope scan|changeset|analyze|digest|bases|debug-ai`).
//!
//! Each subcommand runs against a scratch git repo and the testutil Go fixture; the
//! assertions cover JSON shape, exit codes, the non-repo error contract, and the
//! determinism rule (repo-relative paths only, stable bytes across runs).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use codescope_core::{
    Epoch, FormKind, PlanEdge, PlanEdgeKind, PlanEvidence, PlanNode, PlanNodeChange,
    VisualizationPlan, VizForm,
};
use codescope_testutil::fake_ai::{AiScriptStep, ScriptedProvider};
use codescope_testutil::scenarios;
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
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("branch")),
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
        assert!(
            json.get(key).is_some(),
            "missing top-level key {key}: {json}"
        );
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
        // The refresh-time impact graph is shallow (call-hierarchy is lazy since the perf fix):
        // it carries the changed-symbol nodes; callers/callees are fetched on selection via
        // AnalysisEngine::callers_of/callees_of, not eagerly into the graph.
        assert!(
            json["graph"]["value"]["nodes"]
                .as_array()
                .is_some_and(|n| !n.is_empty()),
            "impact graph has the changed-symbol nodes"
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
    assert_eq!(
        files.len(),
        json["changeset"]["files"].as_array().unwrap().len()
    );

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

    assert!(
        json["lsp"].is_null(),
        "git-only mode reports lsp: null: {json}"
    );
    let notes = json["notes"].as_array().expect("git-only notes");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("git-only")),
        "notes explain the degradation: {notes:?}"
    );
    // Files carry per-file notes and no semantic trees, but hunks survive in the digest.
    let files = json["files"].as_array().unwrap();
    assert!(files
        .iter()
        .all(|f| !f["notes"].as_array().unwrap().is_empty()));
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
    assert!(
        text.starts_with("# change digest\n"),
        "rendered digest: {text:?}"
    );
    assert!(
        serde_json::from_str::<Value>(&text).is_err(),
        "--text is not JSON"
    );
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
// debug-ai: the real dispatcher, no terminal frontend
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_ai_prints_the_validated_dispatcher_plan_headlessly() {
    // One initial repository refresh owns one epoch increment.
    let mut plan = VisualizationPlan::new(Epoch(1), "What does the selected change do?");
    plan.title = "Request handling becomes observable".to_string();
    plan.intent = "Record request metadata before continuing the request path.".to_string();
    plan.review_focus = Some(
        "Confirm the recorded metadata reaches the log sink before errors are returned."
            .to_string(),
    );
    plan.evidence.push(codescope_core::PlanEvidence {
        file: codescope_core::FileId::new_unchecked("internal/api/middleware.go"),
        hunk: Some(0),
        symbol: None,
        range: None,
        reason: "defines the middleware that records the request metadata".to_string(),
    });
    let mut entry = PlanNode::new("entry", "request", PlanNodeChange::Modified)
        .with_detail("enters the changed request path")
        .with_code_ref(codescope_core::PlanCodeRef::new(
            codescope_core::FileId::new_unchecked("internal/api/middleware.go"),
            0,
            codescope_core::DiffSide::New,
            6,
            7,
        ));
    entry.children.push("logging".to_string());
    let logging = PlanNode::new("logging", "record metadata", PlanNodeChange::Added)
        .with_detail("captures context before continuing")
        .with_code_ref(codescope_core::PlanCodeRef::new(
            codescope_core::FileId::new_unchecked("internal/api/middleware.go"),
            0,
            codescope_core::DiffSide::New,
            8,
            8,
        ));
    plan.forms.push(VizForm {
        kind: FormKind::ChangedSymbolTree,
        title: "Request path".to_string(),
        summary: String::new(),
        nodes: vec![entry, logging],
        edges: Vec::new(),
    });
    // The AI input contract requires 1-4 evidence entries over real fixture files.
    plan.evidence.push(PlanEvidence {
        file: codescope_core::FileId::new("internal/api/middleware.go").unwrap(),
        hunk: Some(0),
        symbol: None,
        range: None,
        reason: "the middleware hunk that records request metadata".to_string(),
    });
    let provider = ScriptedProvider::start([AiScriptStep::from_plan(&plan).unwrap()])
        .await
        .unwrap();
    let (_fixture, root) = go_fixture();
    let config_dir = TempDir::new().unwrap();
    let config_path = config_dir.path().join("missing-config.toml");
    let endpoint = format!("{}/v1", provider.base_url());
    let root_string = root.to_string_lossy().to_string();

    let out = tokio::task::spawn_blocking(move || {
        Command::new(BIN)
            .args([
                "debug-ai",
                &root_string,
                "--scope",
                "branch",
                "--timeout-secs",
                "20",
                "--model",
                "codescope-fake",
                "--reasoning-effort",
                "high",
            ])
            .env("CODESCOPE_CONFIG", config_path)
            .env("CODESCOPE_AI", "on")
            .env("CODESCOPE_AI_BASE_URL", endpoint)
            .env("CODESCOPE_AI_TIMEOUT_MS", "5000")
            .env("OPENAI_API_KEY", "sk-test-only")
            .env("CODESCOPE_GOPLS", "/nonexistent/codescope-test-gopls")
            .env_remove("PRIME_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("spawn headless codescope")
    })
    .await
    .unwrap();

    assert!(
        out.status.success(),
        "headless command failed: {}; requests: {:?}",
        stderr(&out),
        provider.requests()
    );
    let json = json_stdout(&out);
    assert_eq!(json["scope"], "branch");
    assert!(json["selection"]["file"].is_string());
    assert_eq!(json["provider"], "custom");
    assert_eq!(json["model"], "codescope-fake");
    assert_eq!(json["reasoning_effort"], "high");
    assert_eq!(
        json["plan"]["intent"],
        "Record request metadata before continuing the request path."
    );
    assert_eq!(
        json["plan"]["forms"][0]["nodes"].as_array().unwrap().len(),
        2
    );
    // The full validation report rides next to the plan (Terra: report preservation).
    assert_eq!(json["report"]["verdict"], "valid");
    assert!(
        json["report"]["dropped"].is_null(),
        "a clean plan serializes no dropped items: {json}"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "one backend AI request");
    let body = requests[0].body_json().expect("debug-ai request JSON");
    assert_eq!(body["reasoning_effort"], "high");
    assert!(body.get("reasoning").is_none());
    let tool_names: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tool_names,
        ["submit_visualization_plan"],
        "the production NoToolExecutor must not advertise unusable read tools"
    );
    assert!(
        requests[0].body.contains("current impact selection"),
        "headless path uses the dispatcher's focused prompt"
    );
}

/// The sequence extra-edge sanitizer's `ValidWithDrops` report must survive the
/// dispatcher/backend boundary: `debug-ai` JSON carries the full report (verdict,
/// dropped items with reasons) next to the sanitized plan — never a silent omission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_ai_json_keeps_the_sanitizer_report_for_dropped_sequence_edges() {
    // A three-step sequence with both required consecutive edges plus one extra
    // back-edge (n3 -> n1): the validator keeps the chain, drops the back-edge, and
    // records it — the plan is sanitized, not trusted.
    // One initial repository refresh owns one epoch increment.
    let mut plan = VisualizationPlan::new(Epoch(1), "How does the request flow change?");
    plan.title = "Request logging wraps the handler".to_string();
    plan.intent = "The middleware logs each request around the handled call.".to_string();
    plan.review_focus = Some(
        "Confirm the log line is written before the handler consumes the request.".to_string(),
    );
    plan.forms.push(VizForm {
        kind: FormKind::Sequence,
        title: "request path".to_string(),
        summary: String::new(),
        nodes: vec![
            PlanNode::new("n1", "request", PlanNodeChange::Modified)
                .with_detail("enters the changed request path")
                .with_code_ref(codescope_core::PlanCodeRef::new(
                    codescope_core::FileId::new_unchecked("internal/api/middleware.go"),
                    0,
                    codescope_core::DiffSide::New,
                    6,
                    7,
                )),
            PlanNode::new("n2", "logging", PlanNodeChange::Added)
                .with_detail("records the request id before continuing")
                .with_code_ref(codescope_core::PlanCodeRef::new(
                    codescope_core::FileId::new_unchecked("internal/api/middleware.go"),
                    0,
                    codescope_core::DiffSide::New,
                    8,
                    8,
                )),
            PlanNode::new("n3", "handler", PlanNodeChange::Modified)
                .with_detail("consumes the logged request")
                .with_code_ref(codescope_core::PlanCodeRef::new(
                    codescope_core::FileId::new_unchecked("internal/api/middleware.go"),
                    0,
                    codescope_core::DiffSide::New,
                    9,
                    13,
                )),
        ],
        edges: vec![
            PlanEdge {
                from: "n1".into(),
                to: "n2".into(),
                kind: PlanEdgeKind::Writes,
                label: Some("carries the request id".into()),
            },
            PlanEdge {
                from: "n2".into(),
                to: "n3".into(),
                kind: PlanEdgeKind::Writes,
                label: Some("passes the logged request".into()),
            },
            PlanEdge {
                from: "n3".into(),
                to: "n1".into(),
                kind: PlanEdgeKind::Writes,
                label: Some("returns the response".into()),
            },
        ],
    });
    plan.evidence.push(PlanEvidence {
        file: codescope_core::FileId::new("internal/api/middleware.go").unwrap(),
        hunk: Some(0),
        symbol: None,
        range: None,
        reason: "the middleware hunk that wraps the handler".to_string(),
    });
    let provider = ScriptedProvider::start([AiScriptStep::from_plan(&plan).unwrap()])
        .await
        .unwrap();
    let (_fixture, root) = go_fixture();
    let config_dir = TempDir::new().unwrap();
    let config_path = config_dir.path().join("missing-config.toml");
    let endpoint = format!("{}/v1", provider.base_url());
    let root_string = root.to_string_lossy().to_string();

    let out = tokio::task::spawn_blocking(move || {
        Command::new(BIN)
            .args([
                "debug-ai",
                &root_string,
                "--scope",
                "branch",
                "--timeout-secs",
                "20",
                "--model",
                "codescope-fake",
            ])
            .env("CODESCOPE_CONFIG", config_path)
            .env("CODESCOPE_AI", "on")
            .env("CODESCOPE_AI_BASE_URL", endpoint)
            .env("CODESCOPE_AI_TIMEOUT_MS", "5000")
            .env("OPENAI_API_KEY", "sk-test-only")
            .env("CODESCOPE_GOPLS", "/nonexistent/codescope-test-gopls")
            .env_remove("PRIME_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("spawn headless codescope")
    })
    .await
    .unwrap();

    assert!(
        out.status.success(),
        "headless command failed: {}; requests: {:?}",
        stderr(&out),
        provider.requests()
    );
    let json = json_stdout(&out);
    // The sanitizer's verdict and dropped items reach the JSON intact.
    assert_eq!(
        json["report"]["verdict"], "valid_with_drops",
        "sanitized plan keeps its report: {json}"
    );
    let dropped = json["report"]["dropped"].as_array().expect("dropped items");
    assert_eq!(dropped.len(), 1, "exactly the extra back-edge: {json}");
    assert_eq!(
        dropped[0]["subject"], "edge n3 -> n1 in form 0",
        "drop subject: {json}"
    );
    let reason = dropped[0]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("nonconsecutive or duplicate sequence edge"),
        "drop reason: {reason}"
    );
    // The published plan itself was sanitized to the consecutive chain.
    let edges = json["plan"]["forms"][0]["edges"]
        .as_array()
        .expect("form edges");
    assert_eq!(edges.len(), 2, "the back-edge no longer renders: {json}");
    // One backend request: the sanitized plan needed no repair turn.
    assert_eq!(provider.requests().len(), 1);
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
    for sub in [
        "scan",
        "changeset",
        "analyze",
        "digest",
        "bases",
        "debug-ai",
    ] {
        let out = codescope(&[sub, &path]);
        let err = json_stderr_error(&out);
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("not a git repository"),
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

// ---------------------------------------------------------------------------
// Extended backend coverage
//
// A repo-shape matrix (every subcommand against every scenario shape), the
// branch-fallback contract, exact fixture scope semantics, digest tier
// structure, CLI parse errors, and byte-level determinism. Scenario repos come
// from codescope-testutil; gopls-dependent assertions stay env-gated and
// git-only runs are forced with CODESCOPE_GOPLS=/nonexistent.
// ---------------------------------------------------------------------------

/// Environment override that forces git-only analysis (no gopls spawn).
const GIT_ONLY_ENV: &[(&str, &str)] = &[("CODESCOPE_GOPLS", "/nonexistent/codescope-test-gopls")];

/// Run the codescope binary with extra environment variables.
fn codescope_env(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.output().expect("spawn codescope")
}

/// Run the codescope binary with `dir` as the working directory (PATH defaults to ".").
fn codescope_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("spawn codescope")
}

/// Build a named codescope-testutil scenario repo (kept alive by the returned handle).
fn scenario_repo(name: &str) -> scenarios::Built {
    let scenario = scenarios::all()
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("scenario {name} exists"));
    scenarios::build(&scenario).unwrap_or_else(|e| panic!("build scenario {name}: {e}"))
}

/// Clean repo: `main` plus `feature` (one committed file), nothing uncommitted.
fn clean_repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).expect("mkdir");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "codescope"]);
    git(&dir, &["config", "user.email", "codescope@example.com"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);
    write(&dir, "main.go", "package main\n\nfunc main() {}\n");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);
    git(&dir, &["checkout", "-qb", "feature"]);
    write(&dir, "extra.go", "package main\n\nfunc Extra() {}\n");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "add extra"]);
    (tmp, dir)
}

/// Expected per-scope file counts for a repo shape (`branch`: `None` = no inferable base).
struct ShapeExpect {
    branch: Option<usize>,
    staged: usize,
    unstaged: usize,
    working: usize,
}

/// The `analyze` top-level keys (the snapshot projection contract).
fn assert_analyze_keys(json: &Value, label: &str) {
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
        assert!(
            json.get(key).is_some(),
            "{label}: analyze missing key {key}: {json}"
        );
    }
}

/// The `digest` tier keys (the five tiers of the AI prompt payload).
fn assert_digest_tiers(json: &Value, label: &str) {
    for key in [
        "scope",
        "changed_symbols",
        "diagnostics",
        "hunks",
        "relations",
        "repo",
    ] {
        assert!(
            json.get(key).is_some(),
            "{label}: digest missing tier {key}: {json}"
        );
    }
}

/// The `(path, status)` pairs of a changeset, structured statuses compacted to JSON text.
fn paths_and_statuses(json: &Value) -> Vec<(String, String)> {
    json["files"]
        .as_array()
        .expect("files array")
        .iter()
        .map(|f| {
            let status = f["status"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| serde_json::to_string(&f["status"]).expect("status json"));
            (f["path"].as_str().expect("path string").to_string(), status)
        })
        .collect()
}

/// The shared subcommand contract for one repo shape: every subcommand runs, JSON parses,
/// the scope counts match, and no absolute path leaks. Analysis runs are forced git-only
/// (fast, deterministic); the LSP-honoring paths get their own gated tests below.
fn check_shape(root: &Path, label: &str, expect: ShapeExpect) {
    let root_s = root.to_string_lossy().to_string();

    // scan: repo context (toplevel stripped) + per-scope counts.
    let out = codescope(&["scan", &root_s]);
    let json = json_stdout(&out);
    for key in ["repo", "scopes", "languages", "language_server"] {
        assert!(
            json.get(key).is_some(),
            "{label}: scan missing key {key}: {json}"
        );
    }
    assert!(
        json["repo"].get("toplevel").is_none(),
        "{label}: absolute toplevel must be stripped: {json}"
    );
    match expect.branch {
        Some(n) => assert_eq!(
            json["scopes"]["branch"].as_u64(),
            Some(n as u64),
            "{label}: scan branch count"
        ),
        None => assert!(
            json["scopes"]["branch"].is_null(),
            "{label}: branch scope without a base reports null: {json}"
        ),
    }
    assert_eq!(
        json["scopes"]["staged"].as_u64(),
        Some(expect.staged as u64),
        "{label}: staged"
    );
    assert_eq!(
        json["scopes"]["unstaged"].as_u64(),
        Some(expect.unstaged as u64),
        "{label}: unstaged"
    );
    assert_eq!(
        json["scopes"]["working"].as_u64(),
        Some(expect.working as u64),
        "{label}: working"
    );
    assert_repo_relative(&stdout(&out), root);

    // changeset --scope working: scope echo + exact file count; fallback stays false.
    let out = codescope(&["changeset", &root_s, "--scope", "working"]);
    let json = json_stdout(&out);
    assert_eq!(json["scope"], "working", "{label}: changeset scope echo");
    assert_eq!(
        json["files"].as_array().expect("files array").len(),
        expect.working,
        "{label}: working file count: {json}"
    );
    assert_eq!(
        json["fallback"].as_bool(),
        Some(false),
        "{label}: only the branch scope ever falls back: {json}"
    );
    assert_repo_relative(&stdout(&out), root);

    // analyze --scope working (git-only): full key shape, lsp null, explanatory notes.
    let out = codescope_env(&["analyze", &root_s, "--scope", "working"], GIT_ONLY_ENV);
    let json = json_stdout(&out);
    assert_analyze_keys(&json, label);
    assert!(
        json["lsp"].is_null(),
        "{label}: forced git-only reports lsp null: {json}"
    );
    assert_eq!(json["changeset"]["scope"], "working");
    assert_eq!(
        json["changeset"]["files"]
            .as_array()
            .expect("files array")
            .len(),
        expect.working,
        "{label}: analyze working file count"
    );
    assert!(
        json["notes"].as_array().is_some_and(|n| !n.is_empty()),
        "{label}: git-only analyze explains itself in notes: {json}"
    );
    assert_repo_relative(&stdout(&out), root);

    // digest --scope working (git-only): tier keys + repo sketch.
    let out = codescope_env(&["digest", &root_s, "--scope", "working"], GIT_ONLY_ENV);
    let json = json_stdout(&out);
    assert_digest_tiers(&json, label);
    assert_eq!(json["scope"], "working");
    assert!(
        json["repo"]["head"].is_string(),
        "{label}: digest sketch head: {json}"
    );
    assert!(
        json["repo"]["dirs"].is_array(),
        "{label}: digest sketch dirs: {json}"
    );
    assert_repo_relative(&stdout(&out), root);

    // bases: always succeeds on a repo, even when empty.
    let out = codescope(&["bases", &root_s]);
    let json = json_stdout(&out);
    assert!(json["bases"].is_array(), "{label}: bases array: {json}");
    assert_repo_relative(&stdout(&out), root);
}

#[test]
fn subcommand_matrix_all_repo_shapes() {
    let shapes: [(&str, ShapeExpect); 7] = [
        // Clean tree, single `main` branch (no inferable base), one dirty file.
        (
            "dirty_worktree",
            ShapeExpect {
                branch: None,
                staged: 0,
                unstaged: 1,
                working: 1,
            },
        ),
        // Staged + unstaged + untracked on a single branch.
        (
            "mixed_staged_unstaged_untracked",
            ShapeExpect {
                branch: None,
                staged: 1,
                unstaged: 2,
                working: 3,
            },
        ),
        // Same-tip upstream: no meaningful branch base; worktree scopes still work.
        (
            "branch_fully_pushed",
            ShapeExpect {
                branch: None,
                staged: 0,
                unstaged: 1,
                working: 1,
            },
        ),
        // main <- a <- b: branch scope of b holds only b.go.
        (
            "stacked_branches",
            ShapeExpect {
                branch: Some(1),
                staged: 0,
                unstaged: 0,
                working: 0,
            },
        ),
        // No commits yet: branch scope has no base, the untracked file is visible.
        (
            "unborn_branch",
            ShapeExpect {
                branch: None,
                staged: 0,
                unstaged: 1,
                working: 1,
            },
        ),
        // Mid-merge with a conflict: the unmerged file counts in every scope.
        (
            "merge_conflict",
            ShapeExpect {
                branch: Some(1),
                staged: 1,
                unstaged: 1,
                working: 1,
            },
        ),
        // A staged pure rename on a single branch.
        (
            "renamed_file",
            ShapeExpect {
                branch: None,
                staged: 1,
                unstaged: 0,
                working: 1,
            },
        ),
    ];
    for (name, expect) in shapes {
        let built = scenario_repo(name);
        check_shape(&built.root, name, expect);
    }

    let (_tmp, root) = clean_repo();
    check_shape(
        &root,
        "clean_repo",
        ShapeExpect {
            branch: Some(1),
            staged: 0,
            unstaged: 0,
            working: 0,
        },
    );

    let (_tmp, root) = go_fixture();
    check_shape(
        &root,
        "go_fixture",
        ShapeExpect {
            branch: Some(2),
            staged: 2,
            unstaged: 2,
            working: 3,
        },
    );
}

// ---------------------------------------------------------------------------
// A same-tip upstream is not a comparison base
// ---------------------------------------------------------------------------

#[test]
fn fully_pushed_branch_reports_no_base_and_keeps_working_scope() {
    let built = scenario_repo("branch_fully_pushed");
    let root = built.root.to_string_lossy().to_string();

    let out = codescope(&["changeset", &root, "--scope", "branch"]);
    let err = json_stderr_error(&out);
    assert!(
        err["error"].as_str().unwrap().contains("no base"),
        "branch scope reports an honest missing base: {err}"
    );

    for scope in ["staged", "unstaged", "working"] {
        let out = codescope(&["changeset", &root, "--scope", scope]);
        let json = json_stdout(&out);
        assert_eq!(json["fallback"].as_bool(), Some(false), "{scope}: {json}");
    }
    let out = codescope(&["changeset", &root, "--scope", "working"]);
    let json = json_stdout(&out);
    assert_eq!(json["files"][0]["path"], "util.go");
}

#[test]
fn bases_fully_pushed_branch_excludes_every_head_equivalent_ref() {
    let built = scenario_repo("branch_fully_pushed");
    let root = built.root.to_string_lossy().to_string();

    let out = codescope(&["bases", &root]);
    let json = json_stdout(&out);
    let bases = json["bases"].as_array().unwrap();
    assert!(
        bases.is_empty(),
        "all refs are at HEAD and must be excluded: {json}"
    );
}

// ---------------------------------------------------------------------------
// Stacked branches with a remote-only parent (review 26)

/// The ROOTFS shape: a local far ancestor (rootfs-1), a REMOTE-ONLY nearest ancestor
/// (origin/rootfs-2, no local branch), and HEAD (rootfs-3) whose upstream is HEAD-equivalent.
/// The inferred base must be the remote-only rootfs-2, and the picker must include it and
/// exclude the empty origin/rootfs-3.
#[test]
fn stacked_branch_with_remote_only_parent_infers_it() {
    let dir = std::env::temp_dir().join(format!(
        "codescope-rootfs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@test.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@test.invalid")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    // rootfs-1: the far ancestor (local).
    git(&["init", "-q", "-b", "rootfs-1"]);
    std::fs::write(
        dir.join("a.txt"),
        "one
",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "--no-verify", "-m", "r1"]);
    // rootfs-2: one commit on top, pushed to a remote so it exists only as origin/rootfs-2.
    git(&["checkout", "-q", "-b", "rootfs-2"]);
    std::fs::write(
        dir.join("a.txt"),
        "one
two
",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "--no-verify", "-m", "r2"]);
    // A bare remote to hold the remote-only ref. It must live OUTSIDE the repo dir, or its
    // files would appear in the diff.
    let bare = dir.parent().unwrap().join(format!(
        "_origin-{}.git",
        dir.file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(&bare).unwrap();
    let bare_str = bare.to_string_lossy().to_string();
    git(&["-C", &bare_str, "init", "-q", "--bare"]);
    git(&["remote", "add", "origin", &bare_str]);
    git(&["push", "-q", "-u", "origin", "rootfs-2"]);
    // rootfs-3: HEAD, one commit on top of rootfs-2, upstream == HEAD (empty).
    git(&["checkout", "-q", "-b", "rootfs-3"]);
    std::fs::write(
        dir.join("a.txt"),
        "one
two
three
",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "--no-verify", "-m", "r3"]);
    git(&["push", "-q", "-u", "origin", "rootfs-3"]);
    // Delete the local rootfs-2 branch so it exists ONLY as origin/rootfs-2.
    git(&["branch", "-q", "-D", "rootfs-2"]);

    let root = dir.to_string_lossy().to_string();
    let out = codescope(&["scan", &root]);
    let json = json_stdout(&out);
    assert_eq!(json["repo"]["head"]["branch"], "rootfs-3");
    assert_eq!(
        json["repo"]["base"]["ref_name"], "origin/rootfs-2",
        "the nearest strict ancestor is the remote-only rootfs-2: {}",
        json["repo"]
    );

    let out = codescope(&["bases", &root]);
    let json = json_stdout(&out);
    let names: Vec<&str> = json["bases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["ref_name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"origin/rootfs-2"),
        "rootfs-2 in the picker: {names:?}"
    );
    assert!(
        !names.contains(&"origin/rootfs-3"),
        "the HEAD-equivalent upstream is excluded: {names:?}"
    );

    // Branch scope shows only the rootfs-3 delta (1 file) vs rootfs-2.
    let out = codescope(&["changeset", &root, "--scope", "branch"]);
    let json = json_stdout(&out);
    let files = json["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "only the rootfs-3 change: {json}");

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Stacked branches (nearest-ancestor base)
// ---------------------------------------------------------------------------

#[test]
fn stacked_branches_branch_scope_uses_nearest_ancestor() {
    let built = scenario_repo("stacked_branches"); // main <- a <- b, HEAD = b
    let root = built.root.to_string_lossy().to_string();

    let out = codescope(&["scan", &root]);
    let json = json_stdout(&out);
    assert_eq!(json["repo"]["head"]["branch"], "b");
    assert_eq!(
        json["repo"]["base"]["ref_name"], "a",
        "base is the nearest ancestor, not main"
    );
    assert_eq!(json["repo"]["base"]["source"], "ancestor");

    // The branch change-set diffs against a: only b.go (a.go belongs to branch a).
    let out = codescope(&["changeset", &root, "--scope", "branch"]);
    let json = json_stdout(&out);
    let files = json["files"].as_array().unwrap();
    assert_eq!(
        files.len(),
        1,
        "a.go must not leak into b's branch scope: {json}"
    );
    assert_eq!(files[0]["path"], "b.go");
    assert_eq!(files[0]["status"], "added");
    assert_eq!(json["fallback"].as_bool(), Some(false));

    // analyze follows the same base: one file under analysis.
    let out = codescope_env(&["analyze", &root, "--scope", "branch"], GIT_ONLY_ENV);
    let json = json_stdout(&out);
    assert_eq!(json["changeset"]["files"].as_array().unwrap().len(), 1);
    assert_eq!(json["files"][0]["file"], "b.go");

    // The digest repo sketch names the ancestor base.
    let out = codescope(&["digest", &root, "--scope", "branch"]);
    let json = json_stdout(&out);
    assert_eq!(json["repo"]["head"], "b");
    assert_eq!(json["repo"]["base_ref"], "a");
}

#[test]
fn bases_stacked_branches_order_nearest_ancestor_first() {
    let built = scenario_repo("stacked_branches");
    let root = built.root.to_string_lossy().to_string();

    let out = codescope(&["bases", &root]);
    let json = json_stdout(&out);
    let bases = json["bases"].as_array().unwrap();
    let names: Vec<&str> = bases
        .iter()
        .map(|b| b["ref_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a", "main"], "nearest ancestor first: {bases:?}");
    assert!(
        bases.iter().all(|b| b["source"] == "ancestor"),
        "both resolved via the ancestor chain: {bases:?}"
    );
    // The current branch is never its own base candidate.
    assert!(!names.contains(&"b"), "current branch excluded: {names:?}");
}

// ---------------------------------------------------------------------------
// Unborn branch (no commits)
// ---------------------------------------------------------------------------

#[test]
fn unborn_branch_scan_reports_unborn_head_and_null_branch_scope() {
    let built = scenario_repo("unborn_branch");
    let root = built.root.to_string_lossy().to_string();

    let out = codescope(&["scan", &root]);
    let json = json_stdout(&out);
    assert_eq!(
        json["repo"]["head"], "unborn",
        "unborn head serialization: {json}"
    );
    assert!(
        json["scopes"]["branch"].is_null(),
        "no base without commits: {json}"
    );
    assert_eq!(json["scopes"]["staged"], 0);
    assert_eq!(
        json["scopes"]["unstaged"], 1,
        "the untracked file is unstaged-visible"
    );
    assert_eq!(json["scopes"]["working"], 1);
    let notes = json["notes"].as_array().expect("unavailable scope noted");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("branch")),
        "note names the branch scope: {notes:?}"
    );
    assert_repo_relative(&stdout(&out), &built.root);
}

#[test]
fn unborn_branch_branch_scope_errors_but_other_subcommands_work() {
    let built = scenario_repo("unborn_branch");
    let root = built.root.to_string_lossy().to_string();

    // Branch scope cannot be computed: JSON error, exit 1, empty stdout.
    let argsets: [&[&str]; 3] = [
        &["changeset", &root, "--scope", "branch"],
        &["analyze", &root, "--scope", "branch"],
        &["digest", &root, "--scope", "branch"],
    ];
    for args in argsets {
        let out = codescope(args);
        let err = json_stderr_error(&out);
        assert!(
            err["error"].as_str().unwrap().contains("branch"),
            "{args:?}: error names the branch scope: {err}"
        );
    }

    // bases: an empty candidate list is a success, not an error.
    let out = codescope(&["bases", &root]);
    let json = json_stdout(&out);
    assert_eq!(json["bases"], serde_json::json!([]));

    // working scope sees the untracked file (no hunks for untracked content).
    let out = codescope(&["changeset", &root, "--scope", "working"]);
    let json = json_stdout(&out);
    let files = json["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "new.go");
    assert_eq!(files[0]["status"], "untracked");
    assert_eq!(files[0]["hunks"], serde_json::json!([]));
    assert_eq!(json["fallback"].as_bool(), Some(false));

    // analyze/digest survive in git-only mode on the unborn repo.
    let out = codescope(&["analyze", &root, "--scope", "working"]);
    let json = json_stdout(&out);
    assert!(json["lsp"].is_null());
    assert_eq!(json["repo"]["head"], "unborn");
    assert_eq!(json["files"][0]["file"], "new.go");
    assert_eq!(json["files"][0]["status"], "untracked");

    let out = codescope(&["digest", &root, "--scope", "working"]);
    let json = json_stdout(&out);
    assert_eq!(json["scope"], "working");
    assert_eq!(
        json["repo"]["head"], "(unborn)",
        "sketch renders the unborn head: {json}"
    );
    assert!(
        json["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("git-only")),
        "git-only digest carries a note: {json}"
    );
}

// ---------------------------------------------------------------------------
// Merge conflict
// ---------------------------------------------------------------------------

#[test]
fn merge_conflict_reports_unmerged_files_without_failing() {
    let built = scenario_repo("merge_conflict");
    let root = built.root.to_string_lossy().to_string();

    // scan succeeds and counts the conflicted file in every scope.
    let out = codescope(&["scan", &root]);
    let json = json_stdout(&out);
    assert_eq!(json["repo"]["head"]["branch"], "other");
    for scope in ["branch", "staged", "unstaged", "working"] {
        assert_eq!(json["scopes"][scope].as_u64(), Some(1), "{scope}: {json}");
    }

    // The tree scopes mark the conflicted file unmerged; combined diffs carry no hunks.
    for scope in ["staged", "unstaged", "working"] {
        let out = codescope(&["changeset", &root, "--scope", scope]);
        let json = json_stdout(&out);
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{scope}: {json}");
        assert_eq!(files[0]["path"], "util.go", "{scope}");
        assert_eq!(files[0]["status"], "unmerged", "{scope}");
        assert_eq!(
            files[0]["hunks"],
            serde_json::json!([]),
            "{scope}: unmerged files are not hunk-parsed"
        );
        assert_eq!(json["fallback"].as_bool(), Some(false), "{scope}");
    }

    // The branch scope is unaffected by the conflict: the committed side is a plain edit.
    let out = codescope(&["changeset", &root, "--scope", "branch"]);
    let json = json_stdout(&out);
    assert_eq!(json["files"][0]["status"], "modified");
    assert!(
        !json["files"][0]["hunks"].as_array().unwrap().is_empty(),
        "committed divergence carries hunks: {json}"
    );

    // bases still resolve (main is the inferred base).
    let out = codescope(&["bases", &root]);
    let json = json_stdout(&out);
    assert!(
        json["bases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["ref_name"] == "main"),
        "conflict does not block base candidates: {json}"
    );
}

#[test]
fn merge_conflict_analyze_and_digest_survive_git_only() {
    let built = scenario_repo("merge_conflict");
    let root = built.root.to_string_lossy().to_string();

    let out = codescope(&["analyze", &root, "--scope", "working"]);
    let json = json_stdout(&out);
    assert_analyze_keys(&json, "merge_conflict analyze");
    assert!(json["lsp"].is_null(), "no go.mod → git-only: {json}");
    let files = json["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["file"], "util.go");
    assert_eq!(files[0]["status"], "unmerged");
    assert!(
        files[0]["worktree"].is_null(),
        "no symbol tree for a conflicted file"
    );
    assert!(
        files[0]["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("Unmerged")),
        "the conflict is explained per file: {files:?}"
    );
    assert_repo_relative(&stdout(&out), &built.root);

    let out = codescope(&["digest", &root, "--scope", "working"]);
    let json = json_stdout(&out);
    assert_digest_tiers(&json, "merge_conflict digest");
    assert_eq!(json["scope"], "working");
    assert_eq!(
        json["hunks"],
        serde_json::json!([]),
        "unmerged files contribute no hunks"
    );
    assert!(
        json["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("git-only")),
        "git-only note present: {json}"
    );
    assert_repo_relative(&stdout(&out), &built.root);
}

// ---------------------------------------------------------------------------
// Renamed file (staged rename on a single branch)
// ---------------------------------------------------------------------------

#[test]
fn renamed_file_change_sets_carry_rename_status_and_old_path() {
    let built = scenario_repo("renamed_file"); // git mv util.go helper.go, staged
    let root = built.root.to_string_lossy().to_string();

    // The staged rename is visible from the staged and the working scope.
    for scope in ["staged", "working"] {
        let out = codescope(&["changeset", &root, "--scope", scope]);
        let json = json_stdout(&out);
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{scope}: {json}");
        assert_eq!(files[0]["path"], "helper.go", "{scope}: post-rename path");
        assert_eq!(files[0]["old_path"], "util.go", "{scope}: pre-rename path");
        assert_eq!(
            files[0]["status"]["renamed"]["score"], 100,
            "{scope}: identical content"
        );
        assert_eq!(
            files[0]["hunks"],
            serde_json::json!([]),
            "{scope}: pure rename has no hunks"
        );
    }

    // Nothing is left unstaged (the rename is fully staged).
    let out = codescope(&["changeset", &root, "--scope", "unstaged"]);
    let json = json_stdout(&out);
    assert_eq!(json["files"], serde_json::json!([]));
    assert_eq!(json["fallback"].as_bool(), Some(false));

    // Single-branch main: the branch scope has no base and errors; bases is empty.
    let out = codescope(&["changeset", &root, "--scope", "branch"]);
    let err = json_stderr_error(&out);
    assert!(err["error"].as_str().unwrap().contains("branch"), "{err}");
    let out = codescope(&["bases", &root]);
    let json = json_stdout(&out);
    assert_eq!(json["bases"], serde_json::json!([]));

    // digest --scope staged succeeds with an empty hunk tier (rename-only change).
    let out = codescope(&["digest", &root, "--scope", "staged"]);
    let json = json_stdout(&out);
    assert_eq!(json["scope"], "staged");
    assert_eq!(json["hunks"], serde_json::json!([]));
}

// ---------------------------------------------------------------------------
// Clean repo (committed branch, nothing dirty)
// ---------------------------------------------------------------------------

#[test]
fn clean_repo_empty_uncommitted_scopes_and_committed_branch_scope() {
    let (_tmp, root) = clean_repo();
    let root = root.to_string_lossy().to_string();

    let out = codescope(&["scan", &root]);
    let json = json_stdout(&out);
    assert_eq!(json["repo"]["head"]["branch"], "feature");
    assert_eq!(json["repo"]["base"]["ref_name"], "main");
    assert_eq!(json["scopes"]["branch"], 1, "the committed feature file");
    assert_eq!(json["scopes"]["staged"], 0);
    assert_eq!(json["scopes"]["unstaged"], 0);
    assert_eq!(json["scopes"]["working"], 0);
    assert!(
        json.get("notes").is_none(),
        "no unavailable scopes → notes key omitted: {json}"
    );

    for scope in ["staged", "unstaged", "working"] {
        let out = codescope(&["changeset", &root, "--scope", scope]);
        let json = json_stdout(&out);
        assert_eq!(json["files"], serde_json::json!([]), "{scope}: clean");
        assert_eq!(json["fallback"].as_bool(), Some(false), "{scope}");
    }

    let out = codescope(&["changeset", &root, "--scope", "branch"]);
    let json = json_stdout(&out);
    let files = json["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "extra.go");
    assert_eq!(files[0]["status"], "added");
    assert_eq!(
        json["fallback"].as_bool(),
        Some(false),
        "committed diff is non-empty"
    );

    // digest of an empty scope: valid, empty tiers, exit 0.
    let out = codescope(&["digest", &root, "--scope", "working"]);
    let json = json_stdout(&out);
    assert_digest_tiers(&json, "clean digest");
    assert_eq!(json["changed_symbols"], serde_json::json!([]));
    assert_eq!(json["hunks"], serde_json::json!([]));
    assert_eq!(json["repo"]["head"], "feature");
    assert_eq!(json["repo"]["base_ref"], "main");
    assert_eq!(json["repo"]["dirs"], serde_json::json!([]));

    // --text renders the tier headers even when every tier is empty.
    let out = codescope(&["digest", &root, "--scope", "working", "--text"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("# change digest\n"), "{text}");
    assert!(text.contains("## changed symbols (0)"), "{text}");
    assert!(text.contains("## hunks (0)"), "{text}");

    // analyze of an empty scope: the snapshot shape holds with empty content.
    let out = codescope(&["analyze", &root, "--scope", "working"]);
    let json = json_stdout(&out);
    assert_analyze_keys(&json, "clean analyze");
    assert_eq!(json["changeset"]["files"], serde_json::json!([]));
    assert_eq!(json["changed"], serde_json::json!([]));
    assert_repo_relative(&stdout(&out), std::path::Path::new(&root));
}

// ---------------------------------------------------------------------------
// Go fixture: exact scope semantics
// ---------------------------------------------------------------------------

#[test]
fn fixture_scope_semantics_exact_file_sets() {
    let (_tmp, root) = go_fixture();
    let root = root.to_string_lossy().to_string();

    // branch: the two feature commits (middleware.go added, postgres.go edited).
    let out = codescope(&["changeset", &root, "--scope", "branch"]);
    let json = json_stdout(&out);
    assert_eq!(
        paths_and_statuses(&json),
        [
            (
                "internal/api/middleware.go".to_string(),
                "added".to_string()
            ),
            (
                "internal/store/postgres.go".to_string(),
                "modified".to_string()
            ),
        ],
        "branch = the 2-commit divergence from main: {json}"
    );
    // A newly added file's hunks contain only add lines.
    let middleware = &json["files"][0];
    assert!(
        middleware["hunks"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|h| h["lines"].as_array().unwrap().iter())
            .all(|l| l["kind"] == "add"),
        "added file carries only add lines: {middleware}"
    );

    // staged: the service edit + the pure rename (score 100, old_path recorded).
    let out = codescope(&["changeset", &root, "--scope", "staged"]);
    let json = json_stdout(&out);
    assert_eq!(
        paths_and_statuses(&json),
        [
            (
                "internal/service/service.go".to_string(),
                "modified".to_string()
            ),
            (
                "internal/store/memstore.go".to_string(),
                r#"{"renamed":{"score":100}}"#.to_string()
            ),
        ],
        "staged = service edit + rename: {json}"
    );
    assert_eq!(json["files"][1]["old_path"], "internal/store/memory.go");

    // unstaged: the memstore edit + the untracked health.go.
    let out = codescope(&["changeset", &root, "--scope", "unstaged"]);
    let json = json_stdout(&out);
    assert_eq!(
        paths_and_statuses(&json),
        [
            (
                "internal/api/health.go".to_string(),
                "untracked".to_string()
            ),
            (
                "internal/store/memstore.go".to_string(),
                "modified".to_string()
            ),
        ],
        "unstaged = memstore edit + untracked: {json}"
    );
    assert_eq!(
        json["files"][0]["hunks"],
        serde_json::json!([]),
        "untracked files carry no hunks"
    );

    // working: all three uncommitted changes; the rename survives with the edit folded in.
    let out = codescope(&["changeset", &root, "--scope", "working"]);
    let json = json_stdout(&out);
    let files = json["files"].as_array().unwrap();
    assert_eq!(
        files
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "internal/api/health.go",
            "internal/service/service.go",
            "internal/store/memstore.go"
        ],
        "working = staged + unstaged + untracked: {json}"
    );
    assert_eq!(files[0]["status"], "untracked");
    assert_eq!(files[1]["status"], "modified");
    let score = files[2]["status"]["renamed"]["score"]
        .as_u64()
        .expect("memstore is a rename in the working scope");
    assert!(
        (50..=100).contains(&score),
        "rename similarity survives the edit: {score}"
    );
    assert_eq!(files[2]["old_path"], "internal/store/memory.go");
}

// ---------------------------------------------------------------------------
// digest: tier structure + rendered text
// ---------------------------------------------------------------------------

#[test]
fn digest_json_has_the_five_tier_structure() {
    let (_tmp, root) = go_fixture();
    let root = root.to_string_lossy().to_string();

    let out = codescope(&["digest", &root, "--scope", "unstaged"]);
    let json = json_stdout(&out);
    assert_digest_tiers(&json, "fixture digest");
    assert_eq!(json["scope"], "unstaged");

    // Tier 3 (hunks) is git-derived and always populated here.
    let hunks = json["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 1, "one unstaged hunk: {json}");
    assert_eq!(hunks[0]["file"], "internal/store/memstore.go");
    assert!(hunks[0]["index"].is_u64());
    assert!(
        hunks[0]["header"].as_str().unwrap().starts_with("@@"),
        "reconstructed hunk header: {hunks:?}"
    );
    assert!(hunks[0]["added"].as_u64().unwrap() >= 1);
    assert_eq!(hunks[0]["deleted"], 0);

    // Tier 5 (repo sketch): head, base, and the changed top-level dirs.
    assert_eq!(json["repo"]["head"], "feature/api-changes");
    assert_eq!(json["repo"]["base_ref"], "main");
    assert_eq!(json["repo"]["dirs"], serde_json::json!([["internal", 2]]));

    // Tiers 2 and 4 exist as arrays (empty on this scope).
    assert!(json["diagnostics"].is_array());
    assert!(json["relations"].is_array());

    if codescope_testutil::require_gopls().is_some() {
        // Tier 1 (changed symbols): the nil-guard edit maps to MemoryRepo.Get and the
        // untracked health.go contributes an added function.
        let symbols = json["changed_symbols"].as_array().unwrap();
        let names: Vec<&str> = symbols
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"(*MemoryRepo).Get"),
            "tier 1 names: {names:?}"
        );
        assert!(names.contains(&"Health"), "tier 1 names: {names:?}");
        let get = symbols
            .iter()
            .find(|s| s["name"] == "(*MemoryRepo).Get")
            .unwrap();
        assert_eq!(get["file"], "internal/store/memstore.go");
        assert_eq!(get["change_kind"], "modified");
        assert_eq!(get["kind"], "method");
        assert!(get["signature_touch"].is_boolean());
        assert!(get["diagnostic_count"].is_u64());
    } else {
        // git-only: tier 1 empties out, tier 3 survives.
        assert_eq!(json["changed_symbols"], serde_json::json!([]));
        assert!(
            json["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap_or_default().contains("git-only")),
            "git-only digest carries a note: {json}"
        );
    }
    assert_repo_relative(&stdout(&out), std::path::Path::new(&root));
}

#[test]
fn digest_text_renders_the_prompt_and_mentions_a_changed_symbol() {
    if codescope_testutil::require_gopls().is_none() {
        eprintln!("SKIP: gopls not available; symbol assertions need it");
        return;
    }
    let (_tmp, root) = go_fixture();
    let root = root.to_string_lossy().to_string();

    let out = codescope(&["digest", &root, "--scope", "unstaged", "--text"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.starts_with("# change digest\n"),
        "rendered digest: {text}"
    );
    for needle in [
        "## changed symbols",
        "(*MemoryRepo).Get",
        "internal/store/memstore.go",
        "## diagnostics",
        "## hunks",
        "## relations",
    ] {
        assert!(
            text.contains(needle),
            "rendered digest missing {needle:?}:\n{text}"
        );
    }
    assert!(
        serde_json::from_str::<Value>(&text).is_err(),
        "--text is prompt text, not JSON"
    );
    assert_repo_relative(&text, std::path::Path::new(&root));
}

#[test]
fn digest_git_only_forced_keeps_git_derived_tiers() {
    let (_tmp, root) = go_fixture();
    let root = root.to_string_lossy().to_string();

    let out = codescope_env(&["digest", &root, "--scope", "unstaged"], GIT_ONLY_ENV);
    let json = json_stdout(&out);
    assert_digest_tiers(&json, "git-only fixture digest");
    assert_eq!(json["scope"], "unstaged");
    // LSP-derived tiers empty out; git-derived tiers survive the degradation.
    assert_eq!(json["changed_symbols"], serde_json::json!([]));
    assert_eq!(json["relations"], serde_json::json!([]));
    assert_eq!(
        json["hunks"].as_array().unwrap().len(),
        1,
        "hunks are git-derived"
    );
    assert_eq!(json["repo"]["base_ref"], "main");
    let notes = json["notes"].as_array().expect("git-only notes");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("git-only")),
        "notes explain the degradation: {notes:?}"
    );
    assert_repo_relative(&stdout(&out), std::path::Path::new(&root));
}

// ---------------------------------------------------------------------------
// analyze: forced git-only on every scope (incl. the staged rename)
// ---------------------------------------------------------------------------

#[test]
fn analyze_fixture_all_scopes_git_only() {
    let (_tmp, root) = go_fixture();
    let root = root.to_string_lossy().to_string();

    for (scope, file_count) in [
        ("branch", 2),
        ("staged", 2),
        ("unstaged", 2),
        ("working", 3),
    ] {
        let out = codescope_env(&["analyze", &root, "--scope", scope], GIT_ONLY_ENV);
        let json = json_stdout(&out);
        assert_analyze_keys(&json, scope);
        assert!(json["lsp"].is_null(), "{scope}: forced git-only: {json}");
        assert_eq!(json["changeset"]["scope"], scope, "{scope}: scope echo");
        assert_eq!(
            json["changeset"]["fallback"].as_bool(),
            Some(false),
            "{scope}"
        );
        assert_eq!(
            json["changeset"]["files"].as_array().unwrap().len(),
            file_count,
            "{scope}: changeset file count"
        );
        assert_eq!(
            json["files"].as_array().unwrap().len(),
            file_count,
            "{scope}: one analysis entry per changed file"
        );
        assert_eq!(json["epoch"], 0, "{scope}: epoch zero on a fresh snapshot");
        // Every file degrades to a note instead of failing the run.
        assert!(
            json["files"].as_array().unwrap().iter().all(|f| !f["notes"]
                .as_array()
                .unwrap()
                .is_empty()
                && f["worktree"].is_null()),
            "{scope}: per-file git-only notes: {json}"
        );
        assert_repo_relative(&stdout(&out), std::path::Path::new(&root));
    }

    // The staged scope carries the rename into the analysis view.
    let out = codescope_env(&["analyze", &root, "--scope", "staged"], GIT_ONLY_ENV);
    let json = json_stdout(&out);
    let renamed = json["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["file"] == "internal/store/memstore.go")
        .expect("memstore analyzed");
    assert_eq!(renamed["status"]["renamed"]["score"], 100);
    assert_eq!(
        json["changed"],
        serde_json::json!([]),
        "no semantic changes git-only"
    );
    assert!(
        !json["digest"]["hunks"].as_array().unwrap().is_empty(),
        "the staged service edit reaches the digest hunks"
    );
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[test]
fn nonexistent_path_errors_with_json_and_exit_1() {
    let tmp = TempDir::new().expect("tempdir");
    let missing = tmp
        .path()
        .join("does-not-exist")
        .to_string_lossy()
        .to_string();
    for sub in [
        "scan",
        "changeset",
        "analyze",
        "digest",
        "bases",
        "debug-ai",
    ] {
        let out = codescope(&[sub, &missing]);
        let err = json_stderr_error(&out);
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("not a git repository"),
            "{sub}: clear non-repo error: {err}"
        );
    }
}

#[test]
fn file_path_is_not_a_repo() {
    let tmp = TempDir::new().expect("tempdir");
    let file = tmp.path().join("plain.txt");
    std::fs::write(&file, "hello\n").expect("write");
    let file = file.to_string_lossy().to_string();
    for sub in [
        "scan",
        "changeset",
        "analyze",
        "digest",
        "bases",
        "debug-ai",
    ] {
        let out = codescope(&[sub, &file]);
        let err = json_stderr_error(&out);
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("not a git repository"),
            "{sub}: a file is not a repo: {err}"
        );
    }
}

#[test]
fn non_git_dir_scenario_errors_on_all_subcommands() {
    let built = scenario_repo("non_git_dir");
    assert!(!built.git, "the scenario builds a plain directory");
    let root = built.root.to_string_lossy().to_string();
    for sub in [
        "scan",
        "changeset",
        "analyze",
        "digest",
        "bases",
        "debug-ai",
    ] {
        let out = codescope(&[sub, &root]);
        let err = json_stderr_error(&out);
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("not a git repository"),
            "{sub}: {err}"
        );
    }
}

#[test]
fn cli_parse_errors_exit_2_with_empty_stdout() {
    let (_tmp, root) = scratch_repo();
    let root = root.to_string_lossy().to_string();

    // Unknown scope value: clap rejects it before the backend runs (no JSON contract).
    let out = codescope(&["changeset", &root, "--scope", "bogus"]);
    assert_eq!(out.status.code(), Some(2), "clap parse error exit code");
    assert!(out.stdout.is_empty(), "no stdout on a parse error");
    let err = stderr(&out);
    assert!(err.contains("invalid value"), "{err}");
    assert!(err.contains("branch"), "possible values are listed: {err}");

    // Scope names are case-sensitive.
    let out = codescope(&["changeset", &root, "--scope", "WORKING"]);
    assert_eq!(out.status.code(), Some(2));

    // Unknown flag.
    let out = codescope(&["scan", &root, "--nope"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("unexpected argument"));
    assert!(out.stdout.is_empty());

    // The `--scope=value` form parses identically to `--scope value`.
    let out = codescope(&["changeset", &root, "--scope=working"]);
    let json = json_stdout(&out);
    assert_eq!(json["scope"], "working");
}

#[test]
fn analyze_and_digest_branch_scope_error_without_a_base() {
    let (_tmp, root) = single_branch_repo();
    let root = root.to_string_lossy().to_string();

    for args in [
        vec!["analyze", "--scope", "branch"],
        vec!["digest", "--scope", "branch"],
        vec!["digest", "--scope", "branch", "--text"],
    ] {
        let full: Vec<&str> = [args[0], root.as_str()]
            .into_iter()
            .chain(args[1..].iter().copied())
            .collect();
        let out = codescope(&full);
        let err = json_stderr_error(&out);
        assert!(
            err["error"].as_str().unwrap().contains("branch"),
            "{full:?}: the missing base is a clean error: {err}"
        );
    }

    // The non-branch scopes keep working on the same repo.
    let out = codescope(&["analyze", &root, "--scope", "working"]);
    json_stdout(&out);
    let out = codescope(&["digest", &root, "--scope", "working"]);
    json_stdout(&out);
}

#[test]
fn bases_empty_when_no_base_candidates_exist() {
    let (_tmp, root) = single_branch_repo();
    let out = codescope(&["bases", &root.to_string_lossy()]);
    let json = json_stdout(&out);
    assert_eq!(
        json["bases"],
        serde_json::json!([]),
        "lone main: no candidates"
    );

    let built = scenario_repo("unborn_branch");
    let out = codescope(&["bases", &built.root.to_string_lossy()]);
    let json = json_stdout(&out);
    assert_eq!(
        json["bases"],
        serde_json::json!([]),
        "unborn: no candidates"
    );
}

// ---------------------------------------------------------------------------
// Path handling
// ---------------------------------------------------------------------------

#[test]
fn subdirectory_path_discovers_the_repo_and_keeps_paths_repo_relative() {
    let (_tmp, root) = scratch_repo(); // dirty app.py at the root, src/ subdir exists
    let sub = root.join("src");
    let sub = sub.to_string_lossy().to_string();

    let out = codescope(&["changeset", &sub, "--scope", "working"]);
    let json = json_stdout(&out);
    assert_eq!(
        json["files"][0]["path"], "app.py",
        "paths are repo-relative, not subdir-relative or absolute: {json}"
    );

    let out = codescope(&["scan", &sub]);
    let json = json_stdout(&out);
    assert_eq!(
        json["repo"]["head"]["branch"], "feature",
        "discovery walks up to the repo"
    );
    assert_repo_relative(&stdout(&out), &root);
}

#[test]
fn omitted_path_uses_the_current_directory() {
    let (_tmp, root) = go_fixture();

    let out = codescope_in(&root, &["scan"]);
    let json = json_stdout(&out);
    assert_eq!(json["repo"]["head"]["branch"], "feature/api-changes");

    let out = codescope_in(&root, &["changeset", "--scope", "staged"]);
    let json = json_stdout(&out);
    assert_eq!(json["files"].as_array().unwrap().len(), 2);

    let out = codescope_in(&root, &["bases"]);
    let json = json_stdout(&out);
    assert!(json["bases"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["ref_name"] == "main"));
}

// ---------------------------------------------------------------------------
// Determinism (byte-identical output across runs)
// ---------------------------------------------------------------------------

#[test]
fn bases_and_git_only_outputs_are_byte_identical_across_runs() {
    let (_tmp, root) = go_fixture();
    let root = root.to_string_lossy().to_string();

    let b1 = codescope(&["bases", &root]);
    let b2 = codescope(&["bases", &root]);
    assert!(b1.status.success());
    assert_eq!(b1.stdout, b2.stdout, "bases deterministic bytes");

    let c1 = codescope(&["changeset", &root, "--scope", "branch"]);
    let c2 = codescope(&["changeset", &root, "--scope", "branch"]);
    assert_eq!(c1.stdout, c2.stdout, "changeset deterministic bytes");

    // analyze/digest involve process spawning; git-only mode must still be byte-stable.
    let a1 = codescope_env(&["analyze", &root, "--scope", "unstaged"], GIT_ONLY_ENV);
    let a2 = codescope_env(&["analyze", &root, "--scope", "unstaged"], GIT_ONLY_ENV);
    assert_eq!(a1.stdout, a2.stdout, "git-only analyze deterministic bytes");

    let d1 = codescope_env(&["digest", &root, "--scope", "unstaged"], GIT_ONLY_ENV);
    let d2 = codescope_env(&["digest", &root, "--scope", "unstaged"], GIT_ONLY_ENV);
    assert_eq!(d1.stdout, d2.stdout, "git-only digest deterministic bytes");
}

// ---------------------------------------------------------------------------
// Detached HEAD (bonus shape)
// ---------------------------------------------------------------------------

#[test]
fn detached_head_scan_reports_the_detached_oid() {
    let built = scenario_repo("detached_head");
    let root = built.root.to_string_lossy().to_string();

    let out = codescope(&["scan", &root]);
    let json = json_stdout(&out);
    let oid = json["repo"]["head"]["detached"]
        .as_str()
        .expect("detached head carries the oid");
    assert_eq!(oid.len(), 40, "full hex oid: {oid}");
    assert!(oid.chars().all(|c| c.is_ascii_hexdigit()), "hex oid: {oid}");
    for scope in ["staged", "unstaged", "working"] {
        assert_eq!(json["scopes"][scope], 0, "{scope}: clean tree");
    }

    // The only named ref is HEAD-equivalent, so it is not offered as an empty base.
    let out = codescope(&["bases", &root]);
    let json = json_stdout(&out);
    assert!(
        json["bases"].as_array().unwrap().is_empty(),
        "HEAD-equivalent main is excluded: {json}"
    );
    let out = codescope(&["changeset", &root, "--scope", "working"]);
    let json = json_stdout(&out);
    assert_eq!(json["files"], serde_json::json!([]));
}
