//! Retained, world-coordinate layout for directly manipulated AI-plan diagrams.
//!
//! The structured plan remains the source of truth. This module deterministically assigns
//! initial positions, applies session-only node overrides, routes connectors, and clips a
//! styled scene into the current terminal viewport. Rendering and mouse geometry consume
//! the same [`CanvasFrame`], so an interaction can target only cells the user saw.

use std::collections::{HashMap, HashSet};

use codescope_core::{FormKind, PlanEdge, PlanEdgeKind, PlanNode, VisualizationPlan, VizForm};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::action::{PlanCanvasPoint, PlanNodeTarget};
use crate::app::PlanCanvasView;
use crate::diagram::{
    displayed_node_detail, edge_label, node_box_text, DiagramLine, DiagramRole, DiagramSpan,
};

const MAX_BOX_WIDTH: i32 = 32;
const NODE_GAP_Y: i32 = 3;
const CANVAS_MARGIN: i32 = 2;
const FORM_GAP: i32 = 4;

/// Signed world-space rectangle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CanvasRect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

impl CanvasRect {
    pub(crate) fn right(self) -> i32 {
        self.x + self.width
    }

    pub(crate) fn bottom(self) -> i32 {
        self.y + self.height
    }

    pub(crate) fn intersects(self, other: Self) -> bool {
        self.x < other.right()
            && self.right() > other.x
            && self.y < other.bottom()
            && self.bottom() > other.y
    }

    fn include(&mut self, other: Self) {
        if self.width == 0 || self.height == 0 {
            *self = other;
            return;
        }
        let left = self.x.min(other.x);
        let top = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        *self = Self {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        };
    }
}

/// One rendered node's world geometry and visible screen hitbox.
#[derive(Debug, Clone)]
pub(crate) struct CanvasNodeFrame {
    pub(crate) target: PlanNodeTarget,
    pub(crate) position: PlanCanvasPoint,
    pub(crate) footprint: CanvasRect,
    pub(crate) screen_rect: Option<ratatui::layout::Rect>,
}

/// One clipped frame plus the retained geometry needed for direct manipulation.
#[derive(Debug, Clone)]
pub(crate) struct CanvasFrame {
    pub(crate) lines: Vec<DiagramLine>,
    pub(crate) nodes: Vec<CanvasNodeFrame>,
    pub(crate) bounds: CanvasRect,
    pub(crate) origin: PlanCanvasPoint,
}

#[derive(Debug, Clone)]
struct SceneNode<'a> {
    target: PlanNodeTarget,
    node: &'a PlanNode,
    position: PlanCanvasPoint,
    actual_lines: Vec<String>,
    footprint: CanvasRect,
}

impl SceneNode<'_> {
    fn actual_rect(&self) -> CanvasRect {
        CanvasRect {
            x: self.position.x,
            y: self.position.y,
            width: self.footprint.width,
            height: i32::try_from(self.actual_lines.len()).unwrap_or(i32::MAX),
        }
    }
}

#[derive(Debug, Clone)]
struct SceneEdge<'a> {
    form: usize,
    edge: PlanEdge,
    source: &'a PlanNode,
    target: &'a PlanNode,
}

#[derive(Debug, Clone)]
struct Cell {
    ch: char,
    role: DiagramRole,
    target: Option<PlanNodeTarget>,
}

/// Render the active plan into exactly `height` viewport rows.
pub(crate) fn render_canvas(
    plan: &VisualizationPlan,
    width: u16,
    height: u16,
    selected_label: &str,
    hovered: Option<&PlanNodeTarget>,
    expanded: &[PlanNodeTarget],
    view: Option<&PlanCanvasView>,
) -> CanvasFrame {
    let width = usize::from(width);
    let height = usize::from(height);
    let empty_view = PlanCanvasView::default();
    let view = view.unwrap_or(&empty_view);
    let box_width = i32::try_from(width)
        .unwrap_or(MAX_BOX_WIDTH)
        .clamp(4, MAX_BOX_WIDTH);
    let (nodes, edges, intent_rows, evidence_rows) =
        build_scene(plan, width.max(40), box_width, expanded, view);

    let mut bounds = CanvasRect::default();
    if !intent_rows.is_empty() {
        bounds.include(CanvasRect {
            x: 0,
            y: 0,
            width: intent_rows
                .iter()
                .map(|row| i32::try_from(row.width()).unwrap_or(i32::MAX))
                .max()
                .unwrap_or(1),
            height: i32::try_from(intent_rows.len()).unwrap_or(i32::MAX),
        });
    }
    for node in &nodes {
        bounds.include(node.footprint);
    }
    let edge_allowance = edges
        .iter()
        .map(|edge| i32::try_from(edge_label(&edge.edge).width()).unwrap_or(i32::MAX))
        .max()
        .unwrap_or_default()
        .saturating_add(4);
    bounds.width = bounds.width.saturating_add(edge_allowance);
    let evidence_y = nodes
        .iter()
        .map(|node| node.footprint.bottom())
        .max()
        .unwrap_or(i32::try_from(intent_rows.len()).unwrap_or_default())
        + 2;
    if !evidence_rows.is_empty() {
        bounds.include(CanvasRect {
            x: 0,
            y: evidence_y,
            width: evidence_rows
                .iter()
                .map(|row| i32::try_from(row.width()).unwrap_or(i32::MAX))
                .max()
                .unwrap_or(1),
            height: i32::try_from(evidence_rows.len()).unwrap_or(i32::MAX),
        });
    }
    bounds = CanvasRect {
        x: bounds.x - CANVAS_MARGIN,
        y: bounds.y - CANVAS_MARGIN,
        width: bounds.width + 2 * CANVAS_MARGIN,
        height: bounds.height + 2 * CANVAS_MARGIN,
    };

    let mut cells = vec![vec![None::<Cell>; width]; height];
    for (row, text) in intent_rows.iter().enumerate() {
        put_text(
            &mut cells,
            0,
            i32::try_from(row).unwrap_or(i32::MAX),
            text,
            DiagramRole::Text,
            None,
            view.origin,
        );
    }
    draw_edges(&mut cells, &nodes, &edges, view.origin);
    for node in &nodes {
        let selected = node_selected(node.node, selected_label);
        let is_hovered = hovered == Some(&node.target);
        for (row, text) in node.actual_lines.iter().enumerate() {
            let role = if is_hovered {
                DiagramRole::Hovered
            } else if selected {
                DiagramRole::Selected
            } else if row == 0 || row + 1 == node.actual_lines.len() {
                DiagramRole::Border
            } else {
                DiagramRole::Text
            };
            put_text(
                &mut cells,
                node.position.x,
                node.position.y + i32::try_from(row).unwrap_or(i32::MAX),
                text,
                role,
                Some(node.target.clone()),
                view.origin,
            );
        }
    }
    for (row, text) in evidence_rows.iter().enumerate() {
        put_text(
            &mut cells,
            0,
            evidence_y + i32::try_from(row).unwrap_or(i32::MAX),
            text,
            DiagramRole::Evidence,
            None,
            view.origin,
        );
    }

    let lines = cells.into_iter().map(cells_to_line).collect();
    let nodes = nodes
        .into_iter()
        .map(|node| {
            let rect = node.actual_rect();
            CanvasNodeFrame {
                target: node.target,
                position: node.position,
                footprint: node.footprint,
                screen_rect: clip_to_screen(rect, view.origin, width, height),
            }
        })
        .collect();
    CanvasFrame {
        lines,
        nodes,
        bounds,
        origin: view.origin,
    }
}

fn build_scene<'a>(
    plan: &'a VisualizationPlan,
    wrap_width: usize,
    box_width: i32,
    expanded: &[PlanNodeTarget],
    view: &PlanCanvasView,
) -> (
    Vec<SceneNode<'a>>,
    Vec<SceneEdge<'a>>,
    Vec<String>,
    Vec<String>,
) {
    let intent_rows = wrap_full(&plan.intent, wrap_width);
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut form_top = i32::try_from(intent_rows.len()).unwrap_or_default() + 2;

    for (form_index, form) in plan.forms.iter().enumerate() {
        let positions = automatic_positions(form, form_top, box_width);
        let mut form_bottom = form_top;
        for node in &form.nodes {
            let target = PlanNodeTarget {
                form: form_index,
                id: node.id.clone(),
            };
            let position = view
                .positions
                .get(&target)
                .copied()
                .or_else(|| positions.get(node.id.as_str()).copied())
                .unwrap_or(PlanCanvasPoint { x: 0, y: form_top });
            let is_expanded = expanded.contains(&target);
            let actual_lines = node_box_text(
                &node.label,
                displayed_node_detail(node, is_expanded),
                usize::try_from(box_width).unwrap_or(32),
                is_expanded,
            );
            let maximum_lines = node_box_text(
                &node.label,
                displayed_node_detail(node, true),
                usize::try_from(box_width).unwrap_or(32),
                true,
            );
            let footprint = CanvasRect {
                x: position.x,
                y: position.y,
                width: box_width,
                height: i32::try_from(maximum_lines.len()).unwrap_or(i32::MAX),
            };
            form_bottom = form_bottom.max(footprint.bottom());
            nodes.push(SceneNode {
                target,
                node,
                position,
                actual_lines,
                footprint,
            });
        }
        edges.extend(scene_edges(form, form_index));
        form_top = form_bottom + FORM_GAP;
    }

    let evidence_rows = plan
        .evidence
        .iter()
        .map(|evidence| {
            let mut source = evidence.file.to_string();
            if let Some(range) = evidence.range {
                source.push(':');
                source.push_str(&range.start_line.saturating_add(1).to_string());
            } else if let Some(hunk) = evidence.hunk {
                source.push_str(&format!(" [h{}]", hunk.saturating_add(1)));
            }
            format!("{source} — {}", evidence.reason.trim())
        })
        .flat_map(|line| wrap_full(&line, wrap_width))
        .collect();
    (nodes, edges, intent_rows, evidence_rows)
}

fn automatic_positions(form: &VizForm, top: i32, box_width: i32) -> HashMap<&str, PlanCanvasPoint> {
    let mut out = HashMap::new();
    let gap_x = form
        .edges
        .iter()
        .map(|edge| i32::try_from(edge_label(edge).width()).unwrap_or(24) + 8)
        .max()
        .unwrap_or(14)
        .max(14);
    match form.kind {
        FormKind::Sequence | FormKind::ImpactSummary | FormKind::FocusedDiff => {
            let mut y = top;
            for node in &form.nodes {
                out.insert(node.id.as_str(), PlanCanvasPoint { x: 0, y });
                y += maximum_height(node, box_width) + NODE_GAP_Y;
            }
        }
        FormKind::BeforeAfter => {
            for (index, node) in form.nodes.iter().enumerate() {
                out.insert(
                    node.id.as_str(),
                    PlanCanvasPoint {
                        x: i32::try_from(index).unwrap_or(i32::MAX) * (box_width + gap_x),
                        y: top,
                    },
                );
            }
        }
        FormKind::ChangedSymbolTree | FormKind::CallTree | FormKind::TypeImplTree => {
            tree_positions(form, top, box_width, gap_x, &mut out);
        }
        FormKind::RelationshipFlow => graph_positions(form, top, box_width, gap_x, &mut out),
    }
    out
}

fn maximum_height(node: &PlanNode, box_width: i32) -> i32 {
    i32::try_from(
        node_box_text(
            &node.label,
            displayed_node_detail(node, true),
            usize::try_from(box_width).unwrap_or(32),
            true,
        )
        .len(),
    )
    .unwrap_or(i32::MAX)
}

fn tree_positions<'a>(
    form: &'a VizForm,
    top: i32,
    box_width: i32,
    gap_x: i32,
    out: &mut HashMap<&'a str, PlanCanvasPoint>,
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
    let mut next_y = top;
    let mut shown = HashSet::new();
    #[allow(clippy::too_many_arguments)]
    fn visit<'a>(
        node: &'a PlanNode,
        depth: i32,
        by_id: &HashMap<&'a str, &'a PlanNode>,
        box_width: i32,
        gap_x: i32,
        next_y: &mut i32,
        shown: &mut HashSet<&'a str>,
        out: &mut HashMap<&'a str, PlanCanvasPoint>,
    ) {
        if !shown.insert(node.id.as_str()) {
            return;
        }
        let y = *next_y;
        out.insert(
            node.id.as_str(),
            PlanCanvasPoint {
                x: depth * (box_width + gap_x),
                y,
            },
        );
        *next_y += maximum_height(node, box_width) + NODE_GAP_Y;
        for child in &node.children {
            if let Some(child) = by_id.get(child.as_str()) {
                visit(
                    child,
                    depth + 1,
                    by_id,
                    box_width,
                    gap_x,
                    next_y,
                    shown,
                    out,
                );
            }
        }
    }
    for root in form
        .nodes
        .iter()
        .filter(|node| !children.contains(node.id.as_str()))
    {
        visit(
            root,
            0,
            &by_id,
            box_width,
            gap_x,
            &mut next_y,
            &mut shown,
            out,
        );
    }
    for node in &form.nodes {
        visit(
            node,
            0,
            &by_id,
            box_width,
            gap_x,
            &mut next_y,
            &mut shown,
            out,
        );
    }
}

fn graph_positions<'a>(
    form: &'a VizForm,
    top: i32,
    box_width: i32,
    gap_x: i32,
    out: &mut HashMap<&'a str, PlanCanvasPoint>,
) {
    let mut layers: HashMap<&str, usize> = form
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), 0))
        .collect();
    for _ in 0..form.nodes.len() {
        let previous = layers.clone();
        let mut changed = false;
        for edge in &form.edges {
            let from = previous.get(edge.from.as_str()).copied().unwrap_or(0);
            let to = layers.entry(edge.to.as_str()).or_insert(0);
            let candidate = from
                .saturating_add(1)
                .min(form.nodes.len().saturating_sub(1));
            if candidate > *to {
                *to = candidate;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut rows: HashMap<usize, i32> = HashMap::new();
    for node in &form.nodes {
        let layer = layers.get(node.id.as_str()).copied().unwrap_or(0);
        let y = rows.entry(layer).or_insert(top);
        out.insert(
            node.id.as_str(),
            PlanCanvasPoint {
                x: i32::try_from(layer).unwrap_or(i32::MAX) * (box_width + gap_x),
                y: *y,
            },
        );
        *y += maximum_height(node, box_width) + NODE_GAP_Y;
    }
}

fn scene_edges(form: &VizForm, form_index: usize) -> Vec<SceneEdge<'_>> {
    let by_id: HashMap<&str, &PlanNode> = form
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let mut edges = Vec::new();
    for edge in &form.edges {
        if let (Some(source), Some(target)) =
            (by_id.get(edge.from.as_str()), by_id.get(edge.to.as_str()))
        {
            edges.push(SceneEdge {
                form: form_index,
                edge: edge.clone(),
                source,
                target,
            });
        }
    }
    for source in &form.nodes {
        for child in &source.children {
            if form
                .edges
                .iter()
                .any(|edge| edge.from == source.id && edge.to == *child)
            {
                continue;
            }
            if let Some(target) = by_id.get(child.as_str()) {
                edges.push(SceneEdge {
                    form: form_index,
                    edge: PlanEdge {
                        from: source.id.clone(),
                        to: child.clone(),
                        kind: PlanEdgeKind::Contains,
                        label: Some("contains".to_string()),
                    },
                    source,
                    target,
                });
            }
        }
    }
    if matches!(form.kind, FormKind::BeforeAfter) && edges.is_empty() && form.nodes.len() >= 2 {
        edges.push(SceneEdge {
            form: form_index,
            edge: PlanEdge {
                from: form.nodes[0].id.clone(),
                to: form.nodes[1].id.clone(),
                kind: PlanEdgeKind::Contains,
                label: Some("becomes".to_string()),
            },
            source: &form.nodes[0],
            target: &form.nodes[1],
        });
    }
    edges
}

fn draw_edges(
    cells: &mut [Vec<Option<Cell>>],
    nodes: &[SceneNode<'_>],
    edges: &[SceneEdge<'_>],
    origin: PlanCanvasPoint,
) {
    let obstacles = nodes.iter().map(SceneNode::actual_rect).collect::<Vec<_>>();
    for scene_edge in edges {
        let Some(source) = nodes.iter().find(|node| {
            node.target.form == scene_edge.form && node.target.id == scene_edge.source.id
        }) else {
            continue;
        };
        let Some(target) = nodes.iter().find(|node| {
            node.target.form == scene_edge.form && node.target.id == scene_edge.target.id
        }) else {
            continue;
        };
        let verified = source.node.entity.is_some()
            && target.node.entity.is_some()
            && matches!(
                scene_edge.edge.kind,
                PlanEdgeKind::Calls
                    | PlanEdgeKind::Imports
                    | PlanEdgeKind::Implements
                    | PlanEdgeKind::Contains
            );
        draw_edge(
            cells,
            source.actual_rect(),
            target.actual_rect(),
            edge_label(&scene_edge.edge),
            verified,
            origin,
            &obstacles,
        );
    }
}

fn draw_edge(
    cells: &mut [Vec<Option<Cell>>],
    from: CanvasRect,
    to: CanvasRect,
    label: &str,
    verified: bool,
    origin: PlanCanvasPoint,
    obstacles: &[CanvasRect],
) {
    let horizontal = (to.x - from.x).abs() >= (to.y - from.y).abs();
    let (start, end, mut arrow) = if horizontal {
        if to.x >= from.x {
            (
                PlanCanvasPoint {
                    x: from.right(),
                    y: from.y + from.height / 2,
                },
                PlanCanvasPoint {
                    x: to.x - 1,
                    y: to.y + to.height / 2,
                },
                '▶',
            )
        } else {
            (
                PlanCanvasPoint {
                    x: from.x - 1,
                    y: from.y + from.height / 2,
                },
                PlanCanvasPoint {
                    x: to.right(),
                    y: to.y + to.height / 2,
                },
                '◀',
            )
        }
    } else if to.y >= from.y {
        (
            PlanCanvasPoint {
                x: from.x + from.width / 2,
                y: from.bottom(),
            },
            PlanCanvasPoint {
                x: to.x + to.width / 2,
                y: to.y - 1,
            },
            '▼',
        )
    } else {
        (
            PlanCanvasPoint {
                x: from.x + from.width / 2,
                y: from.y - 1,
            },
            PlanCanvasPoint {
                x: to.x + to.width / 2,
                y: to.bottom(),
            },
            '▲',
        )
    };
    if !verified {
        arrow = match arrow {
            '▶' => '▷',
            '◀' => '◁',
            '▼' => '▽',
            '▲' => '△',
            other => other,
        };
    }
    let middle = [
        vec![
            start,
            PlanCanvasPoint {
                x: end.x,
                y: start.y,
            },
            end,
        ],
        vec![
            start,
            PlanCanvasPoint {
                x: start.x,
                y: end.y,
            },
            end,
        ],
    ];
    let route_score = |route: &[PlanCanvasPoint]| {
        route
            .windows(2)
            .map(|segment| {
                obstacles
                    .iter()
                    .filter(|rect| **rect != from && **rect != to)
                    .filter(|rect| segment_intersects_rect(segment[0], segment[1], **rect))
                    .count()
            })
            .sum::<usize>()
    };
    let mut route = middle
        .into_iter()
        .min_by_key(|candidate| route_score(candidate))
        .unwrap_or_else(|| vec![start, end]);
    if route_score(&route) > 0 {
        // Both direct elbows cross a box. Route outside the complete obstacle envelope;
        // boxes render over connectors as a final safety net, but this dogleg normally
        // keeps every segment clear.
        if horizontal {
            let detour_y = obstacles.iter().map(|rect| rect.y).min().unwrap_or(start.y) - 2;
            route = vec![
                start,
                PlanCanvasPoint {
                    x: start.x,
                    y: detour_y,
                },
                PlanCanvasPoint {
                    x: end.x,
                    y: detour_y,
                },
                end,
            ];
        } else {
            let detour_x = obstacles
                .iter()
                .map(|rect| rect.right())
                .max()
                .unwrap_or(start.x)
                + 2;
            route = vec![
                start,
                PlanCanvasPoint {
                    x: detour_x,
                    y: start.y,
                },
                PlanCanvasPoint {
                    x: detour_x,
                    y: end.y,
                },
                end,
            ];
        }
    }
    for segment in route.windows(2) {
        draw_segment(cells, segment[0], segment[1], verified, origin);
    }
    put_char(cells, end.x, end.y, arrow, DiagramRole::Arrow, None, origin);

    let horizontal_segments = route
        .windows(2)
        .map(|segment| (segment[0], segment[1]))
        .filter(|(a, b)| a.y == b.y && (a.x - b.x).abs() > 2)
        .collect::<Vec<_>>();
    if let Some((a, b)) = horizontal_segments
        .into_iter()
        .max_by_key(|(a, b)| (a.x - b.x).abs())
    {
        let length = usize::try_from((a.x - b.x).abs().saturating_sub(2)).unwrap_or_default();
        if length > 0 {
            let shown = truncate(label.trim(), length);
            let x = a.x.min(b.x)
                + 1
                + i32::try_from(length.saturating_sub(shown.width()) / 2).unwrap_or_default();
            put_text(cells, x, a.y, &shown, DiagramRole::Arrow, None, origin);
        }
    } else {
        // A vertical sequence keeps its causal text beside the rail instead of dropping
        // it. The virtual canvas may grow wider than the viewport; background panning can
        // reveal the complete label.
        let available = label.width();
        let x = from.right().max(to.right()) + 2;
        let y = start.y.min(end.y) + (start.y - end.y).abs() / 2;
        put_text(
            cells,
            x,
            y,
            &truncate(label.trim(), available),
            DiagramRole::Arrow,
            None,
            origin,
        );
    }
}

fn segment_intersects_rect(a: PlanCanvasPoint, b: PlanCanvasPoint, rect: CanvasRect) -> bool {
    if a.y == b.y {
        a.y >= rect.y
            && a.y < rect.bottom()
            && a.x.min(b.x) < rect.right()
            && a.x.max(b.x) >= rect.x
    } else if a.x == b.x {
        a.x >= rect.x
            && a.x < rect.right()
            && a.y.min(b.y) < rect.bottom()
            && a.y.max(b.y) >= rect.y
    } else {
        false
    }
}

fn draw_segment(
    cells: &mut [Vec<Option<Cell>>],
    a: PlanCanvasPoint,
    b: PlanCanvasPoint,
    verified: bool,
    origin: PlanCanvasPoint,
) {
    if a.y == b.y {
        let ch = if verified { '─' } else { '┄' };
        for x in a.x.min(b.x)..=a.x.max(b.x) {
            put_char(cells, x, a.y, ch, DiagramRole::Arrow, None, origin);
        }
    } else if a.x == b.x {
        let ch = if verified { '│' } else { '┊' };
        for y in a.y.min(b.y)..=a.y.max(b.y) {
            put_char(cells, a.x, y, ch, DiagramRole::Arrow, None, origin);
        }
    }
}

fn put_text(
    cells: &mut [Vec<Option<Cell>>],
    x: i32,
    y: i32,
    text: &str,
    role: DiagramRole,
    target: Option<PlanNodeTarget>,
    origin: PlanCanvasPoint,
) {
    let mut cursor = x;
    for ch in text.chars() {
        let width = i32::try_from(ch.width().unwrap_or(0)).unwrap_or_default();
        if width == 0 {
            continue;
        }
        put_char(cells, cursor, y, ch, role, target.clone(), origin);
        for continuation in 1..width {
            put_char(
                cells,
                cursor + continuation,
                y,
                ' ',
                role,
                target.clone(),
                origin,
            );
        }
        cursor += width;
    }
}

fn put_char(
    cells: &mut [Vec<Option<Cell>>],
    x: i32,
    y: i32,
    ch: char,
    role: DiagramRole,
    target: Option<PlanNodeTarget>,
    origin: PlanCanvasPoint,
) {
    let sx = x - origin.x;
    let sy = y - origin.y;
    let (Ok(sx), Ok(sy)) = (usize::try_from(sx), usize::try_from(sy)) else {
        return;
    };
    if let Some(cell) = cells.get_mut(sy).and_then(|row| row.get_mut(sx)) {
        *cell = Some(Cell { ch, role, target });
    }
}

fn cells_to_line(row: Vec<Option<Cell>>) -> DiagramLine {
    let last = row.iter().rposition(Option::is_some);
    let Some(last) = last else {
        return DiagramLine::default();
    };
    let mut spans: Vec<DiagramSpan> = Vec::new();
    for cell in row.into_iter().take(last + 1) {
        let cell = cell.unwrap_or(Cell {
            ch: ' ',
            role: DiagramRole::Muted,
            target: None,
        });
        if let Some(span) = spans
            .last_mut()
            .filter(|span| span.role == cell.role && span.target == cell.target)
        {
            span.text.push(cell.ch);
        } else {
            spans.push(DiagramSpan {
                text: cell.ch.to_string(),
                role: cell.role,
                target: cell.target,
            });
        }
    }
    DiagramLine { spans }
}

fn clip_to_screen(
    rect: CanvasRect,
    origin: PlanCanvasPoint,
    width: usize,
    height: usize,
) -> Option<ratatui::layout::Rect> {
    let left = (rect.x - origin.x).max(0);
    let top = (rect.y - origin.y).max(0);
    let right = (rect.right() - origin.x).min(i32::try_from(width).unwrap_or(i32::MAX));
    let bottom = (rect.bottom() - origin.y).min(i32::try_from(height).unwrap_or(i32::MAX));
    if left >= right || top >= bottom {
        return None;
    }
    Some(ratatui::layout::Rect::new(
        u16::try_from(left).ok()?,
        u16::try_from(top).ok()?,
        u16::try_from(right - left).ok()?,
        u16::try_from(bottom - top).ok()?,
    ))
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

fn wrap_full(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() && word.width() > width {
            let mut chunk = String::new();
            for ch in word.chars() {
                if chunk.width() + ch.width().unwrap_or(0) > width && !chunk.is_empty() {
                    lines.push(std::mem::take(&mut chunk));
                }
                chunk.push(ch);
            }
            current = chunk;
            continue;
        }
        let candidate = current.width() + usize::from(!current.is_empty()) + word.width();
        if candidate > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
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
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let budget = width.saturating_sub(1);
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if out.width() + ch_width > budget {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use codescope_core::{Epoch, PlanNodeChange};

    fn plan() -> VisualizationPlan {
        let mut plan = VisualizationPlan::new(Epoch(1));
        plan.intent = "The old request path becomes a guarded request path.".to_string();
        plan.forms.push(VizForm {
            kind: FormKind::BeforeAfter,
            nodes: vec![
                PlanNode::new("old", "Old path", PlanNodeChange::Modified)
                    .with_detail("sends immediately")
                    .with_expanded_detail(
                        "Sends the request immediately without checking readiness, downstream capacity, retry ownership, or cancellation state.",
                    ),
                PlanNode::new("new", "Guarded path", PlanNodeChange::Modified)
                    .with_detail("checks readiness")
                    .with_expanded_detail(
                        "Checks readiness before sending the request downstream.",
                    ),
            ],
            edges: vec![PlanEdge {
                from: "old".to_string(),
                to: "new".to_string(),
                kind: PlanEdgeKind::Writes,
                label: Some("is replaced only after the readiness gate opens".to_string()),
            }],
        });
        plan
    }

    #[test]
    fn connector_keeps_its_full_label_when_the_canvas_has_room() {
        let frame = render_canvas(&plan(), 140, 24, "", None, &[], None);
        let text = frame
            .lines
            .iter()
            .map(DiagramLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("is replaced only after the readiness gate opens"),
            "complete relationship label: {text}"
        );
    }

    #[test]
    fn expanded_render_uses_the_same_reserved_collision_footprint() {
        let target = PlanNodeTarget {
            form: 0,
            id: "old".to_string(),
        };
        let collapsed = render_canvas(&plan(), 100, 24, "", None, &[], None);
        let expanded = render_canvas(
            &plan(),
            100,
            24,
            "",
            None,
            std::slice::from_ref(&target),
            None,
        );
        let collapsed_node = collapsed
            .nodes
            .iter()
            .find(|node| node.target == target)
            .unwrap();
        let expanded_node = expanded
            .nodes
            .iter()
            .find(|node| node.target == target)
            .unwrap();
        assert_eq!(collapsed_node.footprint, expanded_node.footprint);
        assert!(
            expanded_node.screen_rect.unwrap().height > collapsed_node.screen_rect.unwrap().height
        );
    }
}
