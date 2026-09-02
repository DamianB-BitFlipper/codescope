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

fn write(root: &std::path::Path, rel: &str, content: &str) -> std::path::PathBuf {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
    path
}

fn init_git(root: &std::path::Path) {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
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
    };
    git(&["init", "--quiet", "-b", "main"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "--no-verify", "-m", "init"]);
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

    assert!(svc
        .features()
        .is_supported(codescope_core::Feature::DocumentSymbols));
    let file = FileId::new("main.go").unwrap();

    // Symbol tree (hierarchical: struct fields nested, methods top-level).
    let tree = tokio::time::timeout(Duration::from_secs(60), svc.document_symbols(&file))
        .await
        .expect("document_symbols timed out")
        .expect("document_symbols failed")
        .value;
    let names: Vec<&str> = tree.roots.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Greeter"), "roots: {names:?}");
    assert!(
        names.iter().any(|n| n.contains("Greet")),
        "roots: {names:?}"
    );

    // Syntax is optional at the protocol boundary, but current gopls advertises it when the
    // client enables semantic tokens. Exercise the same language-neutral service call used by
    // the diff viewer when available so position decoding is covered against a real server.
    if svc
        .features()
        .is_supported(codescope_core::Feature::SemanticTokens)
    {
        let syntax = tokio::time::timeout(Duration::from_secs(60), svc.semantic_tokens(&file))
            .await
            .expect("semantic_tokens timed out")
            .expect("semantic_tokens failed")
            .value;
        assert!(!syntax.is_empty(), "expected gopls syntax tokens");
        assert!(syntax
            .iter()
            .all(|token| token.range.start_line == token.range.end_line));
    }

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
    assert!(
        impls
            .iter()
            .all(|implementation| implementation.range.is_some()),
        "implementation locations should remain available to AI evidence: {impls:?}"
    );

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
    assert!(
        callers.iter().all(|caller| caller.range.is_some()),
        "call hierarchy should preserve source ranges: {callers:?}"
    );

    if svc.features().is_supported(codescope_core::Feature::Hover) {
        let hover = tokio::time::timeout(
            Duration::from_secs(30),
            svc.hover(&file, Position::new(13, 5)),
        )
        .await
        .expect("hover timed out")
        .expect("hover failed");
        assert!(hover.is_some(), "expected hover text for greet");
    }

    if svc
        .features()
        .is_supported(codescope_core::Feature::TypeHierarchySuper)
    {
        let _ = tokio::time::timeout(
            Duration::from_secs(30),
            svc.type_supertypes(&file, Position::new(9, 5)),
        )
        .await
        .expect("type_supertypes timed out")
        .expect("type_supertypes failed");
    }

    // Clean shutdown.
    tokio::time::timeout(Duration::from_secs(10), svc.shutdown())
        .await
        .expect("shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gopls_multi_module_repo() {
    if require_gopls().is_none() {
        eprintln!("gopls not found; skipping live test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

    // packages/greet exports Greet; packages/app imports it.
    write(
        dir.path(),
        "packages/greet/go.mod",
        "module example.com/greet\n\ngo 1.23\n",
    );
    write(
        dir.path(),
        "packages/greet/greet.go",
        r#"package greet

func Greet(name string) string {
	return "hello, " + name
}
"#,
    );
    write(
        dir.path(),
        "packages/app/go.mod",
        "module example.com/app\n\ngo 1.23\n\nrequire example.com/greet v0.0.0\n\nreplace example.com/greet => ../greet\n",
    );
    write(
        dir.path(),
        "go.work",
        "go 1.23\n\nuse (\n\t./packages/greet\n\t./packages/app\n)\n",
    );
    write(
        dir.path(),
        "packages/app/main.go",
        r#"package main

import (
	"fmt"

	"example.com/greet"
)

func main() {
	fmt.Println(greet.Greet("world"))
}
"#,
    );
    init_git(dir.path());

    let svc = tokio::time::timeout(Duration::from_secs(30), LanguageService::start(&root))
        .await
        .expect("start timed out")
        .expect("start failed");

    assert_eq!(svc.language_name(), "Go");
    assert!(svc
        .features()
        .is_supported(codescope_core::Feature::DocumentSymbols));

    // Symbols in the remote module are served.
    let file = FileId::new("packages/greet/greet.go").unwrap();
    let tree = tokio::time::timeout(Duration::from_secs(60), svc.document_symbols(&file))
        .await
        .expect("document_symbols timed out")
        .expect("document_symbols failed")
        .value;
    assert!(tree.roots.iter().any(|s| s.name == "Greet"));

    let app_file = FileId::new("packages/app/main.go").unwrap();
    // Warm up gopls' view of the app module before asking for cross-module refs.
    let _ = tokio::time::timeout(Duration::from_secs(60), svc.document_symbols(&app_file))
        .await
        .expect("app symbols timed out")
        .expect("app symbols failed");
    // Cross-package reference: querying the `Greet` usage in `app` sees the
    // declaration in `greet` (and its own usage), proving gopls loaded both modules.
    let app_file = FileId::new("packages/app/main.go").unwrap();
    let refs = tokio::time::timeout(
        Duration::from_secs(60),
        svc.references(&app_file, Position::new(9, 21)),
    )
    .await
    .expect("references timed out")
    .expect("references failed")
    .value;
    assert!(
        refs.iter()
            .any(|l| l.file.as_path().as_str() == "packages/greet/greet.go"),
        "expected cross-package declaration: {refs:?}"
    );
    assert!(
        refs.iter()
            .any(|l| l.file.as_path().as_str() == "packages/app/main.go"),
        "expected same-file usage: {refs:?}"
    );

    tokio::time::timeout(Duration::from_secs(10), svc.shutdown())
        .await
        .expect("shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gopls_serves_go_in_mixed_repo() {
    if require_gopls().is_none() {
        eprintln!("gopls not found; skipping live test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

    // packages/rootfs/Cargo.toml alongside packages/*/go.mod (vm-sandboxes shape).
    write(
        dir.path(),
        "packages/svc/go.mod",
        "module example.com/svc\n\ngo 1.23\n",
    );
    write(
        dir.path(),
        "packages/svc/main.go",
        "package main\n\nfunc main() {}\n",
    );
    write(
        dir.path(),
        "packages/rootfs/Cargo.toml",
        "[workspace]\nmembers = [\"rfs\"]\n",
    );
    write(
        dir.path(),
        "packages/rootfs/rfs/Cargo.toml",
        "[package]\nname = \"rfs\"\n",
    );
    write(
        dir.path(),
        "packages/rootfs/rfs/src/lib.rs",
        "pub fn rfs() {}\n",
    );
    init_git(dir.path());

    let svc = tokio::time::timeout(Duration::from_secs(30), LanguageService::start(&root))
        .await
        .expect("start timed out")
        .expect("start failed");

    assert_eq!(svc.language_name(), "Go");
    assert!(
        svc.handles(&FileId::new("packages/svc/main.go").unwrap()),
        "service should own nested Go files"
    );
    assert!(
        !svc.handles(&FileId::new("packages/rootfs/rfs/src/lib.rs").unwrap()),
        "service should not own Rust files"
    );

    let tree = tokio::time::timeout(
        Duration::from_secs(60),
        svc.document_symbols(&FileId::new("packages/svc/main.go").unwrap()),
    )
    .await
    .expect("document_symbols timed out")
    .expect("document_symbols failed")
    .value;
    assert!(tree.roots.iter().any(|s| s.name == "main"));

    tokio::time::timeout(Duration::from_secs(10), svc.shutdown())
        .await
        .expect("shutdown timed out");
}
