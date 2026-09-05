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
    /// The domain mapping record (hunk id, run index, side, range, targets, confidence).
    pub mapping: codescope_core::HunkMapping,
    /// The targets whose *selection* (identifier line) this run's changed lines intersect
    /// — a signature-ish change. Per-target so one hunk can touch A's signature and B's
    /// body without conflating them. Empty when no target's signature is touched.
    pub signature_touches: Vec<SymbolId>,
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
    let mut out: Vec<MappedHunk> = Vec::new();
    for (index, hunk) in hunks.iter().enumerate() {
        let hunk_id = HunkId {
            file: file.clone(),
            index: index as u32,
        };
        for (run_index, run) in changed_runs(hunk).into_iter().enumerate() {
            let mapped = map_run(tree, base, &run);
            let mut signature_touches: Vec<SymbolId> = mapped.signature_touches.clone();
            signature_touches.sort();
            signature_touches.dedup();
            let mapped_revision = mapped.revision;
            tracing::trace!(
                hunk = %hunk_id,
                run = run_index,
                side = ?run.side,
                targets = mapped.targets.len(),
                "mapped changed run"
            );
            out.push(MappedHunk {
                mapping: codescope_core::HunkMapping {
                    hunk: hunk_id.clone(),
                    run_index: run_index as u32,
                    side: run.side,
                    range: run.range,
                    mapped_revision,
                    targets: mapped.targets,
                    confidence: mapped.confidence,
                },
                signature_touches,
            });
        }
    }
    out
}

/// A maximal run of consecutive changed (Add or Del) lines on one side of a hunk.
struct ChangeRun {
    side: codescope_core::ChangedSide,
    range: LineRange,
    /// New-side cursor at the run's start (the last new_ln seen before the run, or the
    /// hunk's insertion point). Used only for baseless deletion anchoring.
    anchor_new: u32,
}

/// The (targets, confidence, signature-touching targets, target namespace) for one
/// changed run. `revision` is the tree the targets actually resolve against: `Base` for a
/// base-mapped deletion, `Worktree` for additions AND for a baseless-deletion fallback to
/// a surviving worktree symbol (review 22 M1: never label a worktree target as Base).
struct RunMapping {
    targets: Vec<SymbolId>,
    confidence: MappingConfidence,
    signature_touches: Vec<SymbolId>,
    revision: codescope_core::Revision,
}

/// Extract maximal same-kind, consecutive-coordinate `Add`/`Del` runs from a hunk body.
/// `Context` only separates runs — it is never evidence. Coordinates are 1-based; the
/// ranges returned are zero-based inclusive on the run's own side.
fn changed_runs(hunk: &Hunk) -> Vec<ChangeRun> {
    use codescope_core::{ChangedSide, DiffLineKind};
    let mut runs: Vec<ChangeRun> = Vec::new();
    let mut cur: Option<ChangeRun> = None;
    // The next new-side index where a deletion would be inserted. For a pure deletion
    // (new_len == 0) git's new_start IS the insertion line (0-based after the removed
    // content); for a hunk with a nonempty new side, the cursor before the first body line
    // is new_start - 1. After a Context/Add at 1-based N it is N (the NEXT slot)
    // (review 23 M2).
    let mut last_new = if hunk.new_len == 0 {
        hunk.new_start
    } else {
        hunk.new_start.saturating_sub(1)
    };
    for line in &hunk.lines {
        if let Some(nl) = line.new_ln {
            last_new = nl; // consumed new-side line N (1-based) -> next insertion slot N (0-based)
        }
        let (side, coord) = match line.kind {
            DiffLineKind::Add => (ChangedSide::New, line.new_ln),
            DiffLineKind::Del => (ChangedSide::Old, line.old_ln),
            DiffLineKind::Context => {
                if let Some(r) = cur.take() {
                    runs.push(r);
                }
                continue;
            }
        };
        let Some(coord) = coord else {
            // Malformed body: a changed line without its coordinate. Fail closed — but
            // FLUSH the valid run accumulated so far instead of discarding it (review 22).
            if let Some(r) = cur.take() {
                runs.push(r);
            }
            continue;
        };
        let zl = coord - 1; // 1-based -> 0-based
        match &mut cur {
            Some(r) if r.side == side && r.range.end_line + 1 == zl => {
                r.range.end_line = zl;
            }
            _ => {
                if let Some(r) = cur.take() {
                    runs.push(r);
                }
                cur = Some(ChangeRun {
                    side,
                    range: LineRange::from_line_span(zl, zl),
                    anchor_new: last_new,
                });
            }
        }
    }
    if let Some(r) = cur {
        runs.push(r);
    }
    runs
}

/// Map one changed run against the tree for its side.
fn map_run(tree: &SymbolTree, base: Option<&SymbolTree>, run: &ChangeRun) -> RunMapping {
    use codescope_core::ChangedSide;
    match run.side {
        ChangedSide::New => map_run_worktree(tree, &run.range),
        ChangedSide::Old => map_run_base(tree, base, run),
    }
}

/// The minimal set of deepest symbols covering `target` (the semantic frontier): recurse
/// into each child that intersects the run; add the node itself only for the lines it owns
/// that NO child covers (its own declaration, or body between/around children). A change
/// touching two sibling fields maps to both fields, not their parent; a change to a
/// struct's own declaration maps to the struct (review 22 M2).
fn deepest_frontier<'t>(node: &'t SymbolNode, target: &LineRange, out: &mut Vec<&'t SymbolNode>) {
    if !node.range.intersects_lines(target) {
        return;
    }
    let mut child_covers = false;
    for child in &node.children {
        if child.range.intersects_lines(target) {
            deepest_frontier(child, target, out);
            child_covers = true;
        }
    }
    // The node owns a line the children don't when the run reaches its own region: either
    // it has no intersecting child at all, or its own declaration/selection is touched, or
    // the run spans lines outside every child (a gap the parent owns). We approximate the
    // last with: the run is not fully covered by the union of intersecting children.
    if !child_covers {
        out.push(node);
        return;
    }
    // The parent owns the run (not its children) when its own declaration is touched, or
    // when the run is not fully explained by the intersecting children. A run spanning a
    // child boundary (start before one child or end after another) reaches parent-owned
    // body, so the parent is the target — not the children (which would over-split one
    // edit and mislabel a genuinely-new sibling, review 22 M2).
    let own_decl_touched = node.selection.intersects_lines(target);
    let mut uncovered = false;
    if !own_decl_touched {
        // The parent owns a changed line that no child covers (a gap in its own body).
        // Merge the intersecting children's line intervals over the run and look for an
        // uncovered line in `target ∩ node.range` (review 23 M3).
        let lo = node.range.start_line.max(target.start_line);
        let hi = node.range.end_line.min(target.end_line);
        let mut cursor = lo;
        let mut intervals: Vec<&LineRange> = node
            .children
            .iter()
            .filter(|c| c.range.intersects_lines(target))
            .map(|c| &c.range)
            .collect();
        intervals.sort_by_key(|r| r.start_line);
        for iv in intervals {
            let s = iv.start_line.max(lo);
            let e = iv.end_line.min(hi);
            if s > cursor {
                uncovered = true; // a gap before this child belongs to the parent
                break;
            }
            if e >= cursor {
                cursor = e + 1;
            }
        }
        if cursor <= hi {
            uncovered = true; // trailing lines after the last child belong to the parent
        }
    }
    if own_decl_touched || uncovered {
        out.push(node);
    }
}

/// New-side (addition) run against the worktree tree.
fn map_run_worktree(tree: &SymbolTree, target: &LineRange) -> RunMapping {
    // Deepest semantic frontier across all roots: siblings map to themselves, parents
    // only for their own declaration/body evidence.
    let mut frontier: Vec<&SymbolNode> = Vec::new();
    for root in &tree.roots {
        deepest_frontier(root, target, &mut frontier);
    }
    if !frontier.is_empty() {
        let targets: Vec<SymbolId> = frontier.iter().map(|n| n.id.clone()).collect();
        let touches: Vec<SymbolId> = frontier
            .iter()
            .filter(|n| n.selection.intersects_lines(target))
            .map(|n| n.id.clone())
            .collect();
        // Exact when every target's range is covered by the run (a real edit of those
        // symbols); HunkSpansSymbols when one run genuinely crosses several symbols without
        // covering them; DocCommentOrGap when the run hangs into a gap around one symbol.
        let exact = frontier.iter().all(|n| target.contains_lines(&n.range))
            || frontier.len() == 1 && tree.find_smallest_containing(target).is_some();
        let confidence = if exact {
            MappingConfidence::Exact
        } else if frontier.len() > 1 {
            MappingConfidence::Approximate(ApproxReason::HunkSpansSymbols)
        } else {
            MappingConfidence::Approximate(ApproxReason::DocCommentOrGap)
        };
        return RunMapping {
            revision: codescope_core::Revision::Worktree,
            targets,
            confidence,
            signature_touches: touches,
        };
    }
    // No symbol intersects the run: a gap (doc comment, import block, between symbols).
    map_gap(tree, target)
}

/// Old-side (deletion) run against the base tree; baseless falls back to the worktree.
fn map_run_base(tree: &SymbolTree, base: Option<&SymbolTree>, run: &ChangeRun) -> RunMapping {
    if let Some(base) = base {
        let target = &run.range;
        // Deepest semantic frontier on the BASE tree (review 23 M1): a deletion spanning
        // sibling fields maps to the fields, not their parent — mirroring the worktree path.
        let mut frontier: Vec<&SymbolNode> = Vec::new();
        for root in &base.roots {
            deepest_frontier(root, target, &mut frontier);
        }
        if !frontier.is_empty() {
            let touches: Vec<SymbolId> = frontier
                .iter()
                .filter(|n| n.selection.intersects_lines(target))
                .map(|n| n.id.clone())
                .collect();
            return RunMapping {
                revision: codescope_core::Revision::Base,
                targets: frontier.iter().map(|n| n.id.clone()).collect(),
                confidence: MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped),
                signature_touches: touches,
            };
        }
        if let Some(sym) = nearest_within(base, target, GAP_ATTACH_LINES) {
            return RunMapping {
                revision: codescope_core::Revision::Base,
                targets: vec![sym.id.clone()],
                confidence: MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped),
                signature_touches: vec![],
            };
        }
    }
    // No base tree (or nothing mapped): attach to the nearest surviving symbol around the
    // run's own insertion anchor (not the whole hunk's), staying approximate.
    let point = run.anchor_new;
    let target = LineRange::from_line_span(point, point);
    if let Some(sym) = tree.find_smallest_containing(&target) {
        return RunMapping {
            revision: codescope_core::Revision::Worktree,
            targets: vec![sym.id.clone()],
            confidence: MappingConfidence::Approximate(ApproxReason::DocCommentOrGap),
            signature_touches: vec![],
        };
    }
    map_gap(tree, &target)
}

/// Gap change (no symbol intersected): nearest symbol within [`GAP_ATTACH_LINES`] lines,
/// preferring the symbol *below* (doc comments precede their symbol), else unmapped.
fn map_gap(tree: &SymbolTree, target: &LineRange) -> RunMapping {
    match nearest_within(tree, target, GAP_ATTACH_LINES) {
        Some(sym) => RunMapping {
            revision: codescope_core::Revision::Worktree,
            targets: vec![sym.id.clone()],
            confidence: MappingConfidence::Approximate(ApproxReason::DocCommentOrGap),
            signature_touches: vec![],
        },
        None => RunMapping {
            revision: codescope_core::Revision::Worktree,
            targets: Vec::new(),
            confidence: MappingConfidence::Unmapped,
            signature_touches: vec![],
        },
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
            vec![
                node("0", "main", 5, 15),
                greeter,
                node("2", "(Greeter).Hello", 40, 50),
            ],
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

    /// A modification hunk. `hunk(o, l, n, l)` models an in-place edit: the new side
    /// carries the changed lines (`new_len` adds from `new_start`), and the old side is
    /// present only as the header envelope. This matches how the legacy fixtures were
    /// written (they asserted the new-side mapping). Use [`add_hunk`]/[`del_hunk`] for
    /// pure one-sided edits.
    fn hunk(old_start: u32, old_len: u32, new_start: u32, new_len: u32) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            section: None,
            lines: body(old_start, 0, new_start, new_len),
        }
    }

    /// A pure-deletion hunk body: `old_len` deleted lines starting at `old_start` (1-based).
    fn del_hunk(old_start: u32, old_len: u32, new_start: u32) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start,
            new_len: 0,
            section: None,
            lines: body(old_start, old_len, new_start, 0),
        }
    }

    /// A hunk body with explicit lines (for context/disjoint-edit fixtures).
    fn hunk_with_lines(
        old_start: u32,
        old_len: u32,
        new_start: u32,
        new_len: u32,
        lines: Vec<codescope_core::DiffLine>,
    ) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            section: None,
            lines,
        }
    }

    fn ctx(old_ln: u32, new_ln: u32) -> codescope_core::DiffLine {
        codescope_core::DiffLine {
            kind: codescope_core::DiffLineKind::Context,
            old_ln: Some(old_ln),
            new_ln: Some(new_ln),
            text: String::new(),
        }
    }

    fn add(new_ln: u32) -> codescope_core::DiffLine {
        codescope_core::DiffLine {
            kind: codescope_core::DiffLineKind::Add,
            old_ln: None,
            new_ln: Some(new_ln),
            text: String::new(),
        }
    }

    fn del(old_ln: u32) -> codescope_core::DiffLine {
        codescope_core::DiffLine {
            kind: codescope_core::DiffLineKind::Del,
            old_ln: Some(old_ln),
            new_ln: None,
            text: String::new(),
        }
    }

    /// Build the body: `old_len` deleted lines (old side from `old_start`) then `new_len`
    /// added lines (new side from `new_start`). Coordinates are 1-based.
    fn body(
        old_start: u32,
        old_len: u32,
        new_start: u32,
        new_len: u32,
    ) -> Vec<codescope_core::DiffLine> {
        use codescope_core::{DiffLine, DiffLineKind};
        let mut lines = Vec::new();
        for i in 0..old_len {
            lines.push(DiffLine {
                kind: DiffLineKind::Del,
                old_ln: Some(old_start + i),
                new_ln: None,
                text: String::new(),
            });
        }
        for i in 0..new_len {
            lines.push(DiffLine {
                kind: DiffLineKind::Add,
                old_ln: None,
                new_ln: Some(new_start + i),
                text: String::new(),
            });
        }
        lines
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
        assert!(detailed[0].signature_touches.is_empty());
    }

    #[test]
    fn signature_touch_stays_exact_but_is_noted() {
        // Zero-based line 5 is main's selection line; 1-based new_start = 6.
        let detailed = map_changes_detailed(&tree(), None, &[hunk(6, 2, 6, 2)]);
        assert_eq!(detailed[0].mapping.confidence, MappingConfidence::Exact);
        assert_eq!(detailed[0].mapping.targets, vec![SymbolId::new("0")]);
        assert_eq!(detailed[0].signature_touches, vec![SymbolId::new("0")]);
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
        let del = del_hunk(22, 5, 18);
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
        let del = del_hunk(11, 15, 9);
        let maps = map_changes_with_base(&tree(), Some(&base_tree()), &[del]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped)
        );
        assert_eq!(
            maps[0].targets,
            vec![SymbolId::new("0"), SymbolId::new("1")]
        );
    }

    #[test]
    fn pure_deletion_in_base_gap_attaches_to_nearest_base_symbol() {
        // Old-side zero-based 17..=18: gap in base; Legacy starts at 20 (2 below).
        let del = del_hunk(18, 2, 16);
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
        let del = del_hunk(11, 2, 10);
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
        let del = del_hunk(36, 2, 35);
        let maps = map_changes(&tree(), &[del]);
        assert_eq!(maps[0].confidence, MappingConfidence::Unmapped);
    }

    #[test]
    fn hunk_spanning_two_symbols_lists_all_intersected() {
        // Zero-based 10..=25 intersects main (5-15), Greeter (20-30) and its field
        // (1/0, line 22), covering none fully. The deepest frontier surfaces the nested
        // field alongside the two top-level symbols (review 20: children are real targets).
        let maps = map_changes(&tree(), &[hunk(11, 16, 11, 16)]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::HunkSpansSymbols)
        );
        assert_eq!(
            maps[0].targets,
            vec![SymbolId::new("0"), SymbolId::new("1/0"), SymbolId::new("1")]
        );
    }

    #[test]
    fn hunk_covering_whole_symbols_is_exact_multi_target() {
        // Whole-file-style addition: zero-based 0..=55 covers all symbols. The frontier
        // includes Greeter's nested field (1/0) as a real addition target.
        let maps = map_changes(&tree(), &[hunk(0, 0, 1, 56)]);
        assert_eq!(maps[0].confidence, MappingConfidence::Exact);
        assert_eq!(
            maps[0].targets,
            vec![
                SymbolId::new("0"),
                SymbolId::new("1/0"),
                SymbolId::new("1"),
                SymbolId::new("2")
            ]
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
    fn degenerate_empty_hunk_produces_no_mapping() {
        // A body with no Add/Del lines carries no changed-run evidence (review 20): it
        // emits no mapping record at all rather than an Unmapped one.
        let maps = map_changes(&tree(), &[del_hunk(0, 0, 0)]);
        assert!(maps.is_empty());
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
        let maps = map_changes(&empty, &[hunk(1, 1, 1, 1), del_hunk(5, 3, 4)]);
        assert!(
            maps.iter()
                .all(|m| m.confidence == MappingConfidence::Unmapped)
        );
    }

    /// Review 20: context lines are never evidence. An Add run surrounded by context maps
    /// ONLY to the symbol the added lines touch — the neighboring symbol whose tail appears
    /// as leading context is NOT reported.
    #[test]
    fn context_neighboring_symbols_are_not_mapped() {
        // main 5-15, Greeter 20-30. The edit adds lines 18..=19 (in the gap). The hunk's
        // leading context is main's tail (lines 16,17) and trailing context is Greeter's
        // head (20,21). Old code mapped the whole envelope (16..=21) and would report both
        // main and Greeter; only the gap attachment is honest.
        let h = hunk_with_lines(
            16,
            6,
            16,
            8,
            vec![
                ctx(16, 16),
                ctx(17, 17),
                add(18),
                add(19),
                ctx(18, 20),
                ctx(19, 21),
            ],
        );
        let maps = map_changes(&tree(), &[h]);
        assert_eq!(maps.len(), 1, "one add run, one mapping");
        // The gap between main (ends 15) and Greeter (starts 20): nearest is Greeter below.
        assert_eq!(maps[0].targets, vec![SymbolId::new("1")]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::DocCommentOrGap)
        );
        // Neither main (0) nor the trailing-context Greeter head is reported as touched.
        assert!(!maps[0].targets.contains(&SymbolId::new("0")));
    }

    /// Review 20: two disjoint edits inside ONE git hunk map independently.
    #[test]
    fn disjoint_edits_in_one_hunk_map_independently() {
        // main 5-15, (Greeter).Hello 40-50. Edit main (new 10) AND Hello (new 45) with a
        // big context gap between them in one hunk.
        let h = hunk_with_lines(
            10,
            40,
            10,
            42,
            vec![
                add(10), // edit in main (0-based 9)
                ctx(11, 11),
                ctx(12, 12),
                add(45), // edit in Hello (0-based 44)
            ],
        );
        let maps = map_changes(&tree(), &[h]);
        assert_eq!(maps.len(), 2, "two add runs, two mappings");
        assert_eq!(maps[0].targets, vec![SymbolId::new("0")]);
        assert_eq!(maps[0].run_index, 0);
        assert_eq!(maps[1].targets, vec![SymbolId::new("2")]);
        assert_eq!(maps[1].run_index, 1);
        assert!(
            maps.iter()
                .all(|m| m.confidence == MappingConfidence::Exact)
        );
    }

    /// Review 20: a replacement (Del run + Add run) maps both sides; the deleted base
    /// symbol folds onto the surviving worktree symbol at aggregation.
    #[test]
    fn replacement_maps_both_sides_of_the_run() {
        let h = hunk_with_lines(10, 3, 10, 3, vec![del(10), del(11), add(10), add(11)]);
        let maps = map_changes_detailed(&tree(), Some(&base_tree()), &[h]);
        assert_eq!(maps.len(), 2, "one old-side run + one new-side run");
        assert_eq!(maps[0].mapping.side, codescope_core::ChangedSide::Old);
        assert_eq!(
            maps[0].mapping.mapped_revision,
            codescope_core::Revision::Base
        );
        assert_eq!(maps[1].mapping.side, codescope_core::ChangedSide::New);
        assert_eq!(
            maps[1].mapping.mapped_revision,
            codescope_core::Revision::Worktree
        );
        assert_eq!(maps[1].mapping.targets, vec![SymbolId::new("0")]);
        assert_eq!(maps[1].mapping.confidence, MappingConfidence::Exact);
    }
}
