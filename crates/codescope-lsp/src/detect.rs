//! Language detection for repository roots.
//!
//! Scans the git toplevel (or any directory) for language markers while respecting
//! `.gitignore` rules via the [`ignore`] crate. This is the single place codescope
//! decides which language server(s) apply to a project.

use std::collections::HashSet;

use camino::{Utf8Path, Utf8PathBuf};
use ignore::WalkBuilder;

/// A supported source language. Determined heuristically from marker files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    /// Go: `go.mod` or `go.work`.
    Go,
    /// Rust: `Cargo.toml`.
    Rust,
    /// TypeScript / JavaScript: `package.json` or `tsconfig*.json`.
    TypeScript,
    /// Python: any `*.py` file.
    Python,
}

impl Language {
    /// Stable display name shown in the TUI top bar.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Go => "Go",
            Language::Rust => "Rust",
            Language::TypeScript => "TypeScript",
            Language::Python => "Python",
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// All supported languages, in the deterministic order returned by [`detect_languages`].
const ORDER: [Language; 4] = [
    Language::Go,
    Language::Rust,
    Language::TypeScript,
    Language::Python,
];

/// Scan `root` for language markers and return the languages present.
///
/// The walk respects `.gitignore` files (including the global git ignore file and
/// `.git/info/exclude`), so vendored or generated directories are skipped the same
/// way git does.
#[must_use]
pub fn detect_languages(root: &Utf8Path) -> Vec<Language> {
    let mut seen = HashSet::new();
    let walker = WalkBuilder::new(root.as_std_path()).build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        match name {
            "go.mod" | "go.work" => {
                seen.insert(Language::Go);
            }
            "Cargo.toml" => {
                seen.insert(Language::Rust);
            }
            "package.json" => {
                seen.insert(Language::TypeScript);
            }
            _ if name.starts_with("tsconfig") && name.ends_with(".json") => {
                seen.insert(Language::TypeScript);
            }
            _ => {
                if name.ends_with(".py") || name.ends_with(".pyi") {
                    seen.insert(Language::Python);
                }
            }
        }
    }
    ORDER.into_iter().filter(|l| seen.contains(l)).collect()
}

/// Return every directory under `root` that contains a `go.mod` or `go.work` file.
///
/// These are the gopls workspace-folder roots. The returned directories are absolute
/// and sorted, and nested module roots are preserved (gopls resolves the hierarchy).
#[must_use]
pub fn go_module_folders(root: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut folders = Vec::new();
    let walker = WalkBuilder::new(root.as_std_path()).build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        if name == "go.mod" || name == "go.work" {
            let Some(parent) = entry.path().parent() else {
                continue;
            };
            if let Ok(dir) = Utf8PathBuf::from_path_buf(parent.to_path_buf()) {
                folders.push(dir);
            }
        }
    }
    folders.sort();
    folders.dedup();
    folders
}

/// Find a single Rust project root under `root`.
///
/// The walk respects `.gitignore`. If a `Cargo.toml` with a `[workspace]` section is
/// found, the workspace root is returned. Otherwise the first `Cargo.toml` directory
/// found is used. This keeps the prototype simple while supporting repos where the
/// Rust workspace/package is nested under the git toplevel.
#[must_use]
pub fn rust_project_root(root: &Utf8Path) -> Option<Utf8PathBuf> {
    let mut first: Option<Utf8PathBuf> = None;
    let walker = WalkBuilder::new(root.as_std_path()).build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        if name != "Cargo.toml" {
            continue;
        }
        let Some(parent) = entry.path().parent() else {
            continue;
        };
        let Ok(dir) = Utf8PathBuf::from_path_buf(parent.to_path_buf()) else {
            continue;
        };
        if is_workspace_root(dir.join("Cargo.toml").as_std_path()) {
            return Some(dir);
        }
        if first.is_none() {
            first = Some(dir);
        }
    }
    first
}

/// Find the Python project root Pyright should serve.
///
/// The shallowest `pyrightconfig.json` or `pyproject.toml` containing `[tool.pyright]`
/// wins, with `pyrightconfig.json` preferred when both are beside each other.
/// Repositories without either configuration are served from the Git root.
#[must_use]
pub fn python_project_root(root: &Utf8Path) -> Utf8PathBuf {
    let mut candidates = Vec::new();
    let walker = WalkBuilder::new(root.as_std_path()).build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        let Some(parent) = entry.path().parent() else {
            continue;
        };
        let Ok(dir) = Utf8PathBuf::from_path_buf(parent.to_path_buf()) else {
            continue;
        };
        match name {
            "pyrightconfig.json" => candidates.push((dir, 0_u8)),
            "pyproject.toml"
                if std::fs::read_to_string(entry.path())
                    .is_ok_and(|text| text.contains("[tool.pyright]")) =>
            {
                candidates.push((dir, 1_u8));
            }
            _ => {}
        }
    }

    candidates.sort_by(|(left, left_kind), (right, right_kind)| {
        project_root_depth(root, left)
            .cmp(&project_root_depth(root, right))
            .then_with(|| left_kind.cmp(right_kind))
            .then_with(|| left.cmp(right))
    });
    candidates
        .into_iter()
        .map(|(directory, _)| directory)
        .next()
        .unwrap_or_else(|| root.to_path_buf())
}

fn project_root_depth(root: &Utf8Path, candidate: &Utf8Path) -> usize {
    candidate
        .strip_prefix(root)
        .map(Utf8Path::components)
        .map(Iterator::count)
        .unwrap_or(usize::MAX)
}

fn is_workspace_root(cargo_toml: &std::path::Path) -> bool {
    std::fs::read_to_string(cargo_toml)
        .map(|text| text.contains("[workspace]"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // WalkBuilder applies .gitignore rules only when a git repo is present.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        dir
    }

    fn write(root: &std::path::Path, rel: &str, content: &str) -> std::path::PathBuf {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn detects_go_rust_typescript_python() {
        let tmp = scratch();
        write(
            tmp.path(),
            "packages/alpha/go.mod",
            "module alpha\n\ngo 1.23\n",
        );
        write(
            tmp.path(),
            "packages/beta/Cargo.toml",
            "[package]\nname = \"beta\"\n",
        );
        write(tmp.path(), "web/package.json", "{\"name\":\"web\"}");
        write(tmp.path(), "scripts/util.py", "# python\n");
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        assert_eq!(
            detect_languages(&root),
            vec![
                Language::Go,
                Language::Rust,
                Language::TypeScript,
                Language::Python,
            ]
        );
    }

    #[test]
    fn respects_gitignore() {
        let tmp = scratch();
        write(tmp.path(), ".gitignore", "vendor/\n");
        write(tmp.path(), "pkg/go.mod", "module pkg\n");
        write(tmp.path(), "vendor/nested/main.py", "x = 1\n");
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        // Python is ignored because vendor/ is ignored.
        assert_eq!(detect_languages(&root), vec![Language::Go]);
    }

    #[test]
    fn go_module_folders_collects_multi_module() {
        let tmp = scratch();
        write(tmp.path(), "packages/alpha/go.mod", "module alpha\n");
        write(tmp.path(), "packages/beta/go.mod", "module beta\n");
        write(tmp.path(), "go.work", "go 1.23\n");
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let folders = go_module_folders(&root);
        assert_eq!(folders.len(), 3);
        assert!(
            folders
                .iter()
                .any(|p| p.as_str().ends_with("packages/alpha"))
        );
        assert!(
            folders
                .iter()
                .any(|p| p.as_str().ends_with("packages/beta"))
        );
        assert!(
            folders.iter().any(|p| p.as_path() == root.as_path()),
            "root folder missing: {folders:?}"
        );
    }

    #[test]
    fn rust_project_root_prefers_workspace() {
        let tmp = scratch();
        write(
            tmp.path(),
            "packages/rootfs/Cargo.toml",
            "[workspace]\nmembers = [\"app\"]\n",
        );
        write(
            tmp.path(),
            "packages/rootfs/app/Cargo.toml",
            "[package]\nname = \"app\"\n",
        );
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        assert_eq!(
            rust_project_root(&root),
            Some(Utf8PathBuf::from_path_buf(tmp.path().join("packages/rootfs")).unwrap())
        );
    }

    #[test]
    fn rust_project_root_falls_back_to_first_package() {
        let tmp = scratch();
        write(
            tmp.path(),
            "packages/rootfs/Cargo.toml",
            "[package]\nname = \"rootfs\"\n",
        );
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        assert_eq!(
            rust_project_root(&root),
            Some(Utf8PathBuf::from_path_buf(tmp.path().join("packages/rootfs")).unwrap())
        );
    }

    #[test]
    fn python_project_root_prefers_pyright_config() {
        let tmp = scratch();
        write(
            tmp.path(),
            "packages/app/pyproject.toml",
            "[tool.pyright]\ntypeCheckingMode = \"basic\"\n",
        );
        write(tmp.path(), "packages/app/pyrightconfig.json", "{}\n");
        write(tmp.path(), "packages/app/app.py", "value = 1\n");
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        assert_eq!(
            python_project_root(&root),
            Utf8PathBuf::from_path_buf(tmp.path().join("packages/app")).unwrap()
        );
    }

    #[test]
    fn python_project_root_uses_shallowest_config_and_falls_back_to_repo() {
        let tmp = scratch();
        write(
            tmp.path(),
            "packages/app/pyproject.toml",
            "[tool.pyright]\ntypeCheckingMode = \"basic\"\n",
        );
        write(
            tmp.path(),
            "packages/app/nested/pyproject.toml",
            "[tool.pyright]\ntypeCheckingMode = \"strict\"\n",
        );
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        assert_eq!(
            python_project_root(&root),
            Utf8PathBuf::from_path_buf(tmp.path().join("packages/app")).unwrap()
        );

        let other = scratch();
        let other_root = Utf8PathBuf::from_path_buf(other.path().to_path_buf()).unwrap();
        assert_eq!(python_project_root(&other_root), other_root);
    }
}
