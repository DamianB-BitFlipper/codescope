//! Mouse routing: pure hit-testing + drag state machine. All geometry comes from the
//! retained `UiGeometry` of the frame the user saw; no layout is recomputed here.
//!
//! Precedence (highest first): an open modal swallows everything and cancels any drag;
//! an active drag consumes Drag/Up anywhere; bottom-tab labels; divider drag handles
//! (horizontal wins at their intersection); a selectable file/symbol row; a pane for
//! focus; anything else is inert. Right/middle buttons, double-click, and wheel are
//! no-ops (wheel is deferred — Files has no independent viewport offset).

use crossterm::event::{MouseButton, MouseEvent};

use crate::action::Action;
use crate::app::{App, BottomView, Pane};
use crate::geometry::UiGeometry;
use crate::snapshot::UiSnapshot;

/// Which shared border is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragBoundary {
    /// The files|diff vertical divider (resizes files width).
    FilesDiff,
    /// The work|impact horizontal divider (resizes impact height).
    WorkImpact,
}

/// The drag state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DragState {
    /// No drag in progress.
    #[default]
    Idle,
    /// A drag is active.
    Dragging {
        /// Which boundary.
        boundary: DragBoundary,
        /// The pointer coordinate when the drag started.
        start_x: u16,
        /// The pointer row when the drag started.
        start_y: u16,
        /// The effective extent (width/height) at drag start.
        start_extent: u16,
        /// Whether the pointer has moved since mouse-down.
        moved: bool,
    },
}

/// The result of routing one mouse event: the action to dispatch, the next drag state,
/// and whether the screen needs a redraw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseOutcome {
    /// The action to apply (None = inert).
    pub action: Option<Action>,
    /// The next drag state.
    pub drag: DragState,
    /// Whether the frame must be redrawn.
    pub dirty: bool,
}

impl MouseOutcome {
    fn inert(drag: DragState) -> Self {
        MouseOutcome {
            action: None,
            drag,
            dirty: false,
        }
    }
    fn action(action: Action, drag: DragState) -> Self {
        MouseOutcome {
            action: Some(action),
            drag,
            dirty: true,
        }
    }
}

/// Route one mouse event. Pure: reads state, returns the intent; the caller dispatches.
pub fn map_mouse(
    event: MouseEvent,
    app: &App,
    snap: &UiSnapshot,
    geometry: &UiGeometry,
    drag: DragState,
) -> MouseOutcome {
    use crossterm::event::MouseEventKind as K;
    let (x, y) = (event.column, event.row);

    // Only left-button interactions and only the primary kinds.
    let is_left = matches!(
        event.kind,
        K::Down(MouseButton::Left) | K::Drag(MouseButton::Left) | K::Up(MouseButton::Left)
    );

    // 1. A modal swallows everything and cancels any drag.
    if app.show_help || app.show_model_picker || app.show_base_picker {
        return MouseOutcome::inert(DragState::Idle);
    }

    // 2. An active drag consumes Drag/Up regardless of the pointer's rectangle.
    if let DragState::Dragging { .. } = drag {
        return route_drag(event, app, geometry, drag);
    }

    match event.kind {
        K::Down(MouseButton::Left) => route_down(x, y, app, snap, geometry),
        // Non-left or non-primary kinds are inert.
        _ if !is_left => MouseOutcome::inert(drag),
        K::Drag(_) | K::Up(_) => MouseOutcome::inert(drag), // stray, not dragging
        _ => MouseOutcome::inert(drag),
    }
}

/// Mouse-down routing: tabs > handles > rows > pane focus > inert.
fn route_down(x: u16, y: u16, app: &App, snap: &UiSnapshot, geo: &UiGeometry) -> MouseOutcome {
    // 3. Bottom tab labels.
    if let Some(r) = geo.tabs.impact {
        if hit(r, x, y) {
            return MouseOutcome::action(
                Action::SetBottomView(BottomView::Impact),
                DragState::Idle,
            );
        }
    }
    if let Some(r) = geo.tabs.ai_plan {
        if hit(r, x, y) {
            return MouseOutcome::action(
                Action::SetBottomView(BottomView::AiPlan),
                DragState::Idle,
            );
        }
    }

    // 4. Drag handles (horizontal wins at the intersection).
    if let Some(r) = geo.work_impact_handle {
        if hit(r, x, y) {
            let extent = geo.impact.map(|i| i.height).unwrap_or(app.impact_height);
            return MouseOutcome {
                action: None,
                drag: DragState::Dragging {
                    boundary: DragBoundary::WorkImpact,
                    start_x: x,
                    start_y: y,
                    start_extent: extent,
                    moved: false,
                },
                dirty: false,
            };
        }
    }
    if let Some(r) = geo.files_diff_handle {
        if hit(r, x, y) {
            let extent = geo.files.map(|f| f.width).unwrap_or(app.files_width);
            return MouseOutcome {
                action: None,
                drag: DragState::Dragging {
                    boundary: DragBoundary::FilesDiff,
                    start_x: x,
                    start_y: y,
                    start_extent: extent,
                    moved: false,
                },
                dirty: false,
            };
        }
    }

    // 5. A selectable file/symbol row.
    for (rect, phys) in &geo.file_row_rects {
        if hit(*rect, x, y) {
            let rows = crate::file_rows::project(&snap.files);
            if let Some(row) = rows.get(*phys) {
                if let Some(logical) = row.logical_index() {
                    return MouseOutcome::action(
                        Action::SelectFileRow {
                            logical_index: logical,
                        },
                        DragState::Idle,
                    );
                }
                // A note row: focus Files but do not select.
                return MouseOutcome::action(Action::Focus(Pane::Files), DragState::Idle);
            }
        }
    }

    // 6. A pane rectangle: focus only.
    if let Some(pane) = geo.pane_at(x, y) {
        return MouseOutcome::action(Action::Focus(pane), DragState::Idle);
    }

    MouseOutcome::inert(DragState::Idle)
}

/// Drag/Up routing while a drag is active.
fn route_drag(event: MouseEvent, _app: &App, geo: &UiGeometry, drag: DragState) -> MouseOutcome {
    use crossterm::event::{MouseButton, MouseEventKind as K};
    let DragState::Dragging {
        boundary,
        start_x,
        start_y,
        start_extent,
        moved,
    } = drag
    else {
        return MouseOutcome::inert(drag);
    };
    // Cancel if the handle is gone (zoom/modal removed it) — never resize a hidden pane.
    let handle_present = match boundary {
        DragBoundary::FilesDiff => geo.files_diff_handle.is_some(),
        DragBoundary::WorkImpact => geo.work_impact_handle.is_some(),
    };
    if !handle_present {
        return MouseOutcome::inert(DragState::Idle);
    }
    // One signed, saturating sample: the absolute extent the pointer currently implies.
    let sample = |x: u16, y: u16| -> u16 {
        let delta = match boundary {
            DragBoundary::FilesDiff => x as i64 - start_x as i64,
            DragBoundary::WorkImpact => start_y as i64 - y as i64, // up = taller
        };
        (start_extent as i64 + delta).clamp(0, u16::MAX as i64) as u16
    };
    let extent_action = |x: u16, y: u16| match boundary {
        DragBoundary::FilesDiff => Action::SetFilesWidth(sample(x, y)),
        DragBoundary::WorkImpact => Action::SetImpactHeight(sample(x, y)),
    };
    match event.kind {
        K::Drag(MouseButton::Left) => {
            let did_move = event.column != start_x || event.row != start_y;
            // Only emit a setter on real movement — a same-coordinate Drag must not
            // overwrite a constrained preference with the effective extent.
            MouseOutcome {
                action: if did_move {
                    Some(extent_action(event.column, event.row))
                } else {
                    None
                },
                drag: DragState::Dragging {
                    boundary,
                    start_x,
                    start_y,
                    start_extent,
                    moved: moved || did_move,
                },
                dirty: did_move,
            }
        }
        K::Up(MouseButton::Left) => {
            // Commit the release position when it differs from the start (even without a
            // prior Drag event), then end. A same-coordinate Down/Up is a no-op.
            let released_elsewhere = event.column != start_x || event.row != start_y;
            MouseOutcome {
                action: if moved || released_elsewhere {
                    Some(extent_action(event.column, event.row))
                } else {
                    None
                },
                drag: DragState::Idle,
                dirty: moved || released_elsewhere,
            }
        }
        // Any other button/kind while dragging ends it.
        _ => MouseOutcome::inert(DragState::Idle),
    }
}

/// Point-in-rect.
fn hit(r: ratatui::layout::Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    use crate::app::{App, BottomView, Pane};
    use crate::geometry::UiGeometry;
    use crate::snapshot::{FileRow, FileSemanticLoad, SymbolRow, UiSnapshot};

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }
    fn down(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), x, y)
    }
    fn drag(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::Drag(MouseButton::Left), x, y)
    }
    fn up(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::Up(MouseButton::Left), x, y)
    }

    /// A snapshot with two files: a collapsed one and an expanded one with two symbols.
    fn snap() -> UiSnapshot {
        UiSnapshot {
            files: vec![
                FileRow {
                    path: "a.go".to_string(),
                    status: "M",
                    changed_symbol_count: 0,
                    symbols: vec![],
                    expanded: false,
                    semantic: FileSemanticLoad::Unloaded,
                },
                FileRow {
                    path: "b.go".to_string(),
                    status: "M",
                    changed_symbol_count: 2,
                    symbols: vec![
                        SymbolRow {
                            name: "B_one".to_string(),
                            change: "modified",
                            confidence: "",
                            has_diagnostic: false,
                            position: Some((10, 2)),
                        },
                        SymbolRow {
                            name: "B_two".to_string(),
                            change: "added",
                            confidence: "",
                            has_diagnostic: false,
                            position: Some((20, 2)),
                        },
                    ],
                    expanded: true,
                    semantic: FileSemanticLoad::Ready,
                },
            ],
            ..Default::default()
        }
    }

    fn app_with(snap: &UiSnapshot) -> App {
        let mut app = App::new();
        app.update(snap.clone());
        app
    }

    /// A Normal-tier geometry at 140x40 with the default layout.
    fn geo(app: &App, snap: &UiSnapshot) -> UiGeometry {
        UiGeometry::build(Rect::new(0, 0, 140, 40), app, snap)
    }

    #[test]
    fn click_focuses_each_pane() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        // A point inside the diff pane (right of the files|diff border, in the work row).
        let d = g.diff.expect("diff pane present");
        let out = map_mouse(down(d.x + 5, d.y + 5), &app, &s, &g, DragState::Idle);
        assert_eq!(out.action, Some(Action::Focus(Pane::Diff)));
        // The impact pane (bottom).
        let im = g.impact.expect("impact pane present");
        let out = map_mouse(down(im.x + 5, im.y + 2), &app, &s, &g, DragState::Idle);
        assert_eq!(out.action, Some(Action::Focus(Pane::Impact)));
    }

    #[test]
    fn click_file_row_selects_it() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        // The first file row is at the top of the files inner rect.
        let (rect, phys) = g.file_row_rects[0];
        let out = map_mouse(down(rect.x + 1, rect.y), &app, &s, &g, DragState::Idle);
        assert_eq!(out.action, Some(Action::SelectFileRow { logical_index: 0 }));
        let _ = phys;
    }

    #[test]
    fn click_symbol_row_selects_that_symbol() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        // b.go is expanded with two symbols: physical rows are [a.go(0), b.go(1), B_one(2), B_two(3)].
        // Logical indices: a.go=0, b.go=1, B_one=2, B_two=3.
        let row_rects = &g.file_row_rects;
        let sym_row = row_rects
            .iter()
            .find(|(_, p)| *p == 2)
            .map(|(r, _)| *r)
            .expect("B_one row present");
        let out = map_mouse(
            down(sym_row.x + 2, sym_row.y),
            &app,
            &s,
            &g,
            DragState::Idle,
        );
        assert_eq!(out.action, Some(Action::SelectFileRow { logical_index: 2 }));
    }

    #[test]
    fn click_blank_tail_focuses_without_selecting() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let fi = g.files_inner.expect("files inner");
        // A point below the last row but inside the files pane.
        let y = fi.y + fi.height - 1;
        let out = map_mouse(down(fi.x + 2, y), &app, &s, &g, DragState::Idle);
        assert_eq!(out.action, Some(Action::Focus(Pane::Files)));
    }

    #[test]
    fn click_impact_tab_selects_impact() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let r = g.tabs.impact.expect("impact tab");
        let out = map_mouse(down(r.x + 1, r.y), &app, &s, &g, DragState::Idle);
        assert_eq!(out.action, Some(Action::SetBottomView(BottomView::Impact)));
    }

    #[test]
    fn click_ai_plan_tab_selects_ai_plan() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let r = g.tabs.ai_plan.expect("ai tab");
        let out = map_mouse(down(r.x + 1, r.y), &app, &s, &g, DragState::Idle);
        assert_eq!(out.action, Some(Action::SetBottomView(BottomView::AiPlan)));
    }

    #[test]
    fn vertical_divider_drag_resizes_files() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let h = g.files_diff_handle.expect("files|diff handle");
        // Down arms the drag without resizing.
        let out = map_mouse(down(h.x, h.y + 2), &app, &s, &g, DragState::Idle);
        assert!(matches!(out.drag, DragState::Dragging { .. }));
        assert_eq!(out.action, None);
        // Drag right 4 cells -> files wider by 4.
        let start_extent = match out.drag {
            DragState::Dragging { start_extent, .. } => start_extent,
            _ => unreachable!(),
        };
        let out2 = map_mouse(drag(h.x + 4, h.y + 2), &app, &s, &g, out.drag);
        assert_eq!(out2.action, Some(Action::SetFilesWidth(start_extent + 4)));
        // Up commits and ends the drag.
        let out3 = map_mouse(up(h.x + 4, h.y + 2), &app, &s, &g, out2.drag);
        assert!(matches!(out3.drag, DragState::Idle));
    }

    #[test]
    fn horizontal_divider_drag_resizes_impact() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let h = g.work_impact_handle.expect("work|impact handle");
        let out = map_mouse(down(h.x + 10, h.y), &app, &s, &g, DragState::Idle);
        assert!(matches!(out.drag, DragState::Dragging { .. }));
        let start_extent = match out.drag {
            DragState::Dragging { start_extent, .. } => start_extent,
            _ => unreachable!(),
        };
        // Drag UP 2 cells -> impact taller by 2 (inverse y).
        let out2 = map_mouse(drag(h.x + 10, h.y - 2), &app, &s, &g, out.drag);
        assert_eq!(out2.action, Some(Action::SetImpactHeight(start_extent + 2)));
    }

    #[test]
    fn click_without_movement_does_not_resize() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let h = g.files_diff_handle.expect("handle");
        let armed = map_mouse(down(h.x, h.y + 1), &app, &s, &g, DragState::Idle);
        // Up at the same coordinate: no resize, drag ends.
        let out = map_mouse(up(h.x, h.y + 1), &app, &s, &g, armed.drag);
        assert_eq!(out.action, None);
        assert!(matches!(out.drag, DragState::Idle));
    }

    #[test]
    fn modal_swallows_clicks() {
        let s = snap();
        let mut app = app_with(&s);
        app.show_help = true;
        let g = geo(&app, &s);
        let d = g.diff.expect("diff");
        let out = map_mouse(down(d.x + 1, d.y + 1), &app, &s, &g, DragState::Idle);
        assert_eq!(out.action, None, "modal swallows the click");
    }

    #[test]
    fn zoomed_layout_hides_other_panes() {
        let s = snap();
        let mut app = app_with(&s);
        app.zoomed = true;
        app.focused = Pane::Diff;
        let g = UiGeometry::build(Rect::new(0, 0, 140, 40), &app, &s);
        assert!(g.files.is_none(), "files hidden while diff is zoomed");
        assert!(g.impact.is_none());
        assert!(g.diff.is_some());
        assert!(g.files_diff_handle.is_none(), "no drag handle while zoomed");
    }
}
