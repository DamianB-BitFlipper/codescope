//! Regression: a cold-starting language server answers `textDocument/documentSymbol`
//! with JSON `null` while the file's project is still being linked (observed with
//! rust-analyzer before its crate graph reaches the file's crate). The shared session
//! must retry those answers instead of caching an empty symbol tree for the session.
//!
//! Drives the real subprocess session (`LanguageService::start`) against the
//! `fake_lsp` binary from codescope-testutil, scripted to answer `null`, `null`, then
//! real symbols for the first worktree query and `null`, then symbols for the first base
//! overlay. Skips when the fake binary has not been built.

use std::time::Duration;

use codescope_core::{FileId, Utf8PathBuf};
use codescope_lsp::LanguageService;

const CARGO_TOML: &str = r#"[package]
name = "ra-probe"
version = "0.1.0"
edition = "2021"
"#;

const MAIN_RS: &str = r#"fn double(x: i32) -> i32 {
    x * 2
}

fn main() {
    println!("{}", double(7));
}
"#;

/// Workspace-built `fake_lsp` binary, when it exists (`cargo test --workspace` builds it;
/// a lone `cargo test -p codescope-lsp` may not).
fn fake_lsp_binary() -> Option<std::path::PathBuf> {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let exe = format!("fake_lsp{}", std::env::consts::EXE_SUFFIX);
    let bin = target.join("debug").join(exe);
    bin.is_file().then_some(bin)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_symbol_null_answers_are_retried_not_cached() {
    let Some(fake) = fake_lsp_binary() else {
        eprintln!(
            "fake_lsp binary not built; skipping \
             (cargo build -p codescope-testutil --bin fake_lsp)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), CARGO_TOML).unwrap();
    std::fs::write(root.join("src").join("main.rs"), MAIN_RS).unwrap();

    // LSP positions are zero-based; `kind: 12` is Function.
    let symbols = serde_json::json!([
        {
            "name": "double", "kind": 12,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 1}},
            "selectionRange": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 9}}
        },
        {
            "name": "main", "kind": 12,
            "range": {"start": {"line": 4, "character": 0}, "end": {"line": 6, "character": 1}},
            "selectionRange": {"start": {"line": 4, "character": 3}, "end": {"line": 4, "character": 7}}
        }
    ]);
    let null_answer = serde_json::json!({"kind": "result", "value": null});
    let data_answer = serde_json::json!({"kind": "result", "value": symbols});
    let script = serde_json::json!({
        "initialize_result": {
            "capabilities": {
                "positionEncoding": "utf-16",
                "textDocumentSync": {"openClose": true, "change": 2},
                "documentSymbolProvider": true
            },
            "serverInfo": {"name": "codescope-fake-lsp", "version": "0"}
        },
        "diagnostics_trigger": "initialized",
        "respond_to_shutdown": true,
        // Worktree query: null, null, symbols. Base overlay: null, symbols.
        "response_sequences": {
            "textDocument/documentSymbol": [
                null_answer.clone(),
                null_answer.clone(),
                data_answer.clone(),
                null_answer.clone(),
                data_answer.clone()
            ]
        },
        "responses": {
            "textDocument/documentSymbol": data_answer.clone()
        }
    });
    let script_path = dir.path().join("fake-script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();

    // The Rust adapter spawns the program with no arguments, so wrap the fake binary to
    // attach its script.
    let wrapper = dir.path().join("fake-lsp-wrapper.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec '{}' '{}'\n",
            fake.display(),
            script_path.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // SAFETY: the override is installed only for this test's spawn and restored before
    // any assertion runs; the test binary runs each file in its own process.
    let previous = std::env::var_os("CODESCOPE_RUST_ANALYZER");
    unsafe { std::env::set_var("CODESCOPE_RUST_ANALYZER", &wrapper) };
    let started =
        tokio::time::timeout(Duration::from_secs(30), LanguageService::start(&root)).await;
    // The adapter resolves the program at spawn time; restore the environment before any
    // assertion can fail and leak the override into other tests.
    match previous {
        Some(value) => unsafe { std::env::set_var("CODESCOPE_RUST_ANALYZER", value) },
        None => unsafe { std::env::remove_var("CODESCOPE_RUST_ANALYZER") },
    }
    let svc = started
        .expect("start timed out")
        .expect("fake language service failed to start");

    let file = FileId::new("src/main.rs").unwrap();

    // First worktree query: the two scripted `null` answers must be retried, and the
    // partial "no analysis" evidence must never surface once real symbols arrive.
    let evidence = tokio::time::timeout(Duration::from_secs(30), svc.document_symbols(&file))
        .await
        .expect("document_symbols timed out")
        .expect("document_symbols failed");
    assert!(
        evidence.notes.is_empty(),
        "retry produced real symbols, but notes remain: {:?}",
        evidence.notes
    );
    let names: Vec<&str> = evidence
        .value
        .roots
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(names.contains(&"double"), "roots: {names:?}");
    assert!(names.contains(&"main"), "roots: {names:?}");

    // Second query of the same content: the successful tree is cached; a cached *empty*
    // tree (the pre-fix behavior) would keep this answer empty.
    let evidence = tokio::time::timeout(Duration::from_secs(30), svc.document_symbols(&file))
        .await
        .expect("second document_symbols timed out")
        .expect("second document_symbols failed");
    assert!(
        !evidence.value.roots.is_empty(),
        "a repeated query must not replay an empty tree"
    );

    // First base overlay: one `null` then symbols — the base path retries too.
    let evidence = tokio::time::timeout(
        Duration::from_secs(30),
        svc.base_document_symbols(&file, MAIN_RS),
    )
    .await
    .expect("base_document_symbols timed out")
    .expect("base_document_symbols failed");
    assert!(
        !evidence.value.roots.is_empty(),
        "the base overlay must retry its null answer"
    );
    assert!(
        evidence.notes.is_empty(),
        "base retry produced real symbols, but notes remain: {:?}",
        evidence.notes
    );

    svc.shutdown().await;
}
