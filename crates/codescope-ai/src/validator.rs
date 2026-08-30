//! The deterministic fact-validation boundary for AI plans (research 05 §3).
//!
//! Validation is local and has no AI in the loop. [`validate`] sanitizes a parsed
//! [`VisualizationPlan`] **in place** against a [`FactView`] and returns the
//! [`ValidationReport`] the TUI renders (and the debug pane inspects). Policy per form:
//!
//! - **Epoch gate**: a plan whose epoch differs from the current one is `Stale` — never
//!   silently rendered as fresh.
//! - **Tree forms** (`changed_symbol_tree`, `call_tree`, `type_impl_tree`,
//!   `before_after`): invalid nodes are dropped and their children re-parented; an invalid
//!   root or >20% invalid nodes rejects the form.
//! - **Flow/sequence forms**: any invalid endpoint (node or edge) rejects the form, because
//!   it breaks ordering semantics.
//! - **`impact_summary` / `focused_diff`**: invalid bullets are dropped; an empty result
//!   rejects the form. `focused_diff` bullets reference hunks as
//!   `entity.symbol = "hunk:<index>"` and are re-checked via [`FactView::hunk`].
//! - **Caps** (Show Me rule S4/S5) are enforced with truncation recorded in the report:
//!   ≤ [`MAX_FORMS_PER_PLAN`] forms, ≤ [`MAX_FORM_NODES`] nodes, depth ≤
//!   [`MAX_FORM_DEPTH`], summary ≤ [`MAX_SUMMARY_LINES`] lines (and ≤
//!   [`IMPACT_SUMMARY_MAX_BULLETS`] bullets for `impact_summary`, research 05 §2).
//!
//! Edges may only *select* relationships that exist ([`FactView::edge_exists`]); `reads`/
//! `writes` edges have no impact-graph counterpart in v0 and are kept with an
//! "unverified" note when their endpoints resolve.

use codescope_core::{
    DroppedItem, EntityRef, Epoch, FileId, LineRange, PlanEdgeKind, PlanNode, ValidationReport,
    ValidationVerdict, VisualizationPlan, VizForm, MAX_FORMS_PER_PLAN, MAX_FORM_DEPTH,
    MAX_FORM_NODES, MAX_SUMMARY_LINES, PLAN_VERSION,
};
use std::collections::{HashMap, HashSet, VecDeque};

/// `impact_summary` forms carry at most this many bullets (research 05 §2: "≤8 bullets").
pub const IMPACT_SUMMARY_MAX_BULLETS: usize = 8;

/// Read-only view of the fact store the validator resolves plan entities against.
///
/// `codescope-analysis` wires the real implementation (symbol trees, impact graph, change
/// sets); tests stub it. The `Sync` supertrait keeps futures that hold a `&dyn FactView`
/// across `.await` points spawnable.
pub trait FactView: Sync {
    /// `true` when `file` exists in the current change context (worktree or base overlay).
    fn file_exists(&self, file: &FileId) -> bool;

    /// Resolve a fully-qualified symbol name within `file` to its extent, if it exists.
    fn resolve_symbol(&self, file: &FileId, name: &str) -> Option<LineRange>;

    /// `true` when the impact graph contains a `kind` edge from `from` to `to`.
    fn edge_exists(&self, from: &EntityRef, to: &EntityRef, kind: PlanEdgeKind) -> bool;

    /// `Some(())` when hunk `index` (zero-based, diff order) exists for `file`.
    fn hunk(&self, file: &FileId, index: u32) -> Option<()>;
}

/// Validate and sanitize `plan` in place against `facts`, gated on `current_epoch`.
///
/// On return the plan contains only renderable content: rejected forms are removed,
/// hallucinated nodes are dropped (with children re-parented in tree forms), dangling
/// references are cleaned up, and all caps are enforced. Every removal is recorded in the
/// report ([`ValidationReport::dropped`] / [`ValidationReport::notes`]).
///
/// The plan is **not** mutated when the verdict is [`ValidationVerdict::Stale`] (the TUI
/// keeps showing the last valid render with a badge) or when the whole plan is rejected.
#[tracing::instrument(level = "debug", skip_all, fields(epoch = %plan.epoch, forms = plan.forms.len()))]
pub fn validate(
    plan: &mut VisualizationPlan,
    facts: &dyn FactView,
    current_epoch: Epoch,
) -> ValidationReport {
    if plan.plan_version != PLAN_VERSION {
        return ValidationReport::rejected(format!(
            "unsupported plan_version {} (expected {PLAN_VERSION})",
            plan.plan_version
        ));
    }
    if plan.epoch != current_epoch {
        let mut report = ValidationReport::stale();
        report.notes.push(format!(
            "plan epoch {} != current {current_epoch}; regenerating",
            plan.epoch
        ));
        tracing::info!(plan_epoch = %plan.epoch, %current_epoch, "plan is stale");
        return report;
    }

    let mut dropped: Vec<DroppedItem> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    if plan.focus.trim().is_empty() {
        notes.push("plan focus is empty".to_string());
    }
    if plan.forms.is_empty() {
        return ValidationReport::rejected("plan has no forms");
    }
    while plan.forms.len() > MAX_FORMS_PER_PLAN {
        let form = plan
            .forms
            .pop()
            .unwrap_or_else(|| unreachable!("len checked"));
        dropped.push(DroppedItem {
            subject: format!("form {} ({:?})", plan.forms.len(), form.kind),
            reason: format!("exceeds MAX_FORMS_PER_PLAN ({MAX_FORMS_PER_PLAN})"),
        });
    }

    let mut kept_forms: Vec<VizForm> = Vec::new();
    for (idx, mut form) in plan.forms.drain(..).enumerate() {
        match sanitize_form(&mut form, idx, facts, &mut dropped, &mut notes) {
            Ok(()) => kept_forms.push(form),
            Err(reason) => {
                tracing::info!(form = idx, kind = ?form.kind, %reason, "form rejected");
                dropped.push(DroppedItem {
                    subject: format!("form {idx} ({:?})", form.kind),
                    reason,
                });
            }
        }
    }
    plan.forms = kept_forms;

    let verdict = if plan.forms.is_empty() {
        ValidationVerdict::Rejected
    } else if dropped.is_empty() {
        ValidationVerdict::Valid
    } else {
        ValidationVerdict::ValidWithDrops
    };
    if verdict == ValidationVerdict::Rejected {
        notes.push("no renderable forms remain; use the deterministic fallback".to_string());
    }
    tracing::debug!(?verdict, dropped = dropped.len(), "plan validated");
    ValidationReport {
        verdict,
        dropped,
        notes,
    }
}

/// Why a node failed validation, or `None` when it is valid.
fn node_invalid_reason(
    node: &PlanNode,
    form_kind: FormClass,
    facts: &dyn FactView,
) -> Option<String> {
    match form_kind {
        FormClass::FocusedDiff => {
            let Some(entity) = &node.entity else {
                return Some("focused_diff bullet has no hunk entity".to_string());
            };
            let Some(index) = entity
                .symbol
                .as_deref()
                .and_then(|s| s.strip_prefix("hunk:"))
                .and_then(|s| s.parse::<u32>().ok())
            else {
                return Some("focused_diff entity.symbol must be \"hunk:<index>\"".to_string());
            };
            if facts.hunk(&entity.file, index).is_none() {
                return Some(format!("hunk {}#h{index} does not exist", entity.file));
            }
            None
        }
        _ => {
            let Some(entity) = &node.entity else {
                return None; // presentational node
            };
            if !facts.file_exists(&entity.file) {
                return Some(format!("file {} does not exist", entity.file));
            }
            if let Some(symbol) = &entity.symbol {
                let Some(extent) = facts.resolve_symbol(&entity.file, symbol) else {
                    return Some(format!(
                        "symbol {symbol} does not resolve in {}",
                        entity.file
                    ));
                };
                if let Some(range) = &entity.range {
                    if !extent.contains_lines(range) {
                        return Some(format!(
                            "range {}..{} outside symbol extent {}..{}",
                            range.start_line, range.end_line, extent.start_line, extent.end_line
                        ));
                    }
                }
            }
            None
        }
    }
}

/// Coarse per-form validation class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormClass {
    Tree,
    Flow,
    ImpactSummary,
    FocusedDiff,
}

fn classify(form: &VizForm) -> FormClass {
    use codescope_core::FormKind;
    if form.kind.is_tree_form() {
        FormClass::Tree
    } else if form.kind.is_flow_form() {
        FormClass::Flow
    } else if form.kind == FormKind::ImpactSummary {
        FormClass::ImpactSummary
    } else {
        FormClass::FocusedDiff
    }
}

/// `true` for edge kinds the v0 impact graph can verify (research 05 §3: the AI may select
/// `calls`/`implements`/`imports` — and `contains` — edges, never assert new ones).
fn edge_kind_verifiable(kind: PlanEdgeKind) -> bool {
    matches!(
        kind,
        PlanEdgeKind::Calls
            | PlanEdgeKind::Imports
            | PlanEdgeKind::Implements
            | PlanEdgeKind::Contains
    )
}

/// Defensive bound on raw node count per form before structural analysis (the real cap is
/// [`MAX_FORM_NODES`]; anything far beyond it is rejected outright to keep validation
/// bounded on adversarial input).
const RAW_NODE_SANITY: usize = 64;

/// Defensive bound on raw edge count per form.
const RAW_EDGE_SANITY: usize = 256;

/// Sanitize one form in place. `Ok(())` keeps the (now clean) form; `Err(reason)` rejects
/// the whole form per the research 05 §3 policy.
fn sanitize_form(
    form: &mut VizForm,
    form_idx: usize,
    facts: &dyn FactView,
    dropped: &mut Vec<DroppedItem>,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let class = classify(form);

    if form.nodes.len() > RAW_NODE_SANITY {
        return Err(format!(
            "node count {} far exceeds the cap of {MAX_FORM_NODES}",
            form.nodes.len()
        ));
    }
    if form.edges.len() > RAW_EDGE_SANITY {
        return Err(format!("edge count {} is absurd", form.edges.len()));
    }
    if form.nodes.is_empty() {
        return Err("form has no nodes".to_string());
    }

    let summary_lines = form.summary.lines().count();
    if summary_lines > MAX_SUMMARY_LINES {
        form.summary = form
            .summary
            .lines()
            .take(MAX_SUMMARY_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        notes.push(format!(
            "form {form_idx}: summary truncated from {summary_lines} to {MAX_SUMMARY_LINES} lines"
        ));
    }

    // Per-node validity; duplicate ids are invalid (first occurrence wins).
    let n = form.nodes.len();
    let mut validity: Vec<Option<String>> = vec![None; n];
    let mut id_to_idx: HashMap<String, usize> = HashMap::with_capacity(n);
    for (i, node) in form.nodes.iter().enumerate() {
        if id_to_idx.contains_key(&node.id) {
            validity[i] = Some("duplicate node id".to_string());
            continue;
        }
        id_to_idx.insert(node.id.clone(), i);
        validity[i] = node_invalid_reason(node, class, facts);
    }

    match class {
        FormClass::Tree => {
            sanitize_tree(form, form_idx, &validity, &id_to_idx, facts, dropped, notes)
        }
        FormClass::Flow => {
            sanitize_flow(form, form_idx, &validity, &id_to_idx, facts, dropped, notes)
        }
        FormClass::ImpactSummary | FormClass::FocusedDiff => {
            sanitize_list(form, form_idx, class, &validity, facts, dropped, notes)
        }
    }
}

/// Tree forms: drop invalid nodes and re-parent their children; reject on invalid root or
/// >20% invalid; prune beyond [`MAX_FORM_DEPTH`]; cap at [`MAX_FORM_NODES`].
#[allow(clippy::too_many_lines)]
fn sanitize_tree(
    form: &mut VizForm,
    form_idx: usize,
    validity: &[Option<String>],
    id_to_idx: &HashMap<String, usize>,
    facts: &dyn FactView,
    dropped: &mut Vec<DroppedItem>,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let n = form.nodes.len();
    let ids: Vec<String> = form.nodes.iter().map(|node| node.id.clone()).collect();

    // Resolve children id lists to indices; note dangling and self references.
    let mut children_idx: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut referenced: HashSet<usize> = HashSet::new();
    for (i, node) in form.nodes.iter().enumerate() {
        for child in &node.children {
            match id_to_idx.get(child) {
                Some(&c) if c == i => {
                    notes.push(format!(
                        "form {form_idx}: node {} self-reference dropped",
                        ids[i]
                    ));
                }
                Some(&c) => {
                    children_idx[i].push(c);
                    referenced.insert(c);
                }
                None => {
                    notes.push(format!(
                        "form {form_idx}: node {} references unknown child {child:?}",
                        ids[i]
                    ));
                }
            }
        }
    }

    let roots: Vec<usize> = (0..n).filter(|i| !referenced.contains(i)).collect();
    if roots.is_empty() {
        return Err("no root: children references form a cycle".to_string());
    }
    for &r in &roots {
        if let Some(reason) = &validity[r] {
            return Err(format!("root node {} invalid: {reason}", ids[r]));
        }
    }
    let invalid_count = validity.iter().flatten().count();
    if invalid_count * 5 > n {
        return Err(format!("{invalid_count}/{n} nodes invalid (>20%)"));
    }

    // Effective children of each valid node: invalid children are replaced by their own
    // valid descendants (re-parenting), with a cycle guard.
    let mut reparented = 0usize;
    let adjacency: Vec<Vec<usize>> = (0..n)
        .map(|i| {
            if validity[i].is_some() {
                return Vec::new();
            }
            let mut out = Vec::new();
            let mut visiting = vec![false; n];
            visiting[i] = true;
            for &c in &children_idx[i] {
                if validity[c].is_none() {
                    out.push(c);
                } else {
                    let before = out.len();
                    expand_through_invalid(c, &children_idx, validity, &mut visiting, &mut out);
                    if out.len() > before {
                        reparented += out.len() - before;
                    }
                }
            }
            out
        })
        .collect();
    if reparented > 0 {
        notes.push(format!(
            "form {form_idx}: {reparented} children re-parented past dropped nodes"
        ));
    }
    for (i, reason) in validity.iter().enumerate() {
        if let Some(reason) = reason {
            dropped.push(DroppedItem {
                subject: format!("node {} in form {form_idx}", ids[i]),
                reason: reason.clone(),
            });
        }
    }

    // BFS from roots over valid nodes: enforces depth, detects unreachable nodes, and
    // gives the deterministic keep-order for the node cap (parents before children).
    let mut visited: HashSet<usize> = HashSet::new();
    let mut bfs_order: Vec<usize> = Vec::new();
    let mut queue: VecDeque<(usize, usize)> = roots.iter().map(|&r| (r, 1usize)).collect();
    let mut depth_pruned = 0usize;
    while let Some((i, depth)) = queue.pop_front() {
        if !visited.insert(i) {
            continue;
        }
        bfs_order.push(i);
        if depth >= MAX_FORM_DEPTH {
            let pruned = adjacency[i].iter().filter(|c| !visited.contains(c)).count();
            depth_pruned += pruned;
            continue;
        }
        for &c in &adjacency[i] {
            queue.push_back((c, depth + 1));
        }
    }
    if depth_pruned > 0 {
        notes.push(format!(
            "form {form_idx}: subtrees beyond depth {MAX_FORM_DEPTH} pruned"
        ));
    }

    let keep: HashSet<usize> = bfs_order.iter().copied().take(MAX_FORM_NODES).collect();
    for (rank, &i) in bfs_order.iter().enumerate() {
        if rank >= MAX_FORM_NODES {
            dropped.push(DroppedItem {
                subject: format!("node {} in form {form_idx}", ids[i]),
                reason: format!("exceeds MAX_FORM_NODES cap ({MAX_FORM_NODES})"),
            });
        }
    }
    for i in 0..n {
        if validity[i].is_none() && !bfs_order.contains(&i) {
            dropped.push(DroppedItem {
                subject: format!("node {} in form {form_idx}", ids[i]),
                reason: "not reachable from any root (beyond depth cap or cyclic)".to_string(),
            });
        }
    }

    // Rebuild nodes (original order) and their children lists.
    let mut kept_nodes: Vec<PlanNode> = Vec::with_capacity(keep.len());
    for (i, mut node) in form.nodes.drain(..).enumerate() {
        if !keep.contains(&i) {
            continue;
        }
        node.children = adjacency[i]
            .iter()
            .filter(|c| keep.contains(*c))
            .map(|&c| ids[c].clone())
            .collect();
        kept_nodes.push(node);
    }
    form.nodes = kept_nodes;

    retain_clean_edges(form, form_idx, facts, dropped, notes);
    Ok(())
}

/// Depth-first replacement of an invalid child by its valid descendants.
fn expand_through_invalid(
    idx: usize,
    children_idx: &[Vec<usize>],
    validity: &[Option<String>],
    visiting: &mut [bool],
    out: &mut Vec<usize>,
) {
    if visiting[idx] {
        return; // cycle through invalid nodes; refs are simply dropped
    }
    visiting[idx] = true;
    for &c in &children_idx[idx] {
        if validity[c].is_none() {
            out.push(c);
        } else {
            expand_through_invalid(c, children_idx, validity, visiting, out);
        }
    }
}

/// Flow/sequence forms: any invalid node or edge endpoint rejects the form; asserted
/// relationships must exist in the impact graph.
fn sanitize_flow(
    form: &mut VizForm,
    form_idx: usize,
    validity: &[Option<String>],
    id_to_idx: &HashMap<String, usize>,
    facts: &dyn FactView,
    dropped: &mut Vec<DroppedItem>,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    for (i, reason) in validity.iter().enumerate() {
        if let Some(reason) = reason {
            return Err(format!("endpoint {} invalid: {reason}", form.nodes[i].id));
        }
    }
    // Every edge endpoint must name a declared node.
    for edge in &form.edges {
        for endpoint in [&edge.from, &edge.to] {
            if !id_to_idx.contains_key(endpoint) {
                return Err(format!("edge references unknown node {endpoint:?}"));
            }
        }
    }
    // Asserted relationships must exist (the AI selects edges, never asserts new ones).
    for edge in &form.edges {
        let from = &form.nodes[id_to_idx[&edge.from]];
        let to = &form.nodes[id_to_idx[&edge.to]];
        if edge_kind_verifiable(edge.kind) {
            match (&from.entity, &to.entity) {
                (Some(fe), Some(te)) => {
                    if !facts.edge_exists(fe, te, edge.kind) {
                        return Err(format!(
                            "edge {} -> {} ({:?}) not in the impact graph",
                            edge.from, edge.to, edge.kind
                        ));
                    }
                }
                _ => notes.push(format!(
                    "form {form_idx}: edge {} -> {} unverifiable (presentational endpoint)",
                    edge.from, edge.to
                )),
            }
        } else {
            notes.push(format!(
                "form {form_idx}: edge {} -> {} kind {:?} not verifiable in v0",
                edge.from, edge.to, edge.kind
            ));
        }
    }

    // Node cap: truncate in document order, dropping edges that lose an endpoint.
    if form.nodes.len() > MAX_FORM_NODES {
        let removed: Vec<PlanNode> = form.nodes.split_off(MAX_FORM_NODES);
        let removed_ids: HashSet<&str> = removed.iter().map(|node| node.id.as_str()).collect();
        for node in &removed {
            dropped.push(DroppedItem {
                subject: format!("node {} in form {form_idx}", node.id),
                reason: format!("exceeds MAX_FORM_NODES cap ({MAX_FORM_NODES})"),
            });
        }
        let before = form.edges.len();
        form.edges.retain(|e| {
            !removed_ids.contains(e.from.as_str()) && !removed_ids.contains(e.to.as_str())
        });
        if form.edges.len() < before {
            dropped.push(DroppedItem {
                subject: format!("{} edges in form {form_idx}", before - form.edges.len()),
                reason: "endpoint removed by node cap".to_string(),
            });
        }
    }
    // Flow nodes carry no tree children; scrub any stray references to missing ids.
    let kept: HashSet<&str> = form.nodes.iter().map(|node| node.id.as_str()).collect();
    let kept: HashSet<String> = kept.iter().map(|s| (*s).to_string()).collect();
    for node in &mut form.nodes {
        node.children.retain(|c| kept.contains(c));
    }
    Ok(())
}

/// `impact_summary` / `focused_diff`: drop invalid bullets, reject when nothing remains.
fn sanitize_list(
    form: &mut VizForm,
    form_idx: usize,
    class: FormClass,
    validity: &[Option<String>],
    facts: &dyn FactView,
    dropped: &mut Vec<DroppedItem>,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let mut idx = 0usize;
    form.nodes.retain(|node| {
        let keep = validity[idx].is_none();
        if !keep {
            dropped.push(DroppedItem {
                subject: format!("node {} in form {form_idx}", node.id),
                reason: validity[idx].clone().unwrap_or_default(),
            });
        }
        idx += 1;
        keep
    });
    if form.nodes.is_empty() {
        return Err("no valid bullets remain".to_string());
    }

    let cap = if class == FormClass::ImpactSummary {
        IMPACT_SUMMARY_MAX_BULLETS
    } else {
        MAX_FORM_NODES
    };
    if form.nodes.len() > cap {
        for node in form.nodes.split_off(cap) {
            dropped.push(DroppedItem {
                subject: format!("node {} in form {form_idx}", node.id),
                reason: format!("exceeds bullet cap ({cap})"),
            });
        }
    }
    let kept: HashSet<String> = form.nodes.iter().map(|node| node.id.clone()).collect();
    for node in &mut form.nodes {
        node.children.retain(|c| kept.contains(c));
    }
    retain_clean_edges(form, form_idx, facts, dropped, notes);
    Ok(())
}

/// Non-flow edge cleanup: drop edges with missing endpoints or verifiably absent
/// relationships (dropping, not rejecting — edges are decoration outside flow forms).
fn retain_clean_edges(
    form: &mut VizForm,
    form_idx: usize,
    facts: &dyn FactView,
    dropped: &mut Vec<DroppedItem>,
    notes: &mut Vec<String>,
) {
    let by_id: HashMap<String, EntityRef> = form
        .nodes
        .iter()
        .filter_map(|node| node.entity.clone().map(|e| (node.id.clone(), e)))
        .collect();
    let ids: HashSet<String> = form.nodes.iter().map(|node| node.id.clone()).collect();
    let edges = std::mem::take(&mut form.edges);
    for edge in edges {
        if !ids.contains(&edge.from) || !ids.contains(&edge.to) {
            dropped.push(DroppedItem {
                subject: format!("edge {} -> {} in form {form_idx}", edge.from, edge.to),
                reason: "endpoint not present".to_string(),
            });
            continue;
        }
        if edge_kind_verifiable(edge.kind) {
            match (by_id.get(&edge.from), by_id.get(&edge.to)) {
                (Some(fe), Some(te)) => {
                    if !facts.edge_exists(fe, te, edge.kind) {
                        dropped.push(DroppedItem {
                            subject: format!(
                                "edge {} -> {} in form {form_idx}",
                                edge.from, edge.to
                            ),
                            reason: format!("{:?} edge not in the impact graph", edge.kind),
                        });
                        continue;
                    }
                }
                _ => notes.push(format!(
                    "form {form_idx}: edge {} -> {} unverifiable (presentational endpoint)",
                    edge.from, edge.to
                )),
            }
        } else {
            notes.push(format!(
                "form {form_idx}: edge {} -> {} kind {:?} not verifiable in v0",
                edge.from, edge.to, edge.kind
            ));
        }
        form.edges.push(edge);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codescope_core::{FormKind, PlanEdge, PlanNodeChange};

    #[derive(Default)]
    struct StubFacts {
        files: HashSet<String>,
        symbols: HashMap<(String, String), LineRange>,
        edges: HashSet<(String, String, PlanEdgeKind)>,
        hunks: HashSet<(String, u32)>,
    }

    impl StubFacts {
        fn with_symbol(mut self, file: &str, name: &str, extent: LineRange) -> Self {
            self.files.insert(file.to_string());
            self.symbols
                .insert((file.to_string(), name.to_string()), extent);
            self
        }
        fn with_file(mut self, file: &str) -> Self {
            self.files.insert(file.to_string());
            self
        }
        fn with_edge(mut self, from: &EntityRef, to: &EntityRef, kind: PlanEdgeKind) -> Self {
            self.edges.insert((from.to_string(), to.to_string(), kind));
            self
        }
        fn with_hunk(mut self, file: &str, index: u32) -> Self {
            self.hunks.insert((file.to_string(), index));
            self
        }
    }

    impl FactView for StubFacts {
        fn file_exists(&self, file: &FileId) -> bool {
            self.files.contains(file.as_path().as_str())
        }
        fn resolve_symbol(&self, file: &FileId, name: &str) -> Option<LineRange> {
            self.symbols
                .get(&(file.as_path().as_str().to_string(), name.to_string()))
                .copied()
        }
        fn edge_exists(&self, from: &EntityRef, to: &EntityRef, kind: PlanEdgeKind) -> bool {
            self.edges
                .contains(&(from.to_string(), to.to_string(), kind))
        }
        fn hunk(&self, file: &FileId, index: u32) -> Option<()> {
            self.hunks
                .contains(&(file.as_path().as_str().to_string(), index))
                .then_some(())
        }
    }

    fn sym_entity(file: &str, name: &str) -> EntityRef {
        EntityRef::for_symbol(FileId::new_unchecked(file), name, None)
    }

    fn node(id: &str, entity: Option<EntityRef>, children: &[&str]) -> PlanNode {
        let mut n = PlanNode::new(id, id, PlanNodeChange::Modified);
        n.entity = entity;
        n.children = children.iter().map(|c| (*c).to_string()).collect();
        n
    }

    fn form(kind: FormKind, nodes: Vec<PlanNode>, edges: Vec<PlanEdge>) -> VizForm {
        VizForm {
            kind,
            title: "t".into(),
            summary: "s".into(),
            nodes,
            edges,
        }
    }

    fn plan_with(forms: Vec<VizForm>) -> VisualizationPlan {
        let mut p = VisualizationPlan::new(Epoch(1), "focus?");
        p.forms = forms;
        p
    }

    fn edge(from: &str, to: &str, kind: PlanEdgeKind) -> PlanEdge {
        PlanEdge {
            from: from.into(),
            to: to.into(),
            kind,
            label: None,
        }
    }

    /// Facts with symbols a..e in main.go, extents 10 lines apart.
    fn abc_facts() -> StubFacts {
        let mut f = StubFacts::default();
        for (i, name) in ["A", "B", "C", "D", "E"].iter().enumerate() {
            let start = (i as u32) * 10;
            f = f.with_symbol("main.go", name, LineRange::new(start, 0, start + 5, 1));
        }
        f
    }

    #[test]
    fn stale_epoch_gates_everything() {
        let mut plan = plan_with(vec![form(
            FormKind::ImpactSummary,
            vec![node("n1", Some(sym_entity("main.go", "A")), &[])],
            vec![],
        )]);
        plan.epoch = Epoch(1);
        let report = validate(&mut plan, &abc_facts(), Epoch(2));
        assert_eq!(report.verdict, ValidationVerdict::Stale);
        assert!(!report.is_renderable());
        // Plan untouched: TUI keeps the last valid render.
        assert_eq!(plan.forms.len(), 1);
    }

    #[test]
    fn wrong_version_rejected() {
        let mut plan = plan_with(vec![]);
        plan.plan_version = 99;
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
    }

    #[test]
    fn empty_plan_rejected() {
        let mut plan = plan_with(vec![]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
    }

    #[test]
    fn clean_plan_is_valid() {
        let mut plan = plan_with(vec![form(
            FormKind::CallTree,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &["n2"]),
                node("n2", Some(sym_entity("main.go", "B")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Valid);
        assert!(report.dropped.is_empty());
        assert_eq!(plan.forms[0].nodes.len(), 2);
        assert_eq!(plan.forms[0].nodes[0].children, ["n2"]);
    }

    #[test]
    fn tree_drops_invalid_node_and_reparents_children() {
        // n1(root) -> [n2(ghost), n4]; n2 -> [n3]; n4 -> [n5]. 1/5 invalid = 20% (not >20%).
        let mut plan = plan_with(vec![form(
            FormKind::CallTree,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &["n2", "n4"]),
                node("n2", Some(sym_entity("ghost.go", "Ghost")), &["n3"]),
                node("n3", Some(sym_entity("main.go", "B")), &[]),
                node("n4", Some(sym_entity("main.go", "C")), &["n5"]),
                node("n5", Some(sym_entity("main.go", "D")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert!(report.dropped.iter().any(|d| d.subject.contains("n2")));
        let f = &plan.forms[0];
        assert_eq!(f.nodes.len(), 4);
        assert!(f.node("n2").is_none());
        // n3 re-parented onto n1, in n2's position.
        assert_eq!(f.node("n1").unwrap().children, ["n3", "n4"]);
        assert!(report.notes.iter().any(|n| n.contains("re-parented")));
    }

    #[test]
    fn tree_rejects_when_more_than_20_percent_invalid() {
        // 1/3 invalid = 33% > 20% → reject; single form → plan rejected.
        let mut plan = plan_with(vec![form(
            FormKind::ChangedSymbolTree,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &["n2", "n3"]),
                node("n2", Some(sym_entity("ghost.go", "Ghost")), &[]),
                node("n3", Some(sym_entity("main.go", "B")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(plan.forms.is_empty());
        assert!(report
            .dropped
            .iter()
            .any(|d| d.subject.starts_with("form 0") && d.reason.contains(">20%")));
    }

    #[test]
    fn tree_rejects_invalid_root() {
        let mut plan = plan_with(vec![form(
            FormKind::TypeImplTree,
            vec![
                node(
                    "n1",
                    Some(sym_entity("ghost.go", "Ghost")),
                    &["n2", "n3", "n4", "n5"],
                ),
                node("n2", Some(sym_entity("main.go", "A")), &[]),
                node("n3", Some(sym_entity("main.go", "B")), &[]),
                node("n4", Some(sym_entity("main.go", "C")), &[]),
                node("n5", Some(sym_entity("main.go", "D")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(report
            .dropped
            .iter()
            .any(|d| d.reason.contains("root node n1 invalid")));
    }

    #[test]
    fn flow_rejects_invalid_endpoint() {
        let mut plan = plan_with(vec![form(
            FormKind::RelationshipFlow,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &[]),
                node("n2", Some(sym_entity("ghost.go", "Ghost")), &[]),
            ],
            vec![edge("n1", "n2", PlanEdgeKind::Calls)],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(report
            .dropped
            .iter()
            .any(|d| d.reason.contains("endpoint n2 invalid")));
    }

    #[test]
    fn flow_rejects_edge_missing_from_impact_graph() {
        let a = sym_entity("main.go", "A");
        let b = sym_entity("main.go", "B");
        let facts = abc_facts(); // no edges at all
        let mut plan = plan_with(vec![form(
            FormKind::Sequence,
            vec![node("n1", Some(a), &[]), node("n2", Some(b), &[])],
            vec![edge("n1", "n2", PlanEdgeKind::Calls)],
        )]);
        let report = validate(&mut plan, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(report
            .dropped
            .iter()
            .any(|d| d.reason.contains("not in the impact graph")));
    }

    #[test]
    fn flow_accepts_existing_edges_and_unknown_endpoint_rejects() {
        let a = sym_entity("main.go", "A");
        let b = sym_entity("main.go", "B");
        let facts = abc_facts().with_edge(&a, &b, PlanEdgeKind::Calls);
        let mut plan = plan_with(vec![form(
            FormKind::RelationshipFlow,
            vec![
                node("n1", Some(a.clone()), &[]),
                node("n2", Some(b.clone()), &[]),
            ],
            vec![edge("n1", "n2", PlanEdgeKind::Calls)],
        )]);
        let report = validate(&mut plan, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Valid);

        let mut plan = plan_with(vec![form(
            FormKind::RelationshipFlow,
            vec![node("n1", Some(a), &[])],
            vec![edge("n1", "nope", PlanEdgeKind::Calls)],
        )]);
        let report = validate(&mut plan, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
    }

    #[test]
    fn impact_summary_drops_bullets_and_rejects_when_empty() {
        // One ghost bullet among two → dropped, still renderable.
        let mut plan = plan_with(vec![form(
            FormKind::ImpactSummary,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &[]),
                node("n2", Some(sym_entity("ghost.go", "Ghost")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.forms[0].nodes.len(), 1);

        // All bullets ghost → form rejected → plan rejected.
        let mut plan = plan_with(vec![form(
            FormKind::ImpactSummary,
            vec![node("n1", Some(sym_entity("ghost.go", "Ghost")), &[])],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(report
            .dropped
            .iter()
            .any(|d| d.reason.contains("no valid bullets")));
    }

    #[test]
    fn focused_diff_hunks_rechecked_by_reference() {
        let facts = abc_facts().with_hunk("main.go", 0).with_hunk("main.go", 2);
        let hunk = |i: u32| {
            EntityRef::for_symbol(FileId::new_unchecked("main.go"), format!("hunk:{i}"), None)
        };
        let mut plan = plan_with(vec![form(
            FormKind::FocusedDiff,
            vec![
                node("n1", Some(hunk(0)), &[]),
                node("n2", Some(hunk(1)), &[]), // does not exist
                node("n3", Some(hunk(2)), &[]),
                node("n4", Some(sym_entity("main.go", "A")), &[]), // not a hunk ref
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        let ids: Vec<&str> = plan.forms[0].nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["n1", "n3"]);
        assert!(report.dropped.iter().any(|d| d.subject.contains("n2")));
        assert!(report
            .dropped
            .iter()
            .any(|d| d.subject.contains("n4") && d.reason.contains("hunk:<index>")));
    }

    #[test]
    fn caps_forms_nodes_depth_summary_with_notes() {
        let bullet = |i: usize| node(&format!("n{i}"), Some(sym_entity("main.go", "A")), &[]);
        // Form 0: 10 bullets → capped at 8. Form 1: fine. Form 2: dropped (max 2 forms).
        let mut many_bullets: Vec<PlanNode> = (0..10).map(bullet).collect();
        for (i, n) in many_bullets.iter_mut().enumerate() {
            n.id = format!("n{i}");
        }
        let mut f0 = form(FormKind::ImpactSummary, many_bullets, vec![]);
        f0.summary = "l1\nl2\nl3\nl4\nl5".into();
        let f1 = form(
            FormKind::ImpactSummary,
            vec![node("m1", Some(sym_entity("main.go", "B")), &[])],
            vec![],
        );
        let f2 = form(
            FormKind::ImpactSummary,
            vec![node("k1", Some(sym_entity("main.go", "C")), &[])],
            vec![],
        );
        let mut plan = plan_with(vec![f0, f1, f2]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.forms.len(), MAX_FORMS_PER_PLAN);
        assert_eq!(plan.forms[0].nodes.len(), IMPACT_SUMMARY_MAX_BULLETS);
        assert_eq!(plan.forms[0].summary.lines().count(), MAX_SUMMARY_LINES);
        assert!(report.notes.iter().any(|n| n.contains("summary truncated")));
        assert!(report
            .dropped
            .iter()
            .any(|d| d.subject.starts_with("form 2") && d.reason.contains("MAX_FORMS_PER_PLAN")));
        assert!(report
            .dropped
            .iter()
            .any(|d| d.reason.contains("bullet cap")));
    }

    #[test]
    fn tree_node_cap_keeps_bfs_prefix() {
        // Root with 13 children (14 nodes) → root + first 11 children kept (12 total).
        let mut nodes = vec![node(
            "root",
            Some(sym_entity("main.go", "A")),
            &[
                "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "c9", "c10", "c11", "c12", "c13",
            ],
        )];
        for i in 1..=13 {
            nodes.push(node(
                &format!("c{i}"),
                Some(sym_entity("main.go", "B")),
                &[],
            ));
        }
        let mut plan = plan_with(vec![form(FormKind::CallTree, nodes, vec![])]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        let f = &plan.forms[0];
        assert_eq!(f.nodes.len(), MAX_FORM_NODES);
        assert!(f.node("root").is_some());
        assert!(f.node("c11").is_some());
        assert!(f.node("c12").is_none());
        assert_eq!(f.node("root").unwrap().children.len(), 11);
        assert!(report
            .dropped
            .iter()
            .any(|d| d.subject.contains("c12") && d.reason.contains("MAX_FORM_NODES")));
    }

    #[test]
    fn tree_depth_pruned_beyond_cap() {
        // Chain n1 -> n2 -> n3 -> n4: depth 4 exceeds MAX_FORM_DEPTH=3 → n4 pruned.
        let mut plan = plan_with(vec![form(
            FormKind::BeforeAfter,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &["n2"]),
                node("n2", Some(sym_entity("main.go", "B")), &["n3"]),
                node("n3", Some(sym_entity("main.go", "C")), &["n4"]),
                node("n4", Some(sym_entity("main.go", "D")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        let f = &plan.forms[0];
        assert!(f.node("n4").is_none());
        assert_eq!(f.nodes.len(), 3);
        assert!(f.node("n3").unwrap().children.is_empty());
        assert!(report.notes.iter().any(|n| n.contains("depth")));
    }

    #[test]
    fn duplicate_ids_and_dangling_children_are_cleaned() {
        let mut plan = plan_with(vec![form(
            FormKind::ImpactSummary,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &["nowhere"]),
                node("n1", Some(sym_entity("main.go", "B")), &[]), // duplicate id
                node("n2", Some(sym_entity("main.go", "C")), &[]),
                node("n3", Some(sym_entity("main.go", "D")), &[]),
                node("n4", Some(sym_entity("main.go", "E")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.forms[0].nodes.len(), 4);
        assert!(report
            .dropped
            .iter()
            .any(|d| d.reason.contains("duplicate node id")));
        assert!(plan.forms[0].nodes[0].children.is_empty());
    }

    #[test]
    fn presentational_nodes_are_valid_outside_focused_diff() {
        let mut plan = plan_with(vec![form(
            FormKind::CallTree,
            vec![
                node("n1", None, &["n2"]), // presentational root
                node("n2", Some(sym_entity("main.go", "A")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Valid);
    }

    #[test]
    fn range_outside_symbol_extent_is_invalid() {
        let mut entity = sym_entity("main.go", "A"); // extent 0..5
        entity.range = Some(LineRange::new(100, 0, 120, 0));
        let mut plan = plan_with(vec![form(
            FormKind::ImpactSummary,
            vec![
                node("n1", Some(entity), &[]),
                node("n2", Some(sym_entity("main.go", "B")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert!(report
            .dropped
            .iter()
            .any(|d| d.subject.contains("n1") && d.reason.contains("outside symbol extent")));
        // In-extent range is fine.
        let mut ok = sym_entity("main.go", "A");
        ok.range = Some(LineRange::new(1, 0, 4, 0));
        let mut plan = plan_with(vec![form(
            FormKind::ImpactSummary,
            vec![node("n1", Some(ok), &[])],
            vec![],
        )]);
        assert_eq!(
            validate(&mut plan, &abc_facts(), Epoch(1)).verdict,
            ValidationVerdict::Valid
        );
    }

    #[test]
    fn one_rejected_form_keeps_the_other_renderable() {
        let bad_flow = form(
            FormKind::Sequence,
            vec![node("x1", Some(sym_entity("ghost.go", "Ghost")), &[])],
            vec![],
        );
        let good = form(
            FormKind::ImpactSummary,
            vec![node("n1", Some(sym_entity("main.go", "A")), &[])],
            vec![],
        );
        let mut plan = plan_with(vec![bad_flow, good]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.forms.len(), 1);
        assert_eq!(plan.forms[0].kind, FormKind::ImpactSummary);
    }

    #[test]
    fn list_edges_cleaned_not_rejected() {
        let a = sym_entity("main.go", "A");
        let b = sym_entity("main.go", "B");
        let facts = abc_facts().with_edge(&a, &b, PlanEdgeKind::Calls);
        let mut plan = plan_with(vec![form(
            FormKind::ImpactSummary,
            vec![node("n1", Some(a), &[]), node("n2", Some(b), &[])],
            vec![
                edge("n1", "n2", PlanEdgeKind::Calls),    // exists → kept
                edge("n2", "n1", PlanEdgeKind::Calls),    // absent → dropped
                edge("n1", "zz", PlanEdgeKind::Contains), // dangling → dropped
            ],
        )]);
        let report = validate(&mut plan, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.forms[0].edges.len(), 1);
        assert!(report
            .dropped
            .iter()
            .any(|d| d.reason.contains("not in the impact graph")));
        assert!(report
            .dropped
            .iter()
            .any(|d| d.reason.contains("endpoint not present")));
    }

    #[test]
    fn file_level_entities_resolve_by_file_existence() {
        let facts = StubFacts::default().with_file("docs/readme.md");
        let mut plan = plan_with(vec![form(
            FormKind::ImpactSummary,
            vec![
                node(
                    "n1",
                    Some(EntityRef::for_file(FileId::new_unchecked("docs/readme.md"))),
                    &[],
                ),
                node(
                    "n2",
                    Some(EntityRef::for_file(FileId::new_unchecked("missing.md"))),
                    &[],
                ),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.forms[0].nodes.len(), 1);
        assert!(report
            .dropped
            .iter()
            .any(|d| d.subject.contains("n2") && d.reason.contains("does not exist")));
    }

    #[test]
    fn absurd_node_count_rejected_outright() {
        let nodes: Vec<PlanNode> = (0..100)
            .map(|i| node(&format!("n{i}"), Some(sym_entity("main.go", "A")), &[]))
            .collect();
        let mut plan = plan_with(vec![form(FormKind::ImpactSummary, nodes, vec![])]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
    }
}
