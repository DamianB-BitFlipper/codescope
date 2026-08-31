//! Per-symbol change aggregation: hunk mappings → [`ChangedSymbol`]s (research 03).
//!
//! [`changed_symbols`] combines the pure mapper output with a base-vs-worktree tree diff:
//!
//! - hunk targets in the worktree tree become [`ChangeKind::Modified`] (or
//!   [`ChangeKind::Added`] when the symbol is new / the whole file is new);
//! - base-mapped deletion targets become [`ChangeKind::Deleted`] — unless a symbol with
//!   the same `(qualified name, kind)` still exists in the worktree, in which case the
//!   deletion is folded into that surviving symbol as a modification;
//! - symbols present in only one tree (diffed by qualified name + kind) are emitted even
//!   when no hunk mapped to them directly (research 03 "diff-of-symbols" scope).
//!
//! Confidence merges are worst-wins: `Exact < Approximate < Unmapped`.

use std::collections::HashMap;

use codescope_core::{
    ApproxReason, ChangeKind, ChangedSymbol, FileChange, FileId, FileStatus, HunkId, LineRange,
    MappingConfidence, Revision, SymbolId, SymbolKind, SymbolNode, SymbolTree,
};

use crate::mapper::map_changes_detailed;

/// A changed symbol resolved against its tree: everything downstream layers (impact
/// graph, digest, UI) need without re-walking trees.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChangedSymbolInfo {
    /// Repo-relative file (the change's current path; for deleted files, the old path).
    pub file: FileId,
    /// Qualified symbol name within the file (`Greeter.Name` for nested symbols; Go
    /// methods keep their receiver form `(Greeter).Hello`).
    pub name: String,
    /// Symbol kind.
    pub kind: SymbolKind,
    /// Symbol detail (signature) when the producer supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Full extent in the revision named by [`ChangedSymbolInfo::revision`].
    pub range: LineRange,
    /// Identifier-only range in that revision.
    pub selection: LineRange,
    /// Which tree the ranges (and [`ChangedSymbol::symbol`] id) belong to:
    /// [`Revision::Base`] for deleted symbols, the worktree revision otherwise.
    pub revision: Revision,
    /// The domain record (symbol id, change kind, hunks, worst confidence).
    pub record: ChangedSymbol,
    /// `true` when some exactly-mapped hunk intersected the symbol's selection range —
    /// a signature-ish change (research 03: noted, still exact).
    pub signature_touch: bool,
}

/// Compute per-symbol changes for one file of a change-set.
///
/// `worktree` is the current-content tree (`None` for deleted files); `base` is the
/// base-revision overlay tree (`None` when unavailable — deletions then degrade per the
/// mapper's fallback). See the module docs for the aggregation rules.
#[must_use]
pub fn changed_symbols(
    worktree: Option<&SymbolTree>,
    base: Option<&SymbolTree>,
    change: &FileChange,
) -> Vec<ChangedSymbol> {
    changed_symbols_detailed(worktree, base, change)
        .into_iter()
        .map(|info| info.record)
        .collect()
}

/// Map one file's hunks with the same tree fallbacks [`changed_symbols`] uses (a deleted
/// file maps against an empty worktree tree). Exposed for orchestration layers that want
/// the per-hunk mappings alongside the per-symbol aggregation.
#[must_use]
pub fn file_mappings(
    worktree: Option<&SymbolTree>,
    base: Option<&SymbolTree>,
    change: &FileChange,
) -> Vec<crate::mapper::MappedHunk> {
    let empty_worktree;
    let wt = match worktree {
        Some(t) => t,
        None => {
            empty_worktree = empty_tree_for(change);
            &empty_worktree
        }
    };
    map_changes_detailed(wt, base, &change.hunks)
}

fn empty_tree_for(change: &FileChange) -> SymbolTree {
    SymbolTree::new(
        FileId::new_unchecked(change.path.clone()),
        Revision::Worktree,
        Vec::new(),
    )
}

/// [`changed_symbols`] with tree-resolved detail per symbol.
#[must_use]
pub fn changed_symbols_detailed(
    worktree: Option<&SymbolTree>,
    base: Option<&SymbolTree>,
    change: &FileChange,
) -> Vec<ChangedSymbolInfo> {
    // The mapper needs a worktree tree; a deleted file has an empty one.
    let empty_worktree;
    let wt = match worktree {
        Some(t) => t,
        None => {
            empty_worktree = empty_tree_for(change);
            &empty_worktree
        }
    };

    let wt_keys = index_tree(wt);
    let base_keys = base.map(index_tree);
    let whole_file_added = matches!(change.status, FileStatus::Added | FileStatus::Untracked);

    let mut agg = Aggregator::default();
    let mapped = map_changes_detailed(wt, base, &change.hunks);

    for m in &mapped {
        for target in &m.mapping.targets {
            // The run's mapped revision, not its confidence, selects the tree namespace
            // (review 20: DeletedHunkBaseMapped was an ambiguous proxy).
            let base_mapped = m.mapping.mapped_revision == codescope_core::Revision::Base;
            if base_mapped {
                let Some(base_tree) = base else { continue };
                aggregate_base_target(
                    &mut agg,
                    base_tree,
                    wt,
                    &wt_keys,
                    target,
                    &m.mapping.hunk,
                    m.mapping.confidence,
                );
            } else {
                let kind =
                    worktree_change_kind(&wt_keys, base_keys.as_ref(), target, whole_file_added);
                agg.record(
                    TreeSide::Worktree,
                    target.clone(),
                    kind,
                    Some(&m.mapping.hunk),
                    m.mapping.confidence,
                    m.signature_touches.contains(target),
                );
            }
        }
    }

    // Tree-diff sweep: symbols present in exactly one tree, even when no hunk mapped to
    // them directly (e.g. a new field whose hunk mapped to the enclosing struct).
    if let Some(base_keys) = &base_keys {
        // Record every worktree-only symbol as Added (review 22 M5: an ancestor that only
        // exists on the worktree side IS a real addition — its key is absent from base
        // precisely because it was added, so it must not be suppressed). A parent whose
        // own declaration changed maps via its hunk earlier; the sweep catches the rest.
        for (key, id) in ordered_keys(wt) {
            if !base_keys.contains_key(&key) {
                agg.record_if_absent(
                    TreeSide::Worktree,
                    id,
                    ChangeKind::Added,
                    MappingConfidence::Exact,
                );
            }
        }
        if let Some(base_tree) = base {
            for (key, id) in ordered_keys(base_tree) {
                if !wt_keys.contains_key(&key) {
                    agg.record_if_absent(
                        TreeSide::Base,
                        id,
                        ChangeKind::Deleted,
                        MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped),
                    );
                }
            }
        }
    } else if whole_file_added {
        // New/untracked file without a base tree: every symbol is added (research 03).
        for (_, id) in ordered_keys(wt) {
            agg.record_if_absent(
                TreeSide::Worktree,
                id,
                ChangeKind::Added,
                MappingConfidence::Exact,
            );
        }
    }

    let out = agg.finish(wt, base, change);
    tracing::debug!(
        file = %change.path,
        symbols = out.len(),
        hunks = change.hunks.len(),
        "aggregated changed symbols"
    );
    out
}

/// Which tree a recorded symbol id belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TreeSide {
    Worktree,
    Base,
}

/// Fold a base-mapped deletion target into the aggregate: onto the surviving worktree
/// symbol with the same key when one exists (a modification), else as a deleted symbol.
fn aggregate_base_target(
    agg: &mut Aggregator,
    base_tree: &SymbolTree,
    wt: &SymbolTree,
    wt_keys: &HashMap<SymbolKey, SymbolId>,
    target: &SymbolId,
    hunk: &HunkId,
    confidence: MappingConfidence,
) {
    let survivor = find_key_of(base_tree, target).and_then(|key| wt_keys.get(&key));
    match survivor {
        Some(wt_id) => {
            // The symbol still exists: the deletion modified it.
            debug_assert!(find_by_id(wt, wt_id).is_some());
            agg.record(
                TreeSide::Worktree,
                wt_id.clone(),
                ChangeKind::Modified,
                Some(hunk),
                confidence,
                false,
            );
        }
        None => {
            agg.record(
                TreeSide::Base,
                target.clone(),
                ChangeKind::Deleted,
                Some(hunk),
                confidence,
                false,
            );
        }
    }
}

/// Change kind for a worktree-mapped target: added when the whole file is new or the
/// symbol's key is absent from the base tree; modified otherwise.
fn worktree_change_kind(
    wt_keys: &HashMap<SymbolKey, SymbolId>,
    base_keys: Option<&HashMap<SymbolKey, SymbolId>>,
    target: &SymbolId,
    whole_file_added: bool,
) -> ChangeKind {
    if whole_file_added {
        return ChangeKind::Added;
    }
    let Some(base_keys) = base_keys else {
        return ChangeKind::Modified;
    };
    let key = wt_keys.iter().find_map(|(k, v)| (v == target).then_some(k));
    match key {
        Some(k) if !base_keys.contains_key(k) => ChangeKind::Added,
        _ => ChangeKind::Modified,
    }
}

/// `(qualified name, kind)` identity used to match symbols across revisions.
type SymbolKey = (String, SymbolKind);

/// Index a tree by symbol key (first occurrence wins on the rare duplicate key).
fn index_tree(tree: &SymbolTree) -> HashMap<SymbolKey, SymbolId> {
    let mut map = HashMap::new();
    for (key, id) in ordered_keys(tree) {
        map.entry(key).or_insert(id);
    }
    map
}

/// `(key, id)` pairs in document order, with qualified names (`Parent.Child`).
fn ordered_keys(tree: &SymbolTree) -> Vec<(SymbolKey, SymbolId)> {
    fn walk(node: &SymbolNode, prefix: &str, out: &mut Vec<(SymbolKey, SymbolId)>) {
        let qualified = if prefix.is_empty() {
            node.name.clone()
        } else {
            format!("{prefix}.{}", node.name)
        };
        out.push(((qualified.clone(), node.kind), node.id.clone()));
        for child in &node.children {
            walk(child, &qualified, out);
        }
    }
    let mut out = Vec::new();
    for root in &tree.roots {
        walk(root, "", &mut out);
    }
    out
}

/// The key of the node with `id` in `tree`, if present.
fn find_key_of(tree: &SymbolTree, id: &SymbolId) -> Option<SymbolKey> {
    ordered_keys(tree)
        .into_iter()
        .find_map(|(key, node_id)| (node_id == *id).then_some(key))
}

/// The node with `id` in `tree`, if present.
#[must_use]
pub fn find_by_id<'t>(tree: &'t SymbolTree, id: &SymbolId) -> Option<&'t SymbolNode> {
    tree.iter().find(|n| n.id == *id)
}

/// Qualified name (`Parent.Child`) of the node with `id` in `tree`, if present.
#[must_use]
pub fn qualified_name(tree: &SymbolTree, id: &SymbolId) -> Option<String> {
    find_key_of(tree, id).map(|(name, _)| name)
}

/// Rank for worst-wins confidence merging.
fn confidence_rank(c: MappingConfidence) -> u8 {
    match c {
        MappingConfidence::Exact => 0,
        MappingConfidence::Approximate(_) => 1,
        MappingConfidence::Unmapped => 2,
    }
}

/// Order-preserving per-symbol accumulator.
#[derive(Default)]
struct Aggregator {
    order: Vec<(TreeSide, SymbolId)>,
    entries: HashMap<(TreeSide, SymbolId), Entry>,
}

struct Entry {
    change_kind: ChangeKind,
    hunks: Vec<HunkId>,
    confidence: MappingConfidence,
    signature_touch: bool,
}

impl Aggregator {
    /// Record one hunk-target (or tree-diff) contribution for a symbol.
    fn record(
        &mut self,
        side: TreeSide,
        id: SymbolId,
        change_kind: ChangeKind,
        hunk: Option<&HunkId>,
        confidence: MappingConfidence,
        signature_touch: bool,
    ) {
        let key = (side, id);
        let entry = self.entries.entry(key.clone()).or_insert_with(|| {
            self.order.push(key);
            Entry {
                change_kind,
                hunks: Vec::new(),
                confidence,
                signature_touch: false,
            }
        });
        // Added/Deleted knowledge beats the Modified default.
        if entry.change_kind == ChangeKind::Modified && change_kind != ChangeKind::Modified {
            entry.change_kind = change_kind;
        }
        if confidence_rank(confidence) > confidence_rank(entry.confidence) {
            entry.confidence = confidence;
        }
        if let Some(h) = hunk {
            if !entry.hunks.contains(h) {
                entry.hunks.push(h.clone());
            }
        }
        entry.signature_touch |= signature_touch;
    }

    /// Record a tree-diff-derived symbol only when no hunk already touched it.
    fn record_if_absent(
        &mut self,
        side: TreeSide,
        id: SymbolId,
        change_kind: ChangeKind,
        confidence: MappingConfidence,
    ) {
        let key = (side, id);
        if self.entries.contains_key(&key) {
            // Upgrade the change kind if the tree diff knows better.
            if let Some(entry) = self.entries.get_mut(&key) {
                if entry.change_kind == ChangeKind::Modified {
                    entry.change_kind = change_kind;
                }
            }
            return;
        }
        self.record(side, key.1, change_kind, None, confidence, false);
    }

    /// Resolve accumulated entries against their trees, in insertion order.
    fn finish(
        self,
        wt: &SymbolTree,
        base: Option<&SymbolTree>,
        change: &FileChange,
    ) -> Vec<ChangedSymbolInfo> {
        let Aggregator { order, mut entries } = self;
        let mut out = Vec::with_capacity(order.len());
        for key in order {
            let Some(entry) = entries.remove(&key) else {
                continue;
            };
            let (side, id) = key;
            let (tree, revision) = match side {
                TreeSide::Worktree => (wt, wt.revision),
                TreeSide::Base => match base {
                    Some(b) => (b, Revision::Base),
                    None => continue,
                },
            };
            let Some(node) = find_by_id(tree, &id) else {
                tracing::warn!(symbol = %id, file = %change.path, "mapped symbol id missing from tree");
                continue;
            };
            let name = qualified_name(tree, &id).unwrap_or_else(|| node.name.clone());
            out.push(ChangedSymbolInfo {
                file: tree.file.clone(),
                name,
                kind: node.kind,
                detail: node.detail.clone(),
                range: node.range,
                selection: node.selection,
                revision,
                record: ChangedSymbol::new(id, entry.change_kind, entry.hunks, entry.confidence),
                signature_touch: entry.signature_touch,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use codescope_core::Hunk;

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

    /// Build the body: `old_len` deleted lines (old side from `old_start`) then `new_len`
    /// added lines (new side from `new_start`). Coordinates are 1-based.
    fn hunk_with(
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

    fn file_change(status: FileStatus, hunks: Vec<Hunk>) -> FileChange {
        FileChange {
            path: Utf8PathBuf::from("main.go"),
            old_path: None,
            status,
            hunks,
            binary: false,
        }
    }

    /// Worktree: main 5-15, Greeter{Name,Email} 20-30, (Greeter).Hello 40-50.
    fn worktree() -> SymbolTree {
        let mut greeter = node("1", "Greeter", 20, 30);
        greeter.kind = SymbolKind::Struct;
        greeter.children = vec![
            SymbolNode {
                kind: SymbolKind::Field,
                ..node("1/0", "Name", 22, 22)
            },
            SymbolNode {
                kind: SymbolKind::Field,
                ..node("1/1", "Email", 23, 23)
            },
        ];
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

    /// Base: main 5-15, Greeter{Name} 20-28, Legacy 32-38.
    fn base() -> SymbolTree {
        let mut greeter = node("1", "Greeter", 20, 28);
        greeter.kind = SymbolKind::Struct;
        greeter.children = vec![SymbolNode {
            kind: SymbolKind::Field,
            ..node("1/0", "Name", 22, 22)
        }];
        SymbolTree::new(
            FileId::new("main.go").unwrap(),
            Revision::Base,
            vec![
                node("0", "main", 5, 15),
                greeter,
                node("2", "Legacy", 32, 38),
            ],
        )
    }

    #[test]
    fn body_edit_is_modified_exact() {
        // Hunk inside main's body (zero-based 9..=10).
        let change = file_change(FileStatus::Modified, vec![hunk(10, 2, 10, 2)]);
        let out = changed_symbols(Some(&worktree()), Some(&base()), &change);
        let main = out.iter().find(|c| c.symbol == SymbolId::new("0")).unwrap();
        assert_eq!(main.change_kind, ChangeKind::Modified);
        assert_eq!(main.confidence, MappingConfidence::Exact);
        assert_eq!(main.hunks.len(), 1);
        assert_eq!(main.hunks[0].index, 0);
    }

    #[test]
    fn tree_diff_detects_added_field_and_deleted_symbol() {
        // Hunk covers both fields (zero-based 22..=23) → maps to Greeter (contains both);
        // Email is new vs base; Legacy exists only in base.
        let change = file_change(FileStatus::Modified, vec![hunk(23, 1, 23, 2)]);
        let out = changed_symbols_detailed(Some(&worktree()), Some(&base()), &change);

        let email = out.iter().find(|c| c.name == "Greeter.Email").unwrap();
        assert_eq!(email.record.change_kind, ChangeKind::Added);
        assert_eq!(email.record.confidence, MappingConfidence::Exact);
        assert_eq!(email.revision, Revision::Worktree);

        let legacy = out.iter().find(|c| c.name == "Legacy").unwrap();
        assert_eq!(legacy.record.change_kind, ChangeKind::Deleted);
        assert_eq!(legacy.revision, Revision::Base);
        assert_eq!(
            legacy.record.confidence,
            MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped)
        );
    }

    #[test]
    fn new_symbol_touched_by_hunk_is_added() {
        // Hunk inside (Greeter).Hello (zero-based 42..=44), absent from base.
        let change = file_change(FileStatus::Modified, vec![hunk(43, 0, 43, 3)]);
        let out = changed_symbols(Some(&worktree()), Some(&base()), &change);
        let hello = out.iter().find(|c| c.symbol == SymbolId::new("2")).unwrap();
        assert_eq!(hello.change_kind, ChangeKind::Added);
        assert_eq!(hello.confidence, MappingConfidence::Exact);
        assert_eq!(hello.hunks.len(), 1);
    }

    #[test]
    fn pure_deletion_of_base_only_symbol_is_deleted() {
        // Old-side zero-based 33..=37 inside base Legacy (32-38); gone from worktree.
        let change = file_change(FileStatus::Modified, vec![del_hunk(34, 5, 30)]);
        let out = changed_symbols_detailed(Some(&worktree()), Some(&base()), &change);
        let legacy = out.iter().find(|c| c.name == "Legacy").unwrap();
        assert_eq!(legacy.record.change_kind, ChangeKind::Deleted);
        assert_eq!(legacy.record.symbol, SymbolId::new("2")); // base-tree id
        assert_eq!(legacy.revision, Revision::Base);
        assert_eq!(legacy.record.hunks.len(), 1);
        assert_eq!(
            legacy.record.confidence,
            MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped)
        );
    }

    #[test]
    fn deletion_inside_surviving_symbol_folds_into_modification() {
        // Old-side zero-based 11..=12 inside base main; main survives in the worktree.
        let change = file_change(FileStatus::Modified, vec![del_hunk(12, 2, 11)]);
        let out = changed_symbols_detailed(Some(&worktree()), Some(&base()), &change);
        // main modified + tree-diff extras (Email/Hello added, Legacy deleted).
        assert_eq!(out.len(), 4);
        let main = out.iter().find(|c| c.name == "main").unwrap();
        assert_eq!(main.record.change_kind, ChangeKind::Modified);
        assert_eq!(main.revision, Revision::Worktree);
        assert_eq!(main.record.symbol, SymbolId::new("0"));
        assert_eq!(
            main.record.confidence,
            MappingConfidence::Approximate(ApproxReason::DeletedHunkBaseMapped)
        );
        assert_eq!(main.record.hunks.len(), 1);
    }

    #[test]
    fn worst_confidence_wins_across_hunks() {
        // Exact body hunk + gap hunk attaching approximately to the same symbol (main).
        let change = file_change(
            FileStatus::Modified,
            vec![hunk(10, 1, 10, 1), hunk(17, 1, 17, 1)], // zero-based 9 (inside), 16 (gap→above)
        );
        let out = changed_symbols(Some(&worktree()), None, &change);
        let main = out.iter().find(|c| c.symbol == SymbolId::new("0")).unwrap();
        assert_eq!(main.change_kind, ChangeKind::Modified);
        assert_eq!(
            main.confidence,
            MappingConfidence::Approximate(ApproxReason::DocCommentOrGap)
        );
        assert_eq!(main.hunks.len(), 2);
    }

    #[test]
    fn untracked_file_marks_every_symbol_added() {
        let change = file_change(FileStatus::Untracked, vec![]);
        let out = changed_symbols_detailed(Some(&worktree()), None, &change);
        assert_eq!(out.len(), 5); // main, Greeter, Name, Email, Hello
        assert!(out
            .iter()
            .all(|c| c.record.change_kind == ChangeKind::Added));
        assert!(out
            .iter()
            .all(|c| c.record.confidence == MappingConfidence::Exact));
        assert!(out.iter().all(|c| c.record.hunks.is_empty()));
    }

    #[test]
    fn added_file_with_hunk_marks_symbols_added_with_hunks() {
        // One whole-file hunk covering everything.
        let change = file_change(FileStatus::Added, vec![hunk(0, 0, 1, 60)]);
        let out = changed_symbols(Some(&worktree()), None, &change);
        let with_hunks: Vec<_> = out.iter().filter(|c| !c.hunks.is_empty()).collect();
        // The deepest frontier maps every covered symbol directly (the two fields too).
        assert_eq!(with_hunks.len(), 5);
        assert!(out.iter().all(|c| c.change_kind == ChangeKind::Added));
        assert_eq!(out.len(), 5); // three top-level symbols + the two fields
    }

    #[test]
    fn deleted_file_marks_every_base_symbol_deleted() {
        // Whole-file deletion: no worktree tree, one pure-deletion hunk.
        let change = FileChange {
            path: Utf8PathBuf::from("main.go"),
            old_path: None,
            status: FileStatus::Deleted,
            hunks: vec![del_hunk(1, 40, 0)],
            binary: false,
        };
        let out = changed_symbols_detailed(None, Some(&base()), &change);
        assert_eq!(out.len(), 4); // main, Greeter, Greeter.Name, Legacy
        assert!(out
            .iter()
            .all(|c| c.record.change_kind == ChangeKind::Deleted));
        assert!(out.iter().all(|c| c.revision == Revision::Base));
        // Symbols intersected by the deletion hunk carry it.
        let main = out.iter().find(|c| c.name == "main").unwrap();
        assert_eq!(main.record.hunks.len(), 1);
    }

    #[test]
    fn signature_touch_propagates_to_info() {
        // Hunk on main's selection line (zero-based 5).
        let change = file_change(FileStatus::Modified, vec![hunk(6, 1, 6, 1)]);
        let out = changed_symbols_detailed(Some(&worktree()), Some(&base()), &change);
        let main = out.iter().find(|c| c.name == "main").unwrap();
        assert!(main.signature_touch);
        assert_eq!(main.record.confidence, MappingConfidence::Exact);
    }

    #[test]
    fn no_hunks_no_status_hint_yields_only_tree_diff() {
        let change = file_change(FileStatus::Modified, vec![]);
        let out = changed_symbols_detailed(Some(&worktree()), Some(&base()), &change);
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Greeter.Email", "(Greeter).Hello", "Legacy"]);
    }

    #[test]
    fn helpers_resolve_ids_and_names() {
        let wt = worktree();
        assert_eq!(
            qualified_name(&wt, &SymbolId::new("1/1")).as_deref(),
            Some("Greeter.Email")
        );
        assert!(qualified_name(&wt, &SymbolId::new("9")).is_none());
        assert_eq!(find_by_id(&wt, &SymbolId::new("1/0")).unwrap().name, "Name");
    }

    /// Review 20: a symbol that appears only as hunk CONTEXT (unchanged lines around an
    /// edit) is NOT reported as changed. This is the over-reporting fix: the mapper maps
    /// changed-line runs, never the context-bearing hunk envelope.
    #[test]
    fn context_only_symbol_is_not_reported() {
        // Worktree + base both have main (5-15) and Helper (20-25). The edit adds one line
        // inside main (new 10); Helper appears only as the hunk's trailing context.
        let change = file_change(
            FileStatus::Modified,
            vec![hunk_with(
                10,
                5,
                10,
                6,
                vec![
                    codescope_core::DiffLine::add(10, ""),
                    codescope_core::DiffLine::context(11, 11, ""),
                    codescope_core::DiffLine::context(12, 12, ""),
                ],
            )],
        );
        let out = changed_symbols_detailed(Some(&worktree()), Some(&base()), &change);
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"main"), "the edited symbol is reported");
        assert!(
            !names.contains(&"Helper"),
            "a context-only symbol is NOT reported: {names:?}"
        );
    }

    /// Review 22 M1: a baseless deletion's worktree-fallback target is labeled Worktree,
    /// not Base — so it aggregates instead of being dropped as a phantom base id.
    #[test]
    fn baseless_deletion_aggregates_onto_worktree_survivor() {
        // Delete two lines inside main (old 10,11); NO base tree. The run's anchor (next
        // new-side slot) lands inside worktree main, folding the deletion onto it.
        let change = file_change(FileStatus::Modified, vec![del_hunk(10, 2, 12)]);
        let out = changed_symbols_detailed(Some(&worktree()), None, &change);
        let main = out.iter().find(|c| c.name == "main").unwrap();
        assert_eq!(
            main.revision,
            Revision::Worktree,
            "fallback target is worktree"
        );
        assert!(matches!(
            main.record.confidence,
            MappingConfidence::Approximate(_)
        ));
        assert!(
            !main.record.hunks.is_empty(),
            "the deletion's hunk is recorded"
        );
    }

    /// Review 22 M2: a run covering two adjacent sibling FIELDS maps to the fields (the
    /// semantic frontier), not their parent struct.
    #[test]
    fn sibling_field_edit_maps_to_fields_not_parent() {
        // Worktree: Greeter struct (20-30) with fields Name (22) and Email (23). An Add
        // run covering exactly the two field lines maps to the fields.
        let change = file_change(
            FileStatus::Modified,
            vec![hunk_with(
                23,
                2,
                23,
                2,
                vec![
                    codescope_core::DiffLine::add(23, ""),
                    codescope_core::DiffLine::add(24, ""),
                ],
            )],
        );
        let out = changed_symbols_detailed(Some(&worktree()), Some(&base()), &change);
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"Greeter.Name"),
            "field Name mapped: {names:?}"
        );
        assert!(
            names.contains(&"Greeter.Email"),
            "field Email mapped: {names:?}"
        );
    }

    /// Review 23 M1: a Del+Add replacement across two sibling fields (both present on both
    /// sides) maps the deletion to the BASE fields and the addition to the WORKTREE fields —
    /// the parent struct is NOT reported merely for containing the changed children.
    #[test]
    fn sibling_field_replacement_omits_the_parent() {
        // Base and worktree both have Greeter { Name (22), Email (24) }.
        let mut wt_greeter = node("1", "Greeter", 20, 28);
        wt_greeter.kind = codescope_core::SymbolKind::Struct;
        wt_greeter.children = vec![
            SymbolNode {
                kind: codescope_core::SymbolKind::Field,
                ..node("1/0", "Name", 22, 22)
            },
            SymbolNode {
                kind: codescope_core::SymbolKind::Field,
                ..node("1/1", "Email", 24, 24)
            },
        ];
        let wt = SymbolTree::new(
            codescope_core::FileId::new("main.go").unwrap(),
            Revision::Worktree,
            vec![node("0", "main", 5, 15), wt_greeter],
        );
        let mut base_greeter = node("1", "Greeter", 20, 28);
        base_greeter.kind = codescope_core::SymbolKind::Struct;
        base_greeter.children = vec![
            SymbolNode {
                kind: codescope_core::SymbolKind::Field,
                ..node("1/0", "Name", 22, 22)
            },
            SymbolNode {
                kind: codescope_core::SymbolKind::Field,
                ..node("1/1", "Email", 24, 24)
            },
        ];
        let base = SymbolTree::new(
            codescope_core::FileId::new("main.go").unwrap(),
            Revision::Base,
            vec![node("0", "main", 5, 15), base_greeter],
        );

        // Replace Name (old/new 23) and Email (old/new 25) — a Del+Add on each field.
        let change = file_change(
            FileStatus::Modified,
            vec![hunk_with(
                23,
                3,
                23,
                3,
                vec![
                    codescope_core::DiffLine::del(23, ""),
                    codescope_core::DiffLine::add(23, ""),
                    codescope_core::DiffLine::context(24, 24, ""),
                    codescope_core::DiffLine::del(25, ""),
                    codescope_core::DiffLine::add(25, ""),
                ],
            )],
        );
        let out = changed_symbols_detailed(Some(&wt), Some(&base), &change);
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"Greeter.Name"),
            "field Name mapped: {names:?}"
        );
        assert!(
            names.contains(&"Greeter.Email"),
            "field Email mapped: {names:?}"
        );
        assert!(
            !out.iter().any(|c| c.name == "Greeter"),
            "parent struct must not be reported for a child-only edit: {names:?}"
        );
    }
}
