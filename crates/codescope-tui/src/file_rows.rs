//! The shared files-pane row projection: one structural model used by BOTH rendering and
//! mouse hit-testing, so a click can never resolve to a row the user does not see.
//!
//! Three coordinate spaces are distinct:
//! - logical/selectable index (`App::file_sel`): file rows + expanded symbol rows only;
//! - physical item index: every displayed row, including non-selectable note/empty rows;
//! - visible screen row: physical index minus the scroll offset.
//!
//! The projection maps between them so hit-testing never guesses.

use crate::snapshot::{FileRow, FileSemanticLoad, SymbolRow};

/// One physical row in the files pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectedRow {
    /// A changed file row.
    File {
        /// Index into `UiSnapshot::files`.
        file_index: usize,
        /// The logical (selectable) flat index.
        logical_index: usize,
    },
    /// A symbol row under an expanded file.
    Symbol {
        /// Index into `UiSnapshot::files`.
        file_index: usize,
        /// Index into that file's `symbols`.
        symbol_index: usize,
        /// The logical (selectable) flat index.
        logical_index: usize,
    },
    /// A non-selectable note row (loading / unavailable / failed / empty).
    Note {
        /// Index into `UiSnapshot::files` (the owning file).
        file_index: usize,
    },
}

impl ProjectedRow {
    /// The logical (selectable) index, when this row is selectable.
    pub fn logical_index(self) -> Option<usize> {
        match self {
            ProjectedRow::File { logical_index, .. } => Some(logical_index),
            ProjectedRow::Symbol { logical_index, .. } => Some(logical_index),
            ProjectedRow::Note { .. } => None,
        }
    }
}

/// The physical rows of the files pane, in display order. Note rows occupy a physical row
/// but carry no logical index (they are not selectable).
pub fn project(files: &[FileRow]) -> Vec<ProjectedRow> {
    let mut out = Vec::new();
    let mut logical = 0usize;
    for (fi, f) in files.iter().enumerate() {
        out.push(ProjectedRow::File {
            file_index: fi,
            logical_index: logical,
        });
        logical += 1;
        if f.expanded {
            // Ready with symbols: real symbol rows. Any other state is a note row.
            let shows_symbols = f.semantic == FileSemanticLoad::Ready && !f.symbols.is_empty();
            if shows_symbols {
                for (si, _) in f.symbols.iter().enumerate() {
                    out.push(ProjectedRow::Symbol {
                        file_index: fi,
                        symbol_index: si,
                        logical_index: logical,
                    });
                    logical += 1;
                }
            } else {
                out.push(ProjectedRow::Note { file_index: fi });
            }
        }
    }
    out
}

/// The number of selectable (logical) rows — what `App::file_sel` ranges over.
pub fn logical_row_count(files: &[FileRow]) -> usize {
    files
        .iter()
        .map(|f| {
            1 + if f.expanded && f.semantic == FileSemanticLoad::Ready {
                f.symbols.len()
            } else {
                0
            }
        })
        .sum()
}

/// The physical index of the first visible row so the selected logical row stays on screen.
pub fn first_visible(files: &[FileRow], selected_logical: usize, capacity: usize) -> usize {
    let rows = project(files);
    let selected_physical = rows
        .iter()
        .position(|r| r.logical_index() == Some(selected_logical))
        .unwrap_or(0);
    // Keep the selected row on screen, scrolling by physical rows.
    let base = (selected_physical + 1).saturating_sub(capacity);
    base.min(rows.len().saturating_sub(capacity))
}

/// The (file, symbol) target of a logical index, mirroring the projection.
pub fn resolve_logical(
    files: &[FileRow],
    logical_index: usize,
) -> Option<(&FileRow, Option<&SymbolRow>)> {
    let mut idx = logical_index;
    for f in files {
        if idx == 0 {
            return Some((f, None));
        }
        idx -= 1;
        if f.expanded && f.semantic == FileSemanticLoad::Ready {
            if idx < f.symbols.len() {
                return Some((f, Some(&f.symbols[idx])));
            }
            idx -= f.symbols.len();
        }
    }
    None
}
