//! Content-aware review marks for the changed-files hierarchy.
//!
//! Files retain an explicit mark only while their parsed Git change is identical. A directory
//! mark captures the exact revisions below it at that moment, so it can continue to cover
//! unchanged descendants without accidentally reviewing new or edited work. Removing that
//! directory mark never rewrites explicit child marks.

use std::collections::HashMap;

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
}

/// Effective review state rendered beside a file or directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    /// This entity has its own explicit review mark.
    Explicit,
    /// Every descendant is covered by an ancestor directory mark.
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
    /// Files whose current revisions are explicitly or hierarchically reviewed.
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

#[derive(Debug, Default)]
struct ComparisonReview {
    revisions: HashMap<String, String>,
    explicit_files: HashMap<String, String>,
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
        } else if directory_prefixes(path).iter().any(|directory| {
            review
                .directory_coverage
                .get(directory)
                .and_then(|coverage| coverage.get(path))
                == Some(revision)
        }) {
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
    review.explicit_files.get(path) == Some(revision)
        || directory_prefixes(path).iter().any(|directory| {
            review
                .directory_coverage
                .get(directory)
                .and_then(|coverage| coverage.get(path))
                == Some(revision)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codescope_core::{ChangeSet, Epoch, FileChange, FileStatus, UnifiedDiffSection};

    use super::*;
    use crate::snapshot::{FileRow, FileSemanticLoad};

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
