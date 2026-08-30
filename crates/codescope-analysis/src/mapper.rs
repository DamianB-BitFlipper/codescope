//! Pure hunk → symbol mapping (research 03).
//!
//! [`map_changes`] implements the verified algorithm: new-side hunks map against the
//! **worktree** symbol tree (smallest containing symbol, line-granular); pure deletions
//! map against the **base**-revision tree when one is provided; changes in gaps between
//! symbols (doc comments, import blocks) attach approximately to the nearest symbol
//! within [`GAP_ATTACH_LINES`] lines, else stay [`MappingConfidence::Unmapped`].
//!
//! Everything here is pure — no git, no LSP, no I/O — so it is unit-testable with
//! hand-built [`SymbolTree`]s.

use codescope_core::{
    ApproxReason, Hunk, HunkId, LineRange, MappingConfidence, SymbolId, SymbolNode, SymbolTree,
};

/// Maximum distance in lines for attaching a gap change to a neighbouring symbol
/// (research 03: doc comments / signature edits sit within ~3 lines of their symbol).
pub const GAP_ATTACH_LINES: u32 = 3;

/// One hunk's mapping plus analysis-level detail the core
/// [`HunkMapping`](codescope_core::HunkMapping) cannot carry.
///
/// [`HunkMapping`](codescope_core::HunkMapping) is the wire/domain record; this wrapper
/// adds the signature-touch flag (research 03: selection-range intersection is noted but
/// the mapping stays [`MappingConfidence::Exact`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedHunk {
    /// The domain mapping record (hunk id, targets, confidence).
    pub mapping: codescope_core::HunkMapping,
    /// `true` when the hunk's new-side lines intersect the mapped symbol's *selection*
    /// range (identifier line) — a signature-ish change. Only meaningful for single-target
    /// exact mappings.
    pub signature_touch: bool,
}

/// Map hunks against the worktree tree only (no base-revision tree available).
///
/// Pure deletions then fall back to the nearest surviving symbol around the insertion
/// point ([`ApproxReason::DocCommentOrGap`]); prefer [`map_changes_with_base`] whenever a
/// base tree can be produced.
#[must_use]
pub fn map_changes(tree: &SymbolTree, hunks: &[Hunk]) -> Vec<codescope_core::HunkMapping> {
    map_changes_with_base(tree, None, hunks)
}

/// Map hunks against the worktree tree, using `base` for pure deletions when provided.
///
/// Target ids in the result are tree-local to the revision they were mapped against:
/// [`ApproxReason::DeletedHunkBaseMapped`] targets belong to `base`, everything else to
/// `tree` (see [`codescope_core::HunkMapping::targets`]).
#[must_use]
pub fn map_changes_with_base(
    tree: &SymbolTree,
    base: Option<&SymbolTree>,
    hunks: &[Hunk],
) -> Vec<codescope_core::HunkMapping> {
    map_changes_detailed(tree, base, hunks)
        .into_iter()
        .map(|m| m.mapping)
        .collect()
}

/// [`map_changes_with_base`] plus the signature-touch flag per hunk.
#[must_use]
pub fn map_changes_detailed(
    tree: &SymbolTree,
    base: Option<&SymbolTree>,
    hunks: &[Hunk],
) -> Vec<MappedHunk> {
    let file = tree.file.as_path().to_path_buf();
    hunks
        .iter()
        .enumerate()
        .map(|(index, hunk)| {
            let hunk_id = HunkId {
                file: file.clone(),
                index: index as u32,
            };
            let (targets, confidence, signature_touch) = map_one(tree, base, hunk);
            tracing::trace!(hunk = %hunk_id, ?confidence, targets = targets.len(), "mapped hunk");
            MappedHunk {
                mapping: codescope_core::HunkMapping {
                    hunk: hunk_id,
                    targets,
                    confidence,
                },
                signature_touch,
            }
        })
        .collect()
}

/// Map a single hunk. Returns `(targets, confidence, signature_touch)`.
fn map_one(
    tree: &SymbolTree,
    base: Option<&SymbolTree>,
    hunk: &Hunk,
) -> (Vec<SymbolId>, MappingConfidence, bool) {
    if hunk.new_len == 0 && hunk.old_len == 0 {
        // Degenerate hunk (git never emits this); nothing to map.
        return (Vec::new(), MappingConfidence::Unmapped, false);
    }
    if hunk.is_pure_deletion() {
        return map_pure_deletion(tree, base, hunk);
    }

    // New-side hunk: git 1-based [new_start, new_start+new_len) → zero-based inclusive span.
    let target = zero_based_span(hunk.new_start, hunk.new_len);

    if let Some(sym) = tree.find_smallest_containing(&target) {
        let signature_touch = sym.selection.intersects_lines(&target);
        return (vec![sym.id.clone()], MappingConfidence::Exact, signature_touch);
    }

    // No single container: either the hunk covers whole symbols, spans several, or sits
    // in a gap (doc comment / import block / between top-level symbols).
    let intersected: Vec<&SymbolNode> = tree
        .roots
        .iter()
        .filter(|n| n.range.intersects_lines(&target))
        .collect();
    match intersected.len() {
        0 => map_gap(tree, &target),
        1 => {
            let sym = intersected[0];
            if target.contains_lines(&sym.range) {
                // The hunk fully covers the symbol: a whole-symbol addition/rewrite
                // (research 03: "whole symbol added → Exact on that symbol").
                let signature_touch = sym.selection.intersects_lines(&target);
                (vec![sym.id.clone()], MappingConfidence::Exact, signature_touch)
            } else {
                // Partial overlap hanging into a gap (typically the symbol plus its doc
                // comment, which gopls excludes from the range).
                (
                    vec![sym.id.clone()],
                    MappingConfidence::Approximate(ApproxReason::DocCommentOrGap),
                    false,
                )
            }
        }
        _ => {
            let targets: Vec<SymbolId> = intersected.iter().map(|n| n.id.clone()).collect();
            if intersected.iter().all(|n| target.contains_lines(&n.range)) {
                // Every intersected symbol is fully covered — e.g. a whole-file addition:
                // each top-level symbol is exactly added (research 03).
                (targets, MappingConfidence::Exact, false)
            } else {
                (
                    targets,
                    MappingConfidence::Approximate(ApproxReason::HunkSpansSymbols),
                    false,
                )
            }
        }
    }
}

/// Map a pure-deletion hunk (`new_len == 0`).
///
/// With a base tree: map the old-side span against it (always
/// [`ApproxReason::DeletedHunkBaseMapped`] — the symbol ids refer to the base tree).
/// Without one: attach to the nearest surviving symbol around the insertion point.
fn map_pure_deletion(
    tree: &SymbolTree,
    base: Option<&SymbolTree>,
    hunk: &Hunk,
) -> (Vec<SymbolId>, MappingConfidence, bool) {
    if let Some(base) = base {
        let old = zero_based_span(hunk.old_start, hunk.old_len);
        if let Some(sym) = base.find_smallest_containing(&old) {
            return (
                vec![sym.id.clone()],
                MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped),
                false,
            );
        }
        let intersected: Vec<SymbolId> = base
            .roots
            .iter()
            .filter(|n| n.range.intersects_lines(&old))
            .map(|n| n.id.clone())
            .collect();
        if !intersected.is_empty() {
            return (
                intersected,
                MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped),
                false,
            );
        }
        if let Some(sym) = nearest_within(base, &old, GAP_ATTACH_LINES) {
            return (
                vec![sym.id.clone()],
                MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped),
                false,
            );
        }
        // Fall through to the surviving-neighbour fallback below.
    }

    // No base tree (or the deletion mapped to nothing in it): nearest surviving symbol
    // around the insertion point on the new side (research 03).
    let point = hunk.insertion_point_zero_based();
    let target = LineRange::from_line_span(point, point);
    if let Some(sym) = tree.find_smallest_containing(&target) {
        // Deleted lines from inside a surviving symbol's extent.
        return (
            vec![sym.id.clone()],
            MappingConfidence::Approximate(ApproxReason::DocCommentOrGap),
            false,
        );
    }
    match nearest_within(tree, &target, GAP_ATTACH_LINES) {
        Some(sym) => (
            vec![sym.id.clone()],
            MappingConfidence::Approximate(ApproxReason::DocCommentOrGap),
            false,
        ),
        None => (Vec::new(), MappingConfidence::Unmapped, false),
    }
}

/// Gap change (no symbol intersected): nearest symbol within [`GAP_ATTACH_LINES`] lines,
/// preferring the symbol *below* (doc comments precede their symbol), else unmapped.
fn map_gap(tree: &SymbolTree, target: &LineRange) -> (Vec<SymbolId>, MappingConfidence, bool) {
    match nearest_within(tree, target, GAP_ATTACH_LINES) {
        Some(sym) => (
            vec![sym.id.clone()],
            MappingConfidence::Approximate(ApproxReason::DocCommentOrGap),
            false,
        ),
        None => (Vec::new(), MappingConfidence::Unmapped, false),
    }
}

/// Nearest top-level symbol within `max_lines` of `target`, preferring the one below
/// (doc comments and detached signature edits precede their symbol).
fn nearest_within<'t>(
    tree: &'t SymbolTree,
    target: &LineRange,
    max_lines: u32,
) -> Option<&'t SymbolNode> {
    let below = tree
        .nearest_below(target.end_line)
        .filter(|s| s.range.start_line.saturating_sub(target.end_line) <= max_lines);
    if below.is_some() {
        return below;
    }
    tree.nearest_above(target.start_line)
        .filter(|s| target.start_line.saturating_sub(s.range.end_line) <= max_lines)
}

/// Convert a git 1-based `(start, len)` span (`len >= 1`) into a zero-based inclusive
/// [`LineRange`] line span.
fn zero_based_span(start_1based: u32, len: u32) -> LineRange {
    let start = start_1based.saturating_sub(1);
    let end = start + len.saturating_sub(1);
    LineRange::from_line_span(start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codescope_core::{FileId, Revision, SymbolKind};

    fn node(id: &str, name: &str, start: u32, end: u32) -> SymbolNode {
        SymbolNode {
            id: SymbolId::new(id),
            name: name.to_string(),
            detail: None,
            kind: SymbolKind::Function,
            range: LineRange::new(start, 0, end, 1),
            selection: LineRange::new(start, 5, start, 5 + name.len() as u32),
            children: Vec::new(),
        }
    }

    /// main: 5-15, Greeter{Name}: 20-30 (field 22-22), (Greeter).Hello: 40-50.
    fn tree() -> SymbolTree {
        let mut greeter = node("1", "Greeter", 20, 30);
        greeter.kind = SymbolKind::Struct;
        greeter.children = vec![SymbolNode {
            kind: SymbolKind::Field,
            ..node("1/0", "Name", 22, 22)
        }];
        SymbolTree::new(
            FileId::new("main.go").unwrap(),
            Revision::Worktree,
            vec![node("0", "main", 5, 15), greeter, node("2", "(Greeter).Hello", 40, 50)],
        )
    }

    /// Base tree: main 5-15, Legacy 20-28.
    fn base_tree() -> SymbolTree {
        SymbolTree::new(
            FileId::new("main.go").unwrap(),
            Revision::Base,
            vec![node("0", "main", 5, 15), node("1", "Legacy", 20, 28)],
        )
    }

    fn hunk(old_start: u32, old_len: u32, new_start: u32, new_len: u32) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            section: None,
            lines: Vec::new(),
        }
    }

    #[test]
    fn body_change_maps_exact_without_signature_touch() {
        // New-side lines 9..=11 (1-based) = zero-based 8..=10, inside main (5-15),
        // away from its selection line (5).
        let maps = map_changes(&tree(), &[hunk(9, 3, 9, 3)]);
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].confidence, MappingConfidence::Exact);
        assert_eq!(maps[0].targets, vec![SymbolId::new("0")]);
        assert_eq!(maps[0].hunk.file, "main.go");
        assert_eq!(maps[0].hunk.index, 0);
        let detailed = map_changes_detailed(&tree(), None, &[hunk(9, 3, 9, 3)]);
        assert!(!detailed[0].signature_touch);
    }

    #[test]
    fn signature_touch_stays_exact_but_is_noted() {
        // Zero-based line 5 is main's selection line; 1-based new_start = 6.
        let detailed = map_changes_detailed(&tree(), None, &[hunk(6, 2, 6, 2)]);
        assert_eq!(detailed[0].mapping.confidence, MappingConfidence::Exact);
        assert_eq!(detailed[0].mapping.targets, vec![SymbolId::new("0")]);
        assert!(detailed[0].signature_touch);
    }

    #[test]
    fn nested_field_change_maps_to_field() {
        // Zero-based line 22 = field Name; 1-based 23.
        let maps = map_changes(&tree(), &[hunk(23, 1, 23, 1)]);
        assert_eq!(maps[0].targets, vec![SymbolId::new("1/0")]);
        assert_eq!(maps[0].confidence, MappingConfidence::Exact);
    }

    #[test]
    fn gap_change_attaches_to_symbol_below_within_threshold() {
        // Zero-based lines 17..=18 sit in the gap; Greeter starts at 20 (2 lines below).
        let maps = map_changes(&tree(), &[hunk(18, 2, 18, 2)]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::DocCommentOrGap)
        );
        assert_eq!(maps[0].targets, vec![SymbolId::new("1")]);
    }

    #[test]
    fn gap_change_attaches_above_when_below_is_too_far() {
        // Zero-based line 16: main ends at 15 (1 above), Greeter starts at 20 (4 below).
        let maps = map_changes(&tree(), &[hunk(17, 1, 17, 1)]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::DocCommentOrGap)
        );
        assert_eq!(maps[0].targets, vec![SymbolId::new("0")]);
        // Zero-based line 35: Hello starts at 40 (5 below), Greeter ends 30 (5 above) → far.
        let maps = map_changes(&tree(), &[hunk(36, 1, 36, 1)]);
        assert_eq!(maps[0].confidence, MappingConfidence::Unmapped);
        assert!(maps[0].targets.is_empty());
        // Zero-based line 33: Greeter ends at 30 (3 above, within), Hello starts 40 (7 below).
        let maps = map_changes(&tree(), &[hunk(34, 1, 34, 1)]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::DocCommentOrGap)
        );
        assert_eq!(maps[0].targets, vec![SymbolId::new("1")]);
    }

    #[test]
    fn change_far_from_any_symbol_is_unmapped() {
        // Zero-based line 0 (e.g. package clause / imports): main starts at 5 → too far.
        let maps = map_changes(&tree(), &[hunk(1, 1, 1, 1)]);
        assert_eq!(maps[0].confidence, MappingConfidence::Unmapped);
        assert!(maps[0].targets.is_empty());
    }

    #[test]
    fn pure_deletion_maps_against_base_tree() {
        // Old-side zero-based 21..=25 was inside base Legacy (20-28).
        let del = hunk(22, 5, 18, 0);
        let maps = map_changes_with_base(&tree(), Some(&base_tree()), &[del]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped)
        );
        // Target id refers to the *base* tree.
        assert_eq!(maps[0].targets, vec![SymbolId::new("1")]);
    }

    #[test]
    fn pure_deletion_spanning_base_symbols_lists_all() {
        // Old-side zero-based 10..=25 intersects base main (5-15) and Legacy (20-28).
        let del = hunk(11, 15, 9, 0);
        let maps = map_changes_with_base(&tree(), Some(&base_tree()), &[del]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped)
        );
        assert_eq!(maps[0].targets, vec![SymbolId::new("0"), SymbolId::new("1")]);
    }

    #[test]
    fn pure_deletion_in_base_gap_attaches_to_nearest_base_symbol() {
        // Old-side zero-based 17..=18: gap in base; Legacy starts at 20 (2 below).
        let del = hunk(18, 2, 16, 0);
        let maps = map_changes_with_base(&tree(), Some(&base_tree()), &[del]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped)
        );
        assert_eq!(maps[0].targets, vec![SymbolId::new("1")]);
    }

    #[test]
    fn pure_deletion_without_base_attaches_to_surviving_container() {
        // Insertion point zero-based 10 sits inside worktree main (5-15).
        let del = hunk(11, 2, 10, 0);
        let maps = map_changes(&tree(), &[del]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::DocCommentOrGap)
        );
        assert_eq!(maps[0].targets, vec![SymbolId::new("0")]);
    }

    #[test]
    fn pure_deletion_without_base_far_from_symbols_is_unmapped() {
        // Insertion point zero-based 35: >3 lines from Greeter (ends 30) and Hello (starts 40).
        let del = hunk(36, 2, 35, 0);
        let maps = map_changes(&tree(), &[del]);
        assert_eq!(maps[0].confidence, MappingConfidence::Unmapped);
    }

    #[test]
    fn hunk_spanning_two_symbols_lists_all_intersected() {
        // Zero-based 10..=25 intersects main (5-15) and Greeter (20-30), covers neither.
        let maps = map_changes(&tree(), &[hunk(11, 16, 11, 16)]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::HunkSpansSymbols)
        );
        assert_eq!(maps[0].targets, vec![SymbolId::new("0"), SymbolId::new("1")]);
    }

    #[test]
    fn hunk_covering_whole_symbols_is_exact_multi_target() {
        // Whole-file-style addition: zero-based 0..=55 covers all three top-level symbols.
        let maps = map_changes(&tree(), &[hunk(0, 0, 1, 56)]);
        assert_eq!(maps[0].confidence, MappingConfidence::Exact);
        assert_eq!(
            maps[0].targets,
            vec![SymbolId::new("0"), SymbolId::new("1"), SymbolId::new("2")]
        );
    }

    #[test]
    fn hunk_covering_one_symbol_plus_doc_comment_is_exact() {
        // Zero-based 37..=51: covers Hello (40-50) entirely plus 3 gap lines above
        // (its doc comment) and one below.
        let maps = map_changes(&tree(), &[hunk(38, 15, 38, 15)]);
        assert_eq!(maps[0].confidence, MappingConfidence::Exact);
        assert_eq!(maps[0].targets, vec![SymbolId::new("2")]);
    }

    #[test]
    fn hunk_partially_overlapping_one_symbol_is_gap_approximate() {
        // Zero-based 14..=18: overlaps main's tail (ends 15) plus gap lines; does not
        // reach Greeter (starts 20).
        let maps = map_changes(&tree(), &[hunk(15, 5, 15, 5)]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::DocCommentOrGap)
        );
        assert_eq!(maps[0].targets, vec![SymbolId::new("0")]);
    }

    #[test]
    fn degenerate_empty_hunk_is_unmapped() {
        let maps = map_changes(&tree(), &[hunk(0, 0, 0, 0)]);
        assert_eq!(maps[0].confidence, MappingConfidence::Unmapped);
    }

    #[test]
    fn hunk_ids_carry_file_and_running_index() {
        let hunks = [hunk(9, 1, 9, 1), hunk(23, 1, 23, 1)];
        let maps = map_changes(&tree(), &hunks);
        assert_eq!(maps[0].hunk.index, 0);
        assert_eq!(maps[1].hunk.index, 1);
        assert!(maps.iter().all(|m| m.hunk.file == "main.go"));
    }

    #[test]
    fn empty_tree_maps_everything_unmapped() {
        let empty = SymbolTree::new(FileId::new("empty.go").unwrap(), Revision::Worktree, vec![]);
        let maps = map_changes(&empty, &[hunk(1, 1, 1, 1), hunk(5, 3, 4, 0)]);
        assert!(maps.iter().all(|m| m.confidence == MappingConfidence::Unmapped));
    }
}
