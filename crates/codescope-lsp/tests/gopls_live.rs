//! Live integration test against a real gopls process.
//!
//! Skipped silently when `gopls` is not on `PATH` (or `$CODESCOPE_GOPLS`). Builds a
//! tiny Go module in a temp dir — never touches the fixture or the user's repo.

use std::time::Duration;

use codescope_core::{FileId, Position, Utf8PathBuf};
use codescope_lsp::LanguageService;

const MAIN_GO: &str = r#"package main

import "fmt"

// Greeter is a sample interface.
type Greeter interface {
	Greet(name string) string
}

type friendly struct{}

func (friendly) Greet(name string) string { return fmt.Sprintf("hi %s", name) }

func greet(name string) string {
	var g Greeter = friendly{}
	return g.Greet(name)
}

func main() { fmt.Println(greet("world")) }
"#;

fn require_gopls() -> Option<()> {
    let program = std::env::var("CODESCOPE_GOPLS").unwrap_or_else(|_| "gopls".to_string());
    let ok = std::process::Command::new(program)
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    ok.then_some(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gopls_end_to_end() {
    if require_gopls().is_none() {
        eprintln!("gopls not found; skipping live test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    std::fs::write(root.join("go.mod"), "module example.com/probe\n\ngo 1.23\n").unwrap();
    std::fs::write(root.join("main.go"), MAIN_GO).unwrap();

    let svc = tokio::time::timeout(Duration::from_secs(30), LanguageService::start(&root))
        .await
        .expect("start timed out")
        .expect("start failed");

    assert!(svc.features().is_supported(codescope_core::Feature::DocumentSymbols));
    let file = FileId::new("main.go").unwrap();

    // Symbol tree (hierarchical: struct fields nested, methods top-level).
    let tree = tokio::time::timeout(Duration::from_secs(60), svc.document_symbols(&file))
        .await
        .expect("document_symbols timed out")
        .expect("document_symbols failed")
        .value;
    let names: Vec<&str> = tree.roots.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Greeter"), "roots: {names:?}");
    assert!(names.iter().any(|n| n.contains("Greet")), "roots: {names:?}");

    // Implementations of Greeter.Greet (interface method at line 6, col 1).
    let impls = tokio::time::timeout(
        Duration::from_secs(30),
        svc.implementations(&file, Position::new(6, 1)),
    )
    .await
    .expect("implementations timed out")
    .expect("implementations failed")
    .value;
    assert!(!impls.is_empty(), "expected at least one implementation");

    // Callers of greet (func greet at line 13, col 5): main calls it.
    let callers = tokio::time::timeout(
        Duration::from_secs(30),
        svc.incoming_calls(&file, Position::new(13, 5)),
    )
    .await
    .expect("incoming_calls timed out")
    .expect("incoming_calls failed")
    .value;
    assert!(
        callers.iter().any(|c| c.name == "main"),
        "callers: {callers:?}"
    );

    // Clean shutdown.
    tokio::time::timeout(Duration::from_secs(10), svc.shutdown())
        .await
        .expect("shutdown timed out");
}
