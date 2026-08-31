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
    use codescope_core::{ChangedSide, Revision};
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
            let mapped_revision = match run.side {
                ChangedSide::Old => Revision::Base,
                ChangedSide::New => Revision::Worktree,
            };
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

/// The (targets, confidence, signature-touching targets) for one changed run.
struct RunMapping {
    targets: Vec<SymbolId>,
    confidence: MappingConfidence,
    signature_touches: Vec<SymbolId>,
}

/// Extract maximal same-kind, consecutive-coordinate `Add`/`Del` runs from a hunk body.
/// `Context` only separates runs — it is never evidence. Coordinates are 1-based; the
/// ranges returned are zero-based inclusive on the run's own side.
fn changed_runs(hunk: &Hunk) -> Vec<ChangeRun> {
    use codescope_core::{ChangedSide, DiffLineKind};
    let mut runs: Vec<ChangeRun> = Vec::new();
    let mut cur: Option<ChangeRun> = None;
    let mut last_new = hunk.insertion_point_zero_based(); // 0-based new-side cursor
    for line in &hunk.lines {
        if let Some(nl) = line.new_ln {
            last_new = nl - 1; // track the surviving cursor through context/adds
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
            // Malformed body: a changed line without its coordinate. Fail closed — end the
            // run rather than guess a span.
            cur = None;
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

/// New-side (addition) run against the worktree tree.
fn map_run_worktree(tree: &SymbolTree, target: &LineRange) -> RunMapping {
    if let Some(sym) = tree.find_smallest_containing(target) {
        let sig = sym.selection.intersects_lines(target);
        return RunMapping {
            targets: vec![sym.id.clone()],
            confidence: MappingConfidence::Exact,
            signature_touches: if sig { vec![sym.id.clone()] } else { vec![] },
        };
    }
    let intersected: Vec<&SymbolNode> = tree
        .roots
        .iter()
        .filter(|n| n.range.intersects_lines(target))
        .collect();
    match intersected.len() {
        0 => map_gap(tree, target),
        1 => {
            let sym = intersected[0];
            if target.contains_lines(&sym.range) {
                let sig = sym.selection.intersects_lines(target);
                RunMapping {
                    targets: vec![sym.id.clone()],
                    confidence: MappingConfidence::Exact,
                    signature_touches: if sig { vec![sym.id.clone()] } else { vec![] },
                }
            } else {
                // Partial overlap hanging into a gap (typically the symbol plus its doc
                // comment, which the language server excludes from the range).
                RunMapping {
                    targets: vec![sym.id.clone()],
                    confidence: MappingConfidence::Approximate(ApproxReason::DocCommentOrGap),
                    signature_touches: vec![],
                }
            }
        }
        _ => {
            let targets: Vec<SymbolId> = intersected.iter().map(|n| n.id.clone()).collect();
            if intersected.iter().all(|n| target.contains_lines(&n.range)) {
                RunMapping {
                    targets,
                    confidence: MappingConfidence::Exact,
                    signature_touches: vec![],
                }
            } else {
                RunMapping {
                    targets,
                    confidence: MappingConfidence::Approximate(ApproxReason::HunkSpansSymbols),
                    signature_touches: vec![],
                }
            }
        }
    }
}

/// Old-side (deletion) run against the base tree; baseless falls back to the worktree.
fn map_run_base(tree: &SymbolTree, base: Option<&SymbolTree>, run: &ChangeRun) -> RunMapping {
    if let Some(base) = base {
        let target = &run.range;
        if let Some(sym) = base.find_smallest_containing(target) {
            let sig = sym.selection.intersects_lines(target);
            return RunMapping {
                targets: vec![sym.id.clone()],
                confidence: MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped),
                signature_touches: if sig { vec![sym.id.clone()] } else { vec![] },
            };
        }
        let intersected: Vec<SymbolId> = base
            .roots
            .iter()
            .filter(|n| n.range.intersects_lines(target))
            .map(|n| n.id.clone())
            .collect();
        if !intersected.is_empty() {
            return RunMapping {
                targets: intersected,
                confidence: MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped),
                signature_touches: vec![],
            };
        }
        if let Some(sym) = nearest_within(base, target, GAP_ATTACH_LINES) {
            return RunMapping {
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
            targets: vec![sym.id.clone()],
            confidence: MappingConfidence::Approximate(ApproxReason::DocCommentOrGap),
            signature_touches: vec![],
        },
        None => RunMapping {
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
        // Zero-based 10..=25 intersects main (5-15) and Greeter (20-30), covers neither.
        let maps = map_changes(&tree(), &[hunk(11, 16, 11, 16)]);
        assert_eq!(
            maps[0].confidence,
            MappingConfidence::Approximate(ApproxReason::HunkSpansSymbols)
        );
        assert_eq!(
            maps[0].targets,
            vec![SymbolId::new("0"), SymbolId::new("1")]
        );
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
        assert!(maps
            .iter()
            .all(|m| m.confidence == MappingConfidence::Unmapped));
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
        assert!(maps
            .iter()
            .all(|m| m.confidence == MappingConfidence::Exact));
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
