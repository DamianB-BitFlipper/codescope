//! Shared changed-tree projection used by rendering, navigation, and mouse hit-testing.
//!
//! Directory rows are derived from changed file paths; expanding or collapsing them is
//! local view state and never starts work. File expansion remains snapshot-owned because
//! it exposes asynchronously loaded symbol rows.

use std::collections::{HashMap, HashSet};

use crate::review::ReviewTarget;
use crate::snapshot::{AiSummaryKey, FileRow, FileSemanticLoad};

/// One physical row in the changed-files tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectedRow {
    /// A synthesized directory containing at least one changed file.
    Directory {
        /// Repo-relative directory path without a trailing slash.
        path: String,
        /// Visible path segment, combining unbranched directory ancestors.
        label: String,
        /// Zero-based tree depth.
        depth: usize,
        /// Logical selectable index.
        logical_index: usize,
    },
    /// A changed file row.
    File {
        /// Index into `UiSnapshot::files`.
        file_index: usize,
        /// Zero-based tree depth.
        depth: usize,
        /// Logical selectable index.
        logical_index: usize,
    },
    /// A symbol row under an expanded file.
    Symbol {
        /// Index into `UiSnapshot::files`.
        file_index: usize,
        /// Index into that file's `symbols`.
        symbol_index: usize,
        /// Zero-based tree depth.
        depth: usize,
        /// Logical selectable index.
        logical_index: usize,
    },
    /// A non-selectable analysis note under an expanded file.
    Note {
        /// Index into `UiSnapshot::files`.
        file_index: usize,
        /// Zero-based tree depth.
        depth: usize,
    },
}

impl ProjectedRow {
    /// Logical selectable index, absent only for note rows.
    #[must_use]
    pub fn logical_index(&self) -> Option<usize> {
        match self {
            ProjectedRow::Directory { logical_index, .. }
            | ProjectedRow::File { logical_index, .. }
            | ProjectedRow::Symbol { logical_index, .. } => Some(*logical_index),
            ProjectedRow::Note { .. } => None,
        }
    }

    /// Stable summary identity for selectable rows.
    #[must_use]
    pub fn summary_key(&self, files: &[FileRow]) -> Option<AiSummaryKey> {
        match self {
            ProjectedRow::Directory { path, .. } => Some(AiSummaryKey::Directory(path.clone())),
            ProjectedRow::File { file_index, .. } => files
                .get(*file_index)
                .map(|file| AiSummaryKey::File(file.path.clone())),
            ProjectedRow::Symbol {
                file_index,
                symbol_index,
                ..
            } => {
                let file = files.get(*file_index)?;
                let symbol = file.symbols.get(*symbol_index)?;
                Some(AiSummaryKey::Symbol {
                    file: file.path.clone(),
                    name: symbol.name.clone(),
                    position: symbol.position,
                })
            }
            ProjectedRow::Note { .. } => None,
        }
    }

    /// Review target represented by this row. LSP objects keep independent state beneath their
    /// owning file; analysis notes are inert.
    #[must_use]
    pub fn review_target(&self, files: &[FileRow]) -> Option<ReviewTarget> {
        match self {
            ProjectedRow::Directory { path, .. } => Some(ReviewTarget::Directory(path.clone())),
            ProjectedRow::File { file_index, .. } => files
                .get(*file_index)
                .map(|file| ReviewTarget::File(file.path.clone())),
            ProjectedRow::Symbol {
                file_index,
                symbol_index,
                ..
            } => {
                let file = files.get(*file_index)?;
                let symbol = file.symbols.get(*symbol_index)?;
                Some(ReviewTarget::Symbol {
                    file: file.path.clone(),
                    name: symbol.name.clone(),
                    position: symbol.position,
                })
            }
            ProjectedRow::Note { .. } => None,
        }
    }
}

/// Directory prefixes of a repo-relative file path, shallowest first.
#[must_use]
pub fn directory_prefixes(path: &str) -> Vec<String> {
    let components: Vec<&str> = path.split('/').collect();
    if components.len() < 2 {
        return Vec::new();
    }
    (1..components.len())
        .map(|end| components[..end].join("/"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DirectoryChild {
    Directory(String),
    File(String),
}

fn directory_children(files: &[FileRow]) -> HashMap<String, HashSet<DirectoryChild>> {
    let mut children = HashMap::<String, HashSet<DirectoryChild>>::new();
    for file in files {
        let directories = directory_prefixes(&file.path);
        let mut parent = String::new();
        for directory in &directories {
            children
                .entry(parent.clone())
                .or_default()
                .insert(DirectoryChild::Directory(directory.clone()));
            parent.clone_from(directory);
        }
        children
            .entry(parent)
            .or_default()
            .insert(DirectoryChild::File(file.path.clone()));
    }
    children
}

/// Display directory rows for one file. Consecutive directories are one row while every
/// directory in the run has exactly one child and that child is the next directory.
fn display_directories(
    path: &str,
    children: &HashMap<String, HashSet<DirectoryChild>>,
) -> Vec<(String, String)> {
    let prefixes = directory_prefixes(path);
    let components = path.split('/').collect::<Vec<_>>();
    let mut displayed = Vec::new();
    let mut start = 0usize;
    while start < prefixes.len() {
        let mut end = start;
        while end + 1 < prefixes.len()
            && children.get(&prefixes[end]).is_some_and(|direct| {
                direct.len() == 1
                    && direct.contains(&DirectoryChild::Directory(prefixes[end + 1].clone()))
            })
        {
            end += 1;
        }
        displayed.push((
            prefixes[end].clone(),
            format!("{}/", components[start..=end].join("/")),
        ));
        start = end + 1;
    }
    displayed
}

/// Physical rows in display order. Changed files are already path-sorted by the git
/// boundary, so emitting unseen directory prefixes produces a stable tree walk.
#[must_use]
pub fn project(files: &[FileRow], collapsed_directories: &HashSet<String>) -> Vec<ProjectedRow> {
    let mut out = Vec::new();
    let mut emitted_directories = HashSet::new();
    let mut logical = 0usize;
    let children = directory_children(files);

    for (file_index, file) in files.iter().enumerate() {
        let directories = display_directories(&file.path, &children);
        let mut hidden = false;
        for (depth, (directory, label)) in directories.iter().enumerate() {
            if hidden {
                break;
            }
            if emitted_directories.insert(directory.clone()) {
                out.push(ProjectedRow::Directory {
                    path: directory.clone(),
                    label: label.clone(),
                    depth,
                    logical_index: logical,
                });
                logical += 1;
            }
            if collapsed_directories.contains(directory) {
                hidden = true;
            }
        }
        if hidden {
            continue;
        }

        let file_depth = directories.len();
        out.push(ProjectedRow::File {
            file_index,
            depth: file_depth,
            logical_index: logical,
        });
        logical += 1;

        if !file.expanded {
            continue;
        }
        let child_depth = file_depth + 1;
        if file.semantic == FileSemanticLoad::Ready && !file.symbols.is_empty() {
            for symbol_index in 0..file.symbols.len() {
                out.push(ProjectedRow::Symbol {
                    file_index,
                    symbol_index,
                    depth: child_depth,
                    logical_index: logical,
                });
                logical += 1;
            }
        } else {
            out.push(ProjectedRow::Note {
                file_index,
                depth: child_depth,
            });
        }
    }
    out
}

/// Number of selectable directory, file, and visible symbol rows.
#[must_use]
pub fn logical_row_count(files: &[FileRow], collapsed_directories: &HashSet<String>) -> usize {
    project(files, collapsed_directories)
        .iter()
        .filter(|row| row.logical_index().is_some())
        .count()
}

/// Physical index of the first visible row that keeps the selection on screen.
#[must_use]
pub fn first_visible(
    files: &[FileRow],
    collapsed_directories: &HashSet<String>,
    selected_logical: usize,
    capacity: usize,
) -> usize {
    let rows = project(files, collapsed_directories);
    let selected_physical = rows
        .iter()
        .position(|row| row.logical_index() == Some(selected_logical))
        .unwrap_or(0);
    let base = (selected_physical + 1).saturating_sub(capacity);
    base.min(rows.len().saturating_sub(capacity))
}

/// Resolve a logical index to the exact projected row.
#[must_use]
pub fn resolve_logical(
    files: &[FileRow],
    collapsed_directories: &HashSet<String>,
    logical_index: usize,
) -> Option<ProjectedRow> {
    project(files, collapsed_directories)
        .into_iter()
        .find(|row| row.logical_index() == Some(logical_index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{FileSemanticLoad, SymbolRow};

    fn file(path: &str) -> FileRow {
        FileRow {
            semantic: FileSemanticLoad::Ready,
            path: path.to_string(),
            status: "M",
            changed_symbol_count: 1,
            added_lines: 1,
            removed_lines: 0,
            symbols: vec![SymbolRow {
                name: "run".to_string(),
                change: "modified",
                confidence: "",
                has_diagnostic: false,
                position: Some((1, 0)),
            }],
            expanded: false,
        }
    }

    #[test]
    fn projects_nested_directories_once_and_collapses_descendants() {
        let files = vec![
            file("crates/api/src/lib.rs"),
            file("crates/api/tests/api.rs"),
            file("README.md"),
        ];
        let rows = project(&files, &HashSet::new());
        let directories: Vec<&str> = rows
            .iter()
            .filter_map(|row| match row {
                ProjectedRow::Directory { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            directories,
            ["crates/api", "crates/api/src", "crates/api/tests"]
        );

        let labels: Vec<&str> = rows
            .iter()
            .filter_map(|row| match row {
                ProjectedRow::Directory { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["crates/api/", "src/", "tests/"]);

        let collapsed = HashSet::from(["crates/api".to_string()]);
        let rows = project(&files, &collapsed);
        assert!(rows.iter().any(|row| matches!(
            row,
            ProjectedRow::Directory { path, .. } if path == "crates/api"
        )));
        assert!(!rows.iter().any(|row| matches!(
            row,
            ProjectedRow::File {
                file_index: 0 | 1,
                ..
            }
        )));
        assert!(rows
            .iter()
            .any(|row| matches!(row, ProjectedRow::File { file_index: 2, .. })));
    }

    #[test]
    fn symbol_rows_are_independent_review_targets() {
        let mut source = file("src/lib.rs");
        source.expanded = true;
        let files = vec![source];
        let rows = project(&files, &HashSet::new());
        let symbol = rows
            .iter()
            .find(|row| matches!(row, ProjectedRow::Symbol { .. }))
            .expect("expanded LSP object row");
        assert_eq!(
            symbol.review_target(&files),
            Some(ReviewTarget::Symbol {
                file: "src/lib.rs".into(),
                name: "run".into(),
                position: Some((1, 0)),
            })
        );
    }

    #[test]
    fn unbranched_directory_chains_are_one_selectable_row() {
        let files = vec![
            file("sandbox/vm/packages/module/internal/network/a.go"),
            file("sandbox/vm/packages/module/internal/network/b.go"),
            file("sandbox/vm/packages/module/worker/run.go"),
            file("sandbox/vm/packages/module/main.go"),
        ];
        let rows = project(&files, &HashSet::new());
        let directories = rows
            .iter()
            .filter_map(|row| match row {
                ProjectedRow::Directory {
                    path, label, depth, ..
                } => Some((path.as_str(), label.as_str(), *depth)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            directories,
            [
                (
                    "sandbox/vm/packages/module",
                    "sandbox/vm/packages/module/",
                    0
                ),
                (
                    "sandbox/vm/packages/module/internal/network",
                    "internal/network/",
                    1
                ),
                ("sandbox/vm/packages/module/worker", "worker/", 1),
            ]
        );
    }
}
