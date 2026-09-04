//! Width-aware terminal diagrams for completed, validated AI plans.
//!
//! The dispatcher deliberately publishes structure, not pre-rendered rows. This module is
//! the single layout boundary: it turns that structure into boxes and relationship connectors
//! for the pane width available during the current frame. Connectors distinguish
//! validator-verifiable relationships from hunk-derived interpretation.

use std::collections::{HashMap, HashSet, VecDeque};

use codescope_core::{FormKind, PlanEdge, PlanEdgeKind, PlanNode, VisualizationPlan, VizForm};
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
}

fn plan_scope(plan: &VisualizationPlan) -> DiagramPlanScope {
    DiagramPlanScope { epoch: plan.epoch }
}

/// Persistent, current-plan-only diagram interaction state.
///
/// Positions are keyed by plan-local ids, never labels. A plan refresh calls
/// [`DiagramState::sync_plan`] to remove stale entries while retaining every valid user move
/// and expansion. Incremental AI edits change the plan's shape repeatedly, so membership is
/// pruned per target instead of treating every added node or edge as a new interaction scope.
#[derive(Debug, Clone, Default)]
pub struct DiagramState {
    positions: HashMap<PlanNodeTarget, DiagramPosition>,
    z_order: Vec<PlanNodeTarget>,
    scope: Option<DiagramPlanScope>,
    expanded_nodes: HashSet<PlanNodeTarget>,
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
        self.expanded_nodes.retain(|target| valid.contains(target));
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

    /// Toggle one box's independently retained in-place expansion. Opening raises it and closes
    /// any relationship overlay, but never changes another box's expansion state.
    pub fn toggle_node(&mut self, target: PlanNodeTarget) {
        let expanded = !self.expanded_nodes.contains(&target);
        self.set_node_expanded(target, expanded);
    }

    /// Set one box's expansion without changing any other box.
    pub fn set_node_expanded(&mut self, target: PlanNodeTarget, expanded: bool) {
        if expanded {
            self.z_order.retain(|item| item != &target);
            self.z_order.push(target.clone());
            self.expanded_nodes.insert(target);
            self.expanded_relationship = None;
        } else {
            self.expanded_nodes.remove(&target);
        }
    }

    /// Toggle one relationship overlay without changing boxes or base geometry.
    pub fn toggle_relationship(&mut self, target: PlanRelationshipTarget) {
        if self.expanded_relationship.as_ref() == Some(&target) {
            self.expanded_relationship = None;
            self.overlay_scroll = 0;
        } else {
            // Relationship text is an overlay only. In particular it must not collapse an
            // already expanded nodes, because that would change base diagram geometry.
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

    /// Close every node expansion and the relationship overlay.
    pub fn clear_expansion(&mut self) {
        self.expanded_nodes.clear();
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

    /// Independently expanded nodes in the current plan.
    #[must_use]
    pub fn expanded_nodes(&self) -> &HashSet<PlanNodeTarget> {
        &self.expanded_nodes
    }

    /// Whether one node is independently expanded.
    #[must_use]
    pub fn is_node_expanded(&self, target: &PlanNodeTarget) -> bool {
        self.expanded_nodes.contains(target)
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
    /// Whether the compact rendering omits relationship text that can be expanded.
    pub has_hidden_label: bool,
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
    /// Plan intent rows above the box canvas. These are base geometry, not an overlay.
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
        expanded_nodes: &HashSet<PlanNodeTarget>,
    ) -> Self {
        Self::build_with_z_order(plan, viewport, positions, expanded_nodes, &[])
    }

    /// As [`DiagramCanvas::build`], with persistent back-to-front box ordering.
    #[must_use]
    pub fn build_with_z_order(
        plan: &VisualizationPlan,
        viewport: DiagramViewport,
        positions: &HashMap<PlanNodeTarget, DiagramPosition>,
        expanded_nodes: &HashSet<PlanNodeTarget>,
        z_order: &[PlanNodeTarget],
    ) -> Self {
        Self::build_with_annotations(plan, viewport, positions, expanded_nodes, z_order, &[])
    }

    /// Build with fixed leading annotations (for example a validator warning). Call rendering
    /// and geometry with the same list; it becomes base canvas geometry and shifts defaults.
    #[must_use]
    pub fn build_with_annotations(
        plan: &VisualizationPlan,
        viewport: DiagramViewport,
        positions: &HashMap<PlanNodeTarget, DiagramPosition>,
        expanded_nodes: &HashSet<PlanNodeTarget>,
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
                let lines = canvas_node_lines(node, card_width, expanded_nodes.contains(&target));
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
            .filter(|relationship| relationship.has_hidden_label)
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
        let relationship = self
            .relationships
            .iter()
            .find(|relationship| &relationship.target == target)?;
        if !relationship.has_hidden_label {
            return None;
        }

        // Expand the compact description itself. The endpoints are already unambiguous from
        // the connected cards, and prefixing them made the overlay look like unrelated text.
        let text = &edge.overlay_label;
        let anchor = relationship.label_rect;
        let visible_bottom = scroll_y.saturating_add(visible_height);
        if anchor.y < scroll_y || anchor.y >= visible_bottom || anchor.x >= self.viewport.width {
            return None;
        }
        let width_budget = usize::from(self.viewport.width.saturating_sub(anchor.x).max(1));
        let content = wrap_all(text, width_budget);
        let total_lines = content.len();
        let capacity = usize::from(visible_bottom.saturating_sub(anchor.y).max(1));
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
                x: anchor.x,
                y: anchor.y,
                width: width.min(self.viewport.width.saturating_sub(anchor.x).max(1)),
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
    // Evidence remains part of the validated plan for grounding and revision continuity, but it
    // is deliberately not projected into reviewer-facing canvas rows. Nodes and relationships
    // already carry the useful explanation; repeating model-written citation prose adds noise.
    wrap_text(plan.intent.trim(), width, 2)
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
        let directed_layers = directed_form_layers(form);
        let layout_rows = directed_layers.as_ref().map_or_else(
            || {
                (0..form.nodes.len())
                    .collect::<Vec<_>>()
                    .chunks(columns)
                    .map(<[usize]>::to_vec)
                    .collect::<Vec<_>>()
            },
            |layers| {
                layers
                    .iter()
                    .flat_map(|layer| layer.chunks(columns).map(<[usize]>::to_vec))
                    .collect::<Vec<_>>()
            },
        );
        let mut row_y = form_y;
        for row in layout_rows {
            let row_height = row
                .iter()
                .filter_map(|index| form.nodes.get(*index))
                .map(|node| {
                    let target = PlanNodeTarget {
                        form: form_index,
                        id: node.id.clone(),
                    };
                    canvas_node_lines(node, card_width, expanded == Some(&target)).len() as u16
                })
                .max()
                .unwrap_or(0);
            let centered_single = directed_layers.is_some() && columns == 2 && row.len() == 1;
            for (column, index) in row.iter().enumerate() {
                let Some(node) = form.nodes.get(*index) else {
                    continue;
                };
                positions.insert(
                    PlanNodeTarget {
                        form: form_index,
                        id: node.id.clone(),
                    },
                    DiagramPosition {
                        x: if centered_single {
                            viewport.width.saturating_sub(card_width) / 2
                        } else {
                            (column as u16).saturating_mul(stride)
                        },
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

/// Topological layers for forms whose edges describe directional flow. Keeping every edge aimed
/// from an earlier row to a later row avoids the ambiguous left-right / right-left zigzag produced
/// by a document-order grid. Cycles fall back to that neutral grid because they have no truthful
/// top-to-bottom ordering.
fn directed_form_layers(form: &VizForm) -> Option<Vec<Vec<usize>>> {
    if !matches!(form.kind, FormKind::Sequence | FormKind::RelationshipFlow) || form.nodes.len() < 3
    {
        return None;
    }
    let node_indices = form
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut outgoing = vec![Vec::<usize>::new(); form.nodes.len()];
    let mut indegree = vec![0_usize; form.nodes.len()];
    let mut distinct_edges = HashSet::new();
    for edge in normalized_edges(form) {
        let (Some(&from), Some(&to)) = (
            node_indices.get(edge.from.as_str()),
            node_indices.get(edge.to.as_str()),
        ) else {
            continue;
        };
        if from == to || !distinct_edges.insert((from, to)) {
            continue;
        }
        outgoing[from].push(to);
        indegree[to] = indegree[to].saturating_add(1);
    }
    if distinct_edges.is_empty() {
        return None;
    }

    let mut queue = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, degree)| (*degree == 0).then_some(index))
        .collect::<VecDeque<_>>();
    let mut ranks = vec![0_usize; form.nodes.len()];
    let mut visited = 0_usize;
    while let Some(from) = queue.pop_front() {
        visited = visited.saturating_add(1);
        for &to in &outgoing[from] {
            ranks[to] = ranks[to].max(ranks[from].saturating_add(1));
            indegree[to] = indegree[to].saturating_sub(1);
            if indegree[to] == 0 {
                queue.push_back(to);
            }
        }
    }
    if visited != form.nodes.len() {
        return None;
    }

    let mut layers = vec![Vec::new(); ranks.iter().copied().max().unwrap_or(0) + 1];
    for (node, rank) in ranks.into_iter().enumerate() {
        layers[rank].push(node);
    }
    Some(layers)
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
    let source_above = source.bottom() < destination.y;
    let destination_above = destination.bottom() < source.y;
    let horizontal_overlap_left = source.x.max(destination.x);
    let horizontal_overlap_right = source.right().min(destination.right());
    let has_horizontal_overlap = horizontal_overlap_left <= horizontal_overlap_right;

    // Cards that are visibly stacked should have a vertical connector even if one card was
    // dragged a few columns sideways. Center equality classified that common case as a
    // horizontal route, leaving its label underneath the source card.
    if has_horizontal_overlap && (source_above || destination_above) {
        let down = source_above;
        let centers_midpoint = source_center
            .x
            .min(destination_center.x)
            .saturating_add(source_center.x.abs_diff(destination_center.x) / 2);
        let route_x = centers_midpoint.clamp(horizontal_overlap_left, horizontal_overlap_right);
        let start = DiagramPosition {
            x: route_x,
            y: if down {
                source.bottom().saturating_add(1)
            } else {
                source.y.saturating_sub(1)
            },
        };
        let end = DiagramPosition {
            x: route_x,
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
        let (compact, x, width) =
            vertical_relationship_label(label.trim(), route_x, viewport_width);
        return DiagramRelationship {
            target,
            path: vec![start, end],
            label_rect: DiagramRect {
                x,
                y: label_y,
                width,
                height: 1,
            },
            has_hidden_label: label.trim().is_empty() || compact != label.trim(),
            label: compact,
            verified,
        };
    }

    let source_left = source.right() < destination.x;
    let destination_left = destination.right() < source.x;
    let vertical_overlap_top = source.y.max(destination.y);
    let vertical_overlap_bottom = source.bottom().min(destination.bottom());
    let has_vertical_overlap = vertical_overlap_top <= vertical_overlap_bottom;
    let goes_right = if source_left {
        true
    } else if destination_left {
        false
    } else {
        source_center.x <= destination_center.x
    };
    let route_y = if has_vertical_overlap && (source_left || destination_left) {
        vertical_overlap_top
            .saturating_add(vertical_overlap_bottom.saturating_sub(vertical_overlap_top) / 2)
    } else {
        source_center.y
    };
    // End one cell outside each border. Nodes draw after paths, so a border endpoint would
    // hide the directed head. Deliberate user overlap is resolved by node z-order.
    let start = DiagramPosition {
        x: if goes_right {
            source.right().saturating_add(1)
        } else {
            source.x.saturating_sub(1)
        },
        y: route_y,
    };
    let end = DiagramPosition {
        x: if goes_right {
            destination.x.saturating_sub(1)
        } else {
            destination.right().saturating_add(1)
        },
        y: if has_vertical_overlap && (source_left || destination_left) {
            route_y
        } else {
            destination_center.y
        },
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
    // A horizontal label stays inside the actual free lane and leaves route glyphs on both
    // sides. The vertically stacked case above deliberately uses the whole surrounding row.
    let lane_left = start.x.min(end.x).saturating_add(1);
    let lane_right = start.x.max(end.x).saturating_sub(1);
    let lane_width = usize::from(lane_right.saturating_sub(lane_left).saturating_add(1));
    let budget = lane_width
        .saturating_sub(4)
        .max(1)
        .min(usize::from(viewport_width).max(1));
    let compact_label = truncate(label.trim(), budget);
    let width = compact_label.width() as u16;
    let x = lane_left.saturating_add(
        lane_right
            .saturating_sub(lane_left)
            .saturating_sub(width.saturating_sub(1))
            / 2,
    );
    DiagramRelationship {
        target,
        path,
        label_rect: DiagramRect {
            x,
            y: start.y,
            width: width.max(1),
            height: 1,
        },
        has_hidden_label: label.trim().is_empty() || compact_label != label.trim(),
        label: compact_label,
        verified,
    }
}

/// Place a vertical relationship label in the surrounding row, not inside the route's single
/// column. Prefer the right or left side with a one-cell gap. When neither half is wide enough
/// but the complete viewport is, center the text across the route; the line then reads as entering
/// and leaving the label instead of collapsing useful text to a single ellipsis.
fn vertical_relationship_label(
    label: &str,
    route_x: u16,
    viewport_width: u16,
) -> (String, u16, u16) {
    if label.is_empty() || viewport_width == 0 {
        return (String::new(), route_x, 1);
    }

    let compact = truncate(label, usize::from(viewport_width));
    let width = u16::try_from(compact.width())
        .unwrap_or(viewport_width)
        .min(viewport_width)
        .max(1);
    let right_x = route_x.saturating_add(2);
    let right_room = viewport_width.saturating_sub(right_x);
    if width <= right_room {
        return (compact, right_x, width);
    }

    let left_room = route_x.saturating_sub(1);
    if width <= left_room {
        return (compact, left_room.saturating_sub(width), width);
    }

    (compact, viewport_width.saturating_sub(width) / 2, width)
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
    let label_rows = wrap_all(label.trim(), inner);
    rows.extend(
        label_rows
            .into_iter()
            .map(|line| format!("│{}│", pad(&line, inner))),
    );
    let mut detail_rows = if detail.trim().is_empty() {
        Vec::new()
    } else {
        wrap_all(detail.trim(), inner)
    };
    // Preserve the familiar minimum card height while allowing both fields to grow losslessly.
    let body_rows = rows.len().saturating_sub(1) + detail_rows.len();
    detail_rows.resize(
        detail_rows.len() + 3_usize.saturating_sub(body_rows),
        String::new(),
    );
    rows.extend(
        detail_rows
            .into_iter()
            .map(|line| format!("│{}│", pad(&line, inner))),
    );
    rows.push(format!("└{}┘", "─".repeat(inner)));
    rows
}

const MAX_BOX_WIDTH: usize = 32;
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
    use codescope_core::{
        DiffSide, EntityRef, Epoch, FileId, LineRange, PlanCodeRef, PlanEvidence, PlanNodeChange,
    };

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
            state.expanded_nodes(),
            state.z_order(),
        )
    }

    #[test]
    fn canvas_hides_evidence_descriptions_but_preserves_plan_grounding() {
        let mut plan = VisualizationPlan::new(Epoch(7));
        plan.intent = "Explain the reporter cleanup flow.".to_string();
        let evidence = PlanEvidence {
            file: FileId::new("internal/reporter.go").unwrap(),
            hunk: Some(0),
            symbol: Some("reportDeath".to_string()),
            range: Some(LineRange::new(41, 2, 44, 8)),
            reason: "unexpected deaths close the route before acknowledgement".to_string(),
        };
        plan.evidence.push(evidence.clone());

        let canvas = built(&plan, &DiagramState::default(), 96);

        assert_eq!(
            canvas.annotations,
            vec!["Explain the reporter cleanup flow."],
            "only the diagram title is projected above the canvas"
        );
        assert_eq!(
            plan.evidence,
            vec![evidence],
            "rendering must not remove grounding from the plan"
        );
    }

    #[test]
    fn live_tree_canvas_has_nonoverlap_routes_and_expansion_without_source_footer() {
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
        let expanded_text = rn.lines.join("\n");
        assert!(expanded_text.contains("expanded detail"));
        assert!(!expanded_text.contains("Source"));
        assert!(!expanded_text.contains("src/diagram.rs:10-24"));

        let state_target = PlanNodeTarget {
            form: 0,
            id: "state".into(),
        };
        expanded.toggle_node(state_target.clone());
        assert!(expanded.is_node_expanded(&root));
        assert!(expanded.is_node_expanded(&state_target));
        let both_open = built(&plan, &expanded, 96);
        assert_eq!(
            both_open
                .nodes
                .iter()
                .find(|node| node.target == root)
                .unwrap()
                .lines,
            rn.lines,
            "opening another box retains the first box's complete expansion"
        );
        expanded.toggle_node(state_target.clone());
        assert!(expanded.is_node_expanded(&root));
        assert!(!expanded.is_node_expanded(&state_target));

        let edge = e.relationships[0].target.clone();
        let before = built(&plan, &expanded, 96);
        expanded.toggle_relationship(edge.clone());
        assert_eq!(before, built(&plan, &expanded, 96));
        assert!(
            e.relationship_overlay_in_viewport(&plan, &edge, 0, 8, 0)
                .is_none(),
            "a fully visible relationship has nothing to expand"
        );
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
    fn compact_cards_wrap_all_title_and_detail_text_without_ellipsis() {
        let node = PlanNode::new(
            "reservation",
            "Validate launch reservation identity",
            PlanNodeChange::Modified,
        )
        .with_detail("Skips empty nodes; requires launch UUID");

        let lines = canvas_node_lines(&node, 18, false);
        let text = lines.join("\n");
        assert!(
            !text.contains('…'),
            "compact cards do not discard text: {text}"
        );
        for part in [
            "Validate launch",
            "reservation",
            "identity",
            "Skips empty",
            "nodes; requires",
            "launch UUID",
        ] {
            assert!(
                text.contains(part),
                "missing `{part}` from wrapped card: {text}"
            );
        }
        assert!(lines.len() > 5, "wrapped content grows the card");
        assert!(
            lines.iter().all(|line| line.width() == 18),
            "every boxed row retains the requested width"
        );
    }

    #[test]
    fn vertical_relationship_labels_use_the_full_viewport_before_eliding() {
        let label = "reclaims expired conflicting ownership before publishing the reservation";
        let mut plan = VisualizationPlan::new(Epoch(7));
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            nodes: ["validate", "reap", "publish"]
                .into_iter()
                .map(|id| PlanNode::new(id, id, PlanNodeChange::Modified))
                .collect(),
            edges: vec![
                PlanEdge {
                    from: "validate".into(),
                    to: "reap".into(),
                    kind: PlanEdgeKind::FlowsTo,
                    label: Some(label.into()),
                },
                PlanEdge {
                    from: "reap".into(),
                    to: "publish".into(),
                    kind: PlanEdgeKind::FlowsTo,
                    label: None,
                },
            ],
        });

        let canvas = built(&plan, &DiagramState::default(), 96);
        let relationship = &canvas.relationships[0];
        assert_eq!(relationship.label, label);
        assert_eq!(usize::from(relationship.label_rect.width), label.width());
        assert!(
            relationship.label_rect.x < relationship.path[0].x
                && relationship.label_rect.right() > relationship.path[0].x,
            "a long-but-fitting label may span the route instead of being truncated"
        );
    }

    #[test]
    fn offset_stacks_route_vertically_and_only_clipped_labels_expand_in_place() {
        let mut plan = VisualizationPlan::new(Epoch(7));
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            nodes: vec![
                PlanNode::new("persist", "Persist state", PlanNodeChange::Modified),
                PlanNode::new("abort", "Abort update", PlanNodeChange::Modified),
            ],
            edges: vec![PlanEdge {
                from: "persist".into(),
                to: "abort".into(),
                kind: PlanEdgeKind::FlowsTo,
                label: Some("on persistence error".into()),
            }],
        });
        let persist = PlanNodeTarget {
            form: 0,
            id: "persist".into(),
        };
        let abort = PlanNodeTarget {
            form: 0,
            id: "abort".into(),
        };
        let mut stacked = DiagramState::default();
        stacked.move_node(persist.clone(), DiagramPosition { x: 5, y: 2 });
        stacked.move_node(abort.clone(), DiagramPosition { x: 12, y: 16 });
        let canvas = built(&plan, &stacked, 96);
        let source = canvas
            .nodes
            .iter()
            .find(|node| node.target == persist)
            .unwrap();
        let destination = canvas
            .nodes
            .iter()
            .find(|node| node.target == abort)
            .unwrap();
        let relationship = &canvas.relationships[0];
        assert_eq!(relationship.path.len(), 2);
        assert_eq!(relationship.path[0].x, relationship.path[1].x);
        assert!(relationship.label_rect.y > source.rect.bottom());
        assert!(relationship.label_rect.y < destination.rect.y);
        assert!(!relationship.has_hidden_label);
        assert!(canvas
            .relationship_at(DiagramPosition {
                x: relationship.label_rect.x,
                y: relationship.label_rect.y,
            })
            .is_none());
        assert!(canvas
            .relationship_overlay(&plan, &relationship.target)
            .is_none());

        // A narrow horizontal lane genuinely clips this description. Its expanded text owns
        // the same canvas anchor rather than jumping to the viewport origin.
        let label = "on persistence error, abort the state commit and retain the retry marker";
        plan.forms[0].edges[0].label = Some(label.into());
        let mut side_by_side = DiagramState::default();
        side_by_side.move_node(persist, DiagramPosition { x: 2, y: 2 });
        side_by_side.move_node(abort, DiagramPosition { x: 58, y: 2 });
        let clipped = built(&plan, &side_by_side, 96);
        let relationship = &clipped.relationships[0];
        assert!(relationship.has_hidden_label);
        assert!(relationship.label_rect.x > 0);
        assert_eq!(relationship.path[0].y, relationship.path[1].y);
        assert_eq!(
            clipped.relationship_at(DiagramPosition {
                x: relationship.label_rect.x,
                y: relationship.label_rect.y,
            }),
            Some(relationship.target.clone())
        );
        let overlay = clipped
            .relationship_overlay_in_viewport(&plan, &relationship.target, 0, 20, 0)
            .unwrap();
        assert_eq!(overlay.rect.x, relationship.label_rect.x);
        assert_eq!(overlay.rect.y, relationship.label_rect.y);
        assert_eq!(overlay.lines.join(" "), label);
    }

    #[test]
    fn four_node_flow_uses_edge_order_instead_of_an_ambiguous_grid() {
        let mut plan = VisualizationPlan::new(Epoch(7));
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            // Deliberately scramble document order: placement should follow relationships.
            nodes: ["third", "first", "fourth", "second"]
                .into_iter()
                .map(|id| PlanNode::new(id, id, PlanNodeChange::Modified))
                .collect(),
            edges: [
                ("first", "second"),
                ("second", "third"),
                ("third", "fourth"),
            ]
            .into_iter()
            .map(|(from, to)| PlanEdge {
                from: from.into(),
                to: to.into(),
                kind: PlanEdgeKind::FlowsTo,
                label: None,
            })
            .collect(),
        });

        let canvas = built(&plan, &DiagramState::default(), 96);
        let rect = |id: &str| {
            canvas
                .nodes
                .iter()
                .find(|node| node.target.id == id)
                .expect("flow node is placed")
                .rect
        };
        let ordered = [rect("first"), rect("second"), rect("third"), rect("fourth")];
        assert!(ordered.windows(2).all(|pair| pair[0].y < pair[1].y));
        assert!(ordered
            .windows(2)
            .all(|pair| pair[0].x + pair[0].width / 2 == pair[1].x + pair[1].width / 2));
        assert!(canvas.relationships.iter().all(|relationship| {
            relationship.path.len() == 2
                && relationship.path[0].x == relationship.path[1].x
                && relationship.path[0].y < relationship.path[1].y
        }));
    }

    #[test]
    fn branching_flow_layers_sources_branches_and_sink() {
        let mut plan = VisualizationPlan::new(Epoch(7));
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            nodes: ["sink", "right", "source", "left"]
                .into_iter()
                .map(|id| PlanNode::new(id, id, PlanNodeChange::Modified))
                .collect(),
            edges: [
                ("source", "left"),
                ("source", "right"),
                ("left", "sink"),
                ("right", "sink"),
            ]
            .into_iter()
            .map(|(from, to)| PlanEdge {
                from: from.into(),
                to: to.into(),
                kind: PlanEdgeKind::FlowsTo,
                label: None,
            })
            .collect(),
        });

        let canvas = built(&plan, &DiagramState::default(), 96);
        let rect = |id: &str| {
            canvas
                .nodes
                .iter()
                .find(|node| node.target.id == id)
                .expect("flow node is placed")
                .rect
        };
        let source = rect("source");
        let left = rect("left");
        let right = rect("right");
        let sink = rect("sink");
        assert!(source.y < left.y && left.y == right.y && right.y < sink.y);
        assert!(left.x.min(right.x) < source.x && source.x < left.x.max(right.x));
        assert_eq!(source.x, sink.x);
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
        assert!(scoped.is_node_expanded(&a));
        let mut incremented = plan.clone();
        incremented.forms[0].nodes.push(PlanNode::new(
            "later",
            "incremental node",
            PlanNodeChange::Added,
        ));
        scoped.sync_plan(&incremented);
        assert!(
            scoped.positions().contains_key(&a),
            "adding a node to the live draft retains user placement"
        );
        assert!(
            scoped.is_node_expanded(&a),
            "adding a node to the live draft retains the open card"
        );
        let mut changed = incremented;
        changed.epoch = Epoch(8);
        scoped.sync_plan(&changed);
        assert!(
            scoped.positions().is_empty()
                && scoped.z_order().is_empty()
                && scoped.expanded_nodes().is_empty()
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
        let label_y = c.relationships[0].label_rect.y;
        let p0 = c
            .relationship_overlay_in_viewport(&plan, &target, label_y, 1, 0)
            .unwrap();
        let p1 = c
            .relationship_overlay_in_viewport(&plan, &target, label_y, 1, 1)
            .unwrap();
        assert_eq!(p0.rect.height, 1);
        assert_ne!(p0.lines, p1.lines);
        assert!(!p0.lines[0].starts_with("relationship details"));
        let max = c
            .relationship_overlay_in_viewport(&plan, &target, label_y, 3, usize::MAX)
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
