//! Mouse routing: pure hit-testing + drag state machine. All geometry comes from the
//! retained `UiGeometry` of the frame the user saw; no layout is recomputed here.
//!
//! Precedence (highest first): an open modal swallows everything and cancels any drag;
//! an active drag consumes Drag/Up anywhere; wheel routes to the independently scrollable
//! region under the pointer without changing focus/selection; divider drag handles
//! (horizontal wins at their intersection); a selectable file/symbol row; a pane for
//! focus; anything else is inert. Right/middle buttons and double-click are no-ops.

use crossterm::event::{MouseButton, MouseEvent};

use crate::action::Action;
use crate::action::PlanNodeTarget;
use crate::action::{DiffTextPoint, DiffTextSelection};
use crate::app::{App, Pane};
use crate::divider::DividerId;
use crate::geometry::UiGeometry;
use crate::snapshot::UiSnapshot;

/// The drag state machine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DragState {
    /// No drag in progress.
    #[default]
    Idle,
    /// A drag is active.
    Dragging {
        /// Which registered divider.
        divider: DividerId,
        /// The pointer coordinate when the drag started.
        start_x: u16,
        /// The pointer row when the drag started.
        start_y: u16,
        /// The effective extent (width/height) at drag start.
        start_extent: u16,
        /// Whether the pointer has moved since mouse-down.
        moved: bool,
    },
    /// A generated-plan box is armed for click or being dragged to a new order.
    PlanNode {
        /// Box being moved.
        source: PlanNodeTarget,
        /// Pointer coordinate where the gesture began.
        start_x: u16,
        /// Pointer row where the gesture began.
        start_y: u16,
        /// Most recent valid drop anchor and insertion side.
        drop: Option<(PlanNodeTarget, bool)>,
        /// Whether the pointer has moved.
        moved: bool,
    },
    /// A linear text selection is being dragged across the rendered diff.
    DiffText {
        /// Selection anchor in physical display coordinates.
        start: DiffTextPoint,
        /// Most recent endpoint still inside the diff text viewport.
        end: DiffTextPoint,
        /// Whether the endpoint differs from the anchor.
        moved: bool,
    },
}

/// The result of routing one mouse event: the action to dispatch, the next drag state,
/// and whether the screen needs a redraw.
#[derive(Debug, Clone, PartialEq)]
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

    // 1. The status detail is click-to-dismiss anywhere in the overlay. It precedes the
    // other modal gate because it is opened by clicking the bottom status itself.
    if app.status_detail.is_some() {
        return match event.kind {
            K::Down(MouseButton::Left) => {
                MouseOutcome::action(Action::ToggleStatusDetail, DragState::Idle)
            }
            _ => MouseOutcome::inert(DragState::Idle),
        };
    }

    // 2. A modal swallows clicks and routes its wheel to its own selected list.
    if app.show_help || app.show_model_picker || app.show_base_picker {
        return match event.kind {
            K::ScrollUp if app.show_model_picker || app.show_base_picker => {
                MouseOutcome::action(Action::Up, DragState::Idle)
            }
            K::ScrollDown if app.show_model_picker || app.show_base_picker => {
                MouseOutcome::action(Action::Down, DragState::Idle)
            }
            _ => MouseOutcome::inert(DragState::Idle),
        };
    }

    // A floating plan inspector is intentionally modal: clicking it (or anywhere else)
    // closes it, while the diagram beneath remains in exactly the same layout.
    if app.plan_inspector_open() {
        return match event.kind {
            K::Down(MouseButton::Left) => {
                MouseOutcome::action(Action::ClosePlanInspector, DragState::Idle)
            }
            _ => MouseOutcome::inert(DragState::Idle),
        };
    }

    // 3. An active drag consumes Drag/Up regardless of the pointer's rectangle.
    if !matches!(drag, DragState::Idle) {
        return route_drag(event, app, geometry, drag);
    }

    match event.kind {
        K::Moved => route_hover(x, y, app, geometry),
        K::ScrollUp => route_wheel(x, y, -3, geometry),
        K::ScrollDown => route_wheel(x, y, 3, geometry),
        K::Down(MouseButton::Left) => route_down(x, y, app, snap, geometry),
        // Non-left or non-primary kinds are inert.
        _ if !is_left => MouseOutcome::inert(drag),
        K::Drag(_) | K::Up(_) => MouseOutcome::inert(drag), // stray, not dragging
        _ => MouseOutcome::inert(drag),
    }
}

/// Motion only redraws when the semantic node target changes. A steady stream inside
/// one box is inert, so any-motion terminal tracking cannot starve snapshot delivery.
fn route_hover(x: u16, y: u16, app: &App, geo: &UiGeometry) -> MouseOutcome {
    let target = geo.plan_node_at(x, y);
    if target == app.hovered_plan_node {
        MouseOutcome::inert(DragState::Idle)
    } else {
        MouseOutcome::action(Action::HoverPlanNode(target), DragState::Idle)
    }
}

/// Wheel routing is hover-only: it neither focuses a pane nor changes a row selection.
fn route_wheel(x: u16, y: u16, delta: i32, geo: &UiGeometry) -> MouseOutcome {
    let Some(region) = geo.scroll_region_at(x, y) else {
        return MouseOutcome::inert(DragState::Idle);
    };
    let Some(offset) = region.scrolled_offset(delta) else {
        return MouseOutcome::inert(DragState::Idle);
    };
    MouseOutcome::action(
        Action::ScrollRegion {
            region: region.id,
            offset,
        },
        DragState::Idle,
    )
}

/// Mouse-down routing: handles > rows > pane focus > inert.
fn route_down(x: u16, y: u16, app: &App, snap: &UiSnapshot, geo: &UiGeometry) -> MouseOutcome {
    // 4. The nonempty status-message segment is a chrome action, not a pane. Its exact
    // rectangle excludes the right-justified token/path fields.
    if geo.status_at(x, y) && !snap.status.text.is_empty() {
        return MouseOutcome::action(Action::ToggleStatusDetail, DragState::Idle);
    }

    // The generated fallback owns its failure diagnostic instead of depending on the
    // footer, which may already have advanced to an automatic retry or another message.
    if geo.ai_failure_status_at(x, y) {
        if let Some(status) = snap.ai_failure_status() {
            return MouseOutcome::action(Action::OpenStatusDetail(status), DragState::Idle);
        }
    }

    // 5. Any registered drag handle. Geometry resolves intersection precedence.
    if let Some(handle) = geo.divider_at(x, y) {
        return MouseOutcome {
            action: None,
            drag: DragState::Dragging {
                divider: handle.id,
                start_x: x,
                start_y: y,
                start_extent: handle.effective_extent,
                moved: false,
            },
            dirty: false,
        };
    }

    // 6. Relationship labels expand independently from their endpoint boxes.
    if let Some(target) = geo.plan_relationship_at(x, y) {
        return MouseOutcome::action(Action::TogglePlanRelationship(target), DragState::Idle);
    }

    // 7. A generated-plan node: arm a click-or-drag gesture. A release without motion
    // toggles details; dragging reorders boxes inside the automatic bounded layout.
    if let Some(target) = geo.plan_node_at(x, y) {
        return MouseOutcome {
            action: None,
            drag: DragState::PlanNode {
                source: target,
                start_x: x,
                start_y: y,
                drop: None,
                moved: false,
            },
            dirty: false,
        };
    }

    // 8. Diff text uses native-style drag selection. Release copies the exact retained
    // code text, while a click without movement clears the previous selection.
    if let Some(start) = geo.diff_text_point_at(x, y) {
        return MouseOutcome {
            action: None,
            drag: DragState::DiffText {
                start,
                end: start,
                moved: false,
            },
            dirty: false,
        };
    }

    // 9. A selectable file/symbol row.
    for (rect, phys) in &geo.file_row_rects {
        if hit(*rect, x, y) {
            let rows = app.projected_file_rows();
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

    // 10. A pane rectangle: focus only. Clicking blank diff space also clears any
    // retained text selection, matching a plain click on a rendered code row.
    if let Some(pane) = geo.pane_at(x, y) {
        if pane == Pane::Diff && app.diff_selection.is_some() {
            return MouseOutcome::action(Action::ClearDiffSelection, DragState::Idle);
        }
        return MouseOutcome::action(Action::Focus(pane), DragState::Idle);
    }

    MouseOutcome::inert(DragState::Idle)
}

/// Drag/Up routing while a drag is active.
fn route_drag(event: MouseEvent, _app: &App, geo: &UiGeometry, drag: DragState) -> MouseOutcome {
    use crossterm::event::{MouseButton, MouseEventKind as K};
    match drag {
        DragState::Idle => MouseOutcome::inert(DragState::Idle),
        DragState::Dragging {
            divider,
            start_x,
            start_y,
            start_extent,
            moved,
        } => {
            let Some(handle) = geo.divider(divider) else {
                return MouseOutcome::inert(DragState::Idle);
            };
            let extent_action = |x: u16, y: u16| Action::ResizeDivider {
                divider,
                extent: handle.resized_extent_from(start_extent, start_x, start_y, x, y),
            };
            match event.kind {
                K::Drag(MouseButton::Left) => {
                    let did_move = event.column != start_x || event.row != start_y;
                    MouseOutcome {
                        action: did_move.then(|| extent_action(event.column, event.row)),
                        drag: DragState::Dragging {
                            divider,
                            start_x,
                            start_y,
                            start_extent,
                            moved: moved || did_move,
                        },
                        dirty: did_move,
                    }
                }
                K::Up(MouseButton::Left) => {
                    let released_elsewhere = event.column != start_x || event.row != start_y;
                    MouseOutcome {
                        action: (moved || released_elsewhere)
                            .then(|| extent_action(event.column, event.row)),
                        drag: DragState::Idle,
                        dirty: moved || released_elsewhere,
                    }
                }
                _ => MouseOutcome::inert(DragState::Idle),
            }
        }
        DragState::PlanNode {
            source,
            start_x,
            start_y,
            drop,
            moved,
        } => {
            let did_move = event.column != start_x || event.row != start_y;
            let next_drop = geo
                .plan_node_drop_at(event.column, event.row, &source)
                .or(drop.clone());
            match event.kind {
                K::Drag(MouseButton::Left) => MouseOutcome {
                    action: next_drop
                        .as_ref()
                        .map(|(target, _)| Action::HoverPlanNode(Some(target.clone()))),
                    drag: DragState::PlanNode {
                        source,
                        start_x,
                        start_y,
                        drop: next_drop,
                        moved: moved || did_move,
                    },
                    dirty: moved || did_move,
                },
                K::Up(MouseButton::Left) => {
                    let moved = moved || did_move;
                    let action = if moved {
                        next_drop.map(|(anchor, after)| Action::ReorderPlanNode {
                            dragged: source,
                            anchor,
                            after,
                        })
                    } else {
                        Some(Action::TogglePlanNode(source))
                    };
                    MouseOutcome {
                        dirty: action.is_some(),
                        action,
                        drag: DragState::Idle,
                    }
                }
                _ => MouseOutcome::inert(DragState::Idle),
            }
        }
        DragState::DiffText { start, end, moved } => {
            let next = geo
                .diff_text_point_at(event.column, event.row)
                .unwrap_or(end);
            let selection = DiffTextSelection { start, end: next };
            let did_move = next != start;
            match event.kind {
                K::Drag(MouseButton::Left) => MouseOutcome {
                    action: (did_move || moved).then_some(Action::SetDiffSelection(selection)),
                    drag: DragState::DiffText {
                        start,
                        end: next,
                        moved: moved || did_move,
                    },
                    dirty: did_move || moved,
                },
                K::Up(MouseButton::Left) => {
                    let moved = moved || did_move;
                    MouseOutcome {
                        action: if moved {
                            Some(Action::CommitDiffSelection {
                                selection,
                                text: geo.selected_diff_text(selection),
                            })
                        } else {
                            Some(Action::ClearDiffSelection)
                        },
                        drag: DragState::Idle,
                        dirty: true,
                    }
                }
                _ => MouseOutcome::inert(DragState::Idle),
            }
        }
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

    use crate::app::{App, Pane};
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
    fn wheel_down(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::ScrollDown, x, y)
    }
    fn wheel_up(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::ScrollUp, x, y)
    }
    fn moved(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::Moved, x, y)
    }

    /// A snapshot with two files: a collapsed one and an expanded one with two symbols.
    fn snap() -> UiSnapshot {
        UiSnapshot {
            files: vec![
                FileRow {
                    path: "a.go".to_string(),
                    status: "M",
                    changed_symbol_count: 0,
                    added_lines: 0,
                    removed_lines: 0,
                    symbols: vec![],
                    expanded: false,
                    semantic: FileSemanticLoad::Unloaded,
                },
                FileRow {
                    path: "b.go".to_string(),
                    status: "M",
                    changed_symbol_count: 2,
                    added_lines: 0,
                    removed_lines: 0,
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
    fn click_impact_body_focuses_the_combined_pane() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let r = g.impact.expect("impact pane");
        let out = map_mouse(
            down(r.x + 3, r.y + r.height.saturating_sub(2)),
            &app,
            &s,
            &g,
            DragState::Idle,
        );
        assert_eq!(out.action, Some(Action::Focus(Pane::Impact)));
    }

    #[test]
    fn vertical_divider_drag_resizes_files() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let h = g.divider(DividerId::FilesDiff).expect("files|diff handle");
        let rect = h.rect;
        // Down arms the drag without resizing.
        let out = map_mouse(down(rect.x, rect.y + 2), &app, &s, &g, DragState::Idle);
        assert!(matches!(out.drag, DragState::Dragging { .. }));
        assert_eq!(out.action, None);
        // Drag right 4 cells -> files wider by 4.
        let start_extent = match out.drag {
            DragState::Dragging { start_extent, .. } => start_extent,
            _ => unreachable!(),
        };
        let out2 = map_mouse(drag(rect.x + 4, rect.y + 2), &app, &s, &g, out.drag);
        assert_eq!(
            out2.action,
            Some(Action::ResizeDivider {
                divider: DividerId::FilesDiff,
                extent: start_extent + 4,
            })
        );
        // Up commits and ends the drag.
        let out3 = map_mouse(up(rect.x + 4, rect.y + 2), &app, &s, &g, out2.drag);
        assert!(matches!(out3.drag, DragState::Idle));
    }

    #[test]
    fn horizontal_divider_drag_resizes_impact() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let h = g
            .divider(DividerId::WorkReview)
            .expect("work|review handle");
        let rect = h.rect;
        let out = map_mouse(down(rect.x + 10, rect.y), &app, &s, &g, DragState::Idle);
        assert!(matches!(out.drag, DragState::Dragging { .. }));
        let start_extent = match out.drag {
            DragState::Dragging { start_extent, .. } => start_extent,
            _ => unreachable!(),
        };
        // Drag UP 2 cells -> impact taller by 2 (inverse y).
        let out2 = map_mouse(drag(rect.x + 10, rect.y - 2), &app, &s, &g, out.drag);
        assert_eq!(
            out2.action,
            Some(Action::ResizeDivider {
                divider: DividerId::WorkReview,
                extent: start_extent + 2,
            })
        );
    }

    #[test]
    fn impact_vertical_divider_drag_resizes_relationship_stack() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let h = g
            .divider(DividerId::RelationshipsGenerated)
            .expect("relationship|generated handle");
        let rect = h.rect;
        let armed = map_mouse(down(rect.x, rect.y + 1), &app, &s, &g, DragState::Idle);
        assert!(matches!(
            armed.drag,
            DragState::Dragging {
                divider: DividerId::RelationshipsGenerated,
                ..
            }
        ));
        let start_extent = match armed.drag {
            DragState::Dragging { start_extent, .. } => start_extent,
            _ => unreachable!(),
        };
        let moved = map_mouse(drag(rect.x + 6, rect.y + 1), &app, &s, &g, armed.drag);
        assert_eq!(
            moved.action,
            Some(Action::ResizeDivider {
                divider: DividerId::RelationshipsGenerated,
                extent: start_extent + 6,
            })
        );
    }

    #[test]
    fn every_internal_horizontal_sectional_uses_the_same_drag_path() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        for divider in [DividerId::SelectedCallers, DividerId::CallersDownstream] {
            let handle = g.divider(divider).expect("internal sectional handle");
            let rect = handle.rect;
            let armed = map_mouse(down(rect.x + 2, rect.y), &app, &s, &g, DragState::Idle);
            assert!(matches!(
                armed.drag,
                DragState::Dragging { divider: active, .. } if active == divider
            ));
            let start_extent = match armed.drag {
                DragState::Dragging { start_extent, .. } => start_extent,
                _ => unreachable!(),
            };
            let moved = map_mouse(drag(rect.x + 2, rect.y + 1), &app, &s, &g, armed.drag);
            assert_eq!(
                moved.action,
                Some(Action::ResizeDivider {
                    divider,
                    extent: start_extent + 1,
                })
            );
        }
    }

    #[test]
    fn click_without_movement_does_not_resize() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let h = g.divider(DividerId::FilesDiff).expect("handle").rect;
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
    fn clicking_status_opens_detail_and_clicking_modal_closes_it() {
        let mut s = snap();
        s.status = crate::snapshot::StatusMessage {
            text: "AI provider returned HTTP 400 with a long response".to_string(),
            detail: None,
            level: crate::snapshot::StatusLevel::Warning,
        };
        let mut app = app_with(&s);
        let g = geo(&app, &s);
        let status = g.status.expect("status hit target");
        let open = map_mouse(down(status.x + 1, status.y), &app, &s, &g, DragState::Idle);
        assert_eq!(open.action, Some(Action::ToggleStatusDetail));

        app.apply(open.action.expect("open action"));
        let close = map_mouse(down(20, 10), &app, &s, &g, DragState::Idle);
        assert_eq!(close.action, Some(Action::ToggleStatusDetail));
    }

    #[test]
    fn clicking_ai_failure_banner_opens_the_retained_complete_reason() {
        let reason = "provider HTTP 400\nfull validation response tail";
        let mut s = snap();
        s.ai = codescope_core::AiStatus::Failed {
            reason: reason.to_string(),
        };
        s.status = crate::snapshot::StatusMessage {
            text: "automatic retry queued".to_string(),
            detail: None,
            level: crate::snapshot::StatusLevel::Info,
        };
        let mut app = app_with(&s);
        let g = geo(&app, &s);
        let banner = g
            .ai_failure_status
            .expect("visible AI failure banner hit target");

        let open = map_mouse(down(banner.x + 1, banner.y), &app, &s, &g, DragState::Idle);
        let Some(Action::OpenStatusDetail(status)) = open.action else {
            panic!("failure banner should open its retained detail");
        };
        assert_eq!(status.detail.as_deref(), Some(reason));

        app.apply(Action::OpenStatusDetail(status));
        assert_eq!(
            app.status_detail
                .as_ref()
                .and_then(|message| message.detail.as_deref()),
            Some(reason),
            "the popup is independent of the newer footer message"
        );
    }

    #[test]
    fn picker_modal_owns_the_wheel() {
        let s = snap();
        let mut app = app_with(&s);
        app.show_model_picker = true;
        let g = geo(&app, &s);
        let out = map_mouse(wheel_down(10, 10), &app, &s, &g, DragState::Idle);
        assert_eq!(out.action, Some(Action::Down));
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
        assert!(g.dividers.is_empty(), "no drag handle while diff is zoomed");
    }

    #[test]
    fn wheel_scrolls_hovered_files_without_focus_or_selection_change() {
        let mut s = snap();
        s.files = (0..30)
            .map(|index| FileRow {
                path: format!("file-{index}.go"),
                status: "M",
                changed_symbol_count: 0,
                added_lines: 0,
                removed_lines: 0,
                symbols: Vec::new(),
                expanded: false,
                semantic: FileSemanticLoad::Ready,
            })
            .collect();
        s.diff.rows = (0..30)
            .map(|index| crate::snapshot::DiffRow::Context {
                old_ln: index + 1,
                new_ln: index + 1,
                text: format!("line {index}"),
            })
            .collect();
        let mut app = app_with(&s);
        app.focused = Pane::Diff;
        app.file_sel = 0;
        let g = geo(&app, &s);
        let files = g.files.expect("files");

        let out = map_mouse(
            wheel_down(files.x + 2, files.y + 2),
            &app,
            &s,
            &g,
            DragState::Idle,
        );
        assert_eq!(
            out.action,
            Some(Action::ScrollRegion {
                region: crate::scroll::ScrollRegionId::Files,
                offset: 3,
            })
        );
        app.apply(out.action.unwrap());
        assert_eq!(app.focused, Pane::Diff, "hover scroll never steals focus");
        assert_eq!(app.file_sel, 0, "hover scroll never changes selection");
        assert_eq!(app.files_scroll, 3);

        let g = geo(&app, &s);
        let out = map_mouse(
            wheel_up(files.x + 2, files.y + 2),
            &app,
            &s,
            &g,
            DragState::Idle,
        );
        assert!(matches!(
            out.action,
            Some(Action::ScrollRegion { offset: 0, .. })
        ));

        app.focused = Pane::Files;
        let g = geo(&app, &s);
        let diff = g.diff.expect("diff");
        let out = map_mouse(
            wheel_down(diff.x + 3, diff.y + 3),
            &app,
            &s,
            &g,
            DragState::Idle,
        );
        assert_eq!(
            out.action,
            Some(Action::ScrollRegion {
                region: crate::scroll::ScrollRegionId::Diff,
                offset: 3,
            })
        );
        app.apply(out.action.unwrap());
        assert_eq!(app.focused, Pane::Files, "diff wheel does not steal focus");
        assert_eq!(app.diff_scroll, 3);
    }

    #[test]
    fn wheel_routes_to_each_scrollable_relationship_section() {
        let mut s = snap();
        let rows = (0..12)
            .map(|index| crate::snapshot::ImpactRow {
                label: format!("symbol-{index}"),
                relation: "calls",
                changed: false,
                has_diagnostic: false,
            })
            .collect::<Vec<_>>();
        s.impact.callers.rows = rows.clone();
        s.impact.downstream.rows = rows;
        s.impact.selected_change = Some(crate::snapshot::SelectedChange {
            file: "a.go".to_string(),
            label: "Selected".to_string(),
            change: "modified",
            interpretation: "Coordinates the request and downstream work.".to_string(),
            interpretation_source: crate::snapshot::InterpretationSource::Deterministic,
        });
        let app = app_with(&s);
        let g = geo(&app, &s);

        for id in [
            crate::scroll::ScrollRegionId::Callers,
            crate::scroll::ScrollRegionId::Downstream,
        ] {
            let region = g
                .scroll_regions
                .iter()
                .find(|region| region.id == id)
                .expect("registered Impact scroll region");
            let out = map_mouse(
                wheel_down(region.rect.x + 1, region.rect.y),
                &app,
                &s,
                &g,
                DragState::Idle,
            );
            assert!(matches!(
                out.action,
                Some(Action::ScrollRegion { region, .. }) if region == id
            ));
        }
    }

    #[test]
    fn plan_boxes_open_a_modal_inspector_and_still_drag_between_grid_slots() {
        let mut s = snap();
        let mut plan = codescope_core::VisualizationPlan::new(codescope_core::Epoch(1));
        plan.intent = "A changed entry point forwards work to storage.".to_string();
        plan.forms.push(codescope_core::VizForm {
            kind: codescope_core::FormKind::Sequence,
            nodes: vec![
                codescope_core::PlanNode::new(
                    "n1",
                    "Handle",
                    codescope_core::PlanNodeChange::Modified,
                )
                .with_detail("accepts the request"),
                codescope_core::PlanNode::new(
                    "n2",
                    "Store",
                    codescope_core::PlanNodeChange::Unchanged,
                )
                .with_detail("persists the result"),
                codescope_core::PlanNode::new(
                    "n3",
                    "Publish",
                    codescope_core::PlanNodeChange::Unchanged,
                )
                .with_detail("announces the result"),
                codescope_core::PlanNode::new(
                    "n4",
                    "Observe",
                    codescope_core::PlanNodeChange::Unchanged,
                )
                .with_detail("records completion"),
            ],
            edges: vec![
                codescope_core::PlanEdge {
                    from: "n1".to_string(),
                    to: "n2".to_string(),
                    kind: codescope_core::PlanEdgeKind::Writes,
                    label: Some(
                        "writes a durable record after validating the complete request".to_string(),
                    ),
                },
                codescope_core::PlanEdge {
                    from: "n2".to_string(),
                    to: "n3".to_string(),
                    kind: codescope_core::PlanEdgeKind::Calls,
                    label: Some("publishes the stored result".to_string()),
                },
                codescope_core::PlanEdge {
                    from: "n3".to_string(),
                    to: "n4".to_string(),
                    kind: codescope_core::PlanEdgeKind::Calls,
                    label: Some("records the completion".to_string()),
                },
            ],
        });
        s.semantic.plan = Some(plan);
        s.semantic.ai_generated = true;
        s.ai = codescope_core::AiStatus::Ready {
            epoch: codescope_core::Epoch(1),
        };
        s.impact.selected_change = Some(crate::snapshot::SelectedChange {
            file: "a.go".to_string(),
            label: "Handle".to_string(),
            change: "modified",
            interpretation: "Accepts a request.".to_string(),
            interpretation_source: crate::snapshot::InterpretationSource::Deterministic,
        });
        let mut app = app_with(&s);
        let g = geo(&app, &s);
        let diagram_text = |app: &App| {
            crate::render::generated_impact_content(app, &s, 80)
                .iter()
                .map(crate::diagram::DiagramLine::text)
                .collect::<Vec<_>>()
        };
        let base_diagram = diagram_text(&app);
        let (rect, target) = g
            .plan_node_rects
            .first()
            .expect("visible node hitbox")
            .clone();

        let hover = map_mouse(moved(rect.x, rect.y), &app, &s, &g, DragState::Idle);
        assert_eq!(
            hover.action,
            Some(Action::HoverPlanNode(Some(target.clone())))
        );
        assert!(hover.dirty);
        app.apply(hover.action.unwrap());

        let steady = map_mouse(moved(rect.x, rect.y), &app, &s, &g, DragState::Idle);
        assert_eq!(steady.action, None);
        assert!(!steady.dirty, "steady motion cannot force redraws");

        app.ai_plan_scroll = 10;
        let armed = map_mouse(down(rect.x, rect.y), &app, &s, &g, DragState::Idle);
        assert!(matches!(armed.drag, DragState::PlanNode { .. }));
        let click = map_mouse(up(rect.x, rect.y), &app, &s, &g, armed.drag);
        assert_eq!(click.action, Some(Action::TogglePlanNode(target.clone())));
        app.apply(click.action.expect("node toggle"));
        assert_eq!(app.inspected_plan_node, Some(target.clone()));
        assert_eq!(
            app.ai_plan_scroll, 10,
            "the inspector does not move the canvas"
        );
        assert_eq!(
            diagram_text(&app),
            base_diagram,
            "opening details cannot change any diagram row"
        );
        assert_eq!(
            app.active_code_node().map(|node| node.id.as_str()),
            Some("n1"),
            "the inspected box pins its code links"
        );

        let close = map_mouse(down(0, 0), &app, &s, &g, DragState::Idle);
        assert_eq!(close.action, Some(Action::ClosePlanInspector));
        app.apply(close.action.expect("close inspector"));
        assert!(!app.plan_inspector_open());
        assert!(app.hovered_plan_node.is_none());

        let relationship_geometry = geo(&app, &s);
        let (relationship_rect, relationship_target) = relationship_geometry
            .plan_relationship_rects
            .first()
            .expect("relationship hitbox")
            .clone();
        let expand_relationship = map_mouse(
            down(relationship_rect.x, relationship_rect.y),
            &app,
            &s,
            &relationship_geometry,
            DragState::Idle,
        );
        assert_eq!(
            expand_relationship.action,
            Some(Action::TogglePlanRelationship(relationship_target.clone()))
        );
        app.apply(expand_relationship.action.expect("relationship toggle"));
        assert_eq!(
            app.inspected_plan_relationship,
            Some(relationship_target),
            "the full label is owned by the floating inspector"
        );
        assert_eq!(
            diagram_text(&app),
            base_diagram,
            "opening an arrow label cannot change card positions"
        );
        let close_relationship = map_mouse(
            down(relationship_rect.x, relationship_rect.y),
            &app,
            &s,
            &relationship_geometry,
            DragState::Idle,
        );
        assert_eq!(
            close_relationship.action,
            Some(Action::ClosePlanInspector),
            "clicking again closes the full arrow label"
        );
        app.apply(close_relationship.action.expect("close arrow inspector"));

        let drag_geometry = geo(&app, &s);
        let source_rect = drag_geometry
            .plan_node_rects
            .iter()
            .find(|(_, candidate)| candidate.id == "n1")
            .expect("source box")
            .0;
        let anchor_rect = drag_geometry
            .plan_node_rects
            .iter()
            .rev()
            .find(|(_, candidate)| candidate.id == "n3")
            .expect("anchor box")
            .0;
        let armed = map_mouse(
            down(source_rect.x, source_rect.y),
            &app,
            &s,
            &drag_geometry,
            DragState::Idle,
        );
        let moving = map_mouse(
            drag(
                anchor_rect
                    .x
                    .saturating_add(anchor_rect.width.saturating_sub(1)),
                anchor_rect.y,
            ),
            &app,
            &s,
            &drag_geometry,
            armed.drag,
        );
        let dropped = map_mouse(
            up(
                anchor_rect
                    .x
                    .saturating_add(anchor_rect.width.saturating_sub(1)),
                anchor_rect.y,
            ),
            &app,
            &s,
            &drag_geometry,
            moving.drag,
        );
        assert!(matches!(
            dropped.action,
            Some(Action::ReorderPlanNode { ref dragged, ref anchor, after: true })
                if dragged.id == "n1" && anchor.id == "n3"
        ));
        app.apply(dropped.action.expect("box reorder"));
        assert_eq!(
            app.plan_node_order
                .iter()
                .map(|target| target.id.as_str())
                .collect::<Vec<_>>(),
            ["n2", "n3", "n1", "n4"]
        );
        let reordered_geometry = geo(&app, &s);
        let row = |id: &str| {
            reordered_geometry
                .plan_node_rects
                .iter()
                .find(|(_, target)| target.id == id)
                .expect("reordered box")
                .0
                .y
        };
        assert!(
            row("n1") > row("n3"),
            "drop moves a box into another grid row"
        );
    }

    #[test]
    fn generated_diagram_is_renderer_placed_and_wheel_scrolls_vertically() {
        let mut s = snap();
        let mut plan = codescope_core::VisualizationPlan::new(codescope_core::Epoch(1));
        plan.intent = "A handler passes work to storage.".to_string();
        plan.forms.push(codescope_core::VizForm {
            kind: codescope_core::FormKind::Sequence,
            nodes: vec![
                codescope_core::PlanNode::new(
                    "before",
                    "Before",
                    codescope_core::PlanNodeChange::Modified,
                )
                .with_detail("old behavior"),
                codescope_core::PlanNode::new(
                    "after",
                    "After",
                    codescope_core::PlanNodeChange::Modified,
                )
                .with_detail("new behavior"),
            ],
            edges: vec![codescope_core::PlanEdge {
                from: "before".to_string(),
                to: "after".to_string(),
                kind: codescope_core::PlanEdgeKind::Contains,
                label: Some("is replaced by the safer behavior".to_string()),
            }],
        });
        s.semantic.plan = Some(plan);
        s.semantic.ai_generated = true;
        s.ai = codescope_core::AiStatus::Ready {
            epoch: codescope_core::Epoch(1),
        };
        s.impact.selected_change = Some(crate::snapshot::SelectedChange {
            file: "a.go".to_string(),
            label: "Before".to_string(),
            change: "modified",
            interpretation: "Changes behavior.".to_string(),
            interpretation_source: crate::snapshot::InterpretationSource::Deterministic,
        });
        for index in 2..10 {
            let previous = if index == 2 {
                "after".to_string()
            } else {
                format!("extra-{}", index - 1)
            };
            s.semantic.plan.as_mut().unwrap().forms[0].nodes.push(
                codescope_core::PlanNode::new(
                    format!("extra-{index}"),
                    format!("Extra step {index}"),
                    codescope_core::PlanNodeChange::Modified,
                )
                .with_detail("additional vertically placed behavior"),
            );
            s.semantic.plan.as_mut().unwrap().forms[0]
                .edges
                .push(codescope_core::PlanEdge {
                    from: previous,
                    to: format!("extra-{index}"),
                    kind: codescope_core::PlanEdgeKind::Contains,
                    label: Some("continues vertically".to_string()),
                });
        }
        let app = app_with(&s);
        let g = geo(&app, &s);
        let generated = g
            .scroll_regions
            .iter()
            .find(|region| region.id == crate::scroll::ScrollRegionId::GeneratedImpact)
            .expect("generated pane scroll region");
        assert!(generated.max_offset > 0, "tall diagram scrolls vertically");
        let x = generated.rect.x + 1;
        let y = generated.rect.y + 1;
        let wheeled = map_mouse(wheel_down(x, y), &app, &s, &g, DragState::Idle);
        assert_eq!(
            wheeled.action,
            Some(Action::ScrollRegion {
                region: crate::scroll::ScrollRegionId::GeneratedImpact,
                offset: 3,
            })
        );
    }

    #[test]
    fn dragging_diff_text_retains_selection_and_copies_exact_display_text() {
        let mut s = snap();
        s.diff.title = "a.go".to_string();
        s.diff.rows = vec![crate::snapshot::DiffRow::Context {
            old_ln: 7,
            new_ln: 7,
            text: "copy this text".to_string(),
        }];
        let mut app = app_with(&s);
        let g = geo(&app, &s);
        let frame = g.diff_copy.as_ref().expect("diff copy geometry");
        let y = frame.rect.y;
        let code_start = frame.code_starts[0].expect("source row");
        let start_x = frame.rect.x + code_start as u16;
        let end_x = start_x + 8;
        let armed = map_mouse(down(start_x, y), &app, &s, &g, DragState::Idle);
        assert!(matches!(armed.drag, DragState::DiffText { .. }));
        let preview = map_mouse(drag(end_x, y), &app, &s, &g, armed.drag);
        let selection = match preview.action.expect("selection preview") {
            Action::SetDiffSelection(selection) => selection,
            other => panic!("unexpected action: {other:?}"),
        };
        app.apply(Action::SetDiffSelection(selection));
        assert_eq!(app.diff_selection, Some(selection));
        let committed = map_mouse(up(end_x, y), &app, &s, &g, preview.drag);
        match committed.action.expect("copy on release") {
            Action::CommitDiffSelection {
                selection: final_selection,
                text,
            } => {
                assert_eq!(final_selection, selection);
                assert_eq!(text, "copy this");
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn diff_drag_clamps_the_gutter_and_copies_only_code() {
        let mut s = snap();
        s.diff.rows = vec![
            crate::snapshot::DiffRow::Context {
                old_ln: 7,
                new_ln: 7,
                text: "alpha".to_string(),
            },
            crate::snapshot::DiffRow::HunkHeader("@@ -20,1 +20,1 @@".to_string()),
            crate::snapshot::DiffRow::Add {
                new_ln: 20,
                text: "beta tail".to_string(),
            },
        ];
        let app = app_with(&s);
        let g = geo(&app, &s);
        let frame = g.diff_copy.as_ref().expect("diff copy geometry");
        let start = g
            .diff_text_point_at(frame.rect.x, frame.rect.y)
            .expect("gutter clamps to source body");
        assert_eq!(start.column, frame.code_starts[0].unwrap());
        assert!(g
            .diff_text_point_at(frame.rect.x, frame.rect.y + 1)
            .is_none());
        let end = g
            .diff_text_point_at(
                frame.rect.x + frame.code_starts[2].unwrap() as u16 + 3,
                frame.rect.y + 2,
            )
            .expect("second source row");
        let copied = g.selected_diff_text(DiffTextSelection { start, end });
        assert_eq!(copied, "alpha\nbeta");
        assert!(!copied.contains('7'));
        assert!(!copied.contains("20"));
        assert!(!copied.contains("@@"));
    }

    #[test]
    fn plain_diff_click_clears_the_retained_selection() {
        let mut s = snap();
        s.diff.rows = vec![crate::snapshot::DiffRow::Context {
            old_ln: 7,
            new_ln: 7,
            text: "select me".to_string(),
        }];
        let mut app = app_with(&s);
        let g = geo(&app, &s);
        let frame = g.diff_copy.as_ref().expect("diff copy geometry");
        let x = frame.rect.x + frame.code_starts[0].unwrap() as u16;
        let y = frame.rect.y;
        app.diff_selection = Some(DiffTextSelection {
            start: DiffTextPoint { row: 0, column: 13 },
            end: DiffTextPoint { row: 0, column: 18 },
        });

        let armed = map_mouse(down(x, y), &app, &s, &g, DragState::Idle);
        let clicked = map_mouse(up(x, y), &app, &s, &g, armed.drag);
        assert_eq!(clicked.action, Some(Action::ClearDiffSelection));
        app.apply(clicked.action.unwrap());
        assert!(app.diff_selection.is_none());

        app.diff_selection = Some(DiffTextSelection::default());
        let blank = map_mouse(
            down(frame.rect.x, frame.rect.y + 1),
            &app,
            &s,
            &g,
            DragState::Idle,
        );
        assert_eq!(blank.action, Some(Action::ClearDiffSelection));
    }
}
