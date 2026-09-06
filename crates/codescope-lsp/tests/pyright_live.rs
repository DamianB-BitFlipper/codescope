//! Live integration test against a real Pyright language-server process.
//!
//! Skipped when `pyright-langserver` is not on `PATH` (or `$CODESCOPE_PYRIGHT`).

use std::time::Duration;

use codescope_core::{FileId, Position, Utf8PathBuf};
use codescope_lsp::LanguageService;

const APP_PY: &str = r#"def double(value: int) -> int:
    return value * 2


def main() -> None:
    print(double(7))
"#;

fn require_pyright() -> Option<()> {
    let program =
        std::env::var("CODESCOPE_PYRIGHT").unwrap_or_else(|_| "pyright-langserver".to_string());
    let path = std::path::Path::new(&program);
    if path.components().count() > 1 {
        return path.is_file().then_some(());
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .any(|directory| directory.join(&program).is_file())
            .then_some(())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pyright_end_to_end() {
    if require_pyright().is_none() {
        eprintln!("pyright-langserver not found; skipping live test");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    std::fs::write(root.join("pyrightconfig.json"), "{}\n").unwrap();
    std::fs::write(root.join("app.py"), APP_PY).unwrap();

    let svc = tokio::time::timeout(Duration::from_secs(30), LanguageService::start(&root))
        .await
        .expect("start timed out")
        .expect("start failed");

    assert_eq!(svc.language_name(), "Python");
    assert!(
        svc.features()
            .is_supported(codescope_core::Feature::DocumentSymbols)
    );
    assert!(
        svc.features()
            .is_supported(codescope_core::Feature::References)
    );
    assert!(
        svc.features()
            .is_supported(codescope_core::Feature::CallHierarchyIncoming)
    );

    let file = FileId::new("app.py").unwrap();
    assert!(svc.handles(&file));
    assert!(svc.handles(&FileId::new("types.pyi").unwrap()));
    assert!(!svc.handles(&FileId::new("app.rs").unwrap()));

    let tree = tokio::time::timeout(Duration::from_secs(60), svc.document_symbols(&file))
        .await
        .expect("document_symbols timed out")
        .expect("document_symbols failed")
        .value;
    let names: Vec<&str> = tree
        .roots
        .iter()
        .map(|symbol| symbol.name.as_str())
        .collect();
    assert!(names.contains(&"double"), "roots: {names:?}");
    assert!(names.contains(&"main"), "roots: {names:?}");

    let callers = tokio::time::timeout(
        Duration::from_secs(30),
        svc.incoming_calls(&file, Position::new(0, 4)),
    )
    .await
    .expect("incoming_calls timed out")
    .expect("incoming_calls failed")
    .value;
    assert!(
        callers.iter().any(|caller| caller.name == "main"),
        "callers: {callers:?}"
    );

    tokio::time::timeout(Duration::from_secs(10), svc.shutdown())
        .await
        .expect("shutdown timed out");
}
