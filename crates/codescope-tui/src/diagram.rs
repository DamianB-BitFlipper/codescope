//! Width-aware terminal diagrams for completed, validated AI plans.
//!
//! The dispatcher deliberately publishes structure, not pre-rendered rows. This module is
//! the single layout boundary: it turns that structure into boxes and relationship connectors
//! for the pane width available during the current frame. Connectors distinguish
//! validator-verifiable relationships from hunk-derived interpretation.

use std::collections::{HashMap, HashSet};

use codescope_core::{
    FormKind, PlanEdge, PlanEdgeKind, PlanEvidence, PlanNode, VisualizationPlan, VizForm,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::action::{PlanNodeTarget, PlanRelationshipTarget};

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
            }],
        }
    }

    /// Plain text representation, primarily for golden tests.
    #[must_use]
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

/// Base warning rows for validator-sanitized plans. Rendering and retained geometry must pass
/// this exact list to [`DiagramCanvas::build_with_annotations`].
#[must_use]
pub fn leading_annotations(report: Option<&codescope_core::ValidationReport>) -> Vec<String> {
    report
        .filter(|report| {
            report.verdict == codescope_core::ValidationVerdict::ValidWithDrops
                || !report.dropped.is_empty()
        })
        .map(|report| {
            let items = if report.dropped.len() == 1 {
                "item"
            } else {
                "items"
            };
            format!(
                "⚠ sanitized AI plan · {} {items} removed",
                report.dropped.len()
            )
        })
        .into_iter()
        .collect()
}

/// A content-local cell position in the generated-plan canvas.
///
/// The generated pane translates this position to screen coordinates. It intentionally uses
/// terminal cells rather than pixels, so it is stable across terminal backends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct DiagramPosition {
    /// Horizontal cell from the generated content origin.
    pub x: u16,
    /// Vertical cell from the generated content origin.
    pub y: u16,
}

/// A rectangle in generated-plan content-local terminal cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiagramRect {
    /// Left cell.
    pub x: u16,
    /// Top cell.
    pub y: u16,
    /// Number of columns.
    pub width: u16,
    /// Number of rows.
    pub height: u16,
}

impl DiagramRect {
    /// Whether this rectangle contains a content-local cell.
    #[must_use]
    pub fn contains(self, point: DiagramPosition) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x < self.x.saturating_add(self.width)
            && point.y < self.y.saturating_add(self.height)
    }

    fn right(self) -> u16 {
        self.x.saturating_add(self.width.saturating_sub(1))
    }

    /// Inclusive bottom cell.
    #[must_use]
    pub fn bottom(self) -> u16 {
        self.y.saturating_add(self.height.saturating_sub(1))
    }
}

/// Available content cells while deriving a diagram canvas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiagramViewport {
    /// Visible content width. Cards are horizontally clamped to this width.
    pub width: u16,
    /// Visible content height. It does not clamp y: the canvas can scroll vertically.
    pub height: u16,
}

/// Identity used to prevent stale view state leaking into a different generated plan.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagramPlanScope {
    epoch: codescope_core::Epoch,
    form_kinds: Vec<FormKind>,
    nodes: Vec<PlanNodeTarget>,
    relationships: Vec<PlanRelationshipTarget>,
}

fn plan_scope(plan: &VisualizationPlan) -> DiagramPlanScope {
    let nodes = plan
        .forms
        .iter()
        .enumerate()
        .flat_map(|(form, item)| {
            item.nodes.iter().map(move |node| PlanNodeTarget {
                form,
                id: node.id.clone(),
            })
        })
        .collect();
    let relationships = plan
        .forms
        .iter()
        .enumerate()
        .flat_map(|(form, item)| {
            normalized_edges(item)
                .into_iter()
                .enumerate()
                .map(move |(edge, item)| PlanRelationshipTarget {
                    form,
                    edge,
                    from: item.from,
                    to: item.to,
                })
        })
        .collect();
    DiagramPlanScope {
        epoch: plan.epoch,
        form_kinds: plan.forms.iter().map(|form| form.kind).collect(),
        nodes,
        relationships,
    }
}

/// Persistent, current-plan-only diagram interaction state.
///
/// Positions are keyed by plan-local ids, never labels. A plan refresh calls
/// [`DiagramState::sync_plan`] to remove stale entries while retaining every valid user move.
#[derive(Debug, Clone, Default)]
pub struct DiagramState {
    positions: HashMap<PlanNodeTarget, DiagramPosition>,
    z_order: Vec<PlanNodeTarget>,
    scope: Option<DiagramPlanScope>,
    expanded_node: Option<PlanNodeTarget>,
    expanded_relationship: Option<PlanRelationshipTarget>,
    overlay_scroll: usize,
}

impl DiagramState {
    /// Retain state belonging to `plan`. Missing positions are deliberately not inserted: the
    /// canvas seeds them from its actual current viewport, so a plan arriving while the pane
    /// is hidden cannot accidentally persist a zero-width layout.
    pub fn sync_plan(&mut self, plan: &VisualizationPlan) {
        let scope = plan_scope(plan);
        if self.scope.as_ref() != Some(&scope) {
            self.positions.clear();
            self.z_order.clear();
            self.clear_expansion();
            self.scope = Some(scope);
            return;
        }
        let valid: HashSet<PlanNodeTarget> = plan
            .forms
            .iter()
            .enumerate()
            .flat_map(|(form, form_plan)| {
                form_plan.nodes.iter().map(move |node| PlanNodeTarget {
                    form,
                    id: node.id.clone(),
                })
            })
            .collect();
        self.positions.retain(|target, _| valid.contains(target));
        self.z_order.retain(|target| valid.contains(target));
        if self
            .expanded_node
            .as_ref()
            .is_some_and(|target| !valid.contains(target))
        {
            self.expanded_node = None;
        }
        if self
            .expanded_relationship
            .as_ref()
            .is_some_and(|target| !relationship_exists(plan, target))
        {
            self.expanded_relationship = None;
        }
    }

    /// Persist a free X/Y location for one box. Build-time clamping makes it visible after a
    /// resize without destroying the user's requested location for a later wider viewport.
    pub fn move_node(&mut self, target: PlanNodeTarget, position: DiagramPosition) {
        self.positions.insert(target.clone(), position);
        self.z_order.retain(|item| item != &target);
        self.z_order.push(target);
    }

    /// Toggle one box's in-place expansion. Opening raises it and closes any relationship overlay.
    pub fn toggle_node(&mut self, target: PlanNodeTarget) {
        if self.expanded_node.as_ref() == Some(&target) {
            self.expanded_node = None;
        } else {
            self.z_order.retain(|item| item != &target);
            self.z_order.push(target.clone());
            self.expanded_node = Some(target);
            self.expanded_relationship = None;
        }
    }

    /// Toggle one relationship overlay without changing boxes or base geometry.
    pub fn toggle_relationship(&mut self, target: PlanRelationshipTarget) {
        if self.expanded_relationship.as_ref() == Some(&target) {
            self.expanded_relationship = None;
            self.overlay_scroll = 0;
        } else {
            // Relationship text is an overlay only. In particular it must not collapse an
            // already expanded node, because that would change base diagram geometry.
            self.expanded_relationship = Some(target);
            self.overlay_scroll = 0;
        }
    }

    /// Current relationship-overlay line offset.
    #[must_use]
    pub fn overlay_scroll(&self) -> usize {
        self.overlay_scroll
    }
    /// Set the overlay's retained absolute page offset. Canvas clamps it for the current view.
    pub fn set_overlay_scroll(&mut self, offset: usize) {
        self.overlay_scroll = offset;
    }

    /// Close either expansion state.
    pub fn clear_expansion(&mut self) {
        self.expanded_node = None;
        self.expanded_relationship = None;
        self.overlay_scroll = 0;
    }

    /// The persistent positions, for pure canvas construction.
    #[must_use]
    pub fn positions(&self) -> &HashMap<PlanNodeTarget, DiagramPosition> {
        &self.positions
    }
    /// Nodes in back-to-front order. A dragged node is raised above older boxes.
    #[must_use]
    pub fn z_order(&self) -> &[PlanNodeTarget] {
        &self.z_order
    }

    /// The expanded node, if any.
    #[must_use]
    pub fn expanded_node(&self) -> Option<&PlanNodeTarget> {
        self.expanded_node.as_ref()
    }

    /// The expanded relationship, if any.
    #[must_use]
    pub fn expanded_relationship(&self) -> Option<&PlanRelationshipTarget> {
        self.expanded_relationship.as_ref()
    }
}

/// A box ready for drawing in an arbitrary-position diagram canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramNode {
    /// Stable plan-local identity.
    pub target: PlanNodeTarget,
    /// Box bounds in content-local cells.
    pub rect: DiagramRect,
    /// Complete boxed rows. Each row is no wider than `rect.width` display cells.
    pub lines: Vec<String>,
}

/// One directed relationship ready for drawing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramRelationship {
    /// Stable relationship identity. `edge` distinguishes parallel edges.
    pub target: PlanRelationshipTarget,
    /// Orthogonal route from source boundary to destination boundary, inclusive.
    pub path: Vec<DiagramPosition>,
    /// Compact label's hit and drawing bounds.
    pub label_rect: DiagramRect,
    /// Compact, display-width-bounded label.
    pub label: String,
    /// `true` when the relationship is validator-verifiable; inferred routes are dashed.
    pub verified: bool,
}

/// The full relationship text overlay. It is intentionally not part of [`DiagramCanvas`]
/// bounds, paths, or hit regions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramOverlay {
    /// Relationship toggled open.
    pub target: PlanRelationshipTarget,
    /// Floating overlay bounds, allowed to cover boxes.
    pub rect: DiagramRect,
    /// Current lossless page of wrapped text rows.
    pub lines: Vec<String>,
    /// Number of content rows before paging.
    pub total_lines: usize,
    /// First content row on this page.
    pub scroll: usize,
    /// Largest valid page offset for this viewport.
    pub max_scroll: usize,
}

/// Derived base geometry for one render frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagramCanvas {
    /// Scrollable base-canvas dimensions. Relationship overlays never affect these values.
    pub size: DiagramViewport,
    /// Plan intent and evidence rows above the box canvas. These are base geometry, not an overlay.
    pub annotations: Vec<String>,
    /// Boxes in stable plan order; later boxes draw on top during an overlap.
    pub nodes: Vec<DiagramNode>,
    /// Directed routes and compact label hit regions.
    pub relationships: Vec<DiagramRelationship>,
    viewport: DiagramViewport,
}

impl DiagramCanvas {
    /// Derive base geometry from a plan and persistent view state without mutating either.
    #[must_use]
    pub fn build(
        plan: &VisualizationPlan,
        viewport: DiagramViewport,
        positions: &HashMap<PlanNodeTarget, DiagramPosition>,
        expanded_node: Option<&PlanNodeTarget>,
    ) -> Self {
        Self::build_with_z_order(plan, viewport, positions, expanded_node, &[])
    }

    /// As [`DiagramCanvas::build`], with persistent back-to-front box ordering.
    #[must_use]
    pub fn build_with_z_order(
        plan: &VisualizationPlan,
        viewport: DiagramViewport,
        positions: &HashMap<PlanNodeTarget, DiagramPosition>,
        expanded_node: Option<&PlanNodeTarget>,
        z_order: &[PlanNodeTarget],
    ) -> Self {
        Self::build_with_annotations(plan, viewport, positions, expanded_node, z_order, &[])
    }

    /// Build with fixed leading annotations (for example a validator warning). Call rendering
    /// and geometry with the same list; it becomes base canvas geometry and shifts defaults.
    #[must_use]
    pub fn build_with_annotations(
        plan: &VisualizationPlan,
        viewport: DiagramViewport,
        positions: &HashMap<PlanNodeTarget, DiagramPosition>,
        expanded_node: Option<&PlanNodeTarget>,
        z_order: &[PlanNodeTarget],
        leading_annotations: &[String],
    ) -> Self {
        let mut annotations = leading_annotations.to_vec();
        annotations.extend(canvas_annotations(plan, viewport.width));
        let default_positions = automatic_positions(plan, viewport, None, annotations.len() as u16);
        let card_width = canvas_card_width(viewport);
        let mut nodes = Vec::new();
        for (form_index, form) in plan.forms.iter().enumerate() {
            for node in &form.nodes {
                let target = PlanNodeTarget {
                    form: form_index,
                    id: node.id.clone(),
                };
                let position = positions
                    .get(&target)
                    .copied()
                    .or_else(|| default_positions.get(&target).copied())
                    .unwrap_or_default();
                let lines = canvas_node_lines(node, card_width, expanded_node == Some(&target));
                let width = lines.iter().map(|line| line.width()).max().unwrap_or(1) as u16;
                let height = lines.len().max(1) as u16;
                let max_x = viewport.width.saturating_sub(width);
                nodes.push(DiagramNode {
                    target,
                    rect: DiagramRect {
                        x: position.x.min(max_x),
                        y: position.y,
                        width,
                        height,
                    },
                    lines,
                });
            }
        }

        // Stable plan order is the back layer; recently dragged nodes are raised in their
        // persisted order without changing any rectangle or route geometry.
        nodes.sort_by_key(
            |node| match z_order.iter().position(|target| target == &node.target) {
                // Defaults remain in plan order below every explicitly raised node.
                None => (false, 0),
                Some(rank) => (true, rank),
            },
        );

        let mut relationships = Vec::new();
        for (form_index, form) in plan.forms.iter().enumerate() {
            let edges = normalized_edges(form);
            for (edge_index, edge) in edges.iter().enumerate() {
                let target = PlanRelationshipTarget {
                    form: form_index,
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    edge: edge_index,
                };
                let Some(source) = nodes
                    .iter()
                    .find(|node| node.target.form == form_index && node.target.id == edge.from)
                else {
                    continue;
                };
                let Some(destination) = nodes
                    .iter()
                    .find(|node| node.target.form == form_index && node.target.id == edge.to)
                else {
                    continue;
                };
                relationships.push(canvas_relationship(
                    target,
                    source.rect,
                    destination.rect,
                    &edge.label,
                    edge.verified,
                    viewport.width,
                ));
            }
        }
        let mut max_x = viewport.width;
        let mut max_y = viewport.height.max(annotations.len() as u16);
        for node in &nodes {
            max_x = max_x.max(node.rect.right().saturating_add(1));
            max_y = max_y.max(node.rect.bottom().saturating_add(1));
        }
        for relationship in &relationships {
            max_x = max_x.max(relationship.label_rect.right().saturating_add(1));
            max_y = max_y.max(relationship.label_rect.bottom().saturating_add(1));
            for point in &relationship.path {
                max_x = max_x.max(point.x.saturating_add(1));
                max_y = max_y.max(point.y.saturating_add(1));
            }
        }
        Self {
            size: DiagramViewport {
                width: max_x,
                height: max_y,
            },
            annotations,
            nodes,
            relationships,
            viewport,
        }
    }

    /// Deterministic default placement for a current-plan node.
    #[must_use]
    pub fn default_position(
        plan: &VisualizationPlan,
        target: &PlanNodeTarget,
        viewport: DiagramViewport,
    ) -> Option<DiagramPosition> {
        automatic_positions(
            plan,
            viewport,
            None,
            canvas_annotations(plan, viewport.width).len() as u16,
        )
        .get(target)
        .copied()
    }

    /// Resolve a visible node. Nodes win over routes where they overlap.
    #[must_use]
    pub fn node_at(&self, position: DiagramPosition) -> Option<PlanNodeTarget> {
        self.nodes
            .iter()
            .rev()
            .find(|node| node.rect.contains(position))
            .map(|node| node.target.clone())
    }

    /// Resolve a relationship label or route, except where a visible box covers it.
    #[must_use]
    pub fn relationship_at(&self, position: DiagramPosition) -> Option<PlanRelationshipTarget> {
        if self.node_at(position).is_some() {
            return None;
        }
        self.relationships
            .iter()
            .rev()
            .find(|relationship| {
                relationship.label_rect.contains(position)
                    || relationship
                        .path
                        .windows(2)
                        .any(|segment| point_on_segment(position, segment[0], segment[1]))
            })
            .map(|relationship| relationship.target.clone())
    }

    /// Clamp a requested box top-left to the current visible width. Y stays free in the
    /// scrollable virtual canvas.
    #[must_use]
    pub fn clamp_position(
        &self,
        target: &PlanNodeTarget,
        requested: DiagramPosition,
    ) -> DiagramPosition {
        let Some(node) = self.nodes.iter().find(|node| &node.target == target) else {
            return requested;
        };
        DiagramPosition {
            x: requested
                .x
                .min(self.viewport.width.saturating_sub(node.rect.width)),
            y: requested.y,
        }
    }

    /// Build a viewport-aware, paged relationship overlay without changing base canvas
    /// geometry. `scroll_y` and `visible_height` are the same clamped values used by renderer
    /// and retained geometry. Labels longer than a viewport remain lossless through wheel pages.
    #[must_use]
    pub fn relationship_overlay_in_viewport(
        &self,
        plan: &VisualizationPlan,
        target: &PlanRelationshipTarget,
        scroll_y: u16,
        visible_height: u16,
        requested_scroll: usize,
    ) -> Option<DiagramOverlay> {
        // A zero-height viewport has no visible or hittable overlay page.
        if visible_height == 0 {
            return None;
        }
        let edges = normalized_edges(plan.forms.get(target.form)?);
        let edge = edges.get(target.edge)?;
        if edge.from != target.from || edge.to != target.to {
            return None;
        }
        self.relationships
            .iter()
            .find(|relationship| &relationship.target == target)?;
        let source = node_label(plan, target.form, &target.from).unwrap_or(&target.from);
        let destination = node_label(plan, target.form, &target.to).unwrap_or(&target.to);
        let text = format!("{source} → {destination}\n{}", edge.overlay_label);
        let width_budget = usize::from(self.viewport.width.max(1));
        let content = wrap_all(&text, width_budget);
        let total_lines = content.len();
        let capacity = usize::from(visible_height.max(1));
        let needs_paging = total_lines > capacity;
        // A one-row viewport must show content rather than consuming its only cell on chrome.
        let show_paging_header = needs_paging && capacity >= 2;
        let data_capacity = capacity
            .saturating_sub(usize::from(show_paging_header))
            .max(1);
        let max_scroll = total_lines.saturating_sub(data_capacity);
        let scroll = requested_scroll.min(max_scroll);
        let mut lines = Vec::new();
        if show_paging_header {
            lines.push(format!(
                "relationship details {}-{}/{} · wheel",
                scroll + 1,
                (scroll + data_capacity).min(total_lines),
                total_lines
            ));
        }
        lines.extend(content.into_iter().skip(scroll).take(data_capacity));
        let width = lines.iter().map(|line| line.width()).max().unwrap_or(1) as u16;
        let height = lines.len().max(1) as u16;
        Some(DiagramOverlay {
            target: target.clone(),
            rect: DiagramRect {
                x: 0,
                y: scroll_y,
                width: width.min(self.viewport.width.max(1)),
                height,
            },
            lines,
            total_lines,
            scroll,
            max_scroll,
        })
    }

    /// Compatibility overlay positioned at the canvas label. New render/geometry callers use
    /// [`DiagramCanvas::relationship_overlay_in_viewport`] so pages are never clipped.
    #[must_use]
    pub fn relationship_overlay(
        &self,
        plan: &VisualizationPlan,
        target: &PlanRelationshipTarget,
    ) -> Option<DiagramOverlay> {
        self.relationship_overlay_in_viewport(plan, target, 0, self.viewport.height, 0)
    }

    /// Test whether the visible overlay owns a position. Callers pass `None` when closed.
    #[must_use]
    pub fn overlay_at(
        &self,
        overlay: Option<&DiagramOverlay>,
        position: DiagramPosition,
    ) -> Option<PlanRelationshipTarget> {
        overlay
            .filter(|overlay| overlay.rect.contains(position))
            .map(|overlay| overlay.target.clone())
    }
}

#[derive(Debug, Clone)]
struct CanvasEdge {
    from: String,
    to: String,
    /// Explicit compact description. Empty means the plan did not supply one.
    label: String,
    /// Truthful detail for an absent explicit description, used only in the full overlay.
    overlay_label: String,
    verified: bool,
}

fn relationship_kind_name(kind: PlanEdgeKind) -> &'static str {
    match kind {
        PlanEdgeKind::FlowsTo => "flows to",
        PlanEdgeKind::Calls => "calls",
        PlanEdgeKind::Reads => "reads",
        PlanEdgeKind::Writes => "writes",
        PlanEdgeKind::Imports => "imports",
        PlanEdgeKind::Implements => "implements",
        PlanEdgeKind::Contains => "contains",
    }
}

fn normalized_edges(form: &VizForm) -> Vec<CanvasEdge> {
    let mut edges = form
        .edges
        .iter()
        .map(|edge| CanvasEdge {
            from: edge.from.clone(),
            to: edge.to.clone(),
            label: edge.label.clone().unwrap_or_default(),
            overlay_label: edge
                .label
                .clone()
                .unwrap_or_else(|| relationship_kind_name(edge.kind).to_string()),
            verified: form
                .nodes
                .iter()
                .find(|node| node.id == edge.from)
                .zip(form.nodes.iter().find(|node| node.id == edge.to))
                .is_some_and(|(from, to)| edge_verified(from, to, edge)),
        })
        .collect::<Vec<_>>();
    for node in &form.nodes {
        for child in &node.children {
            if !edges
                .iter()
                .any(|edge| edge.from == node.id && edge.to == *child)
            {
                edges.push(CanvasEdge {
                    from: node.id.clone(),
                    to: child.clone(),
                    label: "contains".into(),
                    overlay_label: "contains".into(),
                    verified: false,
                });
            }
        }
    }
    if form.kind == FormKind::BeforeAfter && edges.is_empty() && form.nodes.len() >= 2 {
        edges.push(CanvasEdge {
            from: form.nodes[0].id.clone(),
            to: form.nodes[1].id.clone(),
            label: "becomes".into(),
            overlay_label: "becomes".into(),
            verified: false,
        });
    }
    edges
}

/// Whether a relationship target still resolves after including deterministic synthetic edges.
#[must_use]
pub fn relationship_exists(plan: &VisualizationPlan, target: &PlanRelationshipTarget) -> bool {
    plan.forms.get(target.form).is_some_and(|form| {
        normalized_edges(form)
            .get(target.edge)
            .is_some_and(|edge| edge.from == target.from && edge.to == target.to)
    })
}

fn node_label<'a>(plan: &'a VisualizationPlan, form: usize, id: &str) -> Option<&'a str> {
    plan.forms
        .get(form)?
        .nodes
        .iter()
        .find(|node| node.id == id)
        .map(|node| node.label.trim())
}

/// Width chosen for automatic cards. At two-column widths it derives from the actual
/// viewport, so the column stride always leaves a gap rather than overlapping cards.
fn canvas_card_width(viewport: DiagramViewport) -> u16 {
    if viewport.width >= 2 * MIN_BOX_WIDTH as u16 + 4 {
        ((viewport.width - 4) / 2).clamp(4, MAX_BOX_WIDTH as u16)
    } else {
        viewport.width.clamp(1, MAX_BOX_WIDTH as u16)
    }
}

/// Base annotation rows. They share the canvas coordinate system, so rendering, scroll range,
/// and retained geometry all reserve exactly the same vertical origin before any box.
fn canvas_annotations(plan: &VisualizationPlan, width: u16) -> Vec<String> {
    let width = usize::from(width).max(1);
    let mut rows = wrap_text(plan.intent.trim(), width, 2);
    if !plan.evidence.is_empty() {
        rows.push(String::new());
        for evidence in &plan.evidence {
            let row = format!(
                "{} — {}",
                evidence_source(evidence, width >= EVIDENCE_REASON_MIN_WIDTH),
                evidence.reason.trim()
            );
            rows.push(truncate(&row, width));
        }
    }
    rows
}

/// Deterministic, non-overlapping placement for every currently unpositioned node.
fn automatic_positions(
    plan: &VisualizationPlan,
    viewport: DiagramViewport,
    expanded: Option<&PlanNodeTarget>,
    annotation_height: u16,
) -> HashMap<PlanNodeTarget, DiagramPosition> {
    let card_width = canvas_card_width(viewport);
    let columns = if viewport.width >= 2 * card_width + 4 {
        2
    } else {
        1
    };
    // In two columns put the second card at the viewport's right edge. The complete middle
    // lane belongs to its directed connector rather than being wasted after the cards.
    let stride = if columns == 2 {
        viewport.width.saturating_sub(card_width)
    } else {
        card_width.saturating_add(4)
    };
    let mut positions = HashMap::new();
    let mut form_y = annotation_height;
    for (form_index, form) in plan.forms.iter().enumerate() {
        let mut row_y = form_y;
        for row in form.nodes.chunks(columns) {
            let row_height = row
                .iter()
                .map(|node| {
                    let target = PlanNodeTarget {
                        form: form_index,
                        id: node.id.clone(),
                    };
                    canvas_node_lines(node, card_width, expanded == Some(&target)).len() as u16
                })
                .max()
                .unwrap_or(0);
            for (column, node) in row.iter().enumerate() {
                positions.insert(
                    PlanNodeTarget {
                        form: form_index,
                        id: node.id.clone(),
                    },
                    DiagramPosition {
                        x: (column as u16).saturating_mul(stride),
                        y: row_y,
                    },
                );
            }
            row_y = row_y.saturating_add(row_height.saturating_add(2));
        }
        form_y = row_y;
    }
    positions
}

fn canvas_node_lines(node: &PlanNode, viewport_width: u16, expanded: bool) -> Vec<String> {
    let width = usize::from(viewport_width).clamp(1, MAX_BOX_WIDTH);
    if width < 4 {
        return vec![truncate("□", width)];
    }
    let compact_detail = node.detail.as_deref().unwrap_or("");
    if !expanded {
        return node_box_text(&node.label, compact_detail, width);
    }
    let expanded_detail = node
        .expanded_detail
        .as_deref()
        .filter(|detail| !detail.trim().is_empty())
        .unwrap_or("");
    let inner = width - 2;
    let mut lines = vec![format!("┌{}┐", "─".repeat(inner))];
    lines.extend(
        wrap_all(node.label.trim(), inner)
            .into_iter()
            .map(|line| format!("│{}│", pad(&line, inner))),
    );
    // Keep the concise card description and then append distinct expanded text. Replacing the
    // former with the latter could hide useful context when an AI supplied both fields.
    if !compact_detail.trim().is_empty() {
        lines.extend(
            wrap_all(compact_detail.trim(), inner)
                .into_iter()
                .map(|line| format!("│{}│", pad(&line, inner))),
        );
    }
    if !expanded_detail.trim().is_empty() && expanded_detail.trim() != compact_detail.trim() {
        if !compact_detail.trim().is_empty() {
            lines.push(format!("│{}│", " ".repeat(inner)));
        }
        lines.extend(
            wrap_all(expanded_detail.trim(), inner)
                .into_iter()
                .map(|line| format!("│{}│", pad(&line, inner))),
        );
    }
    if !node.code_refs.is_empty() {
        lines.push(format!("│{}│", " ".repeat(inner)));
        lines.push(format!("│{}│", pad("Source", inner)));
        for code_ref in &node.code_refs {
            let side = match code_ref.side {
                codescope_core::DiffSide::Old => "old",
                codescope_core::DiffSide::New => "new",
            };
            let reference = format!(
                "{}:{}-{} · {side} · hunk {}",
                code_ref.file,
                code_ref.start_line,
                code_ref.end_line,
                code_ref.hunk.saturating_add(1)
            );
            lines.extend(
                wrap_all(&reference, inner)
                    .into_iter()
                    .map(|line| format!("│{}│", pad(&line, inner))),
            );
        }
    }

    // Expansion always has a visible geometric effect, even where the full text happened to
    // fit in the compact card. The padding remains inside the same anchored border.
    let minimum_expanded_height = node_box_text(&node.label, compact_detail, width).len() + 1;
    while lines.len() + 1 < minimum_expanded_height {
        lines.push(format!("│{}│", " ".repeat(inner)));
    }
    lines.push(format!("└{}┘", "─".repeat(inner)));
    lines
}

fn canvas_relationship(
    target: PlanRelationshipTarget,
    source: DiagramRect,
    destination: DiagramRect,
    label: &str,
    verified: bool,
    viewport_width: u16,
) -> DiagramRelationship {
    let source_center = DiagramPosition {
        x: source.x.saturating_add(source.width / 2),
        y: source.y.saturating_add(source.height / 2),
    };
    let destination_center = DiagramPosition {
        x: destination.x.saturating_add(destination.width / 2),
        y: destination.y.saturating_add(destination.height / 2),
    };
    let vertical =
        source_center.x == destination_center.x && source_center.y != destination_center.y;
    let (path, label_rect) = if vertical {
        let down = source_center.y < destination_center.y;
        let start = DiagramPosition {
            x: source_center.x,
            y: if down {
                source.bottom().saturating_add(1)
            } else {
                source.y.saturating_sub(1)
            },
        };
        let end = DiagramPosition {
            x: destination_center.x,
            y: if down {
                destination.y.saturating_sub(1)
            } else {
                destination.bottom().saturating_add(1)
            },
        };
        let label_y = start
            .y
            .min(end.y)
            .saturating_add((start.y.max(end.y).saturating_sub(start.y.min(end.y))) / 2);
        (
            vec![start, end],
            DiagramRect {
                x: start.x,
                y: label_y,
                width: 1,
                height: 1,
            },
        )
    } else {
        let goes_right = source_center.x <= destination_center.x;
        // End one cell outside each border. Nodes draw after paths, so a border endpoint
        // would hide the directed head. Deliberate user overlap is resolved by node z-order.
        let start = DiagramPosition {
            x: if goes_right {
                source.right().saturating_add(1)
            } else {
                source.x.saturating_sub(1)
            },
            y: source_center.y,
        };
        let end = DiagramPosition {
            x: if goes_right {
                destination.x.saturating_sub(1)
            } else {
                destination.right().saturating_add(1)
            },
            y: destination_center.y,
        };
        let middle_x = if goes_right {
            start.x.saturating_add((end.x.saturating_sub(start.x)) / 2)
        } else {
            end.x.saturating_add((start.x.saturating_sub(end.x)) / 2)
        };
        let path = if start.y == end.y {
            vec![start, end]
        } else {
            vec![
                start,
                DiagramPosition {
                    x: middle_x,
                    y: start.y,
                },
                DiagramPosition {
                    x: middle_x,
                    y: end.y,
                },
                end,
            ]
        };
        // A normal label is restricted to the actual free route lane. It never sits over a
        // node, and leaves both endpoint cells free for arrowheads.
        let lane_left = start.x.min(end.x).saturating_add(1);
        let lane_right = start.x.max(end.x).saturating_sub(1);
        let lane_width = usize::from(lane_right.saturating_sub(lane_left).saturating_add(1));
        // Keep two route glyphs on either side of compact text; endpoint cells are reserved
        // separately for directed heads. Wide panes devote their spare width to this lane.
        let budget = lane_width
            .saturating_sub(4)
            .max(1)
            .min(usize::from(viewport_width).max(1));
        let compact = truncate(label.trim(), budget.max(1));
        let width = compact.width() as u16;
        let x = lane_left.saturating_add(
            lane_right
                .saturating_sub(lane_left)
                .saturating_sub(width.saturating_sub(1))
                / 2,
        );
        return DiagramRelationship {
            target,
            path,
            label_rect: DiagramRect {
                x,
                y: start.y,
                width: width.max(1),
                height: 1,
            },
            label: compact,
            verified,
        };
    };
    // A vertical route has only one cell of horizontal lane; show the compact relation glyph
    // there while retaining the full description in the click-to-expand overlay.
    DiagramRelationship {
        target,
        path,
        label_rect,
        label: truncate(label.trim(), 1),
        verified,
    }
}

fn point_on_segment(point: DiagramPosition, a: DiagramPosition, b: DiagramPosition) -> bool {
    if a.x == b.x {
        point.x == a.x && point.y >= a.y.min(b.y) && point.y <= a.y.max(b.y)
    } else if a.y == b.y {
        point.y == a.y && point.x >= a.x.min(b.x) && point.x <= a.x.max(b.x)
    } else {
        false
    }
}

fn wrap_all(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.trim().is_empty() {
        return vec![String::new()];
    }
    text.lines()
        .flat_map(|line| {
            let mut lines = Vec::new();
            let mut current = String::new();
            for word in line.split_whitespace() {
                let separator = usize::from(!current.is_empty());
                if current.width() + separator + word.width() <= width {
                    if !current.is_empty() {
                        current.push(' ');
                    }
                    current.push_str(word);
                    continue;
                }
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }
                // Do not ellipsize expanded content: split identifiers and URLs at grapheme
                // boundaries so every source character is still present in the box/overlay.
                let chunks = cell_chunks(word, width);
                let last = chunks.len().saturating_sub(1);
                for (index, chunk) in chunks.into_iter().enumerate() {
                    if index == last {
                        current = chunk;
                    } else {
                        lines.push(chunk);
                    }
                }
            }
            if !current.is_empty() {
                lines.push(current);
            }
            if lines.is_empty() {
                lines.push(String::new());
            }
            lines
        })
        .collect()
}

/// Split a non-whitespace token at Unicode grapheme boundaries without dropping text.
fn cell_chunks(text: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let grapheme_width = grapheme.width();
        if grapheme_width > width && current.is_empty() {
            // A width-one terminal cell cannot physically hold a double-width grapheme.
            // Preserve the width contract in this pathological viewport with an ellipsis;
            // normal expanded cards have a two-cell inner width and retain the grapheme.
            chunks.push(truncate(grapheme, width));
            continue;
        }
        if used > 0 && used + grapheme_width > width {
            chunks.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push_str(grapheme);
        used += grapheme_width;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

const MIN_BOX_WIDTH: usize = 18;

fn edge_verified(from: &PlanNode, to: &PlanNode, edge: &PlanEdge) -> bool {
    // `flows_to` is renderer-native sequence grammar, so it has no fact-store proof.
    edge.kind != PlanEdgeKind::FlowsTo
        && from.entity.is_some()
        && to.entity.is_some()
        && matches!(
            edge.kind,
            PlanEdgeKind::Calls
                | PlanEdgeKind::Imports
                | PlanEdgeKind::Implements
                | PlanEdgeKind::Contains
        )
}

fn node_box_text(label: &str, detail: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let inner = width - 2;
    let mut rows = vec![format!("┌{}┐", "─".repeat(inner))];
    rows.push(format!("│{}│", pad(&truncate(label.trim(), inner), inner)));
    let mut detail_rows = wrap_text(detail.trim(), inner, 2);
    detail_rows.resize(2, String::new());
    rows.extend(
        detail_rows
            .into_iter()
            .map(|line| format!("│{}│", pad(&line, inner))),
    );
    rows.push(format!("└{}┘", "─".repeat(inner)));
    rows
}

const MAX_BOX_WIDTH: usize = 32;
const EVIDENCE_REASON_MIN_WIDTH: usize = 60;

fn evidence_source(evidence: &PlanEvidence, include_hunk: bool) -> String {
    let mut source = evidence
        .file
        .to_string()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    if let Some(range) = evidence.range {
        source.push_str(&format!(":{}", range.start_line.saturating_add(1)));
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

fn wrap_text(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    if text.trim().is_empty() || max_lines == 0 {
        return Vec::new();
    }
    let all = wrap_all(text, width.max(1));
    all.into_iter().take(max_lines).collect()
}

fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let cells = grapheme.width();
        if used + cells + 1 > width {
            break;
        }
        out.push_str(grapheme);
        used += cells;
    }
    out.push('…');
    out
}

fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(text.width())))
}

#[cfg(test)]
mod canvas_tests {
    use super::*;
    use codescope_core::{DiffSide, EntityRef, Epoch, FileId, PlanCodeRef, PlanNodeChange};

    fn live_shape() -> VisualizationPlan {
        let mut plan = VisualizationPlan::new(Epoch(7));
        let mut root = PlanNode::new("root", "diagram state", PlanNodeChange::Modified)
            .with_detail("compact state")
            .with_expanded_detail("expanded detail ".repeat(20));
        root.code_refs.push(PlanCodeRef::new(
            FileId::new("src/diagram.rs").unwrap(),
            2,
            DiffSide::New,
            10,
            24,
        ));
        root.children = vec![
            "state".into(),
            "canvas".into(),
            "routes".into(),
            "overlay".into(),
        ];
        plan.forms.push(VizForm {
            kind: FormKind::ChangedSymbolTree,
            nodes: vec![
                root,
                PlanNode::new("state", "state", PlanNodeChange::Modified),
                PlanNode::new("canvas", "canvas", PlanNodeChange::Modified),
                PlanNode::new("routes", "routes", PlanNodeChange::Modified),
                PlanNode::new("overlay", "overlay", PlanNodeChange::Modified),
            ],
            edges: vec![],
        });
        plan
    }
    fn built(plan: &VisualizationPlan, state: &DiagramState, w: u16) -> DiagramCanvas {
        DiagramCanvas::build_with_z_order(
            plan,
            DiagramViewport {
                width: w,
                height: 8,
            },
            state.positions(),
            state.expanded_node(),
            state.z_order(),
        )
    }
    #[test]
    fn live_tree_canvas_has_nonoverlap_routes_expansion_and_source() {
        let plan = live_shape();
        let state = DiagramState::default();
        let base = built(&plan, &state, 96);
        assert_eq!(base.nodes.len(), 5);
        assert_eq!(base.relationships.len(), 4);
        for (i, a) in base.nodes.iter().enumerate() {
            for b in &base.nodes[i + 1..] {
                assert!(
                    a.rect.right() < b.rect.x
                        || b.rect.right() < a.rect.x
                        || a.rect.bottom() < b.rect.y
                        || b.rect.bottom() < a.rect.y
                );
            }
        }
        let root = PlanNodeTarget {
            form: 0,
            id: "root".into(),
        };
        let other = base
            .nodes
            .iter()
            .find(|n| n.target.id == "state")
            .unwrap()
            .rect;
        let mut expanded = state.clone();
        expanded.toggle_node(root.clone());
        let e = built(&plan, &expanded, 96);
        let rn = e.nodes.iter().find(|n| n.target == root).unwrap();
        let collapsed_root = base.nodes.iter().find(|n| n.target == root).unwrap();
        assert_eq!(
            (rn.rect.x, rn.rect.y),
            (collapsed_root.rect.x, collapsed_root.rect.y),
            "inline expansion preserves the box anchor",
        );
        assert!(rn.rect.height > collapsed_root.rect.height);
        assert_eq!(
            e.nodes.last().unwrap().target,
            root,
            "expanded box is raised above overlaps"
        );
        assert_eq!(
            e.nodes
                .iter()
                .find(|n| n.target.id == "state")
                .unwrap()
                .rect,
            other
        );
        let source = rn.lines.join("\n");
        assert!(
            source.contains("Source")
                && source.contains("src/diagram.rs:10-24")
                && source.contains("new")
                && source.contains("hunk 3")
                && source.contains("expanded detail")
        );
        let edge = e.relationships[0].target.clone();
        let before = e.clone();
        expanded.toggle_relationship(edge.clone());
        assert_eq!(before, built(&plan, &expanded, 96));
        let page = e
            .relationship_overlay_in_viewport(&plan, &edge, 0, 1, 0)
            .unwrap();
        assert_eq!(page.rect.height, 1);
        assert!(page.total_lines >= 1);
        assert!(e
            .relationship_overlay_in_viewport(&plan, &edge, 0, 0, 0)
            .is_none());
    }
    #[test]
    fn relationship_kind_names_are_truthful_and_exhaustive() {
        for (kind, expected) in [
            (PlanEdgeKind::FlowsTo, "flows to"),
            (PlanEdgeKind::Calls, "calls"),
            (PlanEdgeKind::Reads, "reads"),
            (PlanEdgeKind::Writes, "writes"),
            (PlanEdgeKind::Imports, "imports"),
            (PlanEdgeKind::Implements, "implements"),
            (PlanEdgeKind::Contains, "contains"),
        ] {
            assert_eq!(relationship_kind_name(kind), expected);
        }
    }

    #[test]
    fn flows_to_renders_as_an_unverified_transition_with_a_fallback_overlay() {
        let mut plan = VisualizationPlan::new(Epoch(7));
        plan.forms.push(VizForm {
            kind: FormKind::Sequence,
            nodes: vec![
                PlanNode::new("request", "request", PlanNodeChange::Modified).with_entity(
                    EntityRef::for_file(FileId::new("src/request.rs").expect("valid file id")),
                ),
                PlanNode::new("handler", "handler", PlanNodeChange::Modified).with_entity(
                    EntityRef::for_file(FileId::new("src/handler.rs").expect("valid file id")),
                ),
            ],
            edges: vec![PlanEdge {
                from: "request".into(),
                to: "handler".into(),
                kind: PlanEdgeKind::FlowsTo,
                label: None,
            }],
        });

        let canvas = built(&plan, &DiagramState::default(), 40);
        let relationship = canvas
            .relationships
            .first()
            .expect("flow transition renders");
        assert!(
            relationship.label.is_empty(),
            "no compact label was supplied"
        );
        assert!(
            !relationship.verified,
            "renderer-native transitions are inferred and therefore dashed"
        );
        let overlay = canvas
            .relationship_overlay(&plan, &relationship.target)
            .expect("transition has a fallback overlay");
        assert!(overlay.lines.join("\n").contains("flows to"));
    }

    #[test]
    fn move_parallel_optional_unicode_and_z_contracts() {
        let mut plan = live_shape();
        plan.forms[0].nodes[0].children.clear();
        plan.forms[0].edges = vec![
            PlanEdge {
                from: "state".into(),
                to: "canvas".into(),
                kind: PlanEdgeKind::Writes,
                label: None,
            },
            PlanEdge {
                from: "state".into(),
                to: "canvas".into(),
                kind: PlanEdgeKind::Writes,
                label: Some("é🙂 relationship".into()),
            },
        ];
        let mut state = DiagramState::default();
        let base = built(&plan, &state, 40);
        assert_eq!(base.relationships.len(), 2);
        assert_ne!(base.relationships[0].target, base.relationships[1].target);
        assert!(base.relationships[0].label.is_empty());
        let truthful = base
            .relationship_overlay_in_viewport(&plan, &base.relationships[0].target, 0, 8, 0)
            .unwrap();
        let text = truthful.lines.join("\n");
        assert!(text.contains("writes") && !text.contains("affects"));
        let a = PlanNodeTarget {
            form: 0,
            id: "state".into(),
        };
        let b = PlanNodeTarget {
            form: 0,
            id: "canvas".into(),
        };
        let mut resize_state = DiagramState::default();
        resize_state.move_node(a.clone(), DiagramPosition { x: 80, y: 2 });
        let narrow_x = built(&plan, &resize_state, 40)
            .nodes
            .iter()
            .find(|node| node.target == a)
            .unwrap()
            .rect
            .x;
        let wide_x = built(&plan, &resize_state, 120)
            .nodes
            .iter()
            .find(|node| node.target == a)
            .unwrap()
            .rect
            .x;
        assert!(narrow_x < 80, "narrow rendering clamps the derived X");
        assert_eq!(wide_x, 80, "widening restores the persisted requested X");
        assert_eq!(resize_state.positions().get(&a).unwrap().x, 80);

        let fixed = base.nodes.iter().find(|n| n.target == b).unwrap().rect;
        let route = base.relationships[0].path.clone();
        state.move_node(a.clone(), DiagramPosition { x: 4, y: 12 });
        let moved = built(&plan, &state, 40);
        assert_ne!(moved.relationships[0].path, route);
        assert_eq!(
            moved.nodes.iter().find(|n| n.target == b).unwrap().rect,
            fixed
        );
        state.move_node(b.clone(), DiagramPosition { x: 4, y: 12 });
        let overlap = built(&plan, &state, 40);
        assert_eq!(overlap.node_at(DiagramPosition { x: 4, y: 12 }), Some(b));
        assert!(built(&plan, &state, 1)
            .nodes
            .iter()
            .flat_map(|n| &n.lines)
            .all(|line| line.width() <= 1));
        assert_eq!(
            built(&plan, &state, 40)
                .nodes
                .iter()
                .find(|n| n.target == a)
                .unwrap()
                .rect
                .x,
            4
        );
        let mut scoped = DiagramState::default();
        scoped.sync_plan(&plan);
        scoped.move_node(a.clone(), DiagramPosition { x: 4, y: 4 });
        scoped.toggle_node(a.clone());
        scoped.sync_plan(&plan);
        assert!(scoped.positions().contains_key(&a));
        let mut changed = plan.clone();
        changed.epoch = Epoch(8);
        scoped.sync_plan(&changed);
        assert!(
            scoped.positions().is_empty()
                && scoped.z_order().is_empty()
                && scoped.expanded_node().is_none()
        );
    }
    #[test]
    fn expansion_keeps_nonincident_route_and_paged_overlay_content() {
        let mut plan = live_shape();
        plan.forms[0].edges.push(PlanEdge {
            from: "state".into(),
            to: "canvas".into(),
            kind: PlanEdgeKind::Writes,
            label: Some("unrelated".into()),
        });
        let state = DiagramState::default();
        let before = built(&plan, &state, 96);
        let unrelated = before.relationships[0].path.clone();
        let mut state = state;
        state.toggle_node(PlanNodeTarget {
            form: 0,
            id: "root".into(),
        });
        assert_eq!(built(&plan, &state, 96).relationships[0].path, unrelated);
        plan.forms[0].edges[0].label = Some("very long relationship ".repeat(80));
        let c = built(&plan, &DiagramState::default(), 40);
        let target = c.relationships[0].target.clone();
        let p0 = c
            .relationship_overlay_in_viewport(&plan, &target, 0, 1, 0)
            .unwrap();
        let p1 = c
            .relationship_overlay_in_viewport(&plan, &target, 0, 1, 1)
            .unwrap();
        assert_eq!(p0.rect.height, 1);
        assert_ne!(p0.lines, p1.lines);
        assert!(!p0.lines[0].starts_with("relationship details"));
        let max = c
            .relationship_overlay_in_viewport(&plan, &target, 0, 3, usize::MAX)
            .unwrap();
        assert_eq!(max.scroll, max.max_scroll);
    }
    #[test]
    fn width_sweep_and_unicode_wrap_are_lossless() {
        let mut plan = live_shape();
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            nodes: vec![PlanNode::new(
                "second-form",
                "second form",
                PlanNodeChange::Unchanged,
            )],
            edges: vec![],
        });
        let state = DiagramState::default();
        for width in [18, 40, 96] {
            let c = built(&plan, &state, width);
            for (i, a) in c.nodes.iter().enumerate() {
                for b in &c.nodes[i + 1..] {
                    assert!(
                        a.rect.right() < b.rect.x
                            || b.rect.right() < a.rect.x
                            || a.rect.bottom() < b.rect.y
                            || b.rect.bottom() < a.rect.y
                    )
                }
            }
        }
        let token = "naïve🙂identifier".repeat(20);
        assert_eq!(wrap_all(&token, 12).concat(), token);
    }
}
