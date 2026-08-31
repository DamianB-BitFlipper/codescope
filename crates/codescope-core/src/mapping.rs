//! Change mapping: how diff hunks relate to symbols (research 03).
//!
//! The mapping algorithm itself lives in `codescope-analysis` (pure function over
//! [`SymbolTree`](crate::SymbolTree) + [`Hunk`](crate::Hunk)); these are its result types.
//! Confidence is carried end-to-end: the UI marks `~`/`?`, and AI plans referencing
//! approximate symbols must carry uncertainty notes.

use crate::git::HunkId;
use crate::semantic::SymbolId;

/// Confidence that a hunk was mapped to the right symbol(s) (research 03).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MappingConfidence {
    /// The hunk lies fully inside the mapped symbol's extent.
    Exact,
    /// Best-effort mapping; the reason explains the approximation.
    Approximate(ApproxReason),
    /// File-level change: no symbol could be identified (import blocks, hunks far from any
    /// symbol, files without a symbol tree).
    Unmapped,
}

impl MappingConfidence {
    /// `true` for [`MappingConfidence::Exact`].
    #[must_use]
    pub fn is_exact(&self) -> bool {
        matches!(self, MappingConfidence::Exact)
    }

    /// `true` for [`MappingConfidence::Approximate`].
    #[must_use]
    pub fn is_approximate(&self) -> bool {
        matches!(self, MappingConfidence::Approximate(_))
    }
}

/// Why a mapping is approximate (research 03, confidence model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApproxReason {
    /// The hunk lies in a gap between symbols (doc comment, import block) and was attached
    /// to the nearest symbol within the line threshold.
    DocCommentOrGap,
    /// A pure-deletion hunk mapped against the base-revision symbol tree.
    DeletedHunkBaseMapped,
    /// The hunk spans multiple symbols; attached to the smallest common ancestor.
    HunkSpansSymbols,
    /// The server returned flat `SymbolInformation` (degraded, top-level-only) instead of a
    /// hierarchical tree.
    FlatSymbolFallback,
}

/// How a symbol changed (research 03).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// The symbol was added (whole-file add, or a new symbol inside a changed file).
    Added,
    /// The symbol's body or signature changed.
    Modified,
    /// The symbol was deleted (mapped via the base-revision tree).
    Deleted,
}

/// Mapping of one hunk to its target symbol(s) (research 03 algorithm output).
/// Which side of the diff a changed run's evidence lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangedSide {
    /// Deleted lines (`old_ln`, base revision).
    Old,
    /// Added lines (`new_ln`, worktree).
    New,
}

/// One changed-run mapping from a hunk (research 03, evolved in review 20). A single Git
/// hunk can contain several context-separated edit islands; each contiguous run of
/// added or deleted lines maps independently, so one hunk can produce several records
/// (distinguished by `run_index`). Context lines are never evidence.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HunkMapping {
    /// The hunk being mapped.
    pub hunk: HunkId,
    /// Stable zero-based index of this run within the hunk's body.
    #[serde(default)]
    pub run_index: u32,
    /// Which side of the diff this run's evidence lives on.
    #[serde(default = "default_side")]
    pub side: ChangedSide,
    /// The run's line range on `side` (zero-based inclusive).
    pub range: crate::position::LineRange,
    /// Which tree namespace `targets` refer to (Base for deletions, Worktree otherwise).
    #[serde(default = "default_revision")]
    pub mapped_revision: crate::semantic::Revision,
    /// Target symbols (tree-local ids within `mapped_revision`). Empty when
    /// [`MappingConfidence::Unmapped`]. Multiple targets when one run genuinely spans
    /// several symbols.
    #[serde(default)]
    pub targets: Vec<SymbolId>,
    /// Mapping confidence.
    pub confidence: MappingConfidence,
}

fn default_side() -> ChangedSide {
    ChangedSide::New
}

fn default_revision() -> crate::semantic::Revision {
    crate::semantic::Revision::Worktree
}

/// A symbol touched by the current change-set (digest unit for UI and AI).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChangedSymbol {
    /// The symbol (tree-local id; resolve names/ranges via the corresponding
    /// [`SymbolTree`](crate::SymbolTree)).
    pub symbol: SymbolId,
    /// Added / modified / deleted.
    pub change_kind: ChangeKind,
    /// Hunks that touched this symbol.
    #[serde(default)]
    pub hunks: Vec<HunkId>,
    /// Worst confidence across the mapping that produced this entry.
    pub confidence: MappingConfidence,
}

impl ChangedSymbol {
    /// Create a changed-symbol entry.
    #[must_use]
    pub fn new(
        symbol: SymbolId,
        change_kind: ChangeKind,
        hunks: Vec<HunkId>,
        confidence: MappingConfidence,
    ) -> Self {
        ChangedSymbol {
            symbol,
            change_kind,
            hunks,
            confidence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    #[test]
    fn confidence_predicates() {
        assert!(MappingConfidence::Exact.is_exact());
        assert!(!MappingConfidence::Exact.is_approximate());
        let approx = MappingConfidence::Approximate(ApproxReason::DocCommentOrGap);
        assert!(approx.is_approximate());
        assert!(!approx.is_exact());
        assert!(!MappingConfidence::Unmapped.is_exact());
    }

    #[test]
    fn changed_symbol_construction() {
        let cs = ChangedSymbol::new(
            SymbolId::new("0/2"),
            ChangeKind::Modified,
            vec![HunkId {
                file: Utf8PathBuf::from("a.go"),
                index: 0,
            }],
            MappingConfidence::Exact,
        );
        assert_eq!(cs.change_kind, ChangeKind::Modified);
        assert_eq!(cs.hunks.len(), 1);
    }
}
