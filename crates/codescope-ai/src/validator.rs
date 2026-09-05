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
//!   A `before_after` node with exact diff-line references may shed an unqueried symbol
//!   decoration and remain as an explicitly presentational state; proven-absent symbols
//!   are never rescued.
//! - **Flow/sequence forms**: any invalid endpoint (node or edge) rejects the form, because
//!   it breaks ordering semantics.
//! - **Reviewer-first contract**: the primary form must be structural. Legacy
//!   `impact_summary` / `focused_diff` forms cannot cross this boundary; typed plan
//!   evidence references hunks directly and is re-checked via [`FactView::hunk`].
//! - **Caps** (Show Me rule S4) are enforced with truncation recorded in the report:
//!   ≤ [`MAX_FORMS_PER_PLAN`] forms, ≤ [`MAX_FORM_NODES`] nodes, depth ≤
//!   [`MAX_FORM_DEPTH`] (and ≤ [`IMPACT_SUMMARY_MAX_BULLETS`] bullets for
//!   `impact_summary`, research 05 §2).
//!
//! Edges may only *select* relationships that exist ([`FactView::edge`]); `reads`/
//! `writes` edges have no impact-graph counterpart in v0 and are kept with an
//! "unverified" note when their endpoints resolve.

use codescope_core::{
    DiffSide, DroppedItem, EntityRef, Epoch, FileId, FormKind, LineRange, MAX_CODE_REF_LINES,
    MAX_FORM_DEPTH, MAX_FORM_NODES, MAX_FORMS_PER_PLAN, MAX_NODE_CODE_REFS, MAX_PLAN_EVIDENCE,
    PLAN_VERSION, PlanCodeRef, PlanEdge, PlanEdgeKind, PlanEvidence, PlanNode, ValidationReport,
    ValidationVerdict, VisualizationPlan, VizForm,
};
use std::collections::{HashMap, HashSet, VecDeque};

/// `impact_summary` forms carry at most this many bullets (research 05 §2: "≤8 bullets").
pub const IMPACT_SUMMARY_MAX_BULLETS: usize = 8;

/// Read-only view of the fact store the validator resolves plan entities against.
///
/// `codescope-analysis` wires the real implementation (symbol trees, impact graph, change
/// sets); tests stub it. The `Sync` supertrait keeps futures that hold a `&dyn FactView`
/// across `.await` points spawnable.
/// Tri-state result of a fact lookup. Distinguishes "a complete query proved this
/// absent" from "this was never queried / the query was partial", so an unqueried
/// relationship is never mistaken for a disproven one (review 19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup<T> {
    /// The fact was found.
    Present(T),
    /// A complete query covering this fact ran and did not return it — authoritative absence.
    Absent,
    /// The fact was not queried, the query was partial/failed, or evidence is unavailable.
    Unknown,
}

impl<T> Lookup<T> {
    /// `true` only for an authoritative absence (a complete query returned nothing).
    pub fn is_absent(self) -> bool {
        matches!(self, Lookup::Absent)
    }
}

/// The fact store a plan is validated against (the deterministic boundary). Implementors
/// answer whether cited files/symbols/edges/hunks exist; the tri-state [`Lookup`] keeps
/// "never queried" distinct from "proven absent".
pub trait FactView: Sync {
    /// Whether `file` exists in the current change context (worktree or base overlay).
    fn file(&self, file: &FileId) -> Lookup<()>;

    /// Resolve a fully-qualified symbol name within `file` to its extent.
    fn symbol(&self, file: &FileId, name: &str) -> Lookup<LineRange>;

    /// Whether the impact evidence contains a `kind` edge from `from` to `to`.
    fn edge(&self, from: &EntityRef, to: &EntityRef, kind: PlanEdgeKind) -> Lookup<()>;

    /// Whether hunk `index` (zero-based, diff order) exists for `file`.
    fn hunk(&self, file: &FileId, index: u32) -> Lookup<()>;

    /// Whether `file` belongs to the file/function or directory scope currently explained.
    /// Generic validators without a selection context may accept all files.
    fn is_focus_file(&self, _file: &FileId) -> bool {
        true
    }

    /// Whether a one-based source `line` exists on `side` of hunk `index` in `file`.
    /// This exact lookup grounds hover highlights in diff rows rather than arbitrary source
    /// ranges that happen to fall inside a file.
    fn diff_line(&self, file: &FileId, index: u32, side: DiffSide, line: u32) -> Lookup<()>;

    /// Whether `line` is an actual changed diff row: an addition on [`DiffSide::New`] or
    /// deletion on [`DiffSide::Old`]. Context rows and non-hunk lines are [`Lookup::Absent`]
    /// when the hunk is known; unavailable diff facts are [`Lookup::Unknown`].
    ///
    /// Validators use this in addition to [`FactView::diff_line`]: a code reference range
    /// may include context, but each node with references must cite at least one changed row.
    fn changed_diff_line(&self, file: &FileId, index: u32, side: DiffSide, line: u32)
    -> Lookup<()>;
}

/// Validate and sanitize `plan` in place against `facts`, gated on `current_epoch`.
///
/// On return the plan contains only renderable content: rejected forms are removed,
/// hallucinated nodes are dropped (with children re-parented in tree forms), dangling
/// references are cleaned up, and all caps are enforced. Every removal is recorded in the
/// report ([`ValidationReport::dropped`] / [`ValidationReport::notes`]).
///
/// The plan is **not** mutated when the verdict is [`ValidationVerdict::Stale`] or when the
/// whole plan is rejected. Neither result is published as current generated output.
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

    if plan.intent.trim().is_empty() {
        return ValidationReport::rejected("plan intent is empty");
    }
    if plan.forms.is_empty() {
        return ValidationReport::rejected("plan has no forms");
    }
    if !is_reviewer_visual(plan.forms[0].kind) {
        return ValidationReport::rejected(
            "primary form must be a structural relationship visual, not a prose or diff list",
        );
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

    // The dropped-evidence reasons recorded by sanitize_evidence stay in `dropped` so
    // rejection feedback names the concrete invalid citations; the Err only fires when
    // nothing valid remains.
    if let Err(reason) = sanitize_evidence(plan, facts, &mut dropped) {
        return ValidationReport {
            verdict: ValidationVerdict::Rejected,
            dropped,
            notes: vec![reason],
        };
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
        notes.push("no renderable forms remain".to_string());
    }
    tracing::debug!(?verdict, dropped = dropped.len(), "plan validated");
    ValidationReport {
        verdict,
        dropped,
        notes,
    }
}

fn is_reviewer_visual(kind: FormKind) -> bool {
    matches!(
        kind,
        FormKind::ChangedSymbolTree
            | FormKind::CallTree
            | FormKind::TypeImplTree
            | FormKind::RelationshipFlow
            | FormKind::BeforeAfter
            | FormKind::Sequence
    )
}

fn evidence_invalid_reason(evidence: &PlanEvidence, facts: &dyn FactView) -> Option<String> {
    if evidence.reason.trim().is_empty() {
        return Some("evidence has no explanation".to_string());
    }
    match facts.file(&evidence.file) {
        Lookup::Present(()) => {}
        Lookup::Absent => return Some(format!("file {} does not exist", evidence.file)),
        Lookup::Unknown => {
            return Some(format!(
                "file {} not queried (cannot validate)",
                evidence.file
            ));
        }
    }
    if let Some(hunk) = evidence.hunk {
        match facts.hunk(&evidence.file, hunk) {
            Lookup::Present(()) => {}
            Lookup::Absent => {
                return Some(format!("hunk {}#h{hunk} does not exist", evidence.file));
            }
            Lookup::Unknown => {
                return Some(format!(
                    "hunk {}#h{hunk} not queried (cannot validate)",
                    evidence.file
                ));
            }
        }
    }
    if let Some(symbol) = &evidence.symbol {
        let extent = match facts.symbol(&evidence.file, symbol) {
            Lookup::Present(extent) => extent,
            Lookup::Absent => {
                return Some(format!(
                    "symbol {symbol} not found in {} (analyzed)",
                    evidence.file
                ));
            }
            Lookup::Unknown => {
                return Some(format!(
                    "symbol {symbol} not queried in {} (cannot validate)",
                    evidence.file
                ));
            }
        };
        if let Some(range) = &evidence.range {
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

/// Sanitize the plan's evidence. Invalid items are dropped with their concrete reasons;
/// if NOTHING valid remains the plan is rejected — a reviewer-first plan with no valid
/// typed source has nothing grounding it (mirroring the parse boundary's nonempty
/// evidence requirement, which this path could previously defeat by dropping every item).
fn sanitize_evidence(
    plan: &mut VisualizationPlan,
    facts: &dyn FactView,
    dropped: &mut Vec<DroppedItem>,
) -> Result<(), String> {
    if plan.evidence.len() > MAX_PLAN_EVIDENCE {
        for evidence in plan.evidence.drain(MAX_PLAN_EVIDENCE..) {
            dropped.push(DroppedItem {
                subject: format!("evidence {}", evidence.file),
                reason: format!("exceeds evidence cap ({MAX_PLAN_EVIDENCE})"),
            });
        }
    }
    let evidence = std::mem::take(&mut plan.evidence);
    let before = evidence.len();
    for mut item in evidence {
        // A focused diff can be perfectly reviewable even when its language has no
        // semantic provider (YAML is the common case). Models occasionally decorate an
        // otherwise exact file+hunk citation with an English concept such as `changes`
        // in the `symbol` field. When the hunk itself resolves but that symbol universe
        // was never queried, retain the exact diff evidence and strip only the
        // unverifiable semantic decoration. Do not do this for a complete analysis that
        // proved the symbol absent, or without an exact hunk to ground the citation.
        if let (Some(hunk), Some(symbol)) = (item.hunk, item.symbol.clone()) {
            if matches!(facts.file(&item.file), Lookup::Present(()))
                && matches!(facts.hunk(&item.file, hunk), Lookup::Present(()))
                && matches!(facts.symbol(&item.file, &symbol), Lookup::Unknown)
            {
                item.symbol = None;
                item.range = None;
                dropped.push(DroppedItem {
                    subject: format!("evidence {}#h{hunk}", item.file),
                    reason: format!(
                        "symbol-level detail {symbol} was unavailable; retained exact hunk evidence"
                    ),
                });
            }
        }
        if let Some(reason) = evidence_invalid_reason(&item, facts) {
            dropped.push(DroppedItem {
                subject: format!("evidence {}", item.file),
                reason,
            });
        } else {
            plan.evidence.push(item);
        }
    }
    if before > 0 && plan.evidence.is_empty() {
        return Err(
            "no valid evidence remains: every cited source was dropped - cite at least one \
             exact supplied file with a zero-based hunk, or an exact catalog symbol or range"
                .to_string(),
        );
    }
    Ok(())
}

/// Why an exact node-to-diff range failed validation, or `None` when every referenced
/// line exists on the declared side of the declared hunk.
fn code_ref_invalid_reason(code_ref: &PlanCodeRef, facts: &dyn FactView) -> Option<String> {
    if !facts.is_focus_file(&code_ref.file) {
        return Some(format!(
            "code_ref {}#h{} is outside the focused selection scope",
            code_ref.file, code_ref.hunk
        ));
    }
    if code_ref.start_line == 0 || code_ref.end_line < code_ref.start_line {
        return Some(format!(
            "code_ref {}#h{} has invalid inclusive range {}..{}",
            code_ref.file, code_ref.hunk, code_ref.start_line, code_ref.end_line
        ));
    }
    let lines = code_ref.end_line - code_ref.start_line + 1;
    if lines > MAX_CODE_REF_LINES {
        return Some(format!(
            "code_ref {}#h{} covers {lines} lines (max {MAX_CODE_REF_LINES})",
            code_ref.file, code_ref.hunk
        ));
    }
    match facts.hunk(&code_ref.file, code_ref.hunk) {
        Lookup::Present(()) => {}
        Lookup::Absent => {
            return Some(format!(
                "code_ref hunk {}#h{} does not exist",
                code_ref.file, code_ref.hunk
            ));
        }
        Lookup::Unknown => {
            return Some(format!(
                "code_ref hunk {}#h{} not queried (cannot validate)",
                code_ref.file, code_ref.hunk
            ));
        }
    }
    for line in code_ref.start_line..=code_ref.end_line {
        match facts.diff_line(&code_ref.file, code_ref.hunk, code_ref.side, line) {
            Lookup::Present(()) => {}
            Lookup::Absent => {
                return Some(format!(
                    "code_ref {}#h{} {:?} line {line} is not in that hunk",
                    code_ref.file, code_ref.hunk, code_ref.side
                ));
            }
            Lookup::Unknown => {
                return Some(format!(
                    "code_ref {}#h{} {:?} line {line} not queried (cannot validate)",
                    code_ref.file, code_ref.hunk, code_ref.side
                ));
            }
        }
    }
    None
}

/// Why a node failed validation, or `None` when it is valid.
fn node_invalid_reason(
    node: &PlanNode,
    form_kind: FormClass,
    facts: &dyn FactView,
) -> Option<String> {
    // AI-facing parsing requires at least one ref per node. Validator-only callers may
    // still construct legacy/internal nodes without refs; any refs that are present must
    // be exact and fully valid before they can drive highlighting.
    if node.code_refs.len() > MAX_NODE_CODE_REFS {
        return Some(format!(
            "node has {} code_refs (max {MAX_NODE_CODE_REFS})",
            node.code_refs.len()
        ));
    }
    if let Some(reason) = node
        .code_refs
        .iter()
        .find_map(|code_ref| code_ref_invalid_reason(code_ref, facts))
    {
        return Some(reason);
    }
    if !node.code_refs.is_empty() {
        let mut has_changed_line = false;
        let mut unknown_changed_line = None;
        for code_ref in &node.code_refs {
            for line in code_ref.start_line..=code_ref.end_line {
                match facts.changed_diff_line(&code_ref.file, code_ref.hunk, code_ref.side, line) {
                    Lookup::Present(()) => has_changed_line = true,
                    Lookup::Absent => {}
                    Lookup::Unknown => {
                        unknown_changed_line.get_or_insert_with(|| {
                            format!(
                                "code_ref {}#h{} {:?} line {line} changed status not queried (cannot validate)",
                                code_ref.file, code_ref.hunk, code_ref.side
                            )
                        });
                    }
                }
            }
        }
        if !has_changed_line {
            return Some(unknown_changed_line.unwrap_or_else(|| {
                "node code_refs cite only unchanged context; cite at least one added/removed line"
                    .to_string()
            }));
        }
    }
    if node
        .detail
        .as_deref()
        .is_none_or(|detail| detail.trim().is_empty())
    {
        return Some("node has no reviewer-facing detail".to_string());
    }
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
            match facts.hunk(&entity.file, index) {
                Lookup::Present(()) => {}
                Lookup::Absent => {
                    return Some(format!("hunk {}#h{index} does not exist", entity.file));
                }
                Lookup::Unknown => {
                    return Some(format!(
                        "hunk {}#h{index} not queried (cannot validate)",
                        entity.file
                    ));
                }
            }
            None
        }
        _ => {
            let Some(entity) = &node.entity else {
                return None; // presentational node
            };
            match facts.file(&entity.file) {
                Lookup::Present(()) => {}
                Lookup::Absent => return Some(format!("file {} does not exist", entity.file)),
                Lookup::Unknown => {
                    return Some(format!(
                        "file {} not queried (cannot validate)",
                        entity.file
                    ));
                }
            }
            if let Some(symbol) = &entity.symbol {
                let extent = match facts.symbol(&entity.file, symbol) {
                    Lookup::Present(extent) => extent,
                    Lookup::Absent => {
                        return Some(format!(
                            "symbol {symbol} not found in {} (analyzed)",
                            entity.file
                        ));
                    }
                    Lookup::Unknown => {
                        return Some(format!(
                            "symbol {symbol} not queried in {} (cannot validate)",
                            entity.file
                        ));
                    }
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

/// Stable wire name for focused repair feedback.
fn edge_kind_name(kind: PlanEdgeKind) -> &'static str {
    match kind {
        PlanEdgeKind::Calls => "calls",
        PlanEdgeKind::Imports => "imports",
        PlanEdgeKind::Implements => "implements",
        PlanEdgeKind::Contains => "contains",
        PlanEdgeKind::Reads => "reads",
        PlanEdgeKind::Writes => "writes",
        PlanEdgeKind::FlowsTo => "flows_to",
    }
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
    if !is_reviewer_visual(form.kind) {
        return Err("list-shaped forms are not renderable reviewer visuals".to_string());
    }
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

    // BeforeAfter renders exactly nodes[0] (before) and nodes[1] (after) with at most one
    // transition edge (render_before_after). The renderer silently ignores anything past
    // two nodes, children, and extra edges, so validation must reject the shape instead
    // of losing content.
    if form.kind == FormKind::BeforeAfter {
        check_before_after_shape(form)?;
        downgrade_unqueried_before_after_entities(form, form_idx, facts, dropped);
    }

    if form.kind != FormKind::Sequence {
        if let Some(edge) = form
            .edges
            .iter()
            .find(|edge| edge.kind == PlanEdgeKind::FlowsTo)
        {
            return Err(format!(
                "flows_to edge {} -> {} is only valid in a sequence form",
                edge.from, edge.to
            ));
        }
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

/// Preserve a diff-grounded before/after visual when semantic coverage is unavailable.
///
/// Before/after nodes describe states, not graph relationships, and the renderer already
/// treats entityless nodes as inferred from their exact code references. A model-provided
/// symbol is therefore optional metadata here: if the symbol universe was never queried,
/// strip it only when at least one exact diff line fully validates. This mirrors the
/// evidence downgrade and avoids rejecting an otherwise honest atomic-change visual.
/// Authoritative [`Lookup::Absent`] results still flow into normal validation unchanged.
fn downgrade_unqueried_before_after_entities(
    form: &mut VizForm,
    form_idx: usize,
    facts: &dyn FactView,
    dropped: &mut Vec<DroppedItem>,
) {
    for node in &mut form.nodes {
        let Some(entity) = node.entity.as_ref() else {
            continue;
        };
        let Some(symbol) = entity.symbol.as_deref() else {
            continue;
        };
        if !matches!(facts.file(&entity.file), Lookup::Present(()))
            || !matches!(facts.symbol(&entity.file, symbol), Lookup::Unknown)
            || node.code_refs.is_empty()
            || node
                .code_refs
                .iter()
                .any(|code_ref| code_ref_invalid_reason(code_ref, facts).is_some())
        {
            continue;
        }
        let file = entity.file.clone();
        let symbol = symbol.to_string();
        node.entity = None;
        dropped.push(DroppedItem {
            subject: format!("node {} in form {form_idx}", node.id),
            reason: format!(
                "symbol-level identity {symbol} was unavailable in {file}; retained presentational before/after state from exact code_refs"
            ),
        });
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
    if form.nodes.len() < 2 {
        return Err("relationship visual needs at least two nodes".to_string());
    }
    if form.edges.is_empty() {
        return Err("relationship visual needs at least one labeled edge".to_string());
    }
    for (i, reason) in validity.iter().enumerate() {
        if let Some(reason) = reason {
            return Err(format!("endpoint {} invalid: {reason}", form.nodes[i].id));
        }
    }

    // Sequences first reduce the raw edge set to the retained consecutive chain, so an
    // irrelevant back/cross/duplicate edge with a blank label or an unknown endpoint is
    // dropped rather than poisoning the form. Only then do the generic label, endpoint,
    // connectivity, and fact checks run — on the retained required edges. A required
    // consecutive edge that is itself malformed still rejects (no synthesis, no rescue).
    // RelationshipFlow keeps the original order and strictness.
    if form.kind == FormKind::Sequence {
        sanitize_sequence_edges(form, form_idx, dropped)?;
    }

    // Every retained edge endpoint must name a declared node.
    for edge in &form.edges {
        if edge
            .label
            .as_deref()
            .is_none_or(|label| label.trim().is_empty())
        {
            return Err(format!(
                "edge {} -> {} has no explanatory label",
                edge.from, edge.to
            ));
        }
        for endpoint in [&edge.from, &edge.to] {
            if !id_to_idx.contains_key(endpoint) {
                return Err(format!("edge references unknown node {endpoint:?}"));
            }
        }
    }

    let mut connected: HashSet<&str> = HashSet::new();
    let mut queue = VecDeque::from([form.nodes[0].id.as_str()]);
    while let Some(id) = queue.pop_front() {
        if !connected.insert(id) {
            continue;
        }
        for edge in &form.edges {
            if edge.from == id {
                queue.push_back(edge.to.as_str());
            } else if edge.to == id {
                queue.push_back(edge.from.as_str());
            }
        }
    }
    if connected.len() != form.nodes.len() {
        return Err("relationship visual is disconnected".to_string());
    }
    // `flows_to` is a presentational chronological transition, never a graph claim.
    // A Sequence may alternatively show a real, proven semantic relation; all other
    // Sequence semantic edges get repair guidance rather than an unverifiable-edge note.
    // RelationshipFlow retains its existing graph-validation behavior.
    for edge in &form.edges {
        if edge.kind == PlanEdgeKind::FlowsTo {
            if form.kind == FormKind::Sequence {
                continue;
            }
            return Err(format!(
                "flows_to edge {} -> {} is only valid in a sequence form",
                edge.from, edge.to
            ));
        }
        let from = &form.nodes[id_to_idx[&edge.from]];
        let to = &form.nodes[id_to_idx[&edge.to]];
        if form.kind == FormKind::Sequence {
            if matches!(
                (&from.entity, &to.entity),
                (Some(fe), Some(te)) if matches!(facts.edge(fe, te, edge.kind), Lookup::Present(()))
            ) {
                continue;
            }
            return Err(format!(
                "sequence edge {} -> {} uses {}; use flows_to for lifecycle order (actual call topology belongs call_tree/relationship_flow)",
                edge.from,
                edge.to,
                edge_kind_name(edge.kind)
            ));
        }
        if edge_kind_verifiable(edge.kind) {
            match (&from.entity, &to.entity) {
                (Some(fe), Some(te)) => match facts.edge(fe, te, edge.kind) {
                    Lookup::Present(()) => {}
                    Lookup::Absent => {
                        return Err(format!(
                            "edge {} -> {} ({:?}) not in the impact graph",
                            edge.from, edge.to, edge.kind
                        ));
                    }
                    Lookup::Unknown => {
                        return Err(format!(
                            "edge {} -> {} ({:?}) not queried (cannot validate)",
                            edge.from, edge.to, edge.kind
                        ));
                    }
                },
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

/// BeforeAfter shape contract: exactly two flat nodes, no children, at most one edge,
/// and that edge directed nodes[0].id -> nodes[1].id. Anything else would be silently
/// truncated by the renderer, so the form is rejected with a precise reason instead.
fn check_before_after_shape(form: &VizForm) -> Result<(), String> {
    if form.nodes.len() != 2 {
        return Err(format!(
            "before_after needs exactly two nodes (before, after); this form has {} - use a \
             tree or flow form for nested structure",
            form.nodes.len()
        ));
    }
    let with_children: Vec<&str> = form
        .nodes
        .iter()
        .filter(|node| !node.children.is_empty())
        .map(|node| node.id.as_str())
        .collect();
    if !with_children.is_empty() {
        return Err(format!(
            "before_after nodes must be flat; nodes {} carry children - use a tree or flow \
             form for nested structure",
            with_children.join(", ")
        ));
    }
    match form.edges.as_slice() {
        [] => Ok(()),
        [edge] => {
            // BeforeAfter classifies as a tree form, so sanitize_flow's nonempty-label
            // check never runs on it; the shape contract enforces the label itself.
            if edge
                .label
                .as_deref()
                .is_none_or(|label| label.trim().is_empty())
            {
                return Err(
                    "before_after transition edge needs an explanatory label naming the \
                     state change"
                        .to_string(),
                );
            }
            if edge.from == form.nodes[0].id && edge.to == form.nodes[1].id {
                Ok(())
            } else {
                Err(format!(
                    "before_after edge must run {} -> {} (before -> after); got {} -> {}",
                    form.nodes[0].id, form.nodes[1].id, edge.from, edge.to
                ))
            }
        }
        _ => Err(format!(
            "before_after allows at most one transition edge; this form has {}",
            form.edges.len()
        )),
    }
}

/// Reduce a sequence form's edges to exactly one directed edge per consecutive node
/// pair in document order (the renderer's linear-chain grammar). For each pair the FIRST
/// matching edge is kept with its label; back edges (e.g. `n5 -> n2`), cross edges, and
/// duplicate consecutive edges are dropped and recorded. Missing required pairs must have
/// been rejected by the caller already — this never synthesizes edges. Edge order in the
/// submission does not matter: each pair's first match wins regardless of position.
/// Sequence edge pipeline. Runs FIRST in `sanitize_flow`, before any generic label,
/// endpoint, connectivity, or fact check, so an irrelevant extra edge cannot poison the
/// form:
///
/// 1. For each consecutive node pair in document order, pick the best matching edge: the
///    first one with a non-blank label and both endpoints declared among the form's node
///    ids. If none matches that bar, retain the first raw match anyway — the generic
///    checks then reject with the real defect named ("no explanatory label" / "unknown
///    endpoint") rather than the misleading "no ordered edge". A pair with NO raw match
///    rejects immediately with the ordered-edge reason: missing pairs are never
///    synthesized.
/// 2. Every other edge (back/cross/duplicate/unmatched extras, including ones pointing at
///    unknown nodes) is dropped and recorded — with whatever defect it carried — so the
///    surviving chain is exactly one edge per consecutive pair in document order.
fn sanitize_sequence_edges(
    form: &mut VizForm,
    form_idx: usize,
    dropped: &mut Vec<DroppedItem>,
) -> Result<(), String> {
    let declared: HashSet<&str> = form.nodes.iter().map(|node| node.id.as_str()).collect();
    let edges = std::mem::take(&mut form.edges);
    let mut consumed: Vec<bool> = vec![false; edges.len()];
    let mut kept: Vec<PlanEdge> = Vec::new();

    for pair in form.nodes.windows(2) {
        let raw_match = edges
            .iter()
            .enumerate()
            .find(|(idx, edge)| !consumed[*idx] && edge.from == pair[0].id && edge.to == pair[1].id)
            .map(|(idx, _)| idx);
        let Some(first_raw) = raw_match else {
            form.edges = edges;
            return Err(format!(
                "sequence has no ordered edge {} -> {}",
                pair[0].id, pair[1].id
            ));
        };
        // Prefer the first well-formed match; fall back to the first raw match so the
        // generic checks reject on the genuine required-edge defect.
        let choice = edges
            .iter()
            .enumerate()
            .find(|(idx, edge)| {
                !consumed[*idx]
                    && edge.from == pair[0].id
                    && edge.to == pair[1].id
                    && edge
                        .label
                        .as_deref()
                        .is_some_and(|label| !label.trim().is_empty())
                    && declared.contains(edge.from.as_str())
                    && declared.contains(edge.to.as_str())
            })
            .map(|(idx, _)| idx)
            .unwrap_or(first_raw);
        consumed[choice] = true;
        kept.push(edges[choice].clone());
    }

    for (idx, edge) in edges.into_iter().enumerate() {
        if !consumed[idx] {
            dropped.push(DroppedItem {
                subject: format!("edge {} -> {} in form {form_idx}", edge.from, edge.to),
                reason: "nonconsecutive or duplicate sequence edge (sequence edges follow \
                          document order, one per pair)"
                    .to_string(),
            });
        }
    }
    form.edges = kept;
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
        // `flows_to` is handled as a presentational transition, not a fact-store edge.
        // Non-Sequence uses have already rejected in `sanitize_form`.
        if edge.kind == PlanEdgeKind::FlowsTo {
            form.edges.push(edge);
            continue;
        }
        if !ids.contains(&edge.from) || !ids.contains(&edge.to) {
            dropped.push(DroppedItem {
                subject: format!("edge {} -> {} in form {form_idx}", edge.from, edge.to),
                reason: "endpoint not present".to_string(),
            });
            continue;
        }
        if edge_kind_verifiable(edge.kind) {
            match (by_id.get(&edge.from), by_id.get(&edge.to)) {
                (Some(fe), Some(te)) => match facts.edge(fe, te, edge.kind) {
                    Lookup::Present(()) => {}
                    Lookup::Absent => {
                        dropped.push(DroppedItem {
                            subject: format!(
                                "edge {} -> {} in form {form_idx}",
                                edge.from, edge.to
                            ),
                            reason: format!("{:?} edge not in the impact graph", edge.kind),
                        });
                        continue;
                    }
                    Lookup::Unknown => {
                        dropped.push(DroppedItem {
                            subject: format!(
                                "edge {} -> {} in form {form_idx}",
                                edge.from, edge.to
                            ),
                            reason: format!("{:?} edge not queried (cannot validate)", edge.kind),
                        });
                        continue;
                    }
                },
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

    struct StubFacts {
        files: HashSet<String>,
        symbols: HashMap<(String, String), LineRange>,
        edges: HashSet<(String, String, PlanEdgeKind)>,
        hunks: HashSet<(String, u32)>,
        diff_lines: HashSet<(String, u32, DiffSide, u32)>,
        changed_diff_lines: HashSet<(String, u32, DiffSide, u32)>,
        changed_diff_lines_available: bool,
        focus_file: Option<String>,
        /// When true, symbol/edge misses are authoritative `Absent` (a complete query ran);
        /// when false they are `Unknown` (never queried). Default true to preserve the
        /// existing fixtures' closed-universe semantics.
        complete: bool,
    }

    impl Default for StubFacts {
        fn default() -> Self {
            StubFacts {
                files: HashSet::new(),
                symbols: HashMap::new(),
                edges: HashSet::new(),
                hunks: HashSet::new(),
                diff_lines: HashSet::new(),
                changed_diff_lines: HashSet::new(),
                changed_diff_lines_available: true,
                focus_file: None,
                complete: true,
            }
        }
    }

    impl StubFacts {
        /// Mark the universe unqueried: misses report `Unknown`, not `Absent`.
        fn incomplete(mut self) -> Self {
            self.complete = false;
            self
        }

        fn focused_on(mut self, file: &str) -> Self {
            self.focus_file = Some(file.to_string());
            self
        }
        fn changed_diff_lines_unavailable(mut self) -> Self {
            self.changed_diff_lines_available = false;
            self
        }

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

        fn with_diff_line(mut self, file: &str, index: u32, side: DiffSide, line: u32) -> Self {
            self.hunks.insert((file.to_string(), index));
            self.diff_lines
                .insert((file.to_string(), index, side, line));
            self.changed_diff_lines
                .insert((file.to_string(), index, side, line));
            self
        }

        fn with_context_diff_line(
            mut self,
            file: &str,
            index: u32,
            side: DiffSide,
            line: u32,
        ) -> Self {
            self.hunks.insert((file.to_string(), index));
            self.diff_lines
                .insert((file.to_string(), index, side, line));
            self
        }
    }

    impl FactView for StubFacts {
        fn is_focus_file(&self, file: &FileId) -> bool {
            self.focus_file
                .as_deref()
                .is_none_or(|focus| focus == file.as_path().as_str())
        }

        fn file(&self, file: &FileId) -> Lookup<()> {
            if self.files.contains(file.as_path().as_str()) {
                Lookup::Present(())
            } else {
                Lookup::Absent
            }
        }
        fn symbol(&self, file: &FileId, name: &str) -> Lookup<LineRange> {
            match self
                .symbols
                .get(&(file.as_path().as_str().to_string(), name.to_string()))
                .copied()
            {
                Some(extent) => Lookup::Present(extent),
                None if self.complete => Lookup::Absent,
                None => Lookup::Unknown,
            }
        }
        fn edge(&self, from: &EntityRef, to: &EntityRef, kind: PlanEdgeKind) -> Lookup<()> {
            if self
                .edges
                .contains(&(from.to_string(), to.to_string(), kind))
            {
                Lookup::Present(())
            } else if self.complete {
                Lookup::Absent
            } else {
                Lookup::Unknown
            }
        }
        fn hunk(&self, file: &FileId, index: u32) -> Lookup<()> {
            if self
                .hunks
                .contains(&(file.as_path().as_str().to_string(), index))
            {
                Lookup::Present(())
            } else {
                Lookup::Absent
            }
        }

        fn diff_line(&self, file: &FileId, index: u32, side: DiffSide, line: u32) -> Lookup<()> {
            if self
                .diff_lines
                .contains(&(file.as_path().as_str().to_string(), index, side, line))
            {
                Lookup::Present(())
            } else if self
                .hunks
                .contains(&(file.as_path().as_str().to_string(), index))
            {
                Lookup::Absent
            } else {
                Lookup::Unknown
            }
        }
        fn changed_diff_line(
            &self,
            file: &FileId,
            index: u32,
            side: DiffSide,
            line: u32,
        ) -> Lookup<()> {
            if self.changed_diff_lines.contains(&(
                file.as_path().as_str().to_string(),
                index,
                side,
                line,
            )) {
                Lookup::Present(())
            } else if !self.changed_diff_lines_available {
                Lookup::Unknown
            } else if self
                .hunks
                .contains(&(file.as_path().as_str().to_string(), index))
            {
                Lookup::Absent
            } else {
                Lookup::Unknown
            }
        }
    }

    fn sym_entity(file: &str, name: &str) -> EntityRef {
        EntityRef::for_symbol(FileId::new_unchecked(file), name, None)
    }

    fn node(id: &str, entity: Option<EntityRef>, children: &[&str]) -> PlanNode {
        let mut n = PlanNode::new(id, id, PlanNodeChange::Modified)
            .with_detail(format!("explains the role of {id}"));
        n.entity = entity;
        n.children = children.iter().map(|c| (*c).to_string()).collect();
        n
    }

    fn form(kind: FormKind, nodes: Vec<PlanNode>, edges: Vec<PlanEdge>) -> VizForm {
        VizForm { kind, nodes, edges }
    }

    fn plan_with(forms: Vec<VizForm>) -> VisualizationPlan {
        let mut p = VisualizationPlan::new(Epoch(1));
        p.intent = "The selected code changes its runtime relationship.".into();
        p.forms = forms;
        p
    }

    fn edge(from: &str, to: &str, kind: PlanEdgeKind) -> PlanEdge {
        PlanEdge {
            from: from.into(),
            to: to.into(),
            kind,
            label: Some(format!("{kind:?} from {from} to {to}")),
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
            FormKind::ChangedSymbolTree,
            vec![node("n1", Some(sym_entity("main.go", "A")), &[])],
            vec![],
        )]);
        plan.epoch = Epoch(1);
        let report = validate(&mut plan, &abc_facts(), Epoch(2));
        assert_eq!(report.verdict, ValidationVerdict::Stale);
        assert!(!report.is_renderable());
        // Plan stays untouched for caller diagnostics/seed handling, but is not renderable.
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
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.subject.starts_with("form 0") && d.reason.contains(">20%"))
        );
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
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("root node n1 invalid"))
        );
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
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("endpoint n2 invalid"))
        );
    }

    #[test]
    fn nonsequence_flows_to_is_rejected_with_targeted_reason() {
        for kind in [
            FormKind::ChangedSymbolTree,
            FormKind::CallTree,
            FormKind::TypeImplTree,
            FormKind::RelationshipFlow,
            FormKind::BeforeAfter,
        ] {
            let mut plan = plan_with(vec![form(
                kind,
                vec![node("n1", None, &[]), node("n2", None, &[])],
                vec![edge("n1", "n2", PlanEdgeKind::FlowsTo)],
            )]);
            let report = validate(&mut plan, &StubFacts::default(), Epoch(1));
            assert_eq!(report.verdict, ValidationVerdict::Rejected, "{kind:?}");
            assert!(report.dropped.iter().any(|item| {
                item.reason
                    .contains("flows_to edge n1 -> n2 is only valid in a sequence form")
            }));
        }
    }

    /// A presentational Sequence can describe lifecycle phases even when no node maps to
    /// a fact-store entity. `flows_to` must not query `FactView::edge` or add an
    /// unverifiable/not-verifiable edge note.
    #[test]
    fn sequence_conceptual_flows_to_chain_is_valid_without_edge_note() {
        let mut plan = plan_with(vec![form(
            FormKind::Sequence,
            vec![
                node("n1", None, &[]),
                node("n2", None, &[]),
                node("n3", None, &[]),
            ],
            vec![
                edge("n1", "n2", PlanEdgeKind::FlowsTo),
                edge("n2", "n3", PlanEdgeKind::FlowsTo),
            ],
        )]);
        let report = validate(&mut plan, &StubFacts::default(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Valid);
        assert!(
            !report.notes.iter().any(|note| note.contains("edge")),
            "flows_to has no graph-verification note: {:?}",
            report.notes
        );
    }

    /// Unproven semantic Sequence edges need actionable repair guidance. A semantic edge
    /// belongs only when FactView proves it; otherwise it should be a presentational
    /// `flows_to` lifecycle transition.
    #[test]
    fn sequence_unproven_semantic_edge_gets_flows_to_repair() {
        let a = sym_entity("main.go", "A");
        let b = sym_entity("main.go", "B");
        for facts in [abc_facts(), abc_facts().incomplete()] {
            let mut plan = plan_with(vec![form(
                FormKind::Sequence,
                vec![
                    node("n1", Some(a.clone()), &[]),
                    node("n2", Some(b.clone()), &[]),
                ],
                vec![edge("n1", "n2", PlanEdgeKind::Calls)],
            )]);
            let report = validate(&mut plan, &facts, Epoch(1));
            assert_eq!(report.verdict, ValidationVerdict::Rejected);
            assert!(
                report.dropped.iter().any(|item| item.reason.contains(
                    "sequence edge n1 -> n2 uses calls; use flows_to for lifecycle order"
                ))
            );
            assert!(report.dropped.iter().any(|item| {
                item.reason
                    .contains("actual call topology belongs call_tree/relationship_flow")
            }));
        }
    }

    #[test]
    fn sequence_entityless_semantic_edge_gets_flows_to_repair() {
        let mut plan = plan_with(vec![form(
            FormKind::Sequence,
            vec![node("n1", None, &[]), node("n2", None, &[])],
            vec![edge("n1", "n2", PlanEdgeKind::Calls)],
        )]);
        let report = validate(&mut plan, &StubFacts::default(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(report.dropped.iter().any(|item| {
            item.reason
                .contains("sequence edge n1 -> n2 uses calls; use flows_to for lifecycle order")
        }));
    }

    /// Review 19: an unqueried symbol is "not queried", distinct from a proven-absent one.
    #[test]
    fn unqueried_symbol_is_unknown_not_absent() {
        let facts = abc_facts().incomplete();
        let mut plan = plan_with(vec![form(
            FormKind::ChangedSymbolTree,
            vec![node("n1", Some(sym_entity("main.go", "ZZZ")), &[])],
            vec![],
        )]);
        let report = validate(&mut plan, &facts, Epoch(1));
        assert!(
            report.dropped.iter().any(|d| d
                .reason
                .contains("not queried in main.go (cannot validate)")),
            "unqueried symbol is honest about coverage: {:?}",
            report.dropped
        );
        assert!(
            !report
                .dropped
                .iter()
                .any(|d| d.reason.contains("not found")),
            "unqueried must not be misreported as proven-absent"
        );
    }

    /// Review 21 m5: a verifiable edge on a NON-flow form is dropped when Unknown
    /// (unqueried), never retained as a rendered relationship.
    #[test]
    fn non_flow_unknown_edge_is_dropped_not_retained() {
        let a = sym_entity("main.go", "A");
        let b = sym_entity("main.go", "B");
        let facts = abc_facts().incomplete();
        let mut plan = plan_with(vec![form(
            FormKind::ChangedSymbolTree,
            vec![node("n1", Some(a), &[]), node("n2", Some(b), &[])],
            vec![edge("n1", "n2", PlanEdgeKind::Calls)],
        )]);
        let report = validate(&mut plan, &facts, Epoch(1));
        // The edge is dropped with the honest coverage reason; the form survives without it.
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("not queried (cannot validate)")),
            "unknown non-flow edge dropped honestly: {:?}",
            report.dropped
        );
        assert!(
            plan.forms[0].edges.is_empty(),
            "unknown edge never rendered"
        );
    }

    /// Review 21 m5: a complete (analyzed) miss for a symbol is Absent and says "not
    /// found", distinct from the unqueried "not queried" wording.
    #[test]
    fn analyzed_missing_symbol_says_not_found() {
        let facts = abc_facts(); // complete universe
        let mut plan = plan_with(vec![form(
            FormKind::ChangedSymbolTree,
            vec![node("n1", Some(sym_entity("main.go", "ZZZ")), &[])],
            vec![],
        )]);
        let report = validate(&mut plan, &facts, Epoch(1));
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("not found in main.go (analyzed)"))
        );
        assert!(
            !report
                .dropped
                .iter()
                .any(|d| d.reason.contains("not queried"))
        );
    }

    #[test]
    fn sequence_accepts_proven_semantic_edge() {
        let a = sym_entity("main.go", "A");
        let b = sym_entity("main.go", "B");
        let facts = abc_facts().with_edge(&a, &b, PlanEdgeKind::Calls);
        let mut plan = plan_with(vec![form(
            FormKind::Sequence,
            vec![node("n1", Some(a), &[]), node("n2", Some(b), &[])],
            vec![edge("n1", "n2", PlanEdgeKind::Calls)],
        )]);
        let report = validate(&mut plan, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Valid);
        assert!(report.notes.is_empty());
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
    fn legacy_list_form_is_rejected_as_a_primary_visual() {
        let mut plan = plan_with(vec![form(
            FormKind::ImpactSummary,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &[]),
                node("n2", Some(sym_entity("ghost.go", "Ghost")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("structural relationship visual"))
        );
    }

    #[test]
    fn evidence_hunks_are_rechecked_by_reference() {
        let facts = abc_facts().with_hunk("main.go", 0).with_hunk("main.go", 2);
        let mut plan = plan_with(vec![form(
            FormKind::ChangedSymbolTree,
            vec![node("n1", Some(sym_entity("main.go", "A")), &[])],
            vec![],
        )]);
        plan.evidence = [0, 1, 2]
            .into_iter()
            .map(|hunk| PlanEvidence {
                file: FileId::new_unchecked("main.go"),
                hunk: Some(hunk),
                symbol: None,
                range: None,
                reason: format!("supports hunk {hunk}"),
            })
            .collect();
        let report = validate(&mut plan, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        let hunks: Vec<u32> = plan.evidence.iter().filter_map(|item| item.hunk).collect();
        assert_eq!(hunks, [0, 2]);
        assert!(
            report
                .dropped
                .iter()
                .any(|item| item.reason.contains("#h1"))
        );
    }

    #[test]
    fn node_code_refs_require_exact_lines_on_the_declared_hunk_side() {
        let facts = abc_facts()
            .focused_on("main.go")
            .with_diff_line("main.go", 0, DiffSide::New, 42)
            .with_diff_line("main.go", 0, DiffSide::New, 43)
            .with_diff_line("main.go", 0, DiffSide::New, 44)
            .with_diff_line("main.go", 0, DiffSide::Old, 41);
        let make_plan = |side, start_line, end_line| {
            let mut focus = node("n1", Some(sym_entity("main.go", "A")), &[]);
            focus.code_refs.push(PlanCodeRef::new(
                FileId::new_unchecked("main.go"),
                0,
                side,
                start_line,
                end_line,
            ));
            plan_with(vec![form(FormKind::ChangedSymbolTree, vec![focus], vec![])])
        };

        let mut valid = make_plan(DiffSide::New, 42, 44);
        let report = validate(&mut valid, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Valid);
        assert_eq!(valid.forms[0].nodes[0].code_refs[0].end_line, 44);

        let mut missing = make_plan(DiffSide::New, 42, 45);
        let report = validate(&mut missing, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|item| item.reason.contains("New line 45 is not in that hunk"))
        );

        let mut cross_file_node = node("n1", Some(sym_entity("main.go", "A")), &[]);
        cross_file_node.code_refs.push(PlanCodeRef::new(
            FileId::new_unchecked("other.go"),
            0,
            DiffSide::New,
            42,
            42,
        ));
        let mut cross_file = plan_with(vec![form(
            FormKind::ChangedSymbolTree,
            vec![cross_file_node],
            vec![],
        )]);
        let report = validate(&mut cross_file, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|item| item.reason.contains("outside the focused selection scope"))
        );

        let mut wrong_side = make_plan(DiffSide::Old, 42, 42);
        let report = validate(&mut wrong_side, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|item| item.reason.contains("Old line 42 is not in that hunk"))
        );
    }

    #[test]
    fn code_refs_need_one_changed_line_but_may_include_context() {
        let facts = abc_facts()
            .focused_on("main.go")
            .with_context_diff_line("main.go", 0, DiffSide::New, 42)
            .with_context_diff_line("main.go", 0, DiffSide::New, 43)
            .with_diff_line("main.go", 0, DiffSide::New, 44)
            .with_diff_line("main.go", 0, DiffSide::Old, 41);
        let make_plan = |refs: Vec<PlanCodeRef>| {
            let mut focus = node("n1", Some(sym_entity("main.go", "A")), &[]);
            focus.code_refs = refs;
            plan_with(vec![form(FormKind::ChangedSymbolTree, vec![focus], vec![])])
        };

        let mut context_only = make_plan(vec![PlanCodeRef::new(
            FileId::new_unchecked("main.go"),
            0,
            DiffSide::New,
            42,
            42,
        )]);
        let report = validate(&mut context_only, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(report.dropped.iter().any(|item| item.reason.contains(
            "node code_refs cite only unchanged context; cite at least one added/removed line"
        )));

        let mut two_context_refs = make_plan(vec![
            PlanCodeRef::new(FileId::new_unchecked("main.go"), 0, DiffSide::New, 42, 42),
            PlanCodeRef::new(FileId::new_unchecked("main.go"), 0, DiffSide::New, 43, 43),
        ]);
        let report = validate(&mut two_context_refs, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(report.dropped.iter().any(|item| item.reason.contains(
            "node code_refs cite only unchanged context; cite at least one added/removed line"
        )));

        let mut context_and_change = make_plan(vec![
            PlanCodeRef::new(FileId::new_unchecked("main.go"), 0, DiffSide::New, 42, 42),
            PlanCodeRef::new(FileId::new_unchecked("main.go"), 0, DiffSide::New, 44, 44),
        ]);
        assert_eq!(
            validate(&mut context_and_change, &facts, Epoch(1)).verdict,
            ValidationVerdict::Valid
        );

        let mut removed_line = make_plan(vec![PlanCodeRef::new(
            FileId::new_unchecked("main.go"),
            0,
            DiffSide::Old,
            41,
            41,
        )]);
        assert_eq!(
            validate(&mut removed_line, &facts, Epoch(1)).verdict,
            ValidationVerdict::Valid
        );
    }

    #[test]
    fn unavailable_changed_line_fact_rejects_deterministically() {
        let facts = abc_facts()
            .focused_on("main.go")
            .with_context_diff_line("main.go", 0, DiffSide::New, 42)
            .changed_diff_lines_unavailable();
        let mut focus = node("n1", Some(sym_entity("main.go", "A")), &[]);
        focus.code_refs.push(PlanCodeRef::new(
            FileId::new_unchecked("main.go"),
            0,
            DiffSide::New,
            42,
            42,
        ));
        let mut plan = plan_with(vec![form(FormKind::ChangedSymbolTree, vec![focus], vec![])]);
        let report = validate(&mut plan, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(report.dropped.iter().any(|item| {
            item.reason
                .contains("changed status not queried (cannot validate)")
        }));
    }

    #[test]
    fn caps_forms_and_nodes_with_notes() {
        let item = |i: usize| node(&format!("n{i}"), Some(sym_entity("main.go", "A")), &[]);
        // Form 0: 14 nodes → capped at 12. Form 2 is dropped (max 2 forms).
        let mut many_nodes: Vec<PlanNode> = (0..14).map(item).collect();
        for (i, n) in many_nodes.iter_mut().enumerate() {
            n.id = format!("n{i}");
        }
        let f0 = form(FormKind::ChangedSymbolTree, many_nodes, vec![]);
        let f1 = form(
            FormKind::ChangedSymbolTree,
            vec![node("m1", Some(sym_entity("main.go", "B")), &[])],
            vec![],
        );
        let f2 = form(
            FormKind::ChangedSymbolTree,
            vec![node("k1", Some(sym_entity("main.go", "C")), &[])],
            vec![],
        );
        let mut plan = plan_with(vec![f0, f1, f2]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.forms.len(), MAX_FORMS_PER_PLAN);
        assert_eq!(plan.forms[0].nodes.len(), MAX_FORM_NODES);
        assert!(report
            .dropped
            .iter()
            .any(|d| d.subject.starts_with("form 2") && d.reason.contains("MAX_FORMS_PER_PLAN")));
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("MAX_FORM_NODES"))
        );
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
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.subject.contains("c12") && d.reason.contains("MAX_FORM_NODES"))
        );
    }

    #[test]
    fn tree_depth_pruned_beyond_cap() {
        // Chain n1 -> n2 -> n3 -> n4: depth 4 exceeds MAX_FORM_DEPTH=3 → n4 pruned.
        // (A true tree form: before_after is strictly two flat nodes since the shape
        // contract was added.)
        let mut plan = plan_with(vec![form(
            FormKind::CallTree,
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
            FormKind::ChangedSymbolTree,
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
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("duplicate node id"))
        );
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
            FormKind::ChangedSymbolTree,
            vec![
                node("n1", Some(entity), &[]),
                node("n2", Some(sym_entity("main.go", "B")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("outside symbol extent"))
        );
        // In-extent range is fine.
        let mut ok = sym_entity("main.go", "A");
        ok.range = Some(LineRange::new(1, 0, 4, 0));
        let mut plan = plan_with(vec![form(
            FormKind::ChangedSymbolTree,
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
            FormKind::ChangedSymbolTree,
            vec![node("n1", Some(sym_entity("main.go", "A")), &[])],
            vec![],
        );
        let mut plan = plan_with(vec![bad_flow, good]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.forms.len(), 1);
        assert_eq!(plan.forms[0].kind, FormKind::ChangedSymbolTree);
    }

    #[test]
    fn list_edges_cleaned_not_rejected() {
        let a = sym_entity("main.go", "A");
        let b = sym_entity("main.go", "B");
        let facts = abc_facts().with_edge(&a, &b, PlanEdgeKind::Calls);
        let mut plan = plan_with(vec![form(
            FormKind::ChangedSymbolTree,
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
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("not in the impact graph"))
        );
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("endpoint not present"))
        );
    }

    #[test]
    fn file_level_entities_resolve_by_file_existence() {
        let facts = StubFacts::default().with_file("docs/readme.md");
        let mut plan = plan_with(vec![form(
            FormKind::ChangedSymbolTree,
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
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("does not exist"))
        );
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

    /// A five-node sequence with the four required consecutive edges plus an extra
    /// back-edge (the round-4 live shape: `n5 -> n2`). The back-edge would break the
    /// renderer's linear-chain detection and imply a cycle the source does not show; it
    /// must be dropped and recorded, leaving exactly nodes-1 ordered edges.
    #[test]
    fn sequence_back_edge_is_dropped_keeping_linear_chain() {
        let nodes: Vec<PlanNode> = ["A", "B", "C", "D", "E"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                node(
                    &format!("n{}", i + 1),
                    Some(sym_entity("main.go", name)),
                    &[],
                )
            })
            .collect();
        let mut edges: Vec<PlanEdge> = (1..5)
            .map(|i| {
                let mut e = edge(
                    &format!("n{i}"),
                    &format!("n{}", i + 1),
                    PlanEdgeKind::FlowsTo,
                );
                e.label = Some(format!("step {i} behavior"));
                e
            })
            .collect();
        let mut back = edge("n5", "n2", PlanEdgeKind::FlowsTo);
        back.label = Some("readiness listener closes last".into());
        edges.push(back);
        let mut plan = plan_with(vec![form(FormKind::Sequence, nodes, edges)]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        let f = &plan.forms[0];
        assert_eq!(f.nodes.len(), 5);
        assert_eq!(
            f.edges.len(),
            f.nodes.len() - 1,
            "exactly one ordered edge per consecutive pair: {:?}",
            f.edges
        );
        // The kept edges are the consecutive pairs in document order.
        for (i, edge) in f.edges.iter().enumerate() {
            assert_eq!(edge.from, format!("n{}", i + 1));
            assert_eq!(edge.to, format!("n{}", i + 2));
            assert_eq!(
                edge.label.as_deref(),
                Some(format!("step {} behavior", i + 1).as_str()),
                "first required edge/label preserved"
            );
        }
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.subject.contains("n5 -> n2") && d.reason.contains("document order"))
        );
    }

    /// A duplicate consecutive edge is a drop, not a rejection: the first edge and its
    /// label win, and the form stays linear and renderable.
    #[test]
    fn sequence_duplicate_consecutive_edge_is_dropped() {
        let nodes: Vec<PlanNode> = ["A", "B", "C"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                node(
                    &format!("n{}", i + 1),
                    Some(sym_entity("main.go", name)),
                    &[],
                )
            })
            .collect();
        let mut first = edge("n1", "n2", PlanEdgeKind::FlowsTo);
        first.label = Some("flips the health flag".into());
        let mut dup = edge("n1", "n2", PlanEdgeKind::FlowsTo);
        dup.label = Some("redundant second label".into());
        let mut third = edge("n2", "n3", PlanEdgeKind::FlowsTo);
        third.label = Some("probes return 503".into());
        let mut plan = plan_with(vec![form(
            FormKind::Sequence,
            nodes,
            vec![first, third, dup],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        let f = &plan.forms[0];
        assert_eq!(f.edges.len(), 2, "nodes-1 edges: {:?}", f.edges);
        assert_eq!(f.edges[0].label.as_deref(), Some("flips the health flag"));
        assert_eq!(f.edges[1].label.as_deref(), Some("probes return 503"));
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.subject.contains("n1 -> n2") && d.reason.contains("duplicate"))
        );
    }

    /// A missing consecutive pair still rejects the form (no synthesis): the repair loop,
    /// not the validator, must supply the edge.
    #[test]
    fn sequence_missing_ordered_edge_still_rejects() {
        let nodes: Vec<PlanNode> = ["A", "B", "C"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                node(
                    &format!("n{}", i + 1),
                    Some(sym_entity("main.go", name)),
                    &[],
                )
            })
            .collect();
        let mut only = edge("n1", "n2", PlanEdgeKind::FlowsTo);
        only.label = Some("one edge is not enough for three nodes".into());
        let mut plan = plan_with(vec![form(FormKind::Sequence, nodes, vec![only])]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        // The sanitizer runs before connectivity, so the missing pair is named directly
        // instead of the indirect "disconnected" reason.
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("sequence has no ordered edge n2 -> n3"))
        );
        let nodes: Vec<PlanNode> = ["A", "B", "C", "D"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                node(
                    &format!("n{}", i + 1),
                    Some(sym_entity("main.go", name)),
                    &[],
                )
            })
            .collect();
        // Connected chain n1->n2->n3->n4 minus the interior n2->n3 edge: the missing-pair
        // reason is the rejection cause, and no edge is synthesized.
        let mut edges: Vec<PlanEdge> = (1..=3)
            .filter(|i| *i != 2)
            .map(|i| {
                let mut e = edge(
                    &format!("n{i}"),
                    &format!("n{}", i + 1),
                    PlanEdgeKind::FlowsTo,
                );
                e.label = Some("step behavior".into());
                e
            })
            .collect();
        let mut cross = edge("n1", "n4", PlanEdgeKind::FlowsTo);
        cross.label = Some("keeps the form connected".into());
        edges.push(cross);
        let mut plan = plan_with(vec![form(FormKind::Sequence, nodes, edges)]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("sequence has no ordered edge n2 -> n3"))
        );
    }

    /// Sanitization runs before fact validation: an extra back-edge whose typed kind is
    /// not in the impact graph must not reject the form — the back-edge is simply dropped.
    #[test]
    fn sequence_extra_typed_back_edge_cannot_reject_the_form() {
        let a = sym_entity("main.go", "A");
        let b = sym_entity("main.go", "B");
        let nodes: Vec<PlanNode> = ["A", "B"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                node(
                    &format!("n{}", i + 1),
                    Some(sym_entity("main.go", name)),
                    &[],
                )
            })
            .collect();
        let mut required = edge("n1", "n2", PlanEdgeKind::FlowsTo);
        required.label = Some("state write drives handler response".into());
        // A Calls back-edge the complete universe proves absent: would reject if validated.
        let mut back = edge("n2", "n1", PlanEdgeKind::Calls);
        back.label = Some("misleading cycle".into());
        let mut plan = plan_with(vec![form(FormKind::Sequence, nodes, vec![required, back])]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(
            report.verdict,
            ValidationVerdict::ValidWithDrops,
            "the dropped back-edge must not be fact-checked: {:?}",
            report.dropped
        );
        assert_eq!(plan.forms[0].edges.len(), 1);
        assert!(plan.forms[0].edges[0].from == "n1" && plan.forms[0].edges[0].to == "n2");
        // Prove the Calls edge really is absent in this universe (so the test is honest).
        assert_eq!(
            abc_facts().edge(&a, &b, PlanEdgeKind::Calls),
            Lookup::Absent
        );
        assert_eq!(
            abc_facts().edge(&b, &a, PlanEdgeKind::Calls),
            Lookup::Absent
        );
    }

    /// A valid consecutive chain plus an irrelevant back-edge with a BLANK label: the
    /// extra edge must be sanitized away, not reject the form.
    #[test]
    fn sequence_blank_back_edge_is_sanitized_not_rejected() {
        let nodes: Vec<PlanNode> = ["A", "B", "C"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                node(
                    &format!("n{}", i + 1),
                    Some(sym_entity("main.go", name)),
                    &[],
                )
            })
            .collect();
        let mut edges: Vec<PlanEdge> = (1..3)
            .map(|i| {
                let mut e = edge(
                    &format!("n{i}"),
                    &format!("n{}", i + 1),
                    PlanEdgeKind::FlowsTo,
                );
                e.label = Some(format!("step {i} behavior"));
                e
            })
            .collect();
        let mut blank_back = edge("n3", "n1", PlanEdgeKind::FlowsTo);
        blank_back.label = Some("   ".into()); // blank label on an irrelevant extra
        edges.push(blank_back);
        let mut plan = plan_with(vec![form(FormKind::Sequence, nodes, edges)]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(
            report.verdict,
            ValidationVerdict::ValidWithDrops,
            "extras dropped, form kept: {:?}",
            report.dropped
        );
        assert_eq!(plan.forms[0].edges.len(), 2, "nodes-1 retained edges");
    }

    /// A valid consecutive chain plus a cross-edge pointing at an UNKNOWN node: sanitized
    /// away, not a rejection.
    #[test]
    fn sequence_unknown_endpoint_cross_edge_is_sanitized_not_rejected() {
        let nodes: Vec<PlanNode> = ["A", "B", "C"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                node(
                    &format!("n{}", i + 1),
                    Some(sym_entity("main.go", name)),
                    &[],
                )
            })
            .collect();
        let mut edges: Vec<PlanEdge> = (1..3)
            .map(|i| {
                let mut e = edge(
                    &format!("n{i}"),
                    &format!("n{}", i + 1),
                    PlanEdgeKind::FlowsTo,
                );
                e.label = Some(format!("step {i} behavior"));
                e
            })
            .collect();
        let mut cross = edge("n1", "ghost", PlanEdgeKind::FlowsTo);
        cross.label = Some("points nowhere".into());
        edges.push(cross);
        let mut plan = plan_with(vec![form(FormKind::Sequence, nodes, edges)]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.forms[0].edges.len(), 2);
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.subject.contains("n1 -> ghost"))
        );
    }

    /// A duplicate required pair where the first duplicate is blank and a later labeled
    /// edge exists: the valid labeled edge is preferred for the chain; the blank duplicate
    /// is dropped as an extra.
    #[test]
    fn sequence_duplicate_pair_prefers_the_valid_labeled_edge() {
        let nodes: Vec<PlanNode> = ["A", "B"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                node(
                    &format!("n{}", i + 1),
                    Some(sym_entity("main.go", name)),
                    &[],
                )
            })
            .collect();
        let mut blank = edge("n1", "n2", PlanEdgeKind::FlowsTo);
        blank.label = Some("   ".into());
        let mut labeled = edge("n1", "n2", PlanEdgeKind::FlowsTo);
        labeled.label = Some("flips the health flag".into());
        let mut plan = plan_with(vec![form(FormKind::Sequence, nodes, vec![blank, labeled])]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.forms[0].edges.len(), 1);
        assert_eq!(
            plan.forms[0].edges[0].label.as_deref(),
            Some("flips the health flag")
        );
    }

    /// A required consecutive edge whose ONLY match is blank still rejects with the label
    /// defect named — the sanitizer never rescues a genuinely malformed required edge.
    #[test]
    fn sequence_required_blank_edge_still_rejects_with_label_reason() {
        let nodes: Vec<PlanNode> = ["A", "B"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                node(
                    &format!("n{}", i + 1),
                    Some(sym_entity("main.go", name)),
                    &[],
                )
            })
            .collect();
        let mut blank = edge("n1", "n2", PlanEdgeKind::FlowsTo);
        blank.label = Some("   ".into());
        let mut plan = plan_with(vec![form(FormKind::Sequence, nodes, vec![blank])]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("has no explanatory label"))
        );
    }

    /// A sole evidence item that is invalid (nonexistent hunk) rejects the plan; the
    /// concrete dropped reason stays in the report for repair feedback.
    #[test]
    fn sole_invalid_evidence_rejects_with_dropped_reason() {
        let facts = abc_facts().with_hunk("main.go", 0);
        let mut plan = plan_with(vec![form(
            FormKind::Sequence,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &[]),
                node("n2", Some(sym_entity("main.go", "B")), &[]),
            ],
            vec![edge("n1", "n2", PlanEdgeKind::FlowsTo)],
        )]);
        plan.evidence.push(PlanEvidence {
            file: FileId::new_unchecked("main.go"),
            hunk: Some(9), // only hunk 0 exists
            symbol: None,
            range: None,
            reason: "cites a hunk that does not exist".into(),
        });
        let report = validate(&mut plan, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        // The concrete dropped reason is preserved (not flattened into the terminal note).
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.subject.contains("evidence") && d.reason.contains("#h9"))
        );
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("no valid evidence remains"))
        );
        assert!(plan.evidence.is_empty());
    }

    /// Mixed valid + invalid evidence stays ValidWithDrops: the invalid item is dropped,
    /// the valid one grounds the plan.
    #[test]
    fn mixed_evidence_stays_valid_with_drops() {
        let facts = abc_facts().with_hunk("main.go", 0);
        let mut plan = plan_with(vec![form(
            FormKind::Sequence,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &[]),
                node("n2", Some(sym_entity("main.go", "B")), &[]),
            ],
            vec![edge("n1", "n2", PlanEdgeKind::FlowsTo)],
        )]);
        plan.evidence.push(PlanEvidence {
            file: FileId::new_unchecked("main.go"),
            hunk: Some(0),
            symbol: None,
            range: None,
            reason: "valid citation of the changed hunk".into(),
        });
        plan.evidence.push(PlanEvidence {
            file: FileId::new_unchecked("main.go"),
            hunk: Some(9),
            symbol: None,
            range: None,
            reason: "invalid hunk citation".into(),
        });
        let report = validate(&mut plan, &facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.evidence.len(), 1);
        assert!(plan.evidence[0].hunk == Some(0));
        assert!(report.dropped.iter().any(|d| d.reason.contains("#h9")));
    }

    /// Non-semantic files still have exact diff facts. An invented symbol decoration on
    /// a valid YAML hunk must not discard the whole explanation: strip the unavailable
    /// symbol/range and retain the file+hunk citation.
    #[test]
    fn unqueried_symbol_on_exact_yaml_hunk_downgrades_to_diff_evidence() {
        let path = ".github/workflows/vm-sandbox-deploy.yaml";
        let facts = StubFacts::default()
            .incomplete()
            .with_file(path)
            .with_hunk(path, 0);
        let mut plan = plan_with(vec![form(
            FormKind::Sequence,
            vec![node("n1", None, &[]), node("n2", None, &[])],
            vec![edge("n1", "n2", PlanEdgeKind::FlowsTo)],
        )]);
        plan.evidence.push(PlanEvidence {
            file: FileId::new_unchecked(path),
            hunk: Some(0),
            symbol: Some("changes".to_string()),
            range: Some(LineRange::new(1, 0, 2, 0)),
            reason: "the workflow changes its deployment behavior".to_string(),
        });

        let report = validate(&mut plan, &facts, Epoch(1));

        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.evidence.len(), 1);
        assert_eq!(plan.evidence[0].hunk, Some(0));
        assert_eq!(plan.evidence[0].symbol, None);
        assert_eq!(plan.evidence[0].range, None);
        assert!(
            report
                .dropped
                .iter()
                .any(|item| { item.reason.contains("retained exact hunk evidence") })
        );
    }

    /// BeforeAfter contract: three nodes reject with the exact-two-nodes reason.
    #[test]
    fn before_after_rejects_three_nodes() {
        let nodes: Vec<PlanNode> = ["A", "B", "C"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                node(
                    &format!("n{}", i + 1),
                    Some(sym_entity("main.go", name)),
                    &[],
                )
            })
            .collect();
        let mut plan = plan_with(vec![form(FormKind::BeforeAfter, nodes, vec![])]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("before_after needs exactly two nodes"))
        );
    }

    /// BeforeAfter contract: children on a node reject with the flat-nodes reason.
    #[test]
    fn before_after_rejects_children() {
        let mut plan = plan_with(vec![form(
            FormKind::BeforeAfter,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &["n2"]),
                node("n2", Some(sym_entity("main.go", "B")), &[]),
                node("n3", Some(sym_entity("main.go", "C")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        // Three nodes also present; the two-node reason fires first by construction.
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("before_after needs exactly two nodes"))
        );
        // The flat case in isolation: exactly two nodes, one carries a child.
        let mut flat_violation = plan_with(vec![form(
            FormKind::BeforeAfter,
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &["n2"]),
                node("n2", Some(sym_entity("main.go", "B")), &[]),
            ],
            vec![],
        )]);
        let report = validate(&mut flat_violation, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("before_after nodes must be flat"))
        );
    }

    #[test]
    fn before_after_downgrades_unqueried_symbols_when_exact_diff_refs_exist() {
        let facts = StubFacts::default()
            .incomplete()
            .with_file("main.go")
            .with_diff_line("main.go", 0, DiffSide::New, 19);
        let state = |id: &str, symbol: &str| {
            node(id, Some(sym_entity("main.go", symbol)), &[]).with_code_ref(PlanCodeRef::new(
                FileId::new_unchecked("main.go"),
                0,
                DiffSide::New,
                19,
                19,
            ))
        };
        let mut plan = plan_with(vec![form(
            FormKind::BeforeAfter,
            vec![state("n1", "old diagnostic"), state("n2", "new diagnostic")],
            vec![],
        )]);

        let report = validate(&mut plan, &facts, Epoch(1));

        assert_eq!(report.verdict, ValidationVerdict::ValidWithDrops);
        assert_eq!(plan.forms.len(), 1, "the before/after visual survives");
        assert!(
            plan.forms[0]
                .nodes
                .iter()
                .all(|node| node.entity.is_none() && !node.code_refs.is_empty())
        );
        assert_eq!(
            report
                .dropped
                .iter()
                .filter(|item| item
                    .reason
                    .contains("retained presentational before/after state"))
                .count(),
            2
        );

        // A complete symbol query proving the same identities absent remains a hard
        // validation failure; only missing semantic coverage is recoverable.
        let complete_facts = StubFacts::default().with_file("main.go").with_diff_line(
            "main.go",
            0,
            DiffSide::New,
            19,
        );
        let mut disproven = plan_with(vec![form(
            FormKind::BeforeAfter,
            vec![state("n1", "old diagnostic"), state("n2", "new diagnostic")],
            vec![],
        )]);
        let report = validate(&mut disproven, &complete_facts, Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|item| item.reason.contains("not found in main.go (analyzed)"))
        );
    }

    /// BeforeAfter contract: two edges reject; a single reversed edge rejects; the valid
    /// no-edge and one-edge shapes pass.
    #[test]
    fn before_after_edge_direction_and_count() {
        let two_nodes = || {
            vec![
                node("n1", Some(sym_entity("main.go", "A")), &[]),
                node("n2", Some(sym_entity("main.go", "B")), &[]),
            ]
        };
        // No edge: valid.
        let mut plan = plan_with(vec![form(FormKind::BeforeAfter, two_nodes(), vec![])]);
        assert_eq!(
            validate(&mut plan, &abc_facts(), Epoch(1)).verdict,
            ValidationVerdict::Valid
        );
        // One correctly directed edge: valid (writes is unverifiable-kind, noted only).
        let mut ok_edge = edge("n1", "n2", PlanEdgeKind::Writes);
        ok_edge.label = Some("becomes".into());
        let mut plan = plan_with(vec![form(
            FormKind::BeforeAfter,
            two_nodes(),
            vec![ok_edge],
        )]);
        assert_eq!(
            validate(&mut plan, &abc_facts(), Epoch(1)).verdict,
            ValidationVerdict::Valid
        );
        // Reversed direction: rejected.
        let mut reversed = edge("n2", "n1", PlanEdgeKind::Writes);
        reversed.label = Some("wrong direction".into());
        let mut plan = plan_with(vec![form(
            FormKind::BeforeAfter,
            two_nodes(),
            vec![reversed],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("before_after edge must run n1 -> n2"))
        );
        // Two edges: rejected.
        let mut e1 = edge("n1", "n2", PlanEdgeKind::Writes);
        e1.label = Some("first".into());
        let mut e2 = edge("n1", "n2", PlanEdgeKind::Writes);
        e2.label = Some("second".into());
        let mut plan = plan_with(vec![form(FormKind::BeforeAfter, two_nodes(), vec![e1, e2])]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("at most one transition edge"))
        );
        // A missing label on the transition edge: rejected with the before_after reason
        // (the tree path never reaches the flow label check).
        let mut unlabeled = edge("n1", "n2", PlanEdgeKind::Writes);
        unlabeled.label = None;
        let mut plan = plan_with(vec![form(
            FormKind::BeforeAfter,
            two_nodes(),
            vec![unlabeled],
        )]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(report.dropped.iter().any(|d| {
            d.reason
                .contains("before_after transition edge needs an explanatory label")
        }));
        // A blank (whitespace-only) label is equally missing.
        let mut blank = edge("n1", "n2", PlanEdgeKind::Writes);
        blank.label = Some("   ".into());
        let mut plan = plan_with(vec![form(FormKind::BeforeAfter, two_nodes(), vec![blank])]);
        let report = validate(&mut plan, &abc_facts(), Epoch(1));
        assert_eq!(report.verdict, ValidationVerdict::Rejected);
        assert!(
            report
                .dropped
                .iter()
                .any(|d| d.reason.contains("explanatory label"))
        );
    }
}
