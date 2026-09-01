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
use crate::app::{App, Pane};
use crate::divider::DividerId;
use crate::geometry::UiGeometry;
use crate::snapshot::UiSnapshot;

/// The drag state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

    // 3. An active drag consumes Drag/Up regardless of the pointer's rectangle.
    if let DragState::Dragging { .. } = drag {
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
fn route_down(x: u16, y: u16, _app: &App, snap: &UiSnapshot, geo: &UiGeometry) -> MouseOutcome {
    // 4. The nonempty status-message segment is a chrome action, not a pane. Its exact
    // rectangle excludes the right-justified token/path fields.
    if geo.status_at(x, y) && !snap.status.text.is_empty() {
        return MouseOutcome::action(Action::ToggleStatusDetail, DragState::Idle);
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

    // 6. A generated-plan node: click pins/unpins its detail inspector. Hover itself
    // remains transient and never dispatches backend work.
    if let Some(target) = geo.plan_node_at(x, y) {
        return MouseOutcome::action(Action::TogglePlanNode(target), DragState::Idle);
    }

    // 7. A selectable file/symbol row.
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

    // 8. A pane rectangle: focus only.
    if let Some(pane) = geo.pane_at(x, y) {
        return MouseOutcome::action(Action::Focus(pane), DragState::Idle);
    }

    MouseOutcome::inert(DragState::Idle)
}

/// Drag/Up routing while a drag is active.
fn route_drag(event: MouseEvent, _app: &App, geo: &UiGeometry, drag: DragState) -> MouseOutcome {
    use crossterm::event::{MouseButton, MouseEventKind as K};
    let DragState::Dragging {
        divider,
        start_x,
        start_y,
        start_extent,
        moved,
    } = drag
    else {
        return MouseOutcome::inert(drag);
    };
    // Cancel if the handle is gone (zoom/modal removed it) — never resize a hidden pane.
    let Some(handle) = geo.divider(divider) else {
        return MouseOutcome::inert(DragState::Idle);
    };
    // Axis and leading/trailing direction live in the divider abstraction.
    let extent_action = |x: u16, y: u16| Action::ResizeDivider {
        divider,
        extent: handle.resized_extent_from(start_extent, start_x, start_y, x, y),
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
                DragState::Idle => unreachable!(),
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
    fn wheel_routes_to_each_scrollable_impact_section() {
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
            crate::scroll::ScrollRegionId::GeneratedImpact,
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
    fn plan_node_motion_redraws_only_on_target_change_and_click_expands() {
        let mut s = snap();
        let mut plan = codescope_core::VisualizationPlan::new(codescope_core::Epoch(1), "focus?");
        plan.title = "Request path".to_string();
        plan.intent = "A changed entry point forwards work to storage.".to_string();
        plan.forms.push(codescope_core::VizForm {
            kind: codescope_core::FormKind::Sequence,
            title: "flow".to_string(),
            summary: String::new(),
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
            ],
            edges: vec![codescope_core::PlanEdge {
                from: "n1".to_string(),
                to: "n2".to_string(),
                kind: codescope_core::PlanEdgeKind::Writes,
                label: Some("writes record".to_string()),
            }],
        });
        s.semantic.plan = Some(plan);
        s.semantic.ai_generated = true;
        s.impact.selected_change = Some(crate::snapshot::SelectedChange {
            file: "a.go".to_string(),
            label: "Handle".to_string(),
            change: "modified",
            interpretation: "Accepts a request.".to_string(),
            interpretation_source: crate::snapshot::InterpretationSource::Deterministic,
        });
        let mut app = app_with(&s);
        let g = geo(&app, &s);
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
        let click = map_mouse(down(rect.x, rect.y), &app, &s, &g, DragState::Idle);
        assert_eq!(click.action, Some(Action::TogglePlanNode(target.clone())));
        app.apply(click.action.unwrap());
        assert_eq!(app.expanded_plan_node, Some(target));
        assert_eq!(app.ai_plan_scroll, 0, "pinned detail strip is revealed");

        let leave = map_mouse(moved(0, 0), &app, &s, &g, DragState::Idle);
        assert_eq!(leave.action, Some(Action::HoverPlanNode(None)));
        app.apply(leave.action.unwrap());
        assert!(app.hovered_plan_node.is_none());
        assert_eq!(
            app.active_code_node().map(|node| node.id.as_str()),
            Some("n1"),
            "expanded details pin code links after hover leaves"
        );
    }
}
