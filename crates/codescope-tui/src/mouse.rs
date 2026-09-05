//! Mouse routing: pure hit-testing + drag state machine. All geometry comes from the
//! retained `UiGeometry` of the frame the user saw; no layout is recomputed here.
//!
//! Precedence (highest first): an open modal swallows everything and cancels any drag;
//! an active drag consumes Drag/Up anywhere; wheel routes to the independently scrollable
//! region under the pointer without changing focus/selection; divider drag handles
//! (horizontal wins at their intersection); a selectable file/symbol row; a pane for
//! focus; anything else is inert. Right/middle buttons and double-click are no-ops.

use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use crate::action::Action;
use crate::action::PlanNodeTarget;
use crate::action::{DiffTextPoint, DiffTextSelection};
use crate::app::{App, Pane};
use crate::divider::DividerId;
use crate::geometry::UiGeometry;
use crate::snapshot::UiSnapshot;

/// Keep one trackpad gesture on its dominant axis. Terminals report diagonal trackpad motion as
/// separate vertical and horizontal wheel events, so a vertical read can otherwise nudge a long
/// unwrapped diff sideways on every gesture.
#[derive(Debug, Default)]
pub(crate) struct WheelAxisFilter {
    locked: Option<WheelAxis>,
    last_dominant: Option<Instant>,
    pending_horizontal: Option<(HorizontalDirection, Instant)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HorizontalDirection {
    Left,
    Right,
}

const WHEEL_AXIS_LOCK_WINDOW: Duration = Duration::from_millis(180);

impl WheelAxisFilter {
    /// Whether this event should reach ordinary mouse routing.
    ///
    /// Vertical input claims a fresh gesture immediately. Horizontal input must provide two
    /// consecutive samples before claiming it, which removes incidental sideways trackpad noise;
    /// once claimed, either axis remains locked while its dominant samples keep arriving.
    pub(crate) fn allows(&mut self, kind: MouseEventKind, now: Instant) -> bool {
        let axis = match kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => WheelAxis::Vertical,
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => WheelAxis::Horizontal,
            _ => return true,
        };

        if self
            .last_dominant
            .is_some_and(|last| now.saturating_duration_since(last) > WHEEL_AXIS_LOCK_WINDOW)
        {
            self.locked = None;
            self.last_dominant = None;
        }

        if let Some(locked) = self.locked {
            if locked != axis {
                return false;
            }
            self.last_dominant = Some(now);
            return true;
        }

        if axis == WheelAxis::Vertical {
            self.locked = Some(WheelAxis::Vertical);
            self.last_dominant = Some(now);
            self.pending_horizontal = None;
            return true;
        }

        let direction = match kind {
            MouseEventKind::ScrollLeft => HorizontalDirection::Left,
            MouseEventKind::ScrollRight => HorizontalDirection::Right,
            _ => unreachable!("horizontal axis was established above"),
        };
        let confirmed = self.pending_horizontal.is_some_and(|(pending, at)| {
            pending == direction && now.saturating_duration_since(at) <= WHEEL_AXIS_LOCK_WINDOW
        });
        if confirmed {
            self.locked = Some(WheelAxis::Horizontal);
            self.last_dominant = Some(now);
            self.pending_horizontal = None;
            true
        } else {
            self.pending_horizontal = Some((direction, now));
            false
        }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

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
    /// A generated-plan box is armed for a click or a free X/Y drag.
    PlanNode {
        /// Box that was under the original press. Release always applies to this target.
        source: PlanNodeTarget,
        /// Pointer coordinate where the gesture began.
        start_x: u16,
        /// Pointer row where the gesture began.
        start_y: u16,
        /// Pointer offset from the box's top-left, retained through the whole drag.
        offset_x: u16,
        /// Pointer offset from the box's top-left, retained through the whole drag.
        offset_y: u16,
        /// Whether the pointer crossed the drag threshold.
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

    // 3. An active drag consumes Drag/Up regardless of the pointer's rectangle.
    if !matches!(drag, DragState::Idle) {
        return route_drag(event, app, geometry, drag);
    }

    match event.kind {
        K::Moved => route_hover(x, y, app, geometry),
        K::ScrollUp => route_wheel(x, y, -3, geometry),
        K::ScrollDown => route_wheel(x, y, 3, geometry),
        K::ScrollLeft => route_horizontal_wheel(x, y, -4, app, geometry),
        K::ScrollRight => route_horizontal_wheel(x, y, 4, app, geometry),
        K::Down(MouseButton::Left) => route_down(x, y, app, snap, geometry),
        // Non-left or non-primary kinds are inert.
        _ if !is_left => MouseOutcome::inert(drag),
        K::Drag(_) | K::Up(_) => MouseOutcome::inert(drag), // stray, not dragging
        _ => MouseOutcome::inert(drag),
    }
}

/// Horizontal trackpad/wheel input is hover-routed directly to an unwrapped diff. It does not
/// focus the pane, so a gesture can inspect a long line without disturbing keyboard navigation.
fn route_horizontal_wheel(x: u16, y: u16, delta: i16, app: &App, geo: &UiGeometry) -> MouseOutcome {
    if app.diff_wrap
        || geo.pane_at(x, y) != Some(Pane::Diff)
        || (delta.is_negative() && app.diff_hscroll == 0)
    {
        return MouseOutcome::inert(DragState::Idle);
    }
    MouseOutcome::action(Action::ScrollDiffHorizontal { delta }, DragState::Idle)
}

/// Motion only redraws when the semantic node target changes. A steady stream inside
/// one box is inert, so any-motion terminal tracking cannot starve snapshot delivery.
fn route_hover(x: u16, y: u16, app: &App, geo: &UiGeometry) -> MouseOutcome {
    // The relationship overlay is the topmost diagram layer. It masks a card below it
    // for hover as well as click, so linked diff emphasis cannot leak through it.
    let target = if geo.plan_relationship_overlay_at(x, y).is_some() {
        None
    } else {
        geo.plan_node_at(x, y)
    };
    if target == app.hovered_plan_node {
        MouseOutcome::inert(DragState::Idle)
    } else {
        MouseOutcome::action(Action::HoverPlanNode(target), DragState::Idle)
    }
}

/// Wheel routing is hover-only: it neither focuses a pane nor changes a row selection.
fn route_wheel(x: u16, y: u16, delta: i32, geo: &UiGeometry) -> MouseOutcome {
    // The overlay is visually above the canvas and owns wheel paging before the diagram.
    if geo.plan_relationship_overlay_at(x, y).is_some() {
        return geo
            .overlay_scrolled_offset(x, y, delta)
            .map(|offset| {
                MouseOutcome::action(
                    Action::ScrollPlanRelationshipOverlay { offset },
                    DragState::Idle,
                )
            })
            .unwrap_or_else(|| MouseOutcome::inert(DragState::Idle));
    }
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

    // 6. The full relationship overlay is visibly above the canvas and receives its own
    // second click before a covered node or route can see it.
    if let Some(target) = geo.plan_relationship_overlay_at(x, y) {
        return MouseOutcome::action(Action::TogglePlanRelationship(target), DragState::Idle);
    }

    // 7. Visible boxes take priority over an arrow routed beneath them.
    if let Some((target, offset_x, offset_y)) = geo.plan_node_drag_at(x, y) {
        return MouseOutcome {
            action: None,
            drag: DragState::PlanNode {
                source: target,
                start_x: x,
                start_y: y,
                offset_x,
                offset_y,
                moved: false,
            },
            dirty: false,
        };
    }

    // 8. Relationship arrows and clipped labels expand independently from boxes.
    if let Some(target) = geo.plan_relationship_at(x, y) {
        return MouseOutcome::action(Action::TogglePlanRelationship(target), DragState::Idle);
    }

    // 9. Diff text uses native-style drag selection. Release copies the exact retained
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

    // 9. A dedicated review marker. It wins over the containing row so review can be toggled
    // without disrupting the current selection or retargeting the diff.
    for (rect, target) in &geo.review_rects {
        if hit(*rect, x, y) {
            return MouseOutcome::action(
                Action::ToggleReviewedTarget(target.clone()),
                DragState::Idle,
            );
        }
    }

    // 10. A selectable file/symbol row.
    for (rect, phys) in &geo.file_row_rects {
        if hit(*rect, x, y) {
            let rows = app.projected_file_rows();
            if let Some(row) = rows.get(*phys) {
                if let Some(logical) = row.logical_index() {
                    return MouseOutcome::action(
                        Action::SelectFileRow {
                            logical_index: logical,
                            viewport_offset: geo.files_first_visible,
                        },
                        DragState::Idle,
                    );
                }
                // A note row: focus Files but do not select.
                return MouseOutcome::action(Action::Focus(Pane::Files), DragState::Idle);
            }
        }
    }

    // 11. A pane rectangle: focus only. Clicking blank diff space also clears any
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
            offset_x,
            offset_y,
            moved,
        } => {
            // A one-cell jitter is still a click. Once the threshold is crossed, retain
            // the original pointer-to-box offset so the card does not jump under the mouse.
            const DRAG_THRESHOLD: u16 = 1;
            let did_move = event.column.abs_diff(start_x) > DRAG_THRESHOLD
                || event.row.abs_diff(start_y) > DRAG_THRESHOLD;
            let move_action = || {
                geo.plan_position_from_screen(event.column, event.row, offset_x, offset_y)
                    .map(|position| Action::MovePlanNode {
                        target: source.clone(),
                        x: position.x,
                        y: position.y,
                    })
            };
            match event.kind {
                K::Drag(MouseButton::Left) => MouseOutcome {
                    action: (moved || did_move).then(move_action).flatten(),
                    drag: DragState::PlanNode {
                        source,
                        start_x,
                        start_y,
                        offset_x,
                        offset_y,
                        moved: moved || did_move,
                    },
                    dirty: moved || did_move,
                },
                K::Up(MouseButton::Left) => {
                    let moved = moved || did_move;
                    let action = if moved {
                        move_action()
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
    fn wheel_left(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::ScrollLeft, x, y)
    }
    fn wheel_right(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::ScrollRight, x, y)
    }

    #[test]
    fn wheel_axis_filter_keeps_vertical_gestures_from_drifting_sideways() {
        let started = Instant::now();
        let mut filter = WheelAxisFilter::default();

        assert!(filter.allows(MouseEventKind::ScrollDown, started));
        assert!(!filter.allows(
            MouseEventKind::ScrollRight,
            started + Duration::from_millis(20)
        ));
        assert!(filter.allows(
            MouseEventKind::ScrollDown,
            started + Duration::from_millis(40)
        ));

        // Once the vertical gesture has ended, two deliberate horizontal samples claim a fresh
        // gesture. The first sample is the horizontal dead-zone.
        assert!(!filter.allows(
            MouseEventKind::ScrollRight,
            started + Duration::from_millis(240)
        ));
        assert!(filter.allows(
            MouseEventKind::ScrollRight,
            started + Duration::from_millis(250)
        ));
    }

    #[test]
    fn wheel_axis_filter_holds_a_deliberate_horizontal_gesture() {
        let started = Instant::now();
        let mut filter = WheelAxisFilter::default();

        assert!(!filter.allows(MouseEventKind::ScrollLeft, started));
        assert!(filter.allows(
            MouseEventKind::ScrollLeft,
            started + Duration::from_millis(10)
        ));
        assert!(!filter.allows(
            MouseEventKind::ScrollUp,
            started + Duration::from_millis(20)
        ));
        assert!(filter.allows(
            MouseEventKind::ScrollUp,
            started + Duration::from_millis(200)
        ));
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

    fn relationship_snap() -> UiSnapshot {
        let mut snapshot = snap();
        snapshot.impact.selected_change = Some(crate::snapshot::SelectedChange {
            file: "a.go".to_string(),
            label: "Selected".to_string(),
            change: "modified",
            interpretation: "Coordinates related work.".to_string(),
            interpretation_source: crate::snapshot::InterpretationSource::Deterministic,
        });
        snapshot
            .impact
            .callers
            .rows
            .push(crate::snapshot::ImpactRow {
                label: "caller".to_string(),
                relation: "calls",
                changed: false,
                has_diagnostic: false,
            });
        snapshot
            .impact
            .downstream
            .rows
            .push(crate::snapshot::ImpactRow {
                label: "callee".to_string(),
                relation: "calls",
                changed: false,
                has_diagnostic: false,
            });
        snapshot
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
        assert_eq!(
            out.action,
            Some(Action::SelectFileRow {
                logical_index: 0,
                viewport_offset: 0,
            })
        );
        let _ = phys;
    }

    #[test]
    fn click_review_marker_toggles_that_file_without_selecting_the_row() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let (rect, target) = g.review_rects[1].clone();
        assert_eq!(target, crate::review::ReviewTarget::File("b.go".into()));
        let out = map_mouse(down(rect.x, rect.y), &app, &s, &g, DragState::Idle);
        assert_eq!(
            out.action,
            Some(Action::ToggleReviewedTarget(
                crate::review::ReviewTarget::File("b.go".into())
            ))
        );
        assert_eq!(
            app.file_sel, 0,
            "pure hit-testing cannot retarget selection"
        );
    }

    #[test]
    fn lsp_object_has_its_own_clickable_review_marker() {
        let s = snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        let (rect, target) = g.review_rects[2].clone();
        let expected = crate::review::ReviewTarget::Symbol {
            file: "b.go".into(),
            name: "B_one".into(),
            position: Some((10, 2)),
        };
        assert_eq!(target, expected);
        let out = map_mouse(down(rect.x, rect.y), &app, &s, &g, DragState::Idle);
        assert_eq!(out.action, Some(Action::ToggleReviewedTarget(expected)));
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
        assert_eq!(
            out.action,
            Some(Action::SelectFileRow {
                logical_index: 2,
                viewport_offset: 0,
            })
        );
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
        let s = relationship_snap();
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
    fn callers_downstream_sectional_uses_the_shared_drag_path() {
        let s = relationship_snap();
        let app = app_with(&s);
        let g = geo(&app, &s);
        assert!(
            g.divider(DividerId::SelectedCallers).is_none(),
            "the removed selected-change section has no hidden drag target"
        );
        let divider = DividerId::CallersDownstream;
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
    fn clicking_a_scrolled_file_keeps_the_files_viewport_in_place() {
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
        let mut app = app_with(&s);
        app.apply(Action::ScrollRegion {
            region: crate::scroll::ScrollRegionId::Files,
            offset: 8,
        });

        let before = geo(&app, &s);
        assert_eq!(before.files_first_visible, 8);
        let row = before
            .file_row_rects
            .iter()
            .find(|(_, physical)| *physical == 10)
            .map(|(rect, _)| *rect)
            .expect("third visible file row");
        let out = map_mouse(down(row.x + 2, row.y), &app, &s, &before, DragState::Idle);
        assert_eq!(
            out.action,
            Some(Action::SelectFileRow {
                logical_index: 10,
                viewport_offset: 8,
            })
        );

        app.apply(out.action.expect("file selection"));
        let after = geo(&app, &s);
        assert_eq!(app.selected_file_path(), Some("file-10.go"));
        assert_eq!(after.files_first_visible, 8, "click must not jump the list");

        // The dispatcher publishes a retargeted diff after the click. That data update must not
        // turn the retained files viewport back into selection-following mode either.
        s.diff.title = "file-10.go".to_string();
        app.update(s.clone());
        assert_eq!(geo(&app, &s).files_first_visible, 8);
    }

    #[test]
    fn horizontal_trackpad_scroll_targets_only_the_unwrapped_diff() {
        let s = snap();
        let mut app = app_with(&s);
        app.focused = Pane::Files;
        let g = geo(&app, &s);
        let diff = g.diff.expect("diff");

        let right = map_mouse(
            wheel_right(diff.x + 3, diff.y + 3),
            &app,
            &s,
            &g,
            DragState::Idle,
        );
        assert_eq!(
            right.action,
            Some(Action::ScrollDiffHorizontal { delta: 4 })
        );
        app.apply(right.action.expect("horizontal scroll"));
        assert_eq!(app.diff_hscroll, 4);
        assert_eq!(
            app.focused,
            Pane::Files,
            "trackpad scroll does not steal focus"
        );

        let left = map_mouse(
            wheel_left(diff.x + 3, diff.y + 3),
            &app,
            &s,
            &g,
            DragState::Idle,
        );
        assert_eq!(
            left.action,
            Some(Action::ScrollDiffHorizontal { delta: -4 })
        );
        app.apply(left.action.expect("horizontal scroll"));
        assert_eq!(app.diff_hscroll, 0);

        let files = g.files.expect("files");
        assert!(
            map_mouse(
                wheel_right(files.x + 2, files.y + 2),
                &app,
                &s,
                &g,
                DragState::Idle,
            )
            .action
            .is_none(),
            "horizontal gestures outside the diff are inert"
        );

        app.diff_wrap = true;
        assert!(
            map_mouse(
                wheel_right(diff.x + 3, diff.y + 3),
                &app,
                &s,
                &g,
                DragState::Idle,
            )
            .action
            .is_none(),
            "wrapped diffs have no horizontal scroll"
        );
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
    fn free_xy_drag_inline_expansion_and_overlay_preserve_base_geometry() {
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
                .with_detail("accepts the complete request and validates every input"),
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
                label: Some(
                    "writes a durable record after validating the complete request ".repeat(40),
                ),
            }],
        });
        plan.forms[0].nodes[0].expanded_detail = Some(
            "The complete description explains validation, persistence, and downstream effects in full.".to_string(),
        );
        s.semantic.plan = Some(plan);
        s.semantic.ai_generated = true;
        s.ai = codescope_core::AiStatus::Ready {
            epoch: codescope_core::Epoch(1),
        };
        let mut app = app_with(&s);
        app.diagram.sync_plan(s.semantic.plan.as_ref().unwrap());
        let g = geo(&app, &s);
        let canvas_before = g.diagram_canvas.as_ref().expect("canvas").clone();
        let (source, source_rect) = g
            .plan_node_rect_at(g.plan_node_rects[0].0.x, g.plan_node_rects[0].0.y)
            .expect("source node");
        let source_before = canvas_before
            .nodes
            .iter()
            .find(|node| node.target == source)
            .expect("source in canvas")
            .rect;
        let edge_before = canvas_before.relationships[0].path.clone();

        // A real drag updates free X/Y from the pointer offset and persists through redraw.
        let armed = map_mouse(
            down(source_rect.x + 1, source_rect.y + 1),
            &app,
            &s,
            &g,
            DragState::Idle,
        );
        let moving = map_mouse(
            drag(source_rect.x + 11, source_rect.y + 5),
            &app,
            &s,
            &g,
            armed.drag,
        );
        let action = moving.action.clone().expect("live XY move");
        assert!(matches!(action, Action::MovePlanNode { ref target, .. } if target == &source));
        app.apply(action);
        let released = map_mouse(
            up(source_rect.x + 11, source_rect.y + 5),
            &app,
            &s,
            &g,
            moving.drag,
        );
        app.apply(released.action.expect("final XY move"));
        let moved_geometry = geo(&app, &s);
        let moved_canvas = moved_geometry
            .diagram_canvas
            .as_ref()
            .expect("moved canvas");
        let moved_node = moved_canvas
            .nodes
            .iter()
            .find(|node| node.target == source)
            .unwrap();
        let expected_position = crate::diagram::DiagramPosition {
            x: source_before.x.saturating_add(10),
            y: source_before.y.saturating_add(4),
        };
        assert_eq!(
            app.diagram.positions().get(&source),
            Some(&expected_position)
        );
        assert_eq!(
            (moved_node.rect.x, moved_node.rect.y),
            (expected_position.x, expected_position.y),
            "screen movement is measured from the Canvas box origin, including annotations"
        );
        assert_ne!(
            moved_canvas.relationships[0].path, edge_before,
            "arrows read moved box geometry"
        );

        // Down/up on the original box toggles only that card's inline height at the same XY.
        let moved_screen = moved_geometry
            .plan_node_rects
            .iter()
            .find(|(_, target)| target == &source)
            .unwrap()
            .0;
        let click = map_mouse(
            down(moved_screen.x, moved_screen.y),
            &app,
            &s,
            &moved_geometry,
            DragState::Idle,
        );
        let click = map_mouse(
            up(moved_screen.x, moved_screen.y),
            &app,
            &s,
            &moved_geometry,
            click.drag,
        );
        app.apply(click.action.expect("inline toggle"));
        let expanded = geo(&app, &s);
        let expanded_canvas = expanded.diagram_canvas.as_ref().unwrap();
        let expanded_node = expanded_canvas
            .nodes
            .iter()
            .find(|node| node.target == source)
            .unwrap();
        assert_eq!(
            (expanded_node.rect.x, expanded_node.rect.y),
            (expected_position.x, expected_position.y),
            "expansion is in place"
        );
        assert!(
            expanded_node.rect.height > moved_node.rect.height,
            "complete detail expands only this box"
        );
        assert!(app.diagram.is_node_expanded(&source));

        // Overlay is a top layer only: all canvas nodes/routes remain exactly unchanged.
        let relationship = expanded_canvas.relationships[0].target.clone();
        let base_nodes = expanded_canvas.nodes.clone();
        let base_routes = expanded_canvas.relationships.clone();
        app.apply(Action::TogglePlanRelationship(relationship.clone()));
        let overlay_geometry = geo(&app, &s);
        let overlay_canvas = overlay_geometry.diagram_canvas.as_ref().unwrap();
        assert_eq!(overlay_canvas.nodes, base_nodes);
        assert_eq!(overlay_canvas.relationships, base_routes);
        let overlay = overlay_geometry
            .plan_relationship_overlay
            .clone()
            .expect("full label overlay");
        assert!(
            overlay.max_offset > 0,
            "long label must page in the viewport"
        );
        let base_scroll = app.ai_plan_scroll;
        let wheel = map_mouse(
            wheel_down(overlay.rect.x, overlay.rect.y),
            &app,
            &s,
            &overlay_geometry,
            DragState::Idle,
        );
        assert_eq!(
            wheel.action,
            Some(Action::ScrollPlanRelationshipOverlay { offset: 1 }),
            "one wheel event advances exactly one wrapped row without skipping text",
        );
        app.apply(wheel.action.expect("overlay page"));
        assert!(app.diagram.overlay_scroll() > 0);
        assert_eq!(
            app.ai_plan_scroll, base_scroll,
            "overlay wheel cannot move canvas"
        );
        let paged = geo(&app, &s);
        let paged_overlay = paged
            .plan_relationship_overlay
            .clone()
            .expect("paged overlay");
        assert!(paged_overlay.rect.height <= paged.generated_content.unwrap().height);
        // Wheel boundaries are consumed by the overlay rather than leaking to base scroll.
        app.diagram.set_overlay_scroll(0);
        let first_page = geo(&app, &s);
        let first_overlay = first_page.plan_relationship_overlay.clone().unwrap();
        assert!(
            map_mouse(
                wheel_up(first_overlay.rect.x, first_overlay.rect.y),
                &app,
                &s,
                &first_page,
                DragState::Idle
            )
            .action
            .is_none()
        );
        assert_eq!(app.ai_plan_scroll, base_scroll);
        app.diagram.set_overlay_scroll(first_overlay.max_offset);
        let last_page = geo(&app, &s);
        let last_overlay = last_page.plan_relationship_overlay.clone().unwrap();
        assert!(
            map_mouse(
                wheel_down(last_overlay.rect.x, last_overlay.rect.y),
                &app,
                &s,
                &last_page,
                DragState::Idle
            )
            .action
            .is_none()
        );
        assert_eq!(app.ai_plan_scroll, base_scroll);
        let second = map_mouse(
            down(last_overlay.rect.x, last_overlay.rect.y),
            &app,
            &s,
            &last_page,
            DragState::Idle,
        );
        assert_eq!(
            second.action,
            Some(Action::TogglePlanRelationship(last_overlay.target.clone()))
        );
        // Hover is also masked by the overlay even when it covers the moved card.
        app.hovered_plan_node = Some(source.clone());
        let hover = map_mouse(
            mouse(
                MouseEventKind::Moved,
                last_overlay.rect.x,
                last_overlay.rect.y,
            ),
            &app,
            &s,
            &overlay_geometry,
            DragState::Idle,
        );
        assert_eq!(hover.action, Some(Action::HoverPlanNode(None)));
    }

    #[test]
    fn fully_visible_relationship_is_not_an_expansion_hit_target() {
        let mut s = snap();
        let mut plan = codescope_core::VisualizationPlan::new(codescope_core::Epoch(1));
        plan.forms.push(codescope_core::VizForm {
            kind: codescope_core::FormKind::Sequence,
            nodes: vec![
                codescope_core::PlanNode::new(
                    "before",
                    "Before",
                    codescope_core::PlanNodeChange::Modified,
                ),
                codescope_core::PlanNode::new(
                    "after",
                    "After",
                    codescope_core::PlanNodeChange::Modified,
                ),
            ],
            edges: vec![codescope_core::PlanEdge {
                from: "before".into(),
                to: "after".into(),
                kind: codescope_core::PlanEdgeKind::FlowsTo,
                label: Some("then evaluates the new state".into()),
            }],
        });
        s.semantic.plan = Some(plan);
        s.semantic.ai_generated = true;
        s.ai = codescope_core::AiStatus::Ready {
            epoch: codescope_core::Epoch(1),
        };
        let app = app_with(&s);
        let geometry = geo(&app, &s);
        let content = geometry.generated_content.expect("generated viewport");
        let relationship = &geometry
            .diagram_canvas
            .as_ref()
            .expect("diagram")
            .relationships[0];
        assert!(!relationship.has_hidden_label);
        let x = content.x.saturating_add(relationship.label_rect.x);
        let y = content.y.saturating_add(
            relationship
                .label_rect
                .y
                .saturating_sub(geometry.ai_plan_scroll as u16),
        );
        assert!(geometry.plan_relationship_at(x, y).is_none());
        let click = map_mouse(down(x, y), &app, &s, &geometry, DragState::Idle);
        assert!(!matches!(
            click.action,
            Some(Action::TogglePlanRelationship(_))
        ));
    }

    #[test]
    fn drag_position_accounts_for_scroll_and_clipped_box_offset() {
        let mut s = snap();
        let mut plan = codescope_core::VisualizationPlan::new(codescope_core::Epoch(1));
        plan.forms.push(codescope_core::VizForm {
            kind: codescope_core::FormKind::Sequence,
            nodes: vec![
                codescope_core::PlanNode::new(
                    "n1",
                    "Handle",
                    codescope_core::PlanNodeChange::Modified,
                ),
                codescope_core::PlanNode::new(
                    "n2",
                    "Tail",
                    codescope_core::PlanNodeChange::Unchanged,
                ),
            ],
            edges: vec![],
        });
        s.semantic.plan = Some(plan);
        s.semantic.ai_generated = true;
        s.ai = codescope_core::AiStatus::Ready {
            epoch: codescope_core::Epoch(1),
        };
        let mut app = app_with(&s);
        app.diagram.move_node(
            PlanNodeTarget {
                form: 0,
                id: "n1".into(),
            },
            crate::diagram::DiagramPosition { x: 4, y: 20 },
        );
        // Make the card begin above the visible viewport, so its screen rect is clipped.
        app.diagram.move_node(
            PlanNodeTarget {
                form: 0,
                id: "n2".into(),
            },
            crate::diagram::DiagramPosition { x: 4, y: 60 },
        );
        app.ai_plan_scroll = 21;
        let g = geo(&app, &s);
        assert!(
            g.ai_plan_scroll > 0,
            "fixture must exercise real canvas scrolling"
        );
        let n1_y = g
            .diagram_canvas
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|node| node.target.id == "n1")
            .unwrap()
            .rect
            .y;
        assert!(
            usize::from(n1_y) < g.ai_plan_scroll,
            "n1 top must be above viewport"
        );
        let (target, clipped) = g
            .plan_node_rect_at(g.plan_node_rects[0].0.x, g.plan_node_rects[0].0.y)
            .unwrap();
        let armed = map_mouse(down(clipped.x, clipped.y), &app, &s, &g, DragState::Idle);
        let moved = map_mouse(drag(clipped.x + 3, clipped.y + 2), &app, &s, &g, armed.drag);
        assert_eq!(
            moved.action,
            Some(Action::MovePlanNode {
                target,
                x: 7,
                y: 22
            }),
            "screen conversion retains the hidden top-row grab offset"
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
        assert!(
            g.diff_text_point_at(frame.rect.x, frame.rect.y + 1)
                .is_none()
        );
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
