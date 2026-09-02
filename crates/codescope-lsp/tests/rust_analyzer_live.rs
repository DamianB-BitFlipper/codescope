//! Live integration test against a real rust-analyzer process.
//!
//! Skipped silently when `rust-analyzer` is not on `PATH` (or
//! `$CODESCOPE_RUST_ANALYZER`). Builds a tiny Cargo bin in a temp dir.

use std::time::Duration;

use codescope_core::{FileId, Position, Utf8PathBuf};
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

fn require_rust_analyzer() -> Option<()> {
    let program =
        std::env::var("CODESCOPE_RUST_ANALYZER").unwrap_or_else(|_| "rust-analyzer".to_string());
    let ok = std::process::Command::new(program)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    ok.then_some(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_analyzer_end_to_end() {
    if require_rust_analyzer().is_none() {
        eprintln!("rust-analyzer not found; skipping live test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), CARGO_TOML).unwrap();
    std::fs::write(root.join("src").join("main.rs"), MAIN_RS).unwrap();

    let svc = tokio::time::timeout(Duration::from_secs(30), LanguageService::start(&root))
        .await
        .expect("start timed out")
        .expect("start failed");

    assert_eq!(svc.language_name(), "Rust");
    assert!(svc
        .features()
        .is_supported(codescope_core::Feature::DocumentSymbols));
    assert!(!svc
        .features()
        .is_supported(codescope_core::Feature::TypeHierarchySub));

    let file = FileId::new("src/main.rs").unwrap();

    // Symbol tree: `double` and `main` at the top level.
    let tree = tokio::time::timeout(Duration::from_secs(60), svc.document_symbols(&file))
        .await
        .expect("document_symbols timed out")
        .expect("document_symbols failed")
        .value;
    let names: Vec<&str> = tree.roots.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"double"), "roots: {names:?}");
    assert!(names.contains(&"main"), "roots: {names:?}");

    if svc
        .features()
        .is_supported(codescope_core::Feature::SemanticTokens)
    {
        let syntax = tokio::time::timeout(Duration::from_secs(60), svc.semantic_tokens(&file))
            .await
            .expect("semantic_tokens timed out")
            .expect("semantic_tokens failed")
            .value;
        assert!(!syntax.is_empty(), "expected rust-analyzer syntax tokens");
        assert!(syntax.iter().any(|token| token.token_type == "function"));
    }

    // Give rust-analyzer a moment to load workspace metadata before semantic queries.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Callers of `double` (line 0, col 3): `main` calls it.
    let callers = tokio::time::timeout(
        Duration::from_secs(30),
        svc.incoming_calls(&file, Position::new(0, 3)),
    )
    .await
    .expect("incoming_calls timed out")
    .expect("incoming_calls failed")
    .value;
    assert!(
        callers.iter().any(|c| c.name == "main"),
        "callers: {callers:?}"
    );
    assert!(callers.iter().all(|caller| caller.range.is_some()));

    // Capability gating: typeHierarchy/subtypes is unsupported and returns before any
    // wire traffic (rust-analyzer advertises no typeHierarchy provider).
    let err = svc
        .type_subtypes(&file, Position::new(0, 3))
        .await
        .expect_err("type_subtypes should be unsupported");
    assert!(
        matches!(
            err,
            codescope_lsp::SemanticError::Unsupported(codescope_core::Feature::TypeHierarchySub)
        ),
        "unexpected err: {err}"
    );
    let err = svc
        .type_supertypes(&file, Position::new(0, 3))
        .await
        .expect_err("type_supertypes should be unsupported");
    assert!(matches!(
        err,
        codescope_lsp::SemanticError::Unsupported(codescope_core::Feature::TypeHierarchySuper)
    ));

    // Clean shutdown.
    tokio::time::timeout(Duration::from_secs(10), svc.shutdown())
        .await
        .expect("shutdown timed out");
}
