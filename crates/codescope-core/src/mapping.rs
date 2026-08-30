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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HunkMapping {
    /// The hunk being mapped.
    pub hunk: HunkId,
    /// Target symbols (tree-local ids within the revision the hunk was mapped against).
    /// Empty when [`MappingConfidence::Unmapped`]. Multiple targets when a hunk spans
    /// several symbols (the first is the smallest common ancestor).
    #[serde(default)]
    pub targets: Vec<SymbolId>,
    /// Mapping confidence.
    pub confidence: MappingConfidence,
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
