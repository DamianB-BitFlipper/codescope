//! Width-aware terminal diagrams for validated AI plans and deterministic impact facts.
//!
//! The dispatcher deliberately publishes structure, not pre-rendered rows. This module is
//! the single layout boundary: it turns that structure into boxes, arrows, or a numbered
//! ladder for the pane width available during the current frame. Connectors distinguish
//! validator-verifiable relationships from hunk-derived interpretation.

use std::collections::{HashMap, HashSet};

use codescope_core::{
    DiffSide, FormKind, PlanCodeRef, PlanEdge, PlanEdgeKind, PlanEvidence, PlanNode,
    VisualizationPlan, VizForm,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::action::PlanNodeTarget;
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
    /// Review question or invariant.
    Review,
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
            }],
        }
    }

    fn for_node(text: impl Into<String>, role: DiagramRole, target: PlanNodeTarget) -> Self {
        Self {
            spans: vec![DiagramSpan {
                text: text.into(),
                role,
                target: Some(target),
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
}

const MIN_BOX_WIDTH: usize = 18;
const MAX_BOX_WIDTH: usize = 32;
const MIN_HORIZONTAL_GAP: usize = 10;
const MAX_HORIZONTAL_GAP: usize = 24;
/// Pane width at which evidence keeps a one-line reason per entry; below it the block
/// collapses to bare `basename:line` references.
const EVIDENCE_REASON_MIN_WIDTH: usize = 60;
/// Provenance note drawn before any form containing inferred connectors. It must fit a
/// 36-cell pane so the default narrow viewport still shows it above the visual.
const BASIS_NOTE: &str = "≈ ┊ = inferred from cited diff";

/// Review-focus prefixes marking a claim this diff cannot verify. The full Review block
/// renders below the visuals; this one-liner surfaces the risk in the default viewport.
const EXTERNAL_PREFIX: &str = "External assumption:";
/// Same treatment for facts deliberately outside this diff's scope.
const NOT_SHOWN_PREFIX: &str = "Not shown by this diff:";
/// `true` when the trimmed review focus carries an unverifiable-claim prefix.
fn needs_external_warning(review: &str) -> bool {
    let review = review.trim();
    review.starts_with(EXTERNAL_PREFIX) || review.starts_with(NOT_SHOWN_PREFIX)
}

/// Lay out a validated plan for the current pane width without transient interaction.
#[must_use]
pub fn plan_lines(plan: &VisualizationPlan, width: u16, selected_label: &str) -> Vec<DiagramLine> {
    interactive_plan_lines(plan, width, selected_label, None, None)
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
    expanded: Option<&PlanNodeTarget>,
) -> Vec<DiagramLine> {
    let width = usize::from(width).max(1);
    let mut lines = Vec::new();
    lines.extend(wrap_role(&plan.title, width, DiagramRole::Title, 2));
    lines.extend(wrap_role(&plan.intent, width, DiagramRole::Text, 2));
    // An unverifiable review claim gets its precise caveat above every visual; the full
    // Review block below repeats it. The upfront caveat precedes the inference
    // basis note so the strongest caveat is always the first thing after the intent.
    if let Some(review) = plan
        .review_focus
        .as_deref()
        .filter(|review| needs_external_warning(review))
    {
        lines.extend(wrap_prefixed("⚠ ", review, width, DiagramRole::Review, 2));
    }

    if let Some(target) = expanded {
        if let Some(node) = plan
            .forms
            .get(target.form)
            .and_then(|form| form.nodes.iter().find(|node| node.id == target.id))
        {
            render_expanded_node(node, target, width, &mut lines);
        }
    }

    for (index, form) in plan.forms.iter().enumerate() {
        if index > 0 {
            lines.push(DiagramLine::plain("", DiagramRole::Muted));
        }
        let context = DiagramContext {
            form: index,
            selected_label,
            hovered,
        };
        match form.kind {
            FormKind::RelationshipFlow | FormKind::Sequence => {
                render_flow(form, width, context, &mut lines);
            }
            FormKind::BeforeAfter => {
                render_before_after(form, width, context, &mut lines);
            }
            FormKind::ChangedSymbolTree | FormKind::CallTree | FormKind::TypeImplTree => {
                render_tree(form, width, context, &mut lines);
            }
            // These legacy variants cannot pass v4 validation. Keeping a safe rendering
            // path makes stale fixtures and hand-built snapshots non-panicking.
            FormKind::ImpactSummary | FormKind::FocusedDiff => {
                render_vertical_nodes(form, width, context, &mut lines);
            }
        }
    }

    if let Some(review) = plan
        .review_focus
        .as_deref()
        .filter(|review| !review.trim().is_empty())
    {
        lines.push(DiagramLine::plain("", DiagramRole::Muted));
        lines.extend(wrap_prefixed(
            "Review: ",
            review,
            width,
            DiagramRole::Review,
            3,
        ));
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

fn render_expanded_node(
    node: &PlanNode,
    target: &PlanNodeTarget,
    width: usize,
    lines: &mut Vec<DiagramLine>,
) {
    let heading = format!("Details · {}", node.label.trim());
    for text in wrap_text(&heading, width, 2) {
        lines.push(DiagramLine::for_node(
            text,
            DiagramRole::Selected,
            target.clone(),
        ));
    }
    let detail = node
        .expanded_detail
        .as_deref()
        .or(node.detail.as_deref())
        .unwrap_or_default()
        .trim();
    for text in wrap_text(detail, width, 5) {
        lines.push(DiagramLine::for_node(
            text,
            DiagramRole::Text,
            target.clone(),
        ));
    }
    for code_ref in &node.code_refs {
        let locator = code_ref_source(code_ref, width >= EVIDENCE_REASON_MIN_WIDTH);
        for text in wrap_text(&format!("Code · {locator}"), width, 2) {
            lines.push(DiagramLine::for_node(
                text,
                DiagramRole::Evidence,
                target.clone(),
            ));
        }
    }
}

fn code_ref_source(code_ref: &PlanCodeRef, full_path: bool) -> String {
    let file = code_ref.file.to_string();
    let file = if full_path {
        file.as_str()
    } else {
        basename(&file)
    };
    let side = match code_ref.side {
        DiffSide::Old => "old:",
        DiffSide::New => "new:",
    };
    let range = if code_ref.start_line == code_ref.end_line {
        code_ref.start_line.to_string()
    } else {
        format!("{}-{}", code_ref.start_line, code_ref.end_line)
    };
    format!("{file}[h{}] {side}{range}", code_ref.hunk.saturating_add(1))
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
        if (2..=4).contains(&count) && width >= min_width {
            render_horizontal_chain(&nodes, &edges, width, context, false, lines);
            return;
        }
        // Chains that do not fit side by side stack as a numbered ladder: one line per
        // step and one labeled rail per edge, so a full causal sequence stays readable
        // in a short pane instead of five-row boxes.
        render_sequence_ladder(&nodes, &edges, width, context, lines);
        return;
    }

    render_branching_flow(form, width, context, lines);
}

/// The vertical form for linear chains: ` 1  label — detail` steps joined by labeled
/// rails. Rails are solid (`│`) for validator-verifiable edges and dashed (`┊`) for
/// inferred ones. The provenance note precedes the ladder so a short viewport shows the
/// trust basis before the visual it explains.
fn render_sequence_ladder(
    nodes: &[&PlanNode],
    edges: &[&PlanEdge],
    width: usize,
    context: DiagramContext<'_>,
    lines: &mut Vec<DiagramLine>,
) {
    if edges
        .iter()
        .enumerate()
        .any(|(index, edge)| !edge_verified(nodes[index], nodes[index + 1], edge))
    {
        push_basis_line(width, lines);
    }
    if width < 6 {
        // The ladder grammar needs five cells for a step prefix and six for a rail;
        // below that, one truncated plain line per step and edge.
        for (index, node) in nodes.iter().enumerate() {
            let role = context.role(node, DiagramRole::Text);
            lines.push(DiagramLine::for_node(
                truncate(&format!("{}. {}", index + 1, node.label.trim()), width),
                role,
                context.target(node),
            ));
            if let Some(edge) = edges.get(index) {
                lines.push(DiagramLine::plain(
                    truncate(edge_label(edge), width),
                    DiagramRole::Arrow,
                ));
            }
        }
        return;
    }
    for (index, node) in nodes.iter().enumerate() {
        let step = format!("{:>2}", index + 1);
        let label = truncate(node.label.trim(), width.saturating_sub(5));
        let target = context.target(node);
        let node_role = context.role(node, DiagramRole::Text);
        let mut spans = vec![
            DiagramSpan {
                text: format!(" {step}  "),
                role: node_role,
                target: Some(target.clone()),
            },
            DiagramSpan {
                text: label.clone(),
                role: node_role,
                target: Some(target.clone()),
            },
        ];
        // The detail rides on the step line only while it fits; it never wraps or
        // pushes the label off-screen.
        if let Some(detail) = node
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
        {
            let remaining = width.saturating_sub(label.width() + 5 + 3);
            if remaining >= 8 {
                spans.push(DiagramSpan {
                    text: format!(" — {}", truncate(detail, remaining)),
                    role: node_role,
                    target: Some(target.clone()),
                });
            }
        }
        lines.push(DiagramLine { spans });
        if let Some(edge) = edges.get(index) {
            let verified = edge_verified(nodes[index], nodes[index + 1], edge);
            let rail = if verified { "│" } else { "┊" };
            let label = truncate(edge_label(edge), width.saturating_sub(6));
            lines.push(DiagramLine {
                spans: vec![
                    DiagramSpan {
                        text: format!("    {rail} "),
                        role: DiagramRole::Muted,
                        target: None,
                    },
                    DiagramSpan {
                        text: label,
                        role: DiagramRole::Arrow,
                        target: None,
                    },
                ],
            });
        }
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

/// One concise muted provenance note under a form that contains inferred connectors.
fn push_basis_line(width: usize, lines: &mut Vec<DiagramLine>) {
    lines.push(DiagramLine::plain(
        truncate(BASIS_NOTE, width),
        DiagramRole::Muted,
    ));
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
    let any_inferred = synthetic
        || edges
            .iter()
            .enumerate()
            .any(|(index, edge)| !edge_verified(nodes[index], nodes[index + 1], edge));
    if any_inferred {
        push_basis_line(width, lines);
    }
    let boxes: Vec<Vec<String>> = nodes
        .iter()
        .map(|node| {
            node_box_text(
                &node.label,
                node.detail.as_deref().unwrap_or_default(),
                box_width,
            )
        })
        .collect();
    for (row, _) in boxes[0].iter().enumerate() {
        let mut spans = vec![DiagramSpan {
            text: left_pad.clone(),
            role: DiagramRole::Muted,
            target: None,
        }];
        for (index, node) in nodes.iter().enumerate() {
            let normal = if row == 1 || row == 2 || row == 3 {
                DiagramRole::Text
            } else {
                DiagramRole::Border
            };
            spans.push(DiagramSpan {
                text: boxes[index][row].clone(),
                role: context.role(node, normal),
                target: Some(context.target(node)),
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
    // The provenance note precedes the visual, so it must be decided up front.
    let any_inferred = form.edges.iter().any(|edge| {
        !matches!((by_id.get(edge.from.as_str()), by_id.get(edge.to.as_str())), (Some(from), Some(to))
            if edge_verified(from, to, edge))
    });
    if any_inferred {
        push_basis_line(width, lines);
    }
    // Compact adjacency grammar: each node renders once in stable document order on a
    // full-width line, followed by one child line per outgoing edge naming BOTH the
    // edge effect and its target. Cycles and shared targets stay clear because every
    // edge names its destination; no DFS recursion, no dangling arrows.
    for node in &form.nodes {
        let target = context.target(node);
        let node_role = context.role(node, DiagramRole::Text);
        let label = truncate(node.label.trim(), width);
        let mut spans = vec![DiagramSpan {
            text: label.clone(),
            role: node_role,
            target: Some(target.clone()),
        }];
        if let Some(detail) = node
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
        {
            let remaining = width.saturating_sub(label.width() + 3);
            if remaining >= 8 {
                spans.push(DiagramSpan {
                    text: format!(" — {}", truncate(detail, remaining)),
                    role: node_role,
                    target: Some(target.clone()),
                });
            }
        }
        lines.push(DiagramLine { spans });
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
            render_adjacency_edge(edge, target, verified, last, width, context, lines);
        }
    }
}

/// One compact outgoing-edge line under its source node: `  ├┄ <effect> → <target>`.
/// The branch is solid only for validator-verified edges; the effect and the target
/// label share the remaining width, the effect yielding first.
fn render_adjacency_edge(
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
    // ` → ` between the effect and the target label.
    const ARROW_WIDTH: usize = 3;
    // Smallest still-recognizable heads for either side of the arrow.
    const MIN_SIDE_WIDTH: usize = 4;
    let Some(target) = target else {
        // Unknown target id: nothing to name; the effect takes the line.
        if width < prefix_width + 2 {
            lines.push(DiagramLine::plain(
                truncate(edge_label(edge), width),
                DiagramRole::Arrow,
            ));
            return;
        }
        lines.push(DiagramLine {
            spans: vec![
                DiagramSpan {
                    text: prefix,
                    role: DiagramRole::Arrow,
                    target: None,
                },
                DiagramSpan {
                    text: truncate(edge_label(edge).trim(), width - prefix_width),
                    role: DiagramRole::Arrow,
                    target: None,
                },
            ],
        });
        return;
    };
    let target_label = target.label.trim();
    // Genuinely tiny: even branch + arrow + one target cell cannot fit.
    if width < prefix_width + ARROW_WIDTH + 2 {
        lines.push(DiagramLine::plain(
            truncate(edge_label(edge), width),
            DiagramRole::Arrow,
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
            },
            DiagramSpan {
                text: truncate(effect_full, effect_budget),
                role: DiagramRole::Arrow,
                target: None,
            },
            DiagramSpan {
                text: " → ".into(),
                role: DiagramRole::Muted,
                target: None,
            },
            DiagramSpan {
                text: truncate(target_label, target_budget),
                role: context.role(target, DiagramRole::Text),
                target: Some(context.target(target)),
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
        if width >= min_width {
            render_horizontal_chain(&nodes, &[edge], width, context, is_synthetic, lines);
        } else {
            let box_width = width.min(MAX_BOX_WIDTH);
            let verified = !is_synthetic
                && form
                    .edges
                    .first()
                    .is_some_and(|real| edge_verified(nodes[0], nodes[1], real));
            if !verified {
                push_basis_line(width, lines);
            }
            lines.extend(plan_node_box(nodes[0], box_width, context));
            lines.push(centered_arrow(edge_label(edge), width, verified));
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
    let by_id: HashMap<&str, &PlanNode> = form
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let children: HashSet<&str> = form
        .nodes
        .iter()
        .flat_map(|node| node.children.iter().map(String::as_str))
        .collect();
    // Trust: tree parent/child links are interpretive unless an explicit edge that
    // passes `edge_verified` backs them. One basis note covers the whole form.
    if form.nodes.iter().any(|node| {
        node.children
            .iter()
            .any(|child| !link_verified(form, &by_id, node, child))
    }) {
        push_basis_line(width, lines);
    }
    // Cycle safety: validation guarantees proper trees, but stale fixtures and
    // hand-built snapshots must never loop; every node renders at most once.
    let mut shown: HashSet<&str> = HashSet::new();
    for root in form
        .nodes
        .iter()
        .filter(|node| !children.contains(node.id.as_str()))
    {
        render_tree_node(
            root, form, &by_id, "", true, true, width, context, &mut shown, lines,
        );
    }
}

/// A parent/child link is verified only when the form carries an explicit edge for it
/// that passes the validator-verifiable test (both endpoints with entities, graph-
/// checkable kind). Absent edges mean the tree shape is the AI's interpretation.
fn link_verified(
    form: &VizForm,
    by_id: &HashMap<&str, &PlanNode>,
    parent: &PlanNode,
    child: &str,
) -> bool {
    form.edges.iter().any(|edge| {
        edge.from == parent.id
            && edge.to == child
            && by_id
                .get(child)
                .is_some_and(|child| edge_verified(parent, child, edge))
    })
}

#[allow(clippy::too_many_arguments)]
fn render_tree_node<'a>(
    node: &'a PlanNode,
    form: &VizForm,
    by_id: &HashMap<&'a str, &'a PlanNode>,
    indent: &str,
    last: bool,
    verified_link: bool,
    width: usize,
    context: DiagramContext<'_>,
    shown: &mut HashSet<&'a str>,
    lines: &mut Vec<DiagramLine>,
) {
    if !shown.insert(node.id.as_str()) {
        return;
    }
    // The compact show-me tree grammar: one physical line per node, guide rails
    // between levels, and a dashed branch when the link is interpretive.
    let branch = if indent.is_empty() {
        ""
    } else {
        match (last, verified_link) {
            (true, true) => "└─ ",
            (true, false) => "└┄ ",
            (false, true) => "├─ ",
            (false, false) => "├┄ ",
        }
    };
    let prefix = format!("{indent}{branch}");
    let prefix_width = prefix.width();
    let node_role = context.role(node, DiagramRole::Text);
    let target = context.target(node);
    if prefix_width >= width {
        // The guide rail cannot fit at this depth and width: a plain truncated line
        // that still respects the requested width.
        lines.push(DiagramLine::for_node(
            truncate(node.label.trim(), width),
            node_role,
            target.clone(),
        ));
    } else {
        let label_budget = width - prefix_width;
        let label = truncate(node.label.trim(), label_budget);
        let mut spans = vec![
            DiagramSpan {
                text: prefix,
                role: DiagramRole::Arrow,
                target: None,
            },
            DiagramSpan {
                text: label.clone(),
                role: node_role,
                target: Some(target.clone()),
            },
        ];
        // The detail rides on the node line only while it fits; it never wraps.
        if let Some(detail) = node
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
        {
            let remaining = label_budget.saturating_sub(label.width() + 3);
            if remaining >= 8 {
                spans.push(DiagramSpan {
                    text: format!(" — {}", truncate(detail, remaining)),
                    role: node_role,
                    target: Some(target.clone()),
                });
            }
        }
        lines.push(DiagramLine { spans });
    }
    let descendants: Vec<&PlanNode> = node
        .children
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .collect();
    // The continuation rail matches the branch that opened this level.
    let rail = if verified_link { "│ " } else { "┊ " };
    let next_indent = format!("{indent}{}", if last { "  " } else { rail });
    for (index, child) in descendants.iter().enumerate() {
        let verified = link_verified(form, by_id, node, child.id.as_str());
        render_tree_node(
            child,
            form,
            by_id,
            &next_indent,
            index + 1 == descendants.len(),
            verified,
            width,
            context,
            shown,
            lines,
        );
    }
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

fn edge_label(edge: &PlanEdge) -> &str {
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

fn node_box(label: &str, detail: &str, width: usize, selected: bool) -> Vec<DiagramLine> {
    if width < 4 {
        let role = if selected {
            DiagramRole::Selected
        } else {
            DiagramRole::Text
        };
        let mut out = vec![DiagramLine::plain(truncate(label.trim(), width), role)];
        out.extend(
            wrap_text(detail.trim(), width, 2)
                .into_iter()
                .map(|line| DiagramLine::plain(line, role)),
        );
        return out;
    }
    node_box_text(label, detail, width)
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
    let detail = node.detail.as_deref().unwrap_or_default();
    // A box needs at least four cells to draw its own borders; below that degrade to
    // truncated plain lines so no line exceeds the requested width.
    if width < 4 {
        let role = context.role(node, DiagramRole::Text);
        let mut out = vec![DiagramLine::for_node(
            truncate(node.label.trim(), width),
            role,
            target.clone(),
        )];
        out.extend(
            wrap_text(detail.trim(), width, 2)
                .into_iter()
                .map(|line| DiagramLine::for_node(line, role, target.clone())),
        );
        return out;
    }
    node_box_text(&node.label, detail, width)
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            let normal = if index == 0 || index == 4 {
                DiagramRole::Border
            } else {
                DiagramRole::Text
            };
            DiagramLine::for_node(text, context.role(node, normal), target.clone())
        })
        .collect()
}

fn node_box_text(label: &str, detail: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let inner = width.saturating_sub(2);
    let label = truncate(label.trim(), inner);
    let mut detail_lines = wrap_text(detail.trim(), inner, 2);
    detail_lines.resize(2, String::new());
    vec![
        format!("┌{}┐", "─".repeat(inner)),
        format!("│{}│", pad(&label, inner)),
        format!("│{}│", pad(&detail_lines[0], inner)),
        format!("│{}│", pad(&detail_lines[1], inner)),
        format!("└{}┘", "─".repeat(inner)),
    ]
}

fn centered_arrow(label: &str, width: usize, verified: bool) -> DiagramLine {
    // The arrow prefix needs four cells; below that only the truncated label fits.
    if width < 4 {
        return DiagramLine::plain(truncate(label.trim(), width), DiagramRole::Arrow);
    }
    let label = truncate(label.trim(), width.saturating_sub(4));
    let glyph = if verified { "▼" } else { "┊" };
    DiagramLine::plain(format!("  {glyph} {label}"), DiagramRole::Arrow)
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
        let mut plan = VisualizationPlan::new(Epoch(1), "How does shutdown drain traffic?");
        plan.title = "Readiness-gated graceful drain".into();
        plan.intent = "Stop new traffic before waiting for in-flight requests.".into();
        plan.review_focus = Some("Confirm the drain budget exceeds probe propagation time.".into());
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            title: "runtime".into(),
            summary: String::new(),
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
        // flow_plan's nodes carry no entities, so its arrows are inferred: dashed, with
        // one provenance note. Solid arrows stay reserved for verified relationships.
        let text = plan_text(&flow_plan(), 100, "shutdown");
        assert!(text.contains("┌"));
        assert!(text.contains("▷"));
        assert!(
            !text.contains("▶"),
            "no solid arrow for inferred edges: {text}"
        );
        assert!(text.contains("readiness becomes"));
        assert!(
            text.contains("≈ ┊ = inferred from cited diff"),
            "basis note: {text}"
        );
        let note = text.find("inferred from cited diff").expect("note");
        let first_box = text.find('┌').expect("boxes");
        assert!(note < first_box, "the note precedes the visual: {text}");
    }

    #[test]
    fn narrow_flow_uses_numbered_ladder() {
        // A 3-step chain that cannot fit side by side renders as a ladder: numbered
        // steps, full-width edge rails, and no five-row boxes.
        let text = plan_text(&flow_plan(), 40, "shutdown");
        assert!(text.contains(" 1  shutdown"), "first step: {text}");
        assert!(text.contains(" 2  load balancer"), "second step: {text}");
        assert!(
            text.contains("readiness becomes 503"),
            "edge label keeps the pane width: {text}"
        );
        assert!(text.contains('┊'), "inferred rail: {text}");
        assert!(
            text.contains("≈ ┊ = inferred from cited diff"),
            "basis note: {text}"
        );
        let note = text.find("inferred from cited diff").expect("note");
        let first_step = text.find(" 1  shutdown").expect("first step");
        assert!(note < first_step, "the note precedes the ladder: {text}");
        assert!(!text.contains('┌'), "no boxes in the ladder: {text}");
    }

    /// The real-world failure shape: a validated 7-step sequence plan must render as a
    /// numbered ladder whose edge labels keep the full pane width (they were previously
    /// truncated against the box width at every terminal size).
    /// The validated 7-step sequence shape from the real AI baseline plan.
    fn seven_step_sequence_plan() -> VisualizationPlan {
        let mut plan = VisualizationPlan::new(Epoch(1), "How does the API drain traffic?");
        plan.title = "API graceful drain".into();
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
            title: "runtime".into(),
            summary: String::new(),
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
    fn seven_node_sequence_ladder_shows_every_step() {
        let plan = seven_step_sequence_plan();
        let lines = plan_lines(&plan, 96, "");
        let text = lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        for step in 1..=7 {
            assert!(
                text.contains(&format!(" {step:>2}  ")),
                "step {step} numbered: {text}"
            );
        }
        assert!(text.contains(" 1  SIGTERM received"));
        assert!(text.contains(" 7  server.Shutdown drains"));
        assert!(
            text.contains("SIGTERM/SIGINT triggers shutdown, unblocks waiters"),
            "the longest causal label is not box-truncated: {text}"
        );
        assert!(text.contains('┊'), "entityless chain is inferred: {text}");
        assert!(
            text.contains("≈ ┊ = inferred from cited diff"),
            "basis note: {text}"
        );
        let note = text.find("inferred from cited diff").expect("note");
        let first_step = text.find(" 1  SIGTERM received").expect("first step");
        assert!(note < first_step, "the note precedes the ladder: {text}");
        // One line per step and one per edge: the whole chain fits a short pane.
        assert!(
            lines.len() <= 20,
            "compact ladder, got {} lines",
            lines.len()
        );
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
            title: "runtime".into(),
            summary: String::new(),
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
        let mut plan = VisualizationPlan::new(Epoch(1), "focus");
        plan.forms.push(form.clone());
        // 40 cells cannot fit two boxes side by side, so the ladder path renders.
        let text = plan_text(&plan, 40, "shutdown");
        assert!(text.contains('│'), "solid rail: {text}");
        assert!(!text.contains('┊'), "no inferred rail: {text}");
        assert!(!text.contains("cited diff"), "no basis note: {text}");

        form.edges[0].kind = PlanEdgeKind::Writes;
        plan.forms[0] = form;
        let text = plan_text(&plan, 40, "");
        assert!(text.contains('┊'), "writes edge is inferred: {text}");
        assert!(
            text.contains("≈ ┊ = inferred from cited diff"),
            "basis note: {text}"
        );
    }

    /// A chain mixing verified and inferred edges marks only the inferred rails and
    /// emits exactly one basis note.
    #[test]
    fn mixed_edges_mark_only_inferred_rails() {
        let entity = |symbol: &str| EntityRef {
            file: FileId::new("src/main.rs").unwrap(),
            symbol: Some(symbol.to_string()),
            range: None,
        };
        let mut plan = VisualizationPlan::new(Epoch(1), "focus");
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            title: "runtime".into(),
            summary: String::new(),
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
            text.contains("│ clears the healthy flag"),
            "verified rail stays solid: {text}"
        );
        assert!(
            text.contains("┊ waits the drain delay"),
            "entityless endpoint makes the rail inferred: {text}"
        );
        assert_eq!(
            text.matches("≈ ┊ = inferred from cited diff").count(),
            1,
            "exactly one basis note: {text}"
        );
    }

    /// Intent is capped at two lines: the ladder and title carry the story.
    #[test]
    fn long_intent_caps_at_two_lines() {
        let mut plan = VisualizationPlan::new(Epoch(1), "focus");
        plan.title = "Title".into();
        plan.intent = "word ".repeat(60);
        plan.forms.push(VizForm {
            kind: FormKind::ChangedSymbolTree,
            title: "tree".into(),
            summary: String::new(),
            nodes: vec![PlanNode::new("n1", "root", PlanNodeChange::Modified)],
            edges: Vec::new(),
        });
        let lines = plan_lines(&plan, 40, "");
        let intent_lines: Vec<&DiagramLine> = lines
            .iter()
            .filter(|line| {
                line.spans
                    .first()
                    .is_some_and(|span| span.role == DiagramRole::Text)
            })
            .collect();
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
        let mut plan = VisualizationPlan::new(Epoch(1), "focus");
        plan.title = "Title".into();
        plan.forms.push(VizForm {
            kind: FormKind::ChangedSymbolTree,
            title: "tree".into(),
            summary: String::new(),
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

    /// At a 17-cell pane the ladder fits every line inside the width and never draws
    /// boxes that would clip their borders.
    #[test]
    fn ladder_fits_narrow_width_without_boxes() {
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
        assert!(!text.contains('┌'), "no boxes at 17 cells: {text}");
        assert!(text.contains(" 1  shutdown"), "steps survive: {text}");
    }

    /// Narrow BeforeAfter boxes never exceed the pane: every top border closes.
    #[test]
    fn before_after_narrow_keeps_box_borders_intact() {
        let mut plan = VisualizationPlan::new(Epoch(1), "How does shutdown change?");
        plan.title = "Shutdown ownership moves".into();
        plan.intent = "Only signal handling is cancelled.".into();
        plan.forms.push(VizForm {
            kind: FormKind::BeforeAfter,
            title: "transition".into(),
            summary: String::new(),
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
        let mut plan = VisualizationPlan::new(Epoch(1), "Where is readiness owned?");
        plan.title = "Readiness ownership".into();
        plan.intent = "The API owns a listener and its readiness handler.".into();
        let mut root = PlanNode::new("root", "API", PlanNodeChange::Modified)
            .with_detail("owns readiness behavior");
        root.children = vec!["listener".into(), "handler".into()];
        plan.forms.push(VizForm {
            kind: FormKind::ChangedSymbolTree,
            title: "ownership".into(),
            summary: String::new(),
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
        // No explicit edges: the compact tree uses dashed branches plus one basis note.
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
            text.contains("≈ ┊ = inferred from cited diff"),
            "basis note: {text}"
        );
        let note = lines
            .iter()
            .position(|line| line.text().contains("inferred from cited diff"))
            .expect("note");
        let root = lines
            .iter()
            .position(|line| line.text().starts_with("API"))
            .expect("root line");
        assert!(note < root, "the note precedes the root: {text}");
        assert!(!text.contains('┌'), "no box glyphs: {text}");
        // One physical line per node: each label lands on exactly one line.
        for label in ["API", "readiness listener", "readinessHandler"] {
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| {
                        if label == "API" {
                            line.text().starts_with("API")
                        } else {
                            line.text().contains(label)
                        }
                    })
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
        let mut plan = VisualizationPlan::new(Epoch(1), "How does shutdown change?");
        plan.title = "Shutdown ownership moves".into();
        plan.intent = "The root context survives while only signal handling is cancelled.".into();
        plan.forms.push(VizForm {
            kind: FormKind::BeforeAfter,
            title: "transition".into(),
            summary: String::new(),
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
        let mut plan = VisualizationPlan::new(
            Epoch(1),
            "How does the new readiness port and reordered shutdown sequence drain the API?",
        );
        plan.title = "Sandbox API drains gracefully via readiness port".into();
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
            title: "Sandbox API graceful drain sequence".into(),
            summary: String::new(),
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

    /// The live six-node tree renders one line per node at both 36 and 96 cells —
    /// no five-row boxes — with the basis note before the root and every line inside
    /// the requested width.
    #[test]
    fn six_node_tree_renders_compactly_at_36_and_96() {
        let plan = six_node_tree_plan();
        let labels = [
            "main.go run()",
            "readinessServer starts",
            "SIGTERM cancels signalCtx",
            "readiness flips to 503",
            "sleep shutdownDrainDelay (10s)",
            "drain mTLS, close readiness last",
        ];
        for width in [36u16, 96] {
            let lines = plan_lines(&plan, width, "");
            let text = lines
                .iter()
                .map(DiagramLine::text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!text.contains('┌'), "no box glyphs at {width}: {text}");
            let note = text.find("inferred from cited diff").expect("note");
            let root = text.find("main.go run()").expect("root");
            assert!(note < root, "basis before the root at {width}: {text}");
            for label in labels {
                // At 36 cells the longest label truncates with an ellipsis; its
                // recognizable head must still be visible. At 96 the full label fits.
                let visible: String = label.chars().take(22).collect();
                assert!(
                    text.contains(visible.as_str()),
                    "{label} visible at {width}: {text}"
                );
            }
            if width == 96 {
                for label in labels {
                    assert!(text.contains(label), "full {label} at 96: {text}");
                }
            }
            // One line per node: each label lands on exactly one physical line.
            for label in labels {
                let visible: String = label.chars().take(22).collect();
                let count = lines
                    .iter()
                    .filter(|line| line.text().contains(visible.as_str()))
                    .count();
                assert_eq!(count, 1, "{label} once at {width}: {text}");
            }
            // The whole plan fits a short pane: 6 tree lines + title/intent/note +
            // review + evidence footer.
            assert!(
                lines.len() <= 18,
                "compact body at {width}, got {} lines",
                lines.len()
            );
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

    /// The live plan's review focus (`External assumption: ...`) yields a precise
    /// warning above every visual — before the basis note and the first form — while
    /// the full Review block stays at the bottom. Ordinary review questions get no
    /// upfront warning.
    #[test]
    fn external_assumption_warning_precedes_everything() {
        let mut plan = seven_step_sequence_plan();
        plan.review_focus = Some(
            "External assumption: the 10s drain delay matches the load balancer's probe interval."
                .into(),
        );
        let lines = plan_lines(&plan, 96, "");
        let text = lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        let warning = lines
            .iter()
            .position(|line| line.text().contains("External assumption"))
            .expect("warning line");
        let basis = lines
            .iter()
            .position(|line| line.text().contains("inferred from cited diff"))
            .expect("basis note");
        let first_step = lines
            .iter()
            .position(|line| line.text().contains(" 1  SIGTERM received"))
            .expect("first step");
        assert!(
            warning < basis && basis < first_step,
            "warning < basis < visual: {text}"
        );
        // The warning line carries the Review role (WARN styling in the renderer).
        assert!(
            lines[warning]
                .spans
                .iter()
                .any(|span| span.role == DiagramRole::Review),
            "warning uses the Review role: {text}"
        );
        // The precise caveat is repeated intentionally: once before the visual and once
        // in the full Review block below it, so the first viewport cannot hide the risk.
        assert!(
            text.matches("External assumption").count() >= 2,
            "precise caveat stays visible above and below: {text}"
        );
        let full_review = lines
            .iter()
            .position(|line| line.text().contains("Review:"))
            .expect("full review block");
        assert!(first_step < full_review, "full block stays at the bottom");

        // The alternative prefix warns too.
        plan.review_focus =
            Some("Not shown by this diff: the load balancer's actual probe interval.".into());
        let text = plan_text(&plan, 96, "");
        assert!(
            text.lines()
                .take(5)
                .any(|line| line.contains("Not shown by this diff")),
            "not-shown caveat is explicit above the visual: {text}"
        );

        // An ordinary review question gets no upfront warning.
        plan.review_focus = Some("Confirm the drain budget exceeds propagation time.".into());
        let text = plan_text(&plan, 96, "");
        assert_eq!(
            text.matches("Confirm the drain budget").count(),
            1,
            "ordinary question stays in the Review block only: {text}"
        );
        assert!(text.contains("Review: Confirm"), "block retained: {text}");
    }

    /// The upfront warning degrades by truncation at tiny widths instead of overflowing.
    #[test]
    fn external_assumption_warning_fits_tiny_widths() {
        let mut plan = seven_step_sequence_plan();
        plan.review_focus = Some("External assumption: probes stop before drain.".into());
        for width in 1..=17u16 {
            for line in plan_lines(&plan, width, "") {
                assert!(
                    line.text().width() <= usize::from(width),
                    "width {width} leaked: {:?}",
                    line.text()
                );
            }
            let text = plan_text(&plan, width, "");
            // Even at one cell the truncated warning still renders (⚠ …).
            assert!(
                text.contains('⚠') || width < 3,
                "warning glyph survives at {width}: {text}"
            );
        }
    }

    /// A tree whose parent/child links are all backed by explicit verified edges is a
    /// fully verified tree: solid branches and NO basis note.
    #[test]
    fn verified_tree_links_render_solid_without_basis_note() {
        let entity = |symbol: &str| EntityRef {
            file: FileId::new("src/main.rs").unwrap(),
            symbol: Some(symbol.to_string()),
            range: None,
        };
        let mut plan = VisualizationPlan::new(Epoch(1), "Where is readiness owned?");
        plan.title = "Readiness ownership".into();
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
            title: "ownership".into(),
            summary: String::new(),
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
            text.contains("≈ ┊ = inferred from cited diff"),
            "note: {text}"
        );
    }

    /// Multiple roots each render without a branch prefix, and depth-two subtrees keep
    /// nested guide rails. With no edges the form carries exactly one basis note.
    #[test]
    fn tree_supports_multiple_roots_and_nested_guides() {
        let mut plan = VisualizationPlan::new(Epoch(1), "Who owns what?");
        plan.title = "Ownership".into();
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
            title: "owners".into(),
            summary: String::new(),
            nodes: vec![first, a1, a1x, a2, second],
            edges: Vec::new(),
        });
        let lines = plan_lines(&plan, 60, "");
        let text = lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        // Both roots render unprefixed, one line each.
        assert!(
            lines.iter().any(|line| line.text() == "API"),
            "first root is a bare line: {text}"
        );
        assert!(
            lines.iter().any(|line| line.text() == "CLI"),
            "second root is a bare line: {text}"
        );
        // Guide rails: a non-last child opens a │ rail for its own subtree, and the
        // nested closing branch renders under it.
        assert!(
            text.contains("  ├┄ handler"),
            "non-last child branch: {text}"
        );
        assert!(
            text.contains("  ┊ └┄ inner"),
            "nested softer rail under an unverified branch: {text}"
        );
        assert!(
            text.contains("  └┄ second"),
            "last child closes the level: {text}"
        );
        assert_eq!(
            text.matches("≈ ┊ = inferred from cited diff").count(),
            1,
            "exactly one basis note: {text}"
        );
        // Exactly one line per node across both roots.
        assert_eq!(
            lines
                .iter()
                .filter(|line| {
                    line.text().contains("API")
                        || line.text().contains("handler")
                        || line.text().contains("inner")
                        || line.text().contains("second")
                        || line.text().contains("CLI")
                })
                .count(),
            5,
            "one line per node: {text}"
        );
    }

    /// The exact five-node/six-edge cyclic relationship_flow from the final completed-
    /// build artifact: document-order nodes, target-bearing adjacency edges, a visible
    /// cycle, and a compact body.
    fn cyclic_flow_plan() -> VisualizationPlan {
        let mut plan = VisualizationPlan::new(
            Epoch(1),
            "How does the new plaintext readiness port interact with the mTLS shutdown sequence?",
        );
        plan.title = "Plaintext readiness port gates graceful API drain".into();
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
            title: "API adds plaintext readiness port driving graceful drain".into(),
            summary: String::new(),
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

    /// The final cyclic artifact renders compactly: no boxes, each node exactly once,
    /// all six edges carrying their target, the cycle visible, one basis note first.

    #[test]
    fn cyclic_flow_renders_compact_adjacency() {
        let plan = cyclic_flow_plan();
        let lines = plan_lines(&plan, 96, "");
        let text = lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains('┌'), "no box glyphs: {text}");
        // One basis note before any node line.
        let basis = lines
            .iter()
            .position(|line| line.text().contains("inferred from cited diff"))
            .expect("basis note");
        let first_node = lines
            .iter()
            .position(|line| line.text().starts_with("readinessHandler"))
            .expect("first node");
        assert!(basis < first_node, "basis before the visual: {text}");
        // Every node renders exactly once on its own document-order line.
        for label in [
            "readinessHandler",
            "apiStore.Healthy",
            "readinessServer on config.ReadinessPort",
            "shutdown goroutine after signalCtx.Done",
            "s.Shutdown (mTLS server)",
        ] {
            let count = lines
                .iter()
                .filter(|line| line.text().starts_with(label))
                .count();
            assert_eq!(count, 1, "{label} once as a node line: {text}");
        }
        // All six edges render with their effect and target visible.
        let adjacency = lines
            .iter()
            .filter(|line| line.text().starts_with("  ├") || line.text().starts_with("  └"))
            .count();
        assert_eq!(adjacency, 6, "six target-bearing edge lines: {text}");
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
        // Compact body: roughly nodes + edges (5 + 6 = 11 lines), not five-row boxes.
        let body = lines.len() - basis;
        assert!(body <= 12, "compact body, got {body} lines: {text}");
        // Every line respects the requested width.
        for line in &lines {
            assert!(line.text().width() <= 96, "leaked: {:?}", line.text());
        }

        // The adjacency contract at constrained widths: every edge line still carries
        // `→ <target-head>`; the effect yields first, it never starves the target.
        for width in [36u16, 48, 60] {
            let narrow = plan_lines(&plan, width, "");
            let adjacency: Vec<&DiagramLine> = narrow
                .iter()
                .filter(|line| line.text().starts_with("  ├") || line.text().starts_with("  └"))
                .collect();
            assert_eq!(adjacency.len(), 6, "six adjacency edges at {width}");
            for line in &adjacency {
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

    /// The target is omitted ONLY in genuinely tiny widths: below the grammar floor
    /// (branch + arrow + a target cell) the edge degrades to a plain effect line.
    #[test]
    fn adjacency_target_survives_until_the_grammar_floor() {
        let plan = cyclic_flow_plan();
        for width in 10..=17u16 {
            let text = plan_text(&plan, width, "");
            let adjacency: Vec<&str> = text
                .lines()
                .filter(|l| l.starts_with("  ├") || l.starts_with("  └"))
                .collect();
            assert!(!adjacency.is_empty(), "grammar renders at {width}");
            for line in adjacency {
                assert!(line.contains(" → "), "target visible at {width}: {line}");
            }
        }
    }

    /// Verified adjacency edges get solid branches and no basis note; a single inferred
    /// edge in the same form dashes its own branch and adds exactly one note.
    #[test]
    fn branching_flow_marks_verified_and_inferred_edges() {
        let entity = |symbol: &str| EntityRef {
            file: FileId::new("src/main.rs").unwrap(),
            symbol: Some(symbol.to_string()),
            range: None,
        };
        let mut plan = VisualizationPlan::new(Epoch(1), "focus");
        plan.title = "Flow".into();
        let nodes = vec![
            PlanNode::new("a", "signal", PlanNodeChange::Modified).with_entity(entity("signal")),
            PlanNode::new("b", "readiness", PlanNodeChange::Modified)
                .with_entity(entity("readiness")),
            PlanNode::new("c", "drain", PlanNodeChange::Modified).with_entity(entity("drain")),
        ];
        // Branching: two outgoing edges from `a` keep this off the ladder path.
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            title: "flow".into(),
            summary: String::new(),
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

        // One Reads edge makes its own branch dashed and adds exactly one note.
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
        assert_eq!(
            text.matches("≈ ┊ = inferred from cited diff").count(),
            1,
            "exactly one basis note: {text}"
        );
    }

    /// The adjacency grammar degrades safely at tiny widths: every line fits.
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
        // render dashed with the basis note before the boxes.
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
            wide.contains("≈ ┊ = inferred from cited diff"),
            "basis note: {wide}"
        );
        let note = wide.find("inferred from cited diff").expect("note");
        let first_box = wide.find('┌').expect("boxes");
        assert!(note < first_box, "the note precedes the visual: {wide}");

        // Narrow: the stacked path keeps the same verdict.
        let narrow = plan_text(&plan, 40, "");
        assert!(narrow.contains("┊ becomes"), "inferred glyph: {narrow}");
        assert!(!narrow.contains("▼ becomes"), "no verified glyph: {narrow}");
        assert!(
            narrow.contains("≈ ┊ = inferred from cited diff"),
            "basis note: {narrow}"
        );
    }

    /// The width contract for direct `plan_lines` calls: every emitted line's display
    /// width must fit the requested width, down to a single cell. The ladder's fixed
    /// prefixes and the box grammar degrade to truncated plain lines instead.
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
    fn interactive_layout_tags_hover_nodes_and_expands_exact_code_refs() {
        let mut plan = flow_plan();
        let node = &mut plan.forms[0].nodes[0];
        node.expanded_detail =
            Some("Marks readiness false before the server begins its bounded drain.".to_string());
        node.code_refs.push(PlanCodeRef::new(
            FileId::new("cmd/server/main.go").unwrap(),
            0,
            DiffSide::New,
            42,
            44,
        ));
        let target = PlanNodeTarget {
            form: 0,
            id: "a".to_string(),
        };
        let lines = interactive_plan_lines(&plan, 80, "shutdown", Some(&target), Some(&target));
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
        assert!(text.contains("Details · shutdown"), "{text}");
        assert!(
            text.find("Details · shutdown") < text.find(" 1  shutdown"),
            "pinned details stay visible above the form: {text}"
        );
        assert!(text.contains("Marks readiness false"), "{text}");
        assert!(text.contains("cmd/server/main.go[h1] new:42-44"), "{text}");
    }
}
