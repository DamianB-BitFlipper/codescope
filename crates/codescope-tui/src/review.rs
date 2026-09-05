//! Content-aware review marks for the changed-files hierarchy.
//!
//! Files and LSP symbols retain explicit marks only while their parsed Git change is identical.
//! A directory or file mark overrides its current descendants without rewriting their independent
//! marks, so removing the parent mark reveals the child state that existed underneath it.

use std::collections::{HashMap, HashSet};

use codescope_core::ChangeScope;
use sha2::{Digest, Sha256};

use crate::file_rows::directory_prefixes;
use crate::snapshot::UiSnapshot;

/// A reviewable entity in the changed-files hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTarget {
    /// A synthesized repo-relative directory.
    Directory(String),
    /// A changed repo-relative file.
    File(String),
    /// A changed language-server object nested beneath its owning file.
    Symbol {
        /// Repo-relative owning file.
        file: String,
        /// Display name published by the language server.
        name: String,
        /// Optional identifier position used to disambiguate repeated names.
        position: Option<(u32, u32)>,
    },
}

/// Effective review state rendered beside a directory, file, or language-server object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    /// This entity has its own explicit review mark.
    Explicit,
    /// This entity is covered by an explicit ancestor mark.
    Inherited,
    /// Every descendant is reviewed, but through child-level marks.
    Reviewed,
    /// Some, but not all, descendants are reviewed.
    Partial,
    /// No current revision is reviewed.
    Unreviewed,
}

impl ReviewState {
    /// Stable telemetry label for this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ReviewState::Explicit => "explicit",
            ReviewState::Inherited => "inherited",
            ReviewState::Reviewed => "reviewed",
            ReviewState::Partial => "partial",
            ReviewState::Unreviewed => "unreviewed",
        }
    }
}

/// Effective changed-file review totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReviewProgress {
    /// Files whose current revisions are explicitly or hierarchically reviewed, including files
    /// completed through all of their LSP objects.
    pub reviewed: usize,
    /// Changed files in the currently displayed snapshot.
    pub total: usize,
    /// Whether the snapshot has current parsed comparison data suitable for review marking.
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ComparisonKey {
    repo: String,
    branch: String,
    scope: ChangeScope,
    base: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SymbolKey {
    file: String,
    name: String,
    position: Option<(u32, u32)>,
}

#[derive(Debug, Default)]
struct ComparisonReview {
    revisions: HashMap<String, String>,
    explicit_files: HashMap<String, String>,
    /// Current LSP objects by file. Inventories update only when that file is semantically ready.
    symbols: HashMap<String, HashSet<SymbolKey>>,
    /// Symbol marks retain the owning file revision so changed content invalidates them.
    explicit_symbols: HashMap<SymbolKey, String>,
    /// Each directory stores the exact file revisions covered when it was marked.
    directory_coverage: HashMap<String, HashMap<String, String>>,
}

/// Session-local owner of explicit and inherited review state for every comparison.
#[derive(Debug, Default)]
pub struct ReviewLedger {
    comparisons: HashMap<ComparisonKey, ComparisonReview>,
    active: Option<ComparisonKey>,
    /// Prevents re-hashing the same complete change set on unrelated snapshot publications such
    /// as LSP progress, AI activity, and selection changes.
    last_source: Option<(ComparisonKey, codescope_core::Epoch)>,
}

impl ReviewLedger {
    /// Reconcile review state with the same parsed comparison data that backs the UI.
    ///
    /// A stale, incomplete, or mismatched change set makes review state unavailable instead of
    /// allowing an earlier comparison's marks to leak into the current view.
    pub fn sync(&mut self, snapshot: &UiSnapshot) {
        let Some(changeset) = snapshot.agent_changeset.as_deref() else {
            self.deactivate();
            return;
        };
        if snapshot.agent_changeset_epoch != snapshot.epoch || changeset.scope != snapshot.scope {
            self.deactivate();
            return;
        }
        let key = comparison_key(snapshot);
        if self.last_source.as_ref() == Some(&(key.clone(), snapshot.agent_changeset_epoch)) {
            if let Some(review) = self.comparisons.get_mut(&key) {
                sync_symbols(review, snapshot);
            }
            self.active = Some(key);
            return;
        }

        let mut revisions = HashMap::with_capacity(snapshot.files.len());
        for row in &snapshot.files {
            let Some(change) = changeset
                .files
                .iter()
                .find(|change| change.path == row.path)
            else {
                self.deactivate();
                return;
            };
            let mut hasher = Sha256::new();
            let Ok(parsed) = serde_json::to_vec(change) else {
                self.deactivate();
                return;
            };
            hasher.update((parsed.len() as u64).to_be_bytes());
            hasher.update(parsed);
            if let Some(sections) = changeset.diff_sections.as_ref() {
                for section in sections
                    .iter()
                    .filter(|section| section.path == change.path)
                {
                    hasher.update((section.text.len() as u64).to_be_bytes());
                    hasher.update(section.text.as_bytes());
                }
            }
            revisions.insert(row.path.clone(), format!("{:x}", hasher.finalize()));
        }
        if revisions.len() != changeset.files.len() {
            // The render model and parsed comparison are between publications. Treat it as
            // unavailable rather than correlating marks to only a subset of the visible diff.
            self.deactivate();
            return;
        }

        let review = self.comparisons.entry(key.clone()).or_default();
        review.explicit_files.retain(|path, marked_revision| {
            revisions
                .get(path)
                .is_some_and(|current| current == marked_revision)
        });
        review.directory_coverage.retain(|directory, coverage| {
            let prefix = format!("{directory}/");
            coverage.retain(|path, marked_revision| {
                path.starts_with(&prefix)
                    && revisions
                        .get(path)
                        .is_some_and(|current| current == marked_revision)
            });
            !coverage.is_empty()
        });
        review.revisions = revisions;
        sync_symbols(review, snapshot);
        self.active = Some(key.clone());
        self.last_source = Some((key, snapshot.agent_changeset_epoch));
    }

    /// Toggle the explicit mark on `target` without rewriting any descendant marks.
    pub fn toggle(&mut self, target: &ReviewTarget) {
        let Some(review) = self.active_review_mut() else {
            return;
        };
        match target {
            ReviewTarget::File(path) => {
                let Some(revision) = review.revisions.get(path).cloned() else {
                    return;
                };
                if review.explicit_files.get(path) == Some(&revision) {
                    review.explicit_files.remove(path);
                } else {
                    review.explicit_files.insert(path.clone(), revision);
                }
            }
            ReviewTarget::Symbol {
                file,
                name,
                position,
            } => {
                let key = SymbolKey {
                    file: file.clone(),
                    name: name.clone(),
                    position: *position,
                };
                if !review
                    .symbols
                    .get(file)
                    .is_some_and(|symbols| symbols.contains(&key))
                {
                    return;
                }
                let Some(revision) = review.revisions.get(file).cloned() else {
                    return;
                };
                if review.explicit_symbols.get(&key) == Some(&revision) {
                    review.explicit_symbols.remove(&key);
                } else {
                    review.explicit_symbols.insert(key, revision);
                }
            }
            ReviewTarget::Directory(directory) => {
                let prefix = format!("{directory}/");
                let coverage = review
                    .revisions
                    .iter()
                    .filter(|(path, _)| path.starts_with(&prefix))
                    .map(|(path, revision)| (path.clone(), revision.clone()))
                    .collect::<HashMap<_, _>>();
                let fully_explicit = !coverage.is_empty()
                    && review.directory_coverage.get(directory) == Some(&coverage);
                if fully_explicit {
                    review.directory_coverage.remove(directory);
                } else if !coverage.is_empty() {
                    // A partial checkbox follows the familiar tri-state rule: one click
                    // includes every current descendant; the next click removes the override.
                    review
                        .directory_coverage
                        .insert(directory.clone(), coverage);
                }
            }
        }
    }

    /// Effective state of a review target in the active comparison.
    #[must_use]
    pub fn state(&self, target: &ReviewTarget) -> ReviewState {
        match target {
            ReviewTarget::File(path) => self.file_state(path),
            ReviewTarget::Directory(directory) => self.directory_state(directory),
            ReviewTarget::Symbol {
                file,
                name,
                position,
            } => self.symbol_state(file, name, *position),
        }
    }

    /// Current aggregate review progress. `total` still reflects visible files while parsed
    /// comparison data is unavailable, but no stale file is reported as reviewed.
    #[must_use]
    pub fn progress(&self, visible_file_count: usize) -> ReviewProgress {
        let Some(review) = self.active_review() else {
            return ReviewProgress {
                reviewed: 0,
                total: visible_file_count,
                available: false,
            };
        };
        ReviewProgress {
            reviewed: review
                .revisions
                .keys()
                .filter(|path| is_file_reviewed(review, path))
                .count(),
            total: review.revisions.len(),
            available: true,
        }
    }

    fn file_state(&self, path: &str) -> ReviewState {
        let Some(review) = self.active_review() else {
            return ReviewState::Unreviewed;
        };
        let Some(revision) = review.revisions.get(path) else {
            return ReviewState::Unreviewed;
        };
        if review.explicit_files.get(path) == Some(revision) {
            ReviewState::Explicit
        } else if has_directory_override(review, path, revision) {
            ReviewState::Inherited
        } else {
            let Some(symbols) = review
                .symbols
                .get(path)
                .filter(|symbols| !symbols.is_empty())
            else {
                return ReviewState::Unreviewed;
            };
            let reviewed = symbols
                .iter()
                .filter(|symbol| is_symbol_explicit(review, symbol, revision))
                .count();
            match reviewed {
                0 => ReviewState::Unreviewed,
                count if count == symbols.len() => ReviewState::Reviewed,
                _ => ReviewState::Partial,
            }
        }
    }

    fn symbol_state(&self, file: &str, name: &str, position: Option<(u32, u32)>) -> ReviewState {
        let Some(review) = self.active_review() else {
            return ReviewState::Unreviewed;
        };
        let Some(revision) = review.revisions.get(file) else {
            return ReviewState::Unreviewed;
        };
        let symbol = SymbolKey {
            file: file.to_string(),
            name: name.to_string(),
            position,
        };
        if !review
            .symbols
            .get(file)
            .is_some_and(|symbols| symbols.contains(&symbol))
        {
            return ReviewState::Unreviewed;
        }
        if is_symbol_explicit(review, &symbol, revision) {
            ReviewState::Explicit
        } else if review.explicit_files.get(file) == Some(revision)
            || has_directory_override(review, file, revision)
        {
            ReviewState::Inherited
        } else {
            ReviewState::Unreviewed
        }
    }

    fn directory_state(&self, directory: &str) -> ReviewState {
        let Some(review) = self.active_review() else {
            return ReviewState::Unreviewed;
        };
        let prefix = format!("{directory}/");
        let descendants = review
            .revisions
            .iter()
            .filter(|(path, _)| path.starts_with(&prefix))
            .collect::<Vec<_>>();
        if descendants.is_empty() {
            return ReviewState::Unreviewed;
        }
        if review
            .directory_coverage
            .get(directory)
            .is_some_and(|coverage| {
                descendants
                    .iter()
                    .all(|(path, revision)| coverage.get(*path) == Some(*revision))
            })
        {
            return ReviewState::Explicit;
        }
        if directory_prefixes(&format!("{directory}/child"))
            .into_iter()
            .filter(|ancestor| ancestor != directory)
            .any(|ancestor| {
                review
                    .directory_coverage
                    .get(&ancestor)
                    .is_some_and(|coverage| {
                        descendants
                            .iter()
                            .all(|(path, revision)| coverage.get(*path) == Some(*revision))
                    })
            })
        {
            return ReviewState::Inherited;
        }
        let reviewed = descendants
            .iter()
            .filter(|(path, _)| is_file_reviewed(review, path))
            .count();
        match reviewed {
            0 => ReviewState::Unreviewed,
            count if count == descendants.len() => ReviewState::Reviewed,
            _ => ReviewState::Partial,
        }
    }

    fn active_review(&self) -> Option<&ComparisonReview> {
        self.comparisons.get(self.active.as_ref()?)
    }

    fn active_review_mut(&mut self) -> Option<&mut ComparisonReview> {
        let key = self.active.as_ref()?;
        self.comparisons.get_mut(key)
    }

    fn deactivate(&mut self) {
        self.active = None;
        self.last_source = None;
    }
}

fn comparison_key(snapshot: &UiSnapshot) -> ComparisonKey {
    let base = if snapshot.scope == ChangeScope::Branch {
        if snapshot.base_ref.is_empty() {
            snapshot.repo.base.clone().unwrap_or_default()
        } else {
            snapshot.base_ref.clone()
        }
    } else {
        String::new()
    };
    ComparisonKey {
        repo: snapshot.repo.repo_name.clone(),
        branch: snapshot.repo.branch.clone(),
        scope: snapshot.scope,
        base,
    }
}

fn is_file_reviewed(review: &ComparisonReview, path: &str) -> bool {
    let Some(revision) = review.revisions.get(path) else {
        return false;
    };
    if review.explicit_files.get(path) == Some(revision)
        || has_directory_override(review, path, revision)
    {
        return true;
    }
    review.symbols.get(path).is_some_and(|symbols| {
        !symbols.is_empty()
            && symbols
                .iter()
                .all(|symbol| is_symbol_explicit(review, symbol, revision))
    })
}

fn has_directory_override(review: &ComparisonReview, path: &str, revision: &str) -> bool {
    directory_prefixes(path).iter().any(|directory| {
        review
            .directory_coverage
            .get(directory)
            .and_then(|coverage| coverage.get(path))
            .is_some_and(|covered| covered == revision)
    })
}

fn is_symbol_explicit(review: &ComparisonReview, symbol: &SymbolKey, revision: &str) -> bool {
    review
        .explicit_symbols
        .get(symbol)
        .is_some_and(|marked| marked == revision)
}

fn sync_symbols(review: &mut ComparisonReview, snapshot: &UiSnapshot) {
    review
        .symbols
        .retain(|path, _| snapshot.files.iter().any(|file| file.path == *path));
    for file in &snapshot.files {
        match file.semantic {
            crate::snapshot::FileSemanticLoad::Ready => {
                review.symbols.insert(
                    file.path.clone(),
                    file.symbols
                        .iter()
                        .map(|symbol| SymbolKey {
                            file: file.path.clone(),
                            name: symbol.name.clone(),
                            position: symbol.position,
                        })
                        .collect(),
                );
            }
            crate::snapshot::FileSemanticLoad::Unsupported => {
                review.symbols.remove(&file.path);
            }
            crate::snapshot::FileSemanticLoad::Unloaded
            | crate::snapshot::FileSemanticLoad::Loading
            | crate::snapshot::FileSemanticLoad::Failed => {}
        }
    }
    let revisions = &review.revisions;
    let symbols = &review.symbols;
    review.explicit_symbols.retain(|symbol, revision| {
        revisions.get(&symbol.file) == Some(revision)
            && symbols
                .get(&symbol.file)
                .is_some_and(|current| current.contains(symbol))
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codescope_core::{ChangeSet, Epoch, FileChange, FileStatus, UnifiedDiffSection};

    use super::*;
    use crate::snapshot::{FileRow, FileSemanticLoad, SymbolRow};

    fn snapshot(files: &[(&str, &str)], epoch: u64) -> UiSnapshot {
        let rows = files
            .iter()
            .map(|(path, _)| FileRow {
                semantic: FileSemanticLoad::Ready,
                path: (*path).to_string(),
                status: "M",
                changed_symbol_count: 0,
                added_lines: 1,
                removed_lines: 1,
                symbols: Vec::new(),
                expanded: false,
            })
            .collect::<Vec<_>>();
        let changes = files
            .iter()
            .map(|(path, _)| FileChange {
                path: (*path).into(),
                old_path: None,
                status: FileStatus::Modified,
                hunks: Vec::new(),
                binary: false,
            })
            .collect::<Vec<_>>();
        let sections = files
            .iter()
            .map(|(path, text)| UnifiedDiffSection {
                path: (*path).into(),
                text: (*text).to_string(),
            })
            .collect();
        UiSnapshot {
            files: rows,
            agent_changeset: Some(Arc::new(
                ChangeSet::new(ChangeScope::Branch, changes).with_diff_sections(sections),
            )),
            agent_changeset_epoch: Epoch(epoch),
            epoch: Epoch(epoch),
            base_ref: "main".to_string(),
            ..UiSnapshot::default()
        }
    }

    fn with_symbols(mut snapshot: UiSnapshot, file: &str, names: &[&str]) -> UiSnapshot {
        let row = snapshot
            .files
            .iter_mut()
            .find(|row| row.path == file)
            .expect("symbol owner");
        row.symbols = names
            .iter()
            .enumerate()
            .map(|(index, name)| SymbolRow {
                name: (*name).to_string(),
                change: "modified",
                confidence: "",
                has_diagnostic: false,
                position: Some((index as u32 + 1, 0)),
            })
            .collect();
        row.changed_symbol_count = row.symbols.len();
        row.expanded = true;
        row.semantic = FileSemanticLoad::Ready;
        snapshot
    }

    fn symbol(file: &str, name: &str, line: u32) -> ReviewTarget {
        ReviewTarget::Symbol {
            file: file.to_string(),
            name: name.to_string(),
            position: Some((line, 0)),
        }
    }

    #[test]
    fn lsp_objects_roll_up_to_files_without_losing_independent_marks() {
        let snap = with_symbols(snapshot(&[("x/a.rs", "a1")], 1), "x/a.rs", &["one", "two"]);
        let mut ledger = ReviewLedger::default();
        ledger.sync(&snap);

        ledger.toggle(&symbol("x/a.rs", "one", 1));
        assert_eq!(
            ledger.state(&symbol("x/a.rs", "one", 1)),
            ReviewState::Explicit
        );
        assert_eq!(
            ledger.state(&ReviewTarget::File("x/a.rs".into())),
            ReviewState::Partial
        );
        assert_eq!(ledger.progress(1).reviewed, 0);

        ledger.toggle(&ReviewTarget::File("x/a.rs".into()));
        assert_eq!(
            ledger.state(&symbol("x/a.rs", "one", 1)),
            ReviewState::Explicit,
            "an explicit object mark remains distinguishable under a file override"
        );
        assert_eq!(
            ledger.state(&symbol("x/a.rs", "two", 2)),
            ReviewState::Inherited
        );

        ledger.toggle(&ReviewTarget::File("x/a.rs".into()));
        assert_eq!(
            ledger.state(&symbol("x/a.rs", "one", 1)),
            ReviewState::Explicit
        );
        assert_eq!(
            ledger.state(&symbol("x/a.rs", "two", 2)),
            ReviewState::Unreviewed
        );

        ledger.toggle(&symbol("x/a.rs", "two", 2));
        assert_eq!(
            ledger.state(&ReviewTarget::File("x/a.rs".into())),
            ReviewState::Reviewed
        );
        assert_eq!(
            ledger.state(&ReviewTarget::Directory("x".into())),
            ReviewState::Reviewed
        );
        assert_eq!(ledger.progress(1).reviewed, 1);
    }

    #[test]
    fn directory_override_covers_lsp_objects_without_rewriting_them() {
        let snap = with_symbols(snapshot(&[("x/a.rs", "a1")], 1), "x/a.rs", &["one", "two"]);
        let mut ledger = ReviewLedger::default();
        ledger.sync(&snap);
        ledger.toggle(&symbol("x/a.rs", "one", 1));

        ledger.toggle(&ReviewTarget::Directory("x".into()));
        assert_eq!(
            ledger.state(&ReviewTarget::File("x/a.rs".into())),
            ReviewState::Inherited
        );
        assert_eq!(
            ledger.state(&symbol("x/a.rs", "one", 1)),
            ReviewState::Explicit
        );
        assert_eq!(
            ledger.state(&symbol("x/a.rs", "two", 2)),
            ReviewState::Inherited
        );

        ledger.toggle(&ReviewTarget::Directory("x".into()));
        assert_eq!(
            ledger.state(&symbol("x/a.rs", "one", 1)),
            ReviewState::Explicit
        );
        assert_eq!(
            ledger.state(&symbol("x/a.rs", "two", 2)),
            ReviewState::Unreviewed
        );
        assert_eq!(
            ledger.state(&ReviewTarget::File("x/a.rs".into())),
            ReviewState::Partial
        );
    }

    #[test]
    fn same_epoch_lsp_arrival_registers_reviewable_objects() {
        let loading = snapshot(&[("x/a.rs", "a1")], 1);
        let ready = with_symbols(loading.clone(), "x/a.rs", &["run"]);
        let mut ledger = ReviewLedger::default();
        ledger.sync(&loading);
        ledger.sync(&ready);
        ledger.toggle(&symbol("x/a.rs", "run", 1));
        assert_eq!(
            ledger.state(&symbol("x/a.rs", "run", 1)),
            ReviewState::Explicit
        );
    }

    #[test]
    fn changed_file_content_invalidates_its_lsp_object_marks() {
        let initial = with_symbols(snapshot(&[("x/a.rs", "a1")], 1), "x/a.rs", &["run"]);
        let changed = with_symbols(snapshot(&[("x/a.rs", "a2")], 2), "x/a.rs", &["run"]);
        let mut ledger = ReviewLedger::default();
        ledger.sync(&initial);
        ledger.toggle(&symbol("x/a.rs", "run", 1));

        ledger.sync(&changed);
        assert_eq!(
            ledger.state(&symbol("x/a.rs", "run", 1)),
            ReviewState::Unreviewed
        );
    }

    #[test]
    fn directory_override_does_not_rewrite_child_marks() {
        let snap = snapshot(&[("x/a.rs", "a1"), ("x/b.rs", "b1")], 1);
        let mut ledger = ReviewLedger::default();
        ledger.sync(&snap);
        ledger.toggle(&ReviewTarget::File("x/a.rs".into()));
        ledger.toggle(&ReviewTarget::Directory("x".into()));
        assert_eq!(
            ledger.state(&ReviewTarget::Directory("x".into())),
            ReviewState::Explicit
        );
        assert_eq!(
            ledger.state(&ReviewTarget::File("x/b.rs".into())),
            ReviewState::Inherited
        );

        ledger.toggle(&ReviewTarget::Directory("x".into()));
        assert_eq!(
            ledger.state(&ReviewTarget::File("x/a.rs".into())),
            ReviewState::Explicit
        );
        assert_eq!(
            ledger.state(&ReviewTarget::File("x/b.rs".into())),
            ReviewState::Unreviewed
        );
        assert_eq!(ledger.progress(2).reviewed, 1);
    }

    #[test]
    fn changed_content_invalidates_only_that_revision() {
        let initial = snapshot(&[("x/a.rs", "a1"), ("x/b.rs", "b1")], 1);
        let mut ledger = ReviewLedger::default();
        ledger.sync(&initial);
        ledger.toggle(&ReviewTarget::Directory("x".into()));

        let changed = snapshot(&[("x/a.rs", "a1"), ("x/b.rs", "b2")], 2);
        ledger.sync(&changed);
        assert_eq!(
            ledger.state(&ReviewTarget::File("x/a.rs".into())),
            ReviewState::Inherited
        );
        assert_eq!(
            ledger.state(&ReviewTarget::File("x/b.rs".into())),
            ReviewState::Unreviewed
        );
        assert_eq!(
            ledger.state(&ReviewTarget::Directory("x".into())),
            ReviewState::Partial
        );

        ledger.toggle(&ReviewTarget::Directory("x".into()));
        assert_eq!(
            ledger.state(&ReviewTarget::Directory("x".into())),
            ReviewState::Explicit,
            "clicking an indeterminate directory reviews all current descendants"
        );
        ledger.toggle(&ReviewTarget::Directory("x".into()));
        assert_eq!(ledger.progress(2).reviewed, 0);
    }

    #[test]
    fn unchanged_refresh_retains_marks_but_stale_data_is_never_active() {
        let snap = snapshot(&[("x/a.rs", "a1")], 1);
        let mut ledger = ReviewLedger::default();
        ledger.sync(&snap);
        ledger.toggle(&ReviewTarget::File("x/a.rs".into()));
        ledger.sync(&snap.clone());
        assert_eq!(ledger.progress(1).reviewed, 1);

        let mut stale = snap;
        stale.epoch = Epoch(2);
        ledger.sync(&stale);
        assert_eq!(
            ledger.progress(1),
            ReviewProgress {
                reviewed: 0,
                total: 1,
                available: false
            }
        );
        assert_eq!(
            ledger.state(&ReviewTarget::File("x/a.rs".into())),
            ReviewState::Unreviewed
        );
    }
}
