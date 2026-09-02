//! Width-aware terminal diagrams for validated AI plans and deterministic impact facts.
//!
//! The dispatcher deliberately publishes structure, not pre-rendered rows. This module is
//! the single layout boundary: it turns that structure into boxes and relationship connectors
//! for the pane width available during the current frame. Connectors distinguish
//! validator-verifiable relationships from hunk-derived interpretation.

use std::collections::{HashMap, HashSet};

use codescope_core::{
    FormKind, PlanEdge, PlanEdgeKind, PlanEvidence, PlanNode, VisualizationPlan, VizForm,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::action::{PlanNodeTarget, PlanRelationshipTarget};
use crate::snapshot::ImpactPane;

/// Semantic styling role for a span in a terminal diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagramRole {
    /// Behavioral title.
    Title,
    /// Intent sentence and node details.
    Text,
    /// Normal node border.
    Border,
    /// Border/label for the current selection.
    Selected,
    /// Node currently under the pointer; also links to highlighted diff rows.
    Hovered,
    /// Labeled connector or arrow.
    Arrow,
    /// Warning or generation-status message.
    Warning,
    /// Source evidence.
    Evidence,
    /// Low-emphasis separators and status.
    Muted,
}

/// One styled span in a physical terminal line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramSpan {
    /// Visible text.
    pub text: String,
    /// Semantic style role.
    pub role: DiagramRole,
    /// Plan node occupying these terminal cells, for retained hover hit-testing.
    pub target: Option<PlanNodeTarget>,
    /// Relationship label occupying these cells, for click-to-expand hit-testing.
    pub relationship: Option<PlanRelationshipTarget>,
}

/// One already width-bounded physical terminal line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagramLine {
    /// Styled spans in display order.
    pub spans: Vec<DiagramSpan>,
}

impl DiagramLine {
    pub(crate) fn plain(text: impl Into<String>, role: DiagramRole) -> Self {
        Self {
            spans: vec![DiagramSpan {
                text: text.into(),
                role,
                target: None,
                relationship: None,
            }],
        }
    }

    fn for_node(text: impl Into<String>, role: DiagramRole, target: PlanNodeTarget) -> Self {
        Self {
            spans: vec![DiagramSpan {
                text: text.into(),
                role,
                target: Some(target),
                relationship: None,
            }],
        }
    }

    fn for_relationship(
        text: impl Into<String>,
        role: DiagramRole,
        relationship: PlanRelationshipTarget,
    ) -> Self {
        Self {
            spans: vec![DiagramSpan {
                text: text.into(),
                role,
                target: None,
                relationship: Some(relationship),
            }],
        }
    }

    /// Plain text representation, primarily for golden tests.
    #[must_use]
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

#[derive(Clone, Copy)]
struct DiagramContext<'a> {
    form: usize,
    selected_label: &'a str,
    hovered: Option<&'a PlanNodeTarget>,
    expanded: &'a [PlanNodeTarget],
    expanded_relationships: &'a [PlanRelationshipTarget],
}

impl DiagramContext<'_> {
    fn target(&self, node: &PlanNode) -> PlanNodeTarget {
        PlanNodeTarget {
            form: self.form,
            id: node.id.clone(),
        }
    }

    fn role(&self, node: &PlanNode, normal: DiagramRole) -> DiagramRole {
        let target = self.target(node);
        if self.hovered == Some(&target) {
            DiagramRole::Hovered
        } else if node_selected(node, self.selected_label) {
            DiagramRole::Selected
        } else {
            normal
        }
    }

    fn expanded(&self, node: &PlanNode) -> bool {
        self.expanded.contains(&self.target(node))
    }

    fn relationship_target(&self, edge: &PlanEdge) -> PlanRelationshipTarget {
        PlanRelationshipTarget {
            form: self.form,
            from: edge.from.clone(),
            to: edge.to.clone(),
        }
    }

    fn relationship_expanded(&self, edge: &PlanEdge) -> bool {
        self.expanded_relationships
            .contains(&self.relationship_target(edge))
    }
}

const MIN_BOX_WIDTH: usize = 18;
const MAX_BOX_WIDTH: usize = 32;
const MIN_HORIZONTAL_GAP: usize = 10;
const MAX_HORIZONTAL_GAP: usize = 24;
/// Pane width at which evidence keeps a one-line reason per entry; below it the block
/// collapses to bare `basename:line` references.
const EVIDENCE_REASON_MIN_WIDTH: usize = 60;
/// Lay out a validated plan for the current pane width without transient interaction.
#[must_use]
pub fn plan_lines(plan: &VisualizationPlan, width: u16, selected_label: &str) -> Vec<DiagramLine> {
    interactive_plan_lines(plan, width, selected_label, None, &[], &[], &[])
}

/// Lay out a validated plan plus transient hover/expansion state.
///
/// Node-bearing spans retain their [`PlanNodeTarget`] so geometry can derive hitboxes from
/// exactly the same width-aware output that the renderer displays.
#[must_use]
pub fn interactive_plan_lines(
    plan: &VisualizationPlan,
    width: u16,
    selected_label: &str,
    hovered: Option<&PlanNodeTarget>,
    expanded: &[PlanNodeTarget],
    order: &[PlanNodeTarget],
    expanded_relationships: &[PlanRelationshipTarget],
) -> Vec<DiagramLine> {
    let width = usize::from(width).max(1);
    let mut lines = Vec::new();
    // The intent is the sole prose description above the visual.
    lines.extend(wrap_role(&plan.intent, width, DiagramRole::Text, 2));
    for (index, original_form) in plan.forms.iter().enumerate() {
        if index > 0 {
            lines.push(DiagramLine::plain("", DiagramRole::Muted));
        }
        let context = DiagramContext {
            form: index,
            selected_label,
            hovered,
            expanded,
            expanded_relationships,
        };
        let manually_ordered = order.iter().any(|target| target.form == index);
        let mut reordered;
        let form = if manually_ordered {
            reordered = original_form.clone();
            reordered.nodes.sort_by_key(|node| {
                order
                    .iter()
                    .position(|target| target.form == index && target.id == node.id)
                    .unwrap_or(usize::MAX)
            });
            &reordered
        } else {
            original_form
        };
        match form.kind {
            FormKind::RelationshipFlow | FormKind::Sequence => {
                if manually_ordered {
                    render_branching_flow(form, width, context, &mut lines);
                } else {
                    render_flow(form, width, context, &mut lines);
                }
            }
            FormKind::BeforeAfter => {
                if manually_ordered {
                    let mut graph = form.clone();
                    if graph.edges.is_empty() && original_form.nodes.len() >= 2 {
                        graph.edges.push(PlanEdge {
                            from: original_form.nodes[0].id.clone(),
                            to: original_form.nodes[1].id.clone(),
                            kind: PlanEdgeKind::Writes,
                            label: Some("becomes".to_string()),
                        });
                    }
                    render_branching_flow(&graph, width, context, &mut lines);
                } else {
                    render_before_after(form, width, context, &mut lines);
                }
            }
            FormKind::ChangedSymbolTree | FormKind::CallTree | FormKind::TypeImplTree => {
                render_tree(form, width, context, &mut lines);
            }
            // These legacy variants cannot pass v5 validation. Keeping a safe rendering
            // path makes stale fixtures and hand-built snapshots non-panicking.
            FormKind::ImpactSummary | FormKind::FocusedDiff => {
                render_vertical_nodes(form, width, context, &mut lines);
            }
        }
    }

    if !plan.evidence.is_empty() {
        lines.push(DiagramLine::plain("", DiagramRole::Muted));
        if width >= EVIDENCE_REASON_MIN_WIDTH {
            // Useful widths: one line per entry, basename + line/hunk + a concise reason.
            for (index, evidence) in plan.evidence.iter().enumerate() {
                let prefix = if index == 0 {
                    "Evidence: "
                } else {
                    "          "
                };
                let entry = format!(
                    "{} — {}",
                    evidence_source(evidence, true),
                    evidence.reason.trim()
                );
                lines.extend(wrap_prefixed(
                    prefix,
                    &entry,
                    width,
                    DiagramRole::Evidence,
                    1,
                ));
            }
        } else {
            // Very narrow panes keep only the locators so the visual stays scannable.
            let refs = plan
                .evidence
                .iter()
                .map(|evidence| evidence_source(evidence, false))
                .collect::<Vec<_>>()
                .join(" · ");
            lines.extend(wrap_prefixed(
                "Evidence: ",
                &refs,
                width,
                DiagramRole::Evidence,
                3,
            ));
        }
    }
    lines
}

/// Compact evidence locator: `basename:line (hunk n)`, both one-based for display. The
/// narrow refs-only form uses bracket hunk markers (`basename[h2]`) so every ref stays a
/// single unbreakable token; a line reference already pins the source without a hunk.
fn evidence_source(evidence: &PlanEvidence, include_hunk: bool) -> String {
    let mut source = basename(&evidence.file.to_string()).to_string();
    if let Some(range) = evidence.range {
        source.push(':');
        source.push_str(&range.start_line.saturating_add(1).to_string());
    }
    if let Some(hunk) = evidence.hunk {
        if include_hunk {
            source.push_str(&format!(" (hunk {})", hunk.saturating_add(1)));
        } else if evidence.range.is_none() {
            source.push_str(&format!("[h{}]", hunk.saturating_add(1)));
        }
    }
    source
}

/// Last path component of a repo-relative file.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A useful relationship visual that remains available without AI.
#[must_use]
pub fn fallback_lines(impact: &ImpactPane, width: u16) -> Vec<DiagramLine> {
    let width = usize::from(width).max(1);
    let Some(selected) = &impact.selected_change else {
        return vec![DiagramLine::plain(
            truncate(
                "Select a changed file or symbol to inspect its relationships.",
                width,
            ),
            DiagramRole::Muted,
        )];
    };

    let mut lines = wrap_role(
        &format!("{} {}", selected.label, selected.change),
        width,
        DiagramRole::Title,
        2,
    );
    lines.extend(wrap_role(
        &selected.interpretation,
        width,
        DiagramRole::Text,
        3,
    ));

    let box_width = width.min(MAX_BOX_WIDTH);
    if !impact.callers.rows.is_empty() {
        for caller in impact.callers.rows.iter().take(3) {
            lines.extend(node_box(&caller.label, "caller", box_width, false));
            lines.push(centered_arrow("reaches", width, true));
        }
    }
    lines.extend(node_box(
        &selected.label,
        &selected.interpretation,
        box_width,
        true,
    ));
    for downstream in impact.downstream.rows.iter().take(3) {
        lines.push(centered_arrow(downstream.relation, width, true));
        lines.extend(node_box(
            &downstream.label,
            "affected downstream",
            box_width,
            false,
        ));
    }
    if impact.callers.rows.is_empty() && impact.downstream.rows.is_empty() {
        lines.push(DiagramLine::plain(
            truncate(
                "No caller or downstream relationship is currently known.",
                width,
            ),
            DiagramRole::Muted,
        ));
    }
    lines
}

fn render_flow(
    form: &VizForm,
    width: usize,
    context: DiagramContext<'_>,
    lines: &mut Vec<DiagramLine>,
) {
    if let Some((nodes, edges)) = linear_chain(form) {
        let count = nodes.len();
        let gap = horizontal_gap(&edges);
        let min_width = count * MIN_BOX_WIDTH + count.saturating_sub(1) * gap;
        if width >= min_width && !edges.iter().any(|edge| context.relationship_expanded(edge)) {
            render_horizontal_chain(&nodes, &edges, width, context, false, lines);
            return;
        }
        render_vertical_chain(&nodes, &edges, width, context, lines);
        return;
    }

    render_branching_flow(form, width, context, lines);
}

/// Stack a linear chain as full node boxes joined by labeled vertical relationships.
fn render_vertical_chain(
    nodes: &[&PlanNode],
    edges: &[&PlanEdge],
    width: usize,
    context: DiagramContext<'_>,
    lines: &mut Vec<DiagramLine>,
) {
    let box_width = width.min(MAX_BOX_WIDTH);
    for (index, node) in nodes.iter().enumerate() {
        if index > 0 {
            let edge = edges[index - 1];
            lines.extend(vertical_relationship_lines(
                edge,
                width,
                edge_verified(nodes[index - 1], node, edge),
                context,
            ));
        }
        lines.extend(plan_node_box(node, box_width, context));
    }
}

/// An edge is visually verified only when both endpoints carry fact-store entities and
/// the kind is one the validator can check against the impact graph. `Reads`/`Writes`
/// edges and entityless (presentational) endpoints are interpretation, not fact.
fn edge_verified(from: &PlanNode, to: &PlanNode, edge: &PlanEdge) -> bool {
    from.entity.is_some()
        && to.entity.is_some()
        && matches!(
            edge.kind,
            PlanEdgeKind::Calls
                | PlanEdgeKind::Imports
                | PlanEdgeKind::Implements
                | PlanEdgeKind::Contains
        )
}

fn render_horizontal_chain(
    nodes: &[&PlanNode],
    edges: &[&PlanEdge],
    width: usize,
    context: DiagramContext<'_>,
    synthetic: bool,
    lines: &mut Vec<DiagramLine>,
) {
    let count = nodes.len();
    let gap = horizontal_gap(edges);
    let box_width = ((width - gap * (count - 1)) / count).min(MAX_BOX_WIDTH);
    let used = box_width * count + gap * (count - 1);
    let left_pad = " ".repeat((width.saturating_sub(used)) / 2);
    // A renderer-synthesized edge (e.g. BeforeAfter's "becomes") is presentational even
    // when both endpoints carry entities: it never borrows their verification.
    let mut boxes: Vec<Vec<String>> = nodes
        .iter()
        .map(|node| {
            node_box_text(
                &node.label,
                displayed_node_detail(node, context.expanded(node)),
                box_width,
                context.expanded(node),
            )
        })
        .collect();
    let box_heights: Vec<usize> = boxes.iter().map(Vec::len).collect();
    let row_count = box_heights.iter().copied().max().unwrap_or_default();
    for node_box in &mut boxes {
        node_box.resize(row_count, " ".repeat(box_width));
    }
    for (row, _) in boxes[0].iter().enumerate() {
        let mut spans = vec![DiagramSpan {
            text: left_pad.clone(),
            role: DiagramRole::Muted,
            target: None,
            relationship: None,
        }];
        for (index, node) in nodes.iter().enumerate() {
            let inside_box = row < box_heights[index];
            let normal = if !inside_box {
                DiagramRole::Muted
            } else if boxes[index][row].starts_with('┌') || boxes[index][row].starts_with('└') {
                DiagramRole::Border
            } else {
                DiagramRole::Text
            };
            spans.push(DiagramSpan {
                text: boxes[index][row].clone(),
                role: if inside_box {
                    context.role(node, normal)
                } else {
                    normal
                },
                target: inside_box.then(|| context.target(node)),
                relationship: None,
            });
            if index + 1 < count {
                let verified =
                    !synthetic && edge_verified(nodes[index], nodes[index + 1], edges[index]);
                let connector = if row == 1 {
                    centered_text(edge_label(edges[index]), gap)
                } else if row == 2 {
                    // Solid arrows stay reserved for verified relationships.
                    if verified {
                        format!("{}▶", "─".repeat(gap.saturating_sub(1)))
                    } else {
                        format!("{}▷", "┄".repeat(gap.saturating_sub(1)))
                    }
                } else {
                    " ".repeat(gap)
                };
                spans.push(DiagramSpan {
                    text: connector,
                    role: DiagramRole::Arrow,
                    target: None,
                    relationship: (row == 1 || row == 2)
                        .then(|| context.relationship_target(edges[index])),
                });
            }
        }
        lines.push(DiagramLine { spans });
    }
}

fn render_branching_flow(
    form: &VizForm,
    width: usize,
    context: DiagramContext<'_>,
    lines: &mut Vec<DiagramLine>,
) {
    let by_id: HashMap<&str, &PlanNode> = form
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    // Nonlinear graphs use the same visual vocabulary as chains: every node is a box,
    // followed by labeled relationship connectors naming its destinations. Cycles and
    // shared targets remain unambiguous because every connector names its target.
    let box_width = width.min(MAX_BOX_WIDTH);
    for node in &form.nodes {
        lines.extend(plan_node_box(node, box_width, context));
        let outgoing: Vec<&PlanEdge> = form
            .edges
            .iter()
            .filter(|edge| edge.from == node.id)
            .collect();
        for (index, edge) in outgoing.iter().enumerate() {
            let last = index + 1 == outgoing.len();
            let target = by_id.get(edge.to.as_str());
            let verified = matches!((by_id.get(edge.from.as_str()), target), (Some(from), Some(to))
                if edge_verified(from, to, edge));
            render_relationship_connector(edge, target, verified, last, width, context, lines);
        }
    }
}

/// One outgoing relationship connector under its source box: `  ├┄ <effect> → <target>`.
/// The connector is solid only for validator-verified edges; the effect and target
/// label share the available width, with the effect yielding first.
fn render_relationship_connector(
    edge: &PlanEdge,
    target: Option<&&PlanNode>,
    verified: bool,
    last: bool,
    width: usize,
    context: DiagramContext<'_>,
    lines: &mut Vec<DiagramLine>,
) {
    let branch = match (last, verified) {
        (true, true) => "└─ ",
        (true, false) => "└┄ ",
        (false, true) => "├─ ",
        (false, false) => "├┄ ",
    };
    let prefix = format!("  {branch}");
    let prefix_width = prefix.width();
    let relationship = context.relationship_target(edge);
    if context.relationship_expanded(edge) {
        if width <= prefix_width {
            lines.push(DiagramLine::for_relationship(
                truncate(if verified { "▼" } else { "┊" }, width),
                DiagramRole::Arrow,
                relationship,
            ));
            return;
        }
        let target_label = target.map_or(edge.to.as_str(), |target| target.label.trim());
        let full = format!("{} → {target_label}", edge_label(edge).trim());
        let available = width.saturating_sub(prefix_width).max(1);
        for (index, text) in wrap_text_full(&full, available).into_iter().enumerate() {
            lines.push(DiagramLine {
                spans: vec![
                    DiagramSpan {
                        text: if index == 0 {
                            prefix.clone()
                        } else {
                            " ".repeat(prefix_width.min(width.saturating_sub(1)))
                        },
                        role: DiagramRole::Arrow,
                        target: None,
                        relationship: Some(relationship.clone()),
                    },
                    DiagramSpan {
                        text,
                        role: DiagramRole::Arrow,
                        target: None,
                        relationship: Some(relationship.clone()),
                    },
                ],
            });
        }
        return;
    }
    // ` → ` between the effect and the target label.
    const ARROW_WIDTH: usize = 3;
    // Smallest still-recognizable heads for either side of the arrow.
    const MIN_SIDE_WIDTH: usize = 4;
    let Some(target) = target else {
        // Unknown target id: nothing to name; the effect takes the line.
        if width < prefix_width + 2 {
            lines.push(DiagramLine::for_relationship(
                truncate(if verified { "▼" } else { "┊" }, width),
                DiagramRole::Arrow,
                relationship,
            ));
            return;
        }
        lines.push(DiagramLine {
            spans: vec![
                DiagramSpan {
                    text: prefix,
                    role: DiagramRole::Arrow,
                    target: None,
                    relationship: Some(relationship.clone()),
                },
                DiagramSpan {
                    text: truncate(edge_label(edge).trim(), width - prefix_width),
                    role: DiagramRole::Arrow,
                    target: None,
                    relationship: Some(relationship),
                },
            ],
        });
        return;
    };
    let target_label = target.label.trim();
    // Genuinely tiny: even branch + arrow + one target cell cannot fit.
    if width < prefix_width + ARROW_WIDTH + 2 {
        lines.push(DiagramLine::for_relationship(
            truncate(if verified { "▼" } else { "┊" }, width),
            DiagramRole::Arrow,
            relationship,
        ));
        return;
    }
    let budget = width - prefix_width;
    let effect_full = edge_label(edge).trim();
    // Effect yields first, but both sides stay recognizable: cap each side's truncation
    // so neither starves, and let whichever side is naturally shorter keep its spare
    // cells.
    let pair_budget = budget.saturating_sub(ARROW_WIDTH);
    let effect_width = effect_full.width();
    let target_width = target_label.width();
    let (effect_budget, target_budget) = if effect_width + target_width <= pair_budget {
        // Both fit completely.
        (effect_width, target_width)
    } else if pair_budget < 2 * MIN_SIDE_WIDTH {
        // Squeeze: split evenly with at least one cell each side. Each side renders
        // at least its ellipsis, and the pair always fits the budget exactly.
        let half = (pair_budget / 2).max(1);
        let b = pair_budget.saturating_sub(half).max(1);
        (half, b)
    } else {
        // The target is reserved FIRST with a bounded share (its full width, or half
        // the pair when it is long); the effect truncates into the remainder. The
        // effect therefore yields, but neither side can starve the other.
        let target_share = target_width.min(pair_budget / 2).max(MIN_SIDE_WIDTH);
        (pair_budget - target_share, target_share)
    };
    lines.push(DiagramLine {
        spans: vec![
            DiagramSpan {
                text: prefix,
                role: DiagramRole::Arrow,
                target: None,
                relationship: Some(relationship.clone()),
            },
            DiagramSpan {
                text: truncate(effect_full, effect_budget),
                role: DiagramRole::Arrow,
                target: None,
                relationship: Some(relationship.clone()),
            },
            DiagramSpan {
                text: " → ".into(),
                role: DiagramRole::Muted,
                target: None,
                relationship: Some(relationship.clone()),
            },
            DiagramSpan {
                text: truncate(target_label, target_budget),
                role: context.role(target, DiagramRole::Text),
                target: Some(context.target(target)),
                relationship: Some(relationship),
            },
        ],
    });
}

fn render_before_after(
    form: &VizForm,
    width: usize,
    context: DiagramContext<'_>,
    lines: &mut Vec<DiagramLine>,
) {
    if form.nodes.len() >= 2 {
        // The renderer synthesizes a "becomes" Contains edge when the form ships none.
        // That edge is presentational: it stays inferred in BOTH layouts, even when both
        // endpoints carry entities that the validator could otherwise verify.
        let is_synthetic = form.edges.is_empty();
        let edge = form.edges.first();
        let synthetic;
        let edge = if let Some(edge) = edge {
            edge
        } else {
            synthetic = PlanEdge {
                from: form.nodes[0].id.clone(),
                to: form.nodes[1].id.clone(),
                kind: codescope_core::PlanEdgeKind::Contains,
                label: Some("becomes".to_string()),
            };
            &synthetic
        };
        let nodes = [&form.nodes[0], &form.nodes[1]];
        let min_width = 2 * MIN_BOX_WIDTH + horizontal_gap(&[edge]);
        if width >= min_width && !context.relationship_expanded(edge) {
            render_horizontal_chain(&nodes, &[edge], width, context, is_synthetic, lines);
        } else {
            let box_width = width.min(MAX_BOX_WIDTH);
            let verified = !is_synthetic
                && form
                    .edges
                    .first()
                    .is_some_and(|real| edge_verified(nodes[0], nodes[1], real));
            lines.extend(plan_node_box(nodes[0], box_width, context));
            lines.extend(vertical_relationship_lines(edge, width, verified, context));
            lines.extend(plan_node_box(nodes[1], box_width, context));
        }
    } else {
        render_vertical_nodes(form, width, context, lines);
    }
}

fn render_tree(
    form: &VizForm,
    width: usize,
    context: DiagramContext<'_>,
    lines: &mut Vec<DiagramLine>,
) {
    // Tree children are structural relationships. Convert any child relationship not
    // already represented by an explicit model edge into an inferred connector, then
    // use the ordinary boxed graph renderer. `Writes` keeps synthesized containment
    // links dashed even when both endpoints happen to carry verifiable entities.
    let mut graph = form.clone();
    for node in &form.nodes {
        for child in &node.children {
            if !graph
                .edges
                .iter()
                .any(|edge| edge.from == node.id && edge.to == *child)
            {
                graph.edges.push(PlanEdge {
                    from: node.id.clone(),
                    to: child.clone(),
                    kind: PlanEdgeKind::Writes,
                    label: Some("contains".to_string()),
                });
            }
        }
    }
    render_branching_flow(&graph, width, context, lines);
}

fn render_vertical_nodes(
    form: &VizForm,
    width: usize,
    context: DiagramContext<'_>,
    lines: &mut Vec<DiagramLine>,
) {
    let box_width = width.min(MAX_BOX_WIDTH);
    for (index, node) in form.nodes.iter().enumerate() {
        if index > 0 {
            lines.push(centered_arrow("then", width, true));
        }
        lines.extend(plan_node_box(node, box_width, context));
    }
}

fn linear_chain(form: &VizForm) -> Option<(Vec<&PlanNode>, Vec<&PlanEdge>)> {
    if form.nodes.len() < 2 || form.edges.len() + 1 != form.nodes.len() {
        return None;
    }
    let by_id: HashMap<&str, &PlanNode> = form
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let incoming: HashSet<&str> = form.edges.iter().map(|edge| edge.to.as_str()).collect();
    let mut current = form
        .nodes
        .iter()
        .find(|node| !incoming.contains(node.id.as_str()))?;
    let mut nodes = vec![current];
    let mut edges = Vec::new();
    let mut seen = HashSet::from([current.id.as_str()]);
    loop {
        let outgoing: Vec<&PlanEdge> = form
            .edges
            .iter()
            .filter(|edge| edge.from == current.id)
            .collect();
        if outgoing.is_empty() {
            break;
        }
        if outgoing.len() != 1 {
            return None;
        }
        let edge = outgoing[0];
        current = by_id.get(edge.to.as_str()).copied()?;
        if !seen.insert(current.id.as_str()) {
            return None;
        }
        edges.push(edge);
        nodes.push(current);
    }
    (nodes.len() == form.nodes.len()).then_some((nodes, edges))
}

fn node_selected(node: &PlanNode, selected_label: &str) -> bool {
    node.hint.highlight
        || (!selected_label.is_empty()
            && (node.label == selected_label
                || node
                    .entity
                    .as_ref()
                    .and_then(|entity| entity.symbol.as_deref())
                    == Some(selected_label)))
}

pub(crate) fn edge_label(edge: &PlanEdge) -> &str {
    edge.label.as_deref().unwrap_or("affects")
}

fn horizontal_gap(edges: &[&PlanEdge]) -> usize {
    edges
        .iter()
        .map(|edge| edge_label(edge).width().saturating_add(2))
        .max()
        .unwrap_or(MIN_HORIZONTAL_GAP)
        .clamp(MIN_HORIZONTAL_GAP, MAX_HORIZONTAL_GAP)
}

/// The compact preview is always `detail`; expansion swaps in the complete explanation
/// when the model supplied one. Keeping this choice in one helper gives every box the
/// same interaction semantics.
pub(crate) fn displayed_node_detail(node: &PlanNode, expanded: bool) -> &str {
    if expanded {
        node.expanded_detail
            .as_deref()
            .filter(|detail| !detail.trim().is_empty())
            .or(node.detail.as_deref())
            .unwrap_or_default()
    } else {
        node.detail.as_deref().unwrap_or_default()
    }
}

fn node_box(label: &str, detail: &str, width: usize, selected: bool) -> Vec<DiagramLine> {
    if width < 4 {
        let role = if selected {
            DiagramRole::Selected
        } else {
            DiagramRole::Text
        };
        return vec![DiagramLine::plain(truncate("□", width), role)];
    }
    node_box_text(label, detail, width, false)
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            let role = if selected {
                DiagramRole::Selected
            } else if index == 0 || index == 4 {
                DiagramRole::Border
            } else {
                DiagramRole::Text
            };
            DiagramLine::plain(text, role)
        })
        .collect()
}

fn plan_node_box(node: &PlanNode, width: usize, context: DiagramContext<'_>) -> Vec<DiagramLine> {
    let target = context.target(node);
    let expanded = context.expanded(node);
    let detail = displayed_node_detail(node, expanded);
    // A full box needs at least four cells; at pathological widths retain the box
    // vocabulary with a single-cell box glyph rather than switching to plain text.
    if width < 4 {
        return vec![DiagramLine::for_node(
            truncate("□", width),
            context.role(node, DiagramRole::Border),
            target,
        )];
    }
    let box_lines = node_box_text(&node.label, detail, width, expanded);
    let last = box_lines.len().saturating_sub(1);
    box_lines
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            let normal = if index == 0 || index == last {
                DiagramRole::Border
            } else {
                DiagramRole::Text
            };
            DiagramLine::for_node(text, context.role(node, normal), target.clone())
        })
        .collect()
}

/// Draw a node in its collapsed or expanded state.
///
/// Collapsed nodes reserve two rows for the model's short `detail` preview. Expanded
/// nodes replace that preview with the complete `expanded_detail` and grow vertically;
/// nothing is ellipsized in that state, including identifiers longer than one row.
pub(crate) fn node_box_text(
    label: &str,
    detail: &str,
    width: usize,
    expanded: bool,
) -> Vec<String> {
    let width = width.max(4);
    let inner = width.saturating_sub(2);
    let label_lines = if expanded {
        wrap_text_full(label.trim(), inner)
    } else {
        vec![truncate(label.trim(), inner)]
    };
    let mut detail_lines = if expanded {
        wrap_text_full(detail.trim(), inner)
    } else {
        wrap_text(detail.trim(), inner, 2)
    };
    if expanded {
        if detail_lines.is_empty() {
            detail_lines.push(String::new());
        }
    } else {
        detail_lines.resize(2, String::new());
    }
    let mut lines = Vec::with_capacity(label_lines.len() + detail_lines.len() + 2);
    lines.push(format!("┌{}┐", "─".repeat(inner)));
    lines.extend(
        label_lines
            .into_iter()
            .map(|label| format!("│{}│", pad(&label, inner))),
    );
    lines.extend(
        detail_lines
            .into_iter()
            .map(|detail| format!("│{}│", pad(&detail, inner))),
    );
    lines.push(format!("└{}┘", "─".repeat(inner)));
    lines
}

fn centered_arrow(label: &str, width: usize, verified: bool) -> DiagramLine {
    // The labeled arrow needs four cells; retain a relationship glyph below that
    // floor instead of switching to prose.
    if width <= 4 {
        let glyph = if verified { "▼" } else { "┊" };
        return DiagramLine::plain(truncate(glyph, width), DiagramRole::Arrow);
    }
    let label = truncate(label.trim(), width.saturating_sub(4));
    let glyph = if verified { "▼" } else { "┊" };
    DiagramLine::plain(format!("  {glyph} {label}"), DiagramRole::Arrow)
}

fn vertical_relationship_lines(
    edge: &PlanEdge,
    width: usize,
    verified: bool,
    context: DiagramContext<'_>,
) -> Vec<DiagramLine> {
    let relationship = context.relationship_target(edge);
    if !context.relationship_expanded(edge) {
        let mut line = centered_arrow(edge_label(edge), width, verified);
        for span in &mut line.spans {
            span.relationship = Some(relationship.clone());
        }
        return vec![line];
    }
    if width <= 4 {
        let glyph = if verified { "▼" } else { "┊" };
        return vec![DiagramLine::for_relationship(
            truncate(glyph, width),
            DiagramRole::Arrow,
            relationship,
        )];
    }
    let prefix = if verified { "  ▼ " } else { "  ┊ " };
    wrap_text_full(edge_label(edge).trim(), width - 4)
        .into_iter()
        .enumerate()
        .map(|(index, text)| DiagramLine {
            spans: vec![
                DiagramSpan {
                    text: if index == 0 {
                        prefix.to_string()
                    } else {
                        "    ".to_string()
                    },
                    role: DiagramRole::Arrow,
                    target: None,
                    relationship: Some(relationship.clone()),
                },
                DiagramSpan {
                    text,
                    role: DiagramRole::Arrow,
                    target: None,
                    relationship: Some(relationship.clone()),
                },
            ],
        })
        .collect()
}

fn centered_text(text: &str, width: usize) -> String {
    let text = truncate(text.trim(), width);
    let text_width = text.width();
    let left = width.saturating_sub(text_width) / 2;
    format!(
        "{}{}{}",
        " ".repeat(left),
        text,
        " ".repeat(width.saturating_sub(left + text_width))
    )
}

fn wrap_prefixed(
    prefix: &str,
    text: &str,
    width: usize,
    role: DiagramRole,
    max_lines: usize,
) -> Vec<DiagramLine> {
    let prefix_width = prefix.width();
    if width <= prefix_width {
        // The prefix alone cannot fit the requested width: degrade to plain truncated
        // lines instead of emitting a line wider than the pane.
        return wrap_text(text.trim(), width, max_lines)
            .into_iter()
            .map(|line| DiagramLine::plain(line, role))
            .collect();
    }
    let available = width.saturating_sub(prefix_width).max(1);
    wrap_text(text.trim(), available, max_lines)
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            DiagramLine::plain(
                format!("{}{text}", if index == 0 { prefix } else { "" }),
                role,
            )
        })
        .collect()
}

fn wrap_role(text: &str, width: usize, role: DiagramRole, max_lines: usize) -> Vec<DiagramLine> {
    wrap_text(text.trim(), width, max_lines)
        .into_iter()
        .map(|line| DiagramLine::plain(line, role))
        .collect()
}

fn wrap_text(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    if text.is_empty() || max_lines == 0 {
        return Vec::new();
    }
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate_width = current.width() + usize::from(!current.is_empty()) + word.width();
        if candidate_width <= width {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        } else {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                if lines.len() == max_lines {
                    break;
                }
            }
            current = truncate(word, width);
        }
    }
    if lines.len() < max_lines && !current.is_empty() {
        lines.push(current);
    }
    if lines.len() == max_lines && text.width() > lines.iter().map(|line| line.width() + 1).sum() {
        if let Some(last) = lines.last_mut() {
            *last = truncate_with_ellipsis(last, width);
        }
    }
    lines
}

/// Lossless word wrapping for expanded content. Unlike [`wrap_text`], this has no line
/// cap and splits long identifiers across rows instead of replacing their tail with an
/// ellipsis. A double-width glyph in a one-cell viewport is the sole impossible case;
/// it degrades to a one-cell ellipsis to preserve the renderer's width contract.
fn wrap_text_full(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        if word.width() <= width {
            let candidate_width = current.width() + usize::from(!current.is_empty()) + word.width();
            if candidate_width <= width {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            } else {
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }
                current.push_str(word);
            }
            continue;
        }

        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        let mut chunk = String::new();
        let mut chunk_width = 0usize;
        for ch in word.chars() {
            let char_width = ch.width().unwrap_or(0);
            if char_width > width {
                if !chunk.is_empty() {
                    lines.push(std::mem::take(&mut chunk));
                    chunk_width = 0;
                }
                lines.push("…".to_string());
                continue;
            }
            if chunk_width + char_width > width && !chunk.is_empty() {
                lines.push(std::mem::take(&mut chunk));
                chunk_width = 0;
            }
            chunk.push(ch);
            chunk_width += char_width;
            if chunk_width == width {
                lines.push(std::mem::take(&mut chunk));
                chunk_width = 0;
            }
        }
        current = chunk;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    truncate_with_ellipsis(text, width)
}

fn truncate_with_ellipsis(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let char_width = ch.width().unwrap_or(0);
        if used + char_width + 1 > width {
            break;
        }
        out.push(ch);
        used += char_width;
    }
    out.push('…');
    out
}

fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(text.width())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codescope_core::{
        EntityRef, Epoch, FileId, LineRange, PlanEdgeKind, PlanEvidence, PlanNodeChange,
        VisualizationPlan,
    };

    fn flow_plan() -> VisualizationPlan {
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.intent = "Stop new traffic before waiting for in-flight requests.".into();
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            nodes: vec![
                PlanNode::new("a", "shutdown", PlanNodeChange::Modified)
                    .with_detail("marks the service unready"),
                PlanNode::new("b", "load balancer", PlanNodeChange::Unchanged)
                    .with_detail("stops routing new requests"),
                PlanNode::new("c", "Server.Shutdown", PlanNodeChange::Modified)
                    .with_detail("drains in-flight requests"),
            ],
            edges: vec![
                PlanEdge {
                    from: "a".into(),
                    to: "b".into(),
                    kind: PlanEdgeKind::Writes,
                    label: Some("readiness becomes 503".into()),
                },
                PlanEdge {
                    from: "b".into(),
                    to: "c".into(),
                    kind: PlanEdgeKind::Calls,
                    label: Some("new traffic stops".into()),
                },
            ],
        });
        plan.evidence.push(PlanEvidence {
            file: FileId::new("src/main.rs").unwrap(),
            hunk: Some(0),
            symbol: Some("shutdown".into()),
            range: None,
            reason: "sets readiness false before the drain delay".into(),
        });
        plan
    }

    fn plan_text(plan: &VisualizationPlan, width: u16, selected: &str) -> String {
        plan_lines(plan, width, selected)
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wide_flow_uses_horizontal_boxes_and_arrows() {
        // flow_plan's nodes carry no entities, so its arrows are inferred and dashed.
        // Solid arrows stay reserved for verified relationships.
        let text = plan_text(&flow_plan(), 100, "shutdown");
        assert!(text.contains("┌"));
        assert!(text.contains("▷"));
        assert!(
            !text.contains("▶"),
            "no solid arrow for inferred edges: {text}"
        );
        assert!(text.contains("readiness becomes"));
        assert!(
            !text.contains("inferred from cited diff"),
            "no legend: {text}"
        );
    }

    #[test]
    fn narrow_flow_stacks_boxes_and_relationships() {
        // A chain that cannot fit side by side keeps the same visual grammar and stacks.
        let text = plan_text(&flow_plan(), 40, "shutdown");
        assert!(text.contains("shutdown"), "first box: {text}");
        assert!(text.contains("load balancer"), "second box: {text}");
        assert!(
            text.contains("readiness becomes 503"),
            "edge label keeps the pane width: {text}"
        );
        assert!(text.contains('┊'), "inferred rail: {text}");
        assert!(
            !text.contains("inferred from cited diff"),
            "no legend: {text}"
        );
        assert_eq!(text.matches('┌').count(), 3, "one box per node: {text}");
    }

    /// The validated 7-step sequence shape from the real AI baseline plan.
    fn seven_step_sequence_plan() -> VisualizationPlan {
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.intent = "Stop new traffic before waiting for in-flight requests to finish.".into();
        let steps = [
            ("SIGTERM received", "run() begins teardown"),
            ("shutdown goroutine wakes", "drain delay is 10s"),
            ("Healthy flag → false", "readiness reports draining"),
            ("readinessHandler → 503", "plaintext port answers probes"),
            ("LB stops routing", "probes see unhealthy"),
            ("wait shutdownDrainDelay", "in-flight requests keep serving"),
            ("server.Shutdown drains", "signalCtx cancel spares root ctx"),
        ];
        plan.forms.push(VizForm {
            kind: FormKind::Sequence,
            nodes: steps
                .iter()
                .enumerate()
                .map(|(index, (label, detail))| {
                    PlanNode::new(format!("n{index}"), *label, PlanNodeChange::Unchanged)
                        .with_detail(*detail)
                })
                .collect(),
            edges: (1..steps.len())
                .map(|index| PlanEdge {
                    from: format!("n{}", index - 1),
                    to: format!("n{index}"),
                    kind: PlanEdgeKind::Writes,
                    label: Some(if index == 1 {
                        "SIGTERM/SIGINT triggers shutdown, unblocks waiters".to_string()
                    } else {
                        format!("edge label {index}")
                    }),
                })
                .collect(),
        });
        plan
    }

    #[test]
    fn seven_node_sequence_stacks_every_box_and_relationship() {
        let plan = seven_step_sequence_plan();
        let lines = plan_lines(&plan, 96, "");
        let text = lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        for label in [
            "SIGTERM received",
            "shutdown goroutine wakes",
            "Healthy flag → false",
            "readinessHandler → 503",
            "LB stops routing",
            "wait shutdownDrainDelay",
            "server.Shutdown drains",
        ] {
            assert!(text.contains(label), "box {label} visible: {text}");
        }
        assert_eq!(text.matches('┌').count(), 7, "seven boxes: {text}");
        assert!(
            text.contains("SIGTERM/SIGINT triggers shutdown, unblocks waiters"),
            "the longest causal label is not box-truncated: {text}"
        );
        assert!(text.contains('┊'), "entityless chain is inferred: {text}");
        assert!(
            !text.contains("inferred from cited diff"),
            "no legend: {text}"
        );
        assert!(lines.len() > 20, "boxes intentionally grow vertically");
    }

    /// Verified edges (both endpoints with entities, graph-checkable kind) keep solid
    /// rails; Reads/Writes edges stay interpretation even with entities.
    #[test]
    fn verified_edges_render_solid_connectors() {
        let entity = |symbol: &str| EntityRef {
            file: FileId::new("src/main.rs").unwrap(),
            symbol: Some(symbol.to_string()),
            range: None,
        };
        let mut form = VizForm {
            kind: FormKind::RelationshipFlow,
            nodes: vec![
                PlanNode::new("a", "shutdown", PlanNodeChange::Modified)
                    .with_entity(entity("shutdown"))
                    .with_detail("marks the service unready"),
                PlanNode::new("b", "drain", PlanNodeChange::Modified)
                    .with_entity(entity("drain"))
                    .with_detail("waits for in-flight requests"),
            ],
            edges: vec![PlanEdge {
                from: "a".into(),
                to: "b".into(),
                kind: PlanEdgeKind::Calls,
                label: Some("readiness becomes 503".into()),
            }],
        };
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.forms.push(form.clone());
        // 40 cells cannot fit two boxes side by side, so the boxes stack.
        let text = plan_text(&plan, 40, "shutdown");
        assert!(text.contains('│'), "solid rail: {text}");
        assert!(!text.contains('┊'), "no inferred rail: {text}");
        assert!(!text.contains("cited diff"), "no basis note: {text}");

        form.edges[0].kind = PlanEdgeKind::Writes;
        plan.forms[0] = form;
        let text = plan_text(&plan, 40, "");
        assert!(text.contains('┊'), "writes edge is inferred: {text}");
        assert!(
            !text.contains("inferred from cited diff"),
            "no legend: {text}"
        );
    }

    /// A chain mixing verified and inferred edges marks only the inferred rails.
    #[test]
    fn mixed_edges_mark_only_inferred_rails() {
        let entity = |symbol: &str| EntityRef {
            file: FileId::new("src/main.rs").unwrap(),
            symbol: Some(symbol.to_string()),
            range: None,
        };
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            nodes: vec![
                PlanNode::new("n1", "signal", PlanNodeChange::Unchanged)
                    .with_entity(entity("signal")),
                PlanNode::new("n2", "readiness", PlanNodeChange::Modified)
                    .with_entity(entity("readiness")),
                PlanNode::new("n3", "drain", PlanNodeChange::Modified),
            ],
            edges: vec![
                PlanEdge {
                    from: "n1".into(),
                    to: "n2".into(),
                    kind: PlanEdgeKind::Calls,
                    label: Some("clears the healthy flag".into()),
                },
                PlanEdge {
                    from: "n2".into(),
                    to: "n3".into(),
                    kind: PlanEdgeKind::Calls,
                    label: Some("waits the drain delay".into()),
                },
            ],
        });
        let text = plan_text(&plan, 40, "");
        assert!(
            text.contains("▼ clears the healthy flag"),
            "verified connector stays solid: {text}"
        );
        assert!(
            text.contains("┊ waits the drain delay"),
            "entityless endpoint makes the rail inferred: {text}"
        );
        assert!(
            !text.contains("inferred from cited diff"),
            "no legend: {text}"
        );
    }

    /// Intent is capped at two lines: boxes carry the remaining detail.
    #[test]
    fn long_intent_caps_at_two_lines() {
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.intent = "word ".repeat(60);
        plan.forms.push(VizForm {
            kind: FormKind::ChangedSymbolTree,
            nodes: vec![PlanNode::new("n1", "root", PlanNodeChange::Modified)],
            edges: Vec::new(),
        });
        let lines = plan_lines(&plan, 40, "");
        let intent_lines = &lines[..2];
        assert_eq!(intent_lines.len(), 2, "intent is capped at two lines");
        assert!(
            intent_lines[1].text().ends_with('…'),
            "overflow is elided: {}",
            intent_lines[1].text()
        );
    }

    /// Evidence keeps a one-line reason per entry at useful widths and collapses to
    /// bare basename:line references when the pane is too narrow for prose.
    #[test]
    fn evidence_compacts_to_basename_refs() {
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.forms.push(VizForm {
            kind: FormKind::ChangedSymbolTree,
            nodes: vec![PlanNode::new("n1", "root", PlanNodeChange::Modified)],
            edges: Vec::new(),
        });
        plan.evidence.push(PlanEvidence {
            file: FileId::new("sandbox/vm-sandboxes/packages/api/main.go").unwrap(),
            hunk: Some(2),
            symbol: Some("run".into()),
            range: Some(LineRange {
                start_line: 292,
                start_col: 0,
                end_line: 308,
                end_col: 0,
            }),
            reason: "New plaintext readiness listener on ReadinessPort".into(),
        });
        let wide = plan_text(&plan, 96, "");
        assert!(
            wide.contains("main.go:293 (hunk 3)"),
            "basename + one-based line/hunk: {wide}"
        );
        assert!(
            wide.contains("New plaintext readiness listener"),
            "concise reason retained: {wide}"
        );
        assert!(!wide.contains("vm-sandboxes"), "no long paths: {wide}");
        let narrow = plan_text(&plan, 36, "");
        assert!(narrow.contains("main.go:293"), "refs only: {narrow}");
        assert!(
            !narrow.contains("New plaintext readiness listener"),
            "reasons dropped when narrow: {narrow}"
        );
        assert!(!narrow.contains("vm-sandboxes"), "no long paths: {narrow}");
    }

    /// At a 17-cell pane stacked boxes fit every line and close their borders.
    #[test]
    fn stacked_boxes_fit_narrow_width() {
        let lines = plan_lines(&flow_plan(), 17, "shutdown");
        for line in &lines {
            assert!(
                line.text().width() <= 17,
                "line exceeds the pane: {:?}",
                line.text()
            );
        }
        let text = lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.matches('┌').count(), 3, "boxes survive: {text}");
        assert!(text.contains("shutdown"), "labels survive: {text}");
    }

    /// Narrow BeforeAfter boxes never exceed the pane: every top border closes.
    #[test]
    fn before_after_narrow_keeps_box_borders_intact() {
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.intent = "Only signal handling is cancelled.".into();
        plan.forms.push(VizForm {
            kind: FormKind::BeforeAfter,
            nodes: vec![
                PlanNode::new("before", "cancel root", PlanNodeChange::Removed),
                PlanNode::new("after", "cancel signal", PlanNodeChange::Added),
            ],
            edges: Vec::new(),
        });
        let lines = plan_lines(&plan, 17, "");
        for line in &lines {
            let text = line.text();
            assert!(text.width() <= 17, "line exceeds the pane: {text:?}");
            if text.contains('┌') {
                assert!(text.ends_with('┐'), "clipped top border: {text}");
            }
        }
        let text = lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("becomes"));
        assert!(text.matches('┌').count() >= 2);
    }

    #[test]
    fn evidence_displays_hunks_one_based() {
        let text = plan_lines(&flow_plan(), 80, "")
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("(hunk 1)"));
        assert!(!text.contains("hunk 0"));
    }

    #[test]
    fn old_badges_and_form_labels_are_absent() {
        let text = plan_lines(&flow_plan(), 80, "")
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        for unwanted in ["HUNK WALKTHROUGH", "diff modified", "LSP info", "AI Plan"] {
            assert!(!text.contains(unwanted), "unexpected {unwanted:?}");
        }
    }

    fn tree_plan() -> VisualizationPlan {
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.intent = "The API owns a listener and its readiness handler.".into();
        let mut root = PlanNode::new("root", "API", PlanNodeChange::Modified)
            .with_detail("owns readiness behavior");
        root.children = vec!["listener".into(), "handler".into()];
        plan.forms.push(VizForm {
            kind: FormKind::ChangedSymbolTree,
            nodes: vec![
                root,
                PlanNode::new("listener", "readiness listener", PlanNodeChange::Added)
                    .with_detail("accepts plaintext probes"),
                PlanNode::new("handler", "readinessHandler", PlanNodeChange::Added)
                    .with_detail("returns the current state"),
            ],
            edges: Vec::new(),
        });
        plan
    }

    #[test]
    fn tree_visual_keeps_parent_child_shape() {
        // No explicit edges: parent/child links become dashed relationship connectors.
        let plan = tree_plan();
        let lines = plan_lines(&plan, 60, "API");
        let text = lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("├┄") || text.contains("└┄"),
            "branches: {text}"
        );
        assert!(text.contains("readiness listener"));
        assert!(text.contains("readinessHandler"));
        assert!(
            !text.contains("inferred from cited diff"),
            "no legend: {text}"
        );
        assert_eq!(text.matches('┌').count(), 3, "one box per node: {text}");
        assert!(text.contains("contains → readiness listener"));
        // Each label lands in one box.
        for (id, label) in [
            ("root", "API"),
            ("listener", "readiness listener"),
            ("handler", "readinessHandler"),
        ] {
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| line.text().contains(label)
                        && line.spans.iter().any(|span| {
                            span.target.as_ref().is_some_and(|target| target.id == id)
                                && span.relationship.is_none()
                        }))
                    .count(),
                1,
                "{label} on exactly one line: {text}"
            );
        }
        // The selected root keeps the highlight role.
        assert!(lines.iter().any(|line| {
            line.text().contains("API")
                && line
                    .spans
                    .iter()
                    .any(|span| span.role == DiagramRole::Selected)
        }));
    }

    fn before_after_plan() -> VisualizationPlan {
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.intent = "The root context survives while only signal handling is cancelled.".into();
        plan.forms.push(VizForm {
            kind: FormKind::BeforeAfter,
            nodes: vec![
                PlanNode::new("before", "cancel root", PlanNodeChange::Removed)
                    .with_detail("aborts every in-flight request"),
                PlanNode::new("after", "cancel signal", PlanNodeChange::Added)
                    .with_detail("keeps request contexts alive"),
            ],
            edges: Vec::new(),
        });
        plan
    }

    #[test]
    fn before_after_stays_visual_when_narrow() {
        let plan = before_after_plan();
        let text = plan_lines(&plan, 32, "")
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("becomes"));
        assert!(text.matches('┌').count() >= 2);
    }

    /// The exact six-node file-root tree from the live run-2 fallback plan: one root
    /// with five children, no explicit edges.
    fn six_node_tree_plan() -> VisualizationPlan {
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.intent = "A new readiness port flips to 503 on shutdown so the load balancer stops routing before the server drains.".into();
        let mut root = PlanNode::new("n1", "main.go run()", PlanNodeChange::Unchanged).with_detail(
            "run() gains plaintext readiness listener and reordered graceful shutdown path.",
        );
        root.entity = Some(EntityRef {
            file: FileId::new("sandbox/vm-sandboxes/packages/api/main.go").unwrap(),
            symbol: None,
            range: None,
        });
        root.children = vec![
            "n2".into(),
            "n3".into(),
            "n4".into(),
            "n5".into(),
            "n6".into(),
        ];
        let details = [
            (
                "readinessServer starts",
                "Unauthenticated /health listener on ReadinessPort.",
            ),
            (
                "SIGTERM cancels signalCtx",
                "Shutdown goroutine unblocks; sigCancel only.",
            ),
            (
                "readiness flips to 503",
                "Healthy flips false; readinessHandler returns 503.",
            ),
            (
                "sleep shutdownDrainDelay (10s)",
                "Fixed 15s sleep becomes 10s.",
            ),
            (
                "drain mTLS, close readiness last",
                "s.Shutdown drains requests; readiness closes last.",
            ),
        ];
        plan.forms.push(VizForm {
            kind: FormKind::ChangedSymbolTree,
            nodes: std::iter::once(root)
                .chain(details.iter().enumerate().map(|(index, (label, detail))| {
                    PlanNode::new(format!("n{}", index + 2), *label, PlanNodeChange::Unchanged)
                        .with_detail(*detail)
                }))
                .collect(),
            edges: Vec::new(),
        });
        for (hunk, reason) in [
            (
                1,
                "readinessHandler serves 200/503 from process-local atomic Healthy flag",
            ),
            (
                2,
                "New plaintext readiness listener on config.ReadinessPort for LB/Nomad probes",
            ),
            (
                5,
                "Shutdown order: 10s drain delay, mTLS Shutdown, readiness listener closed last",
            ),
            (
                3,
                "sigCancel-only cancellation keeps in-flight requests on root context",
            ),
        ] {
            plan.evidence.push(PlanEvidence {
                file: FileId::new("sandbox/vm-sandboxes/packages/api/main.go").unwrap(),
                hunk: Some(hunk),
                symbol: None,
                range: None,
                reason: reason.into(),
            });
        }
        plan
    }

    /// The live six-node tree renders six boxes at both 36 and 96 cells and every line
    /// stays inside the requested width.
    #[test]
    fn six_node_tree_renders_compactly_at_36_and_96() {
        let plan = six_node_tree_plan();
        let labels = [
            ("n1", "main.go run()"),
            ("n2", "readinessServer starts"),
            ("n3", "SIGTERM cancels signalCtx"),
            ("n4", "readiness flips to 503"),
            ("n5", "sleep shutdownDrainDelay (10s)"),
            ("n6", "drain mTLS, close readiness last"),
        ];
        for width in [36u16, 96] {
            let lines = plan_lines(&plan, width, "");
            let text = lines
                .iter()
                .map(DiagramLine::text)
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(text.matches('┌').count(), 6, "six boxes at {width}: {text}");
            assert!(
                !text.contains("inferred from cited diff"),
                "no legend: {text}"
            );
            for (_, label) in labels {
                // At 36 cells the longest label truncates with an ellipsis; its
                // recognizable head must still be visible. At 96 the full label fits.
                let visible: String = label.chars().take(22).collect();
                assert!(
                    text.contains(visible.as_str()),
                    "{label} visible at {width}: {text}"
                );
            }
            if width == 96 {
                for (_, label) in labels {
                    assert!(text.contains(label), "full {label} at 96: {text}");
                }
            }
            // Each label lands in exactly one box row.
            for (id, label) in labels {
                let visible: String = label.chars().take(22).collect();
                let count = lines
                    .iter()
                    .filter(|line| {
                        line.text().contains(visible.as_str())
                            && line.spans.iter().any(|span| {
                                span.target.as_ref().is_some_and(|target| target.id == id)
                                    && span.relationship.is_none()
                            })
                    })
                    .count();
                assert_eq!(count, 1, "{label} once at {width}: {text}");
            }
            for line in &lines {
                assert!(
                    line.text().width() <= usize::from(width),
                    "width {width} leaked: {:?}",
                    line.text()
                );
            }
            // The compact refs-only footer keeps all four evidence items visible at
            // narrow widths within its three lines (the live plan's real shape).
            if width == 36 {
                for hunk in ["main.go[h2]", "main.go[h3]", "main.go[h6]", "main.go[h4]"] {
                    assert!(text.contains(hunk), "{hunk} visible at 36: {text}");
                }
                let footer = lines
                    .iter()
                    .filter(|line| line.text().contains("main.go["))
                    .count();
                assert!(footer <= 3, "footer fits three lines: {text}");
            }
        }
    }

    /// A tree whose parent/child links are all backed by explicit verified edges uses
    /// solid branches; inferred links remain dashed without adding a legend.
    #[test]
    fn verified_tree_links_render_solid_without_basis_note() {
        let entity = |symbol: &str| EntityRef {
            file: FileId::new("src/main.rs").unwrap(),
            symbol: Some(symbol.to_string()),
            range: None,
        };
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.intent = "The API owns a listener and its readiness handler.".into();
        let mut root = PlanNode::new("root", "API", PlanNodeChange::Modified)
            .with_entity(entity("API"))
            .with_detail("owns readiness behavior");
        root.children = vec!["listener".into()];
        let listener = PlanNode::new("listener", "readiness listener", PlanNodeChange::Added)
            .with_entity(entity("readinessListener"))
            .with_detail("accepts plaintext probes");
        plan.forms.push(VizForm {
            kind: FormKind::ChangedSymbolTree,
            nodes: vec![root, listener],
            edges: vec![PlanEdge {
                from: "root".into(),
                to: "listener".into(),
                kind: PlanEdgeKind::Contains,
                label: Some("owns".into()),
            }],
        });
        let text = plan_text(&plan, 60, "");
        assert!(text.contains("└─"), "solid branch: {text}");
        assert!(!text.contains('┄'), "no dashed branch: {text}");
        assert!(!text.contains("cited diff"), "no basis note: {text}");
        // A Reads edge (even with entities) does not verify the link.
        plan.forms[0].edges[0].kind = PlanEdgeKind::Reads;
        let text = plan_text(&plan, 60, "");
        assert!(text.contains("└┄"), "reads link is inferred: {text}");
        assert!(
            !text.contains("inferred from cited diff"),
            "no legend: {text}"
        );
    }

    /// Multiple roots and depth-two subtrees all remain boxes with explicit connectors.
    #[test]
    fn tree_supports_multiple_roots_and_nested_guides() {
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.intent = "Two owners each own a subtree.".into();
        let mut first = PlanNode::new("a", "API", PlanNodeChange::Modified);
        first.children = vec!["a1".into(), "a2".into()];
        let mut a1 = PlanNode::new("a1", "handler", PlanNodeChange::Added);
        a1.children = vec!["a1x".into()];
        let a1x = PlanNode::new("a1x", "inner", PlanNodeChange::Added);
        let a2 = PlanNode::new("a2", "second", PlanNodeChange::Added);
        let second = PlanNode::new("b", "CLI", PlanNodeChange::Modified);
        plan.forms.push(VizForm {
            kind: FormKind::ChangedSymbolTree,
            nodes: vec![first, a1, a1x, a2, second],
            edges: Vec::new(),
        });
        let lines = plan_lines(&plan, 60, "");
        let text = lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.matches('┌').count(), 5, "five boxes: {text}");
        assert!(
            text.contains("contains → handler"),
            "first child relationship: {text}"
        );
        assert!(
            text.contains("contains → inner"),
            "nested relationship: {text}"
        );
        assert!(
            text.contains("contains → second"),
            "second child relationship: {text}"
        );
        assert!(
            !text.contains("inferred from cited diff"),
            "no legend: {text}"
        );
        assert_eq!(text.matches('┌').count(), 5, "one box per node: {text}");
    }

    /// The exact five-node/six-edge cyclic relationship_flow from the final completed-
    /// build artifact: boxed nodes, target-bearing connectors, and a visible cycle.
    fn cyclic_flow_plan() -> VisualizationPlan {
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.intent = "Adds an unauthenticated /health listener, flips it to 503 on shutdown, waits 10s, then drains.".into();
        let details = [
            (
                "readinessHandler",
                "Plaintext /health returns 200 while Healthy, 503 once draining",
            ),
            (
                "apiStore.Healthy",
                "Process-local atomic flag; flipped false when shutdown begins",
            ),
            (
                "readinessServer on config.ReadinessPort",
                "Separate listener for LB/Nomad probes lacking client certs",
            ),
            (
                "shutdown goroutine after signalCtx.Done",
                "Sleeps fixed 10s drain delay, then drains mTLS requests",
            ),
            (
                "s.Shutdown (mTLS server)",
                "Drains in-flight; readiness listener closed only afterward",
            ),
        ];
        let edges = [
            (
                "n1",
                "n2",
                PlanEdgeKind::Calls,
                "healthy.Load() → 200 ok, else 503 draining",
            ),
            (
                "n3",
                "n2",
                PlanEdgeKind::Contains,
                "signal flips Healthy=false via shutdown path",
            ),
            (
                "n3",
                "n1",
                PlanEdgeKind::Contains,
                "shared ServeMux /health on plaintext port",
            ),
            (
                "n3",
                "n4",
                PlanEdgeKind::Contains,
                "fixed 10s sleep sized for probe mark-down",
            ),
            (
                "n4",
                "n5",
                PlanEdgeKind::Calls,
                "s.Shutdown(ctx) drains in-flight requests",
            ),
            (
                "n5",
                "n1",
                PlanEdgeKind::Calls,
                "readinessServer.Shutdown last, after drain",
            ),
        ];
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            nodes: details
                .iter()
                .enumerate()
                .map(|(index, (label, detail))| {
                    PlanNode::new(format!("n{}", index + 1), *label, PlanNodeChange::Unchanged)
                        .with_detail(*detail)
                })
                .collect(),
            edges: edges
                .iter()
                .map(|(from, to, kind, label)| PlanEdge {
                    from: (*from).into(),
                    to: (*to).into(),
                    kind: *kind,
                    label: Some((*label).into()),
                })
                .collect(),
        });
        plan
    }

    /// The final cyclic artifact renders each node as a box, with all six relationships
    /// carrying their target and making the cycle visible.

    #[test]
    fn cyclic_flow_renders_boxes_and_targeted_relationships() {
        let plan = cyclic_flow_plan();
        let lines = plan_lines(&plan, 96, "");
        let text = lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.matches('┌').count(), 5, "five boxes: {text}");
        let first_node = lines
            .iter()
            .position(|line| line.text().contains("readinessHandler"))
            .expect("first node");
        assert!(
            !text.contains("inferred from cited diff"),
            "no legend: {text}"
        );
        // Every node label renders exactly once in document order.
        for (id, label) in [
            ("n1", "readinessHandler"),
            ("n2", "apiStore.Healthy"),
            ("n3", "readinessServer on config.ReadinessPort"),
            ("n4", "shutdown goroutine after signalCtx.Done"),
            ("n5", "s.Shutdown (mTLS server)"),
        ] {
            let visible: String = label.chars().take(22).collect();
            let count = lines
                .iter()
                .filter(|line| {
                    line.text().contains(visible.as_str())
                        && line.spans.iter().any(|span| {
                            span.target.as_ref().is_some_and(|target| target.id == id)
                                && span.relationship.is_none()
                        })
                })
                .count();
            assert_eq!(count, 1, "{label} once as a node line: {text}");
        }
        // All six edges render with their effect and target visible.
        let relationships = lines
            .iter()
            .filter(|line| line.text().starts_with("  ├") || line.text().starts_with("  └"))
            .count();
        assert_eq!(relationships, 6, "six target-bearing connectors: {text}");
        assert!(
            text.contains("→ apiStore.Healthy"),
            "edges name their target: {text}"
        );
        assert!(
            text.contains("→ readinessHandler"),
            "cycle/back edges name an earlier node: {text}"
        );
        assert!(
            text.contains("readinessServer.Shutdown last, after drain → readinessHandler"),
            "the closing cycle edge is explicit: {text}"
        );
        let body = lines.len() - first_node;
        assert!(
            body >= 30,
            "five boxes grow vertically, got {body} lines: {text}"
        );
        // Every line respects the requested width.
        for line in &lines {
            assert!(line.text().width() <= 96, "leaked: {:?}", line.text());
        }

        // The relationship contract at constrained widths: every connector still carries
        // `→ <target-head>`; the effect yields first, it never starves the target.
        for width in [36u16, 48, 60] {
            let narrow = plan_lines(&plan, width, "");
            let relationships: Vec<&DiagramLine> = narrow
                .iter()
                .filter(|line| line.text().starts_with("  ├") || line.text().starts_with("  └"))
                .collect();
            assert_eq!(relationships.len(), 6, "six relationships at {width}");
            for line in &relationships {
                let text = line.text();
                assert!(
                    text.contains(" → "),
                    "edge names its target at {width}: {text}"
                );
                let target_head = text
                    .rsplit_once(" → ")
                    .map(|(_, target)| target.trim())
                    .unwrap_or_default();
                assert!(
                    target_head.width() >= 3,
                    "recognizable target head at {width}: {text}"
                );
                assert!(
                    text.width() <= usize::from(width),
                    "width {width} leaked: {text}"
                );
            }
        }
    }

    /// The target stays visible whenever the connector, arrow, and one target cell fit.
    #[test]
    fn relationship_target_survives_until_the_label_floor() {
        let plan = cyclic_flow_plan();
        for width in 10..=17u16 {
            let text = plan_text(&plan, width, "");
            let relationships: Vec<&str> = text
                .lines()
                .filter(|l| l.starts_with("  ├") || l.starts_with("  └"))
                .collect();
            assert!(!relationships.is_empty(), "connectors render at {width}");
            for line in relationships {
                assert!(line.contains(" → "), "target visible at {width}: {line}");
            }
        }
    }

    /// Verified relationship edges get solid branches; an inferred edge in the same form
    /// dashes only its own branch.
    #[test]
    fn branching_flow_marks_verified_and_inferred_edges() {
        let entity = |symbol: &str| EntityRef {
            file: FileId::new("src/main.rs").unwrap(),
            symbol: Some(symbol.to_string()),
            range: None,
        };
        let mut plan = VisualizationPlan::new(Epoch(1));
        let nodes = vec![
            PlanNode::new("a", "signal", PlanNodeChange::Modified).with_entity(entity("signal")),
            PlanNode::new("b", "readiness", PlanNodeChange::Modified)
                .with_entity(entity("readiness")),
            PlanNode::new("c", "drain", PlanNodeChange::Modified).with_entity(entity("drain")),
        ];
        // Branching: two outgoing edges from `a` keep this off the linear-chain path.
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            nodes,
            edges: vec![
                PlanEdge {
                    from: "a".into(),
                    to: "b".into(),
                    kind: PlanEdgeKind::Calls,
                    label: Some("flips readiness".into()),
                },
                PlanEdge {
                    from: "a".into(),
                    to: "c".into(),
                    kind: PlanEdgeKind::Calls,
                    label: Some("starts drain".into()),
                },
                PlanEdge {
                    from: "b".into(),
                    to: "c".into(),
                    kind: PlanEdgeKind::Calls,
                    label: Some("releases after 503".into()),
                },
            ],
        });
        let text = plan_text(&plan, 96, "");
        assert!(
            text.contains("  ├─ flips readiness → readiness"),
            "solid verified branch: {text}"
        );
        assert!(
            text.contains("  └─ starts drain → drain"),
            "last branch: {text}"
        );
        assert!(!text.contains('┄'), "no dashed branch: {text}");
        assert!(!text.contains("cited diff"), "no basis note: {text}");

        // One Reads edge makes only its own branch dashed.
        plan.forms[0].edges[1].kind = PlanEdgeKind::Reads;
        let text = plan_text(&plan, 96, "");
        assert!(
            text.contains("  └┄ starts drain → drain"),
            "the inferred branch is dashed and still names its target: {text}"
        );
        assert!(
            text.contains("  ├─ flips readiness"),
            "the verified sibling stays solid: {text}"
        );
        assert!(
            !text.contains("inferred from cited diff"),
            "no legend: {text}"
        );
    }

    /// Box and relationship glyphs degrade safely at tiny widths: every line fits.
    #[test]
    fn cyclic_flow_fits_tiny_widths() {
        let plan = cyclic_flow_plan();
        for width in 1..=17u16 {
            for line in plan_lines(&plan, width, "") {
                assert!(
                    line.text().width() <= usize::from(width),
                    "width {width} leaked: {:?}",
                    line.text()
                );
            }
        }
    }

    /// A renderer-synthesized `becomes` edge must stay inferred in BOTH layouts, even
    /// when both nodes carry entities the validator could verify: the horizontal chain
    /// must not borrow the endpoints' trust for a presentational transition.
    #[test]
    fn synthetic_before_after_stays_inferred_even_with_entities() {
        let entity = |symbol: &str| EntityRef {
            file: FileId::new("src/main.rs").unwrap(),
            symbol: Some(symbol.to_string()),
            range: None,
        };
        let mut plan = before_after_plan();
        let form = &mut plan.forms[0];
        form.nodes[0].entity = Some(entity("cancelRoot"));
        form.nodes[1].entity = Some(entity("cancelSignal"));

        // Wide: the horizontal path (2 boxes + arrow). The synthetic Contains edge must
        // render dashed without adding a textual legend.
        let wide = plan_text(&plan, 100, "");
        assert!(
            wide.contains("┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄▷") || wide.contains('▷'),
            "dashed arrow in the horizontal path: {wide}"
        );
        assert!(
            !wide.contains('▶'),
            "no solid arrow for the synthetic edge: {wide}"
        );
        assert!(
            !wide.contains("inferred from cited diff"),
            "no legend: {wide}"
        );

        // Narrow: the stacked path keeps the same verdict.
        let narrow = plan_text(&plan, 40, "");
        assert!(narrow.contains("┊ becomes"), "inferred glyph: {narrow}");
        assert!(!narrow.contains("▼ becomes"), "no verified glyph: {narrow}");
        assert!(
            !narrow.contains("inferred from cited diff"),
            "no legend: {narrow}"
        );
    }

    /// The width contract for direct `plan_lines` calls: every emitted line's display
    /// width must fit the requested width, down to a single cell. Boxes and connectors
    /// degrade to their single-cell glyphs at pathological widths.
    #[test]
    fn plan_lines_respects_requested_width_at_tiny_sizes() {
        let plans = vec![
            flow_plan(),
            tree_plan(),
            before_after_plan(),
            seven_step_sequence_plan(),
        ];
        for width in 1..=17usize {
            for plan in &plans {
                for line in plan_lines(plan, width as u16, "shutdown") {
                    assert!(
                        line.text().width() <= width,
                        "width {width} leaked a {}-cell line: {:?}",
                        line.text().width(),
                        line.text()
                    );
                }
            }
        }
    }

    #[test]
    fn interactive_layout_expands_the_clicked_box_in_place_without_truncation() {
        let mut plan = before_after_plan();
        let node = &mut plan.forms[0].nodes[0];
        node.detail = Some("Prints the unversioned parse error and exits".to_string());
        node.expanded_detail = Some(
            "When parsing fails, the existing fatal branch prints the original error without a diagnostic-version marker, then preserves status one."
                .to_string(),
        );
        let target = PlanNodeTarget {
            form: 0,
            id: "before".to_string(),
        };
        let collapsed = interactive_plan_lines(&plan, 100, "", None, &[], &[], &[]);
        let expanded = [target.clone()];
        let lines = interactive_plan_lines(&plan, 100, "", Some(&target), &expanded, &[], &[]);
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.target.as_ref() == Some(&target) && span.role == DiagramRole::Hovered
        }));
        let text = lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        assert!(
            !text.contains("Details ·"),
            "expansion belongs to the clicked box: {text}"
        );
        assert!(text.contains("When parsing fails"), "{text}");
        assert!(text.contains("diagnostic-version"), "{text}");
        assert!(text.contains("preserves status one"), "{text}");
        assert!(
            lines.len() > collapsed.len(),
            "the selected box grows vertically"
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| span.target.as_ref() == Some(&target))
                .all(|span| !span.text.contains('…')),
            "expanded target has no ellipsis: {text}"
        );

        let unhovered = interactive_plan_lines(&plan, 100, "", None, &expanded, &[], &[]);
        assert!(
            unhovered
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.role != DiagramRole::Hovered),
            "an expanded box is not implicitly hovered"
        );
    }

    #[test]
    fn truncated_relationship_click_state_reveals_the_full_label() {
        let plan = seven_step_sequence_plan();
        let target = PlanRelationshipTarget {
            form: 0,
            from: "n0".to_string(),
            to: "n1".to_string(),
        };
        let collapsed = interactive_plan_lines(&plan, 40, "", None, &[], &[], &[]);
        let collapsed_text = collapsed
            .iter()
            .filter(|line| {
                line.spans
                    .iter()
                    .any(|span| span.relationship.as_ref() == Some(&target))
            })
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            collapsed_text.contains('…'),
            "label starts truncated: {collapsed_text}"
        );
        assert!(!collapsed_text.contains("unblocks waiters"));

        let expanded =
            interactive_plan_lines(&plan, 40, "", None, &[], &[], std::slice::from_ref(&target));
        let expanded_text = expanded
            .iter()
            .filter(|line| {
                line.spans
                    .iter()
                    .any(|span| span.relationship.as_ref() == Some(&target))
            })
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            expanded_text.contains("unblocks waiters"),
            "full label wraps after expansion: {expanded_text}"
        );
        for width in 1..=17u16 {
            for line in interactive_plan_lines(
                &plan,
                width,
                "",
                None,
                &[],
                &[],
                std::slice::from_ref(&target),
            ) {
                assert!(
                    line.text().width() <= usize::from(width),
                    "expanded relationship exceeds width {width}: {:?}",
                    line.text()
                );
            }
        }
    }
}
