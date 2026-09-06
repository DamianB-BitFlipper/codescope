//! The tokio run loop: terminal events + snapshot updates. Terminal init/restore is the CALLER's responsibility
//! (`ratatui::init()` / `ratatui::restore()`); this only drives the loop.

use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use codescope_core::ChangeScope;

use crate::action::{Action, ExternalControl, map_key};
use crate::app::{App, Pane};
use crate::render::render;
use crate::snapshot::UiSnapshot;

/// Run the TUI until the user quits.
///
/// - `rx` carries new snapshots from the dispatcher (watch = latest-wins).
/// - `tx` receives Actions that require work the TUI cannot do itself
///   (RefreshGit, model/base selection, scope changes); view-only actions are applied
///   locally.
/// - `control_rx` receives typed actions and correlation metadata from a local control-protocol
///   server.
pub async fn run(
    terminal: &mut DefaultTerminal,
    mut app: App,
    mut rx: watch::Receiver<UiSnapshot>,
    tx: mpsc::Sender<Action>,
    mut control_rx: mpsc::Receiver<ExternalControl>,
) -> std::io::Result<()> {
    // Defensive: ensure the terminal is in raw mode so key events are delivered. Idempotent;
    // ratatui::init already does this, but a missed enable leaves the app unresponsive.
    let _ = crossterm::terminal::enable_raw_mode();
    let mut events = EventStream::new();
    let mut pending_scope = PendingScope::default();
    let mut selection = SelectionTracker::default();

    // The frame plan the user last saw; mouse hit-testing reads only this (never
    // recomputed). Rebuilt on every draw.
    let mut last_geometry = crate::geometry::UiGeometry::default();
    let mut drag = crate::mouse::DragState::Idle;
    let mut wheel_axis = crate::mouse::WheelAxisFilter::default();
    // Drag setters are previews: coalesce any number of motion samples into one write on
    // mouse-up (or flush when the interaction is cancelled/exits).
    let mut preferences_dirty = false;
    let mut control_open = true;
    // Draw only when state changed. Mouse `Moved`/no-op events do not force a redraw, and a
    // steady mouse stream cannot starve snapshot delivery (review 24 B2).
    let mut dirty = true; // first frame draws
    loop {
        if dirty {
            terminal.draw(|frame| {
                let geo = crate::geometry::UiGeometry::build(frame.area(), &app, &app.snapshot);
                render(frame, &app, &app.snapshot);
                last_geometry = geo;
            })?;
            app.sync_generated_viewport(
                last_geometry.ai_plan_scroll,
                last_geometry.ai_plan_max_scroll,
            );
            dirty = false;
        }
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            command = control_rx.recv(), if control_open => {
                match command {
                    Some(control) => {
                        let before = telemetry_state(&app);
                        let ExternalControl {
                            command_id,
                            operation,
                            view_id,
                            action,
                        } = control;
                        let action_name = format!("{action:?}");
                        preferences_dirty |= dispatch(
                            &mut app,
                            action,
                            &tx,
                            &mut pending_scope,
                            &mut selection,
                        )
                        .await;
                        codescope_telemetry::record_with_origin(
                            codescope_telemetry::TelemetryOrigin::ExternalAgent,
                            "input.control",
                            json!({
                                "command_id": command_id,
                                "operation": operation,
                                "view_id": view_id,
                                "action": action_name,
                                "state_before": before,
                                "state_after": telemetry_state(&app),
                            }),
                        );
                        dirty = true;
                    }
                    None => control_open = false,
                }
            }
            // Keyboard/mouse input first — never starve interactivity.
            maybe = events.next() => {
                match maybe {
                    Some(Ok(Event::Key(key))) => {
                        let action = map_key(key, &app);
                        let before = telemetry_state(&app);
                        let action_name = format!("{action:?}");
                        preferences_dirty |= dispatch(
                            &mut app,
                            action,
                            &tx,
                            &mut pending_scope,
                            &mut selection,
                        )
                        .await;
                        codescope_telemetry::record_with_origin(
                            codescope_telemetry::TelemetryOrigin::User,
                            "input.key",
                            json!({
                                "code": format!("{:?}", key.code),
                                "modifiers": format!("{:?}", key.modifiers),
                                "kind": format!("{:?}", key.kind),
                                "mapped_action": action_name,
                                "state_before": before,
                                "state_after": telemetry_state(&app),
                            }),
                        );
                        if app.should_quit && preferences_dirty {
                            persist_preferences(&app, &tx).await;
                            preferences_dirty = false;
                        }
                        dirty = true;
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        // Route against the retained frame plan. Row clicks reuse the
                        // existing SelectionTracker: the returned action is dispatched
                        // through the same path as a keypress.
                        let previous_drag = drag.clone();
                        let before = telemetry_state(&app);
                        let wheel_allowed = wheel_axis.allows(
                            mouse.kind,
                            std::time::Instant::now(),
                        );
                        let wheel_axis_filtered = !wheel_allowed;
                        let outcome = if wheel_allowed {
                            crate::mouse::map_mouse(
                                mouse,
                                &app,
                                &app.snapshot,
                                &last_geometry,
                                drag,
                            )
                        } else {
                            crate::mouse::MouseOutcome {
                                action: None,
                                drag,
                                dirty: false,
                            }
                        };
                        let action_name = outcome.action.as_ref().map(|action| format!("{action:?}"));
                        let next_drag = format!("{:?}", outcome.drag);
                        let record_mouse = !matches!(
                            mouse.kind,
                            crossterm::event::MouseEventKind::Moved
                        ) || outcome.action.is_some();
                        drag = outcome.drag;
                        dirty |= outcome.dirty;
                        if let Some(action) = outcome.action {
                            preferences_dirty |= dispatch(
                                &mut app,
                                action,
                                &tx,
                                &mut pending_scope,
                                &mut selection,
                            )
                            .await;
                        }
                        if record_mouse {
                            codescope_telemetry::record_with_origin(
                                codescope_telemetry::TelemetryOrigin::User,
                                "input.mouse",
                                json!({
                                    "kind": format!("{:?}", mouse.kind),
                                    "button_modifiers": format!("{:?}", mouse.modifiers),
                                    "column": mouse.column,
                                    "row": mouse.row,
                                    "mapped_action": action_name,
                                    "wheel_axis_filtered": wheel_axis_filtered,
                                    "drag_after": next_drag,
                                    "state_before": before,
                                    "state_after": telemetry_state(&app),
                                }),
                            );
                        }
                        if !matches!(previous_drag, crate::mouse::DragState::Idle)
                            && matches!(drag, crate::mouse::DragState::Idle)
                            && preferences_dirty
                        {
                            persist_preferences(&app, &tx).await;
                            preferences_dirty = false;
                        }
                    }
                    Some(Ok(Event::Resize(columns, rows))) => {
                        // A resize invalidates the retained geometry, hover target, and any
                        // drag anchored to it. The next draw rebuilds the whole frame plan.
                        drag = crate::mouse::DragState::Idle;
                        wheel_axis.reset();
                        app.apply(Action::HoverPlanNode(None));
                        if preferences_dirty {
                            persist_preferences(&app, &tx).await;
                            preferences_dirty = false;
                        }
                        dirty = true;
                        codescope_telemetry::record(
                            "input.resize",
                            json!({
                                "columns": columns,
                                "rows": rows,
                                "state": telemetry_state(&app),
                            }),
                        );
                    }
                    // Event stream ended or errored: the loop cannot stay interactive; exit
                    // cleanly rather than hot-loop on a permanently-ready source.
                    Some(Ok(_)) | Some(Err(_)) | None => {
                        if preferences_dirty {
                            persist_preferences(&app, &tx).await;
                        }
                        return Ok(());
                    },
                }
            }
            // A new repository/analysis state arrived.
            changed = rx.changed() => {
                if changed.is_err() {
                    // Dispatcher dropped the sender: nothing more will arrive; stop.
                    if preferences_dirty {
                        persist_preferences(&app, &tx).await;
                    }
                    return Ok(());
                }
                let mut snapshot = rx.borrow_and_update().clone();
                pending_scope.reconcile(&mut snapshot);
                app.update(snapshot);
                // The new state may have moved the selection (clamp / re-expanded rows):
                // keep the dispatcher's diff + relations aimed at it.
                selection.sync(&app, &tx).await;
                codescope_telemetry::record("ui.snapshot", telemetry_state(&app));
                dirty = true;
            }
        }
    }
}

/// A scope the user picked that the dispatcher has not yet confirmed.
///
/// The dispatcher owns the scope: scope actions are forwarded to it and every published
/// snapshot carries its scope. Between the keypress and the dispatcher's next publish, a
/// snapshot computed *before* the dispatcher saw the action can still arrive; without this
/// guard it would carry the old scope and `App::update` would flip the label back (the
/// "scope resets to branch on every refresh" bug).
#[derive(Debug, Default)]
struct PendingScope(Option<ChangeScope>);

impl PendingScope {
    /// Record a user-picked scope (already applied locally and forwarded).
    fn record(&mut self, scope: ChangeScope) {
        self.0 = Some(scope);
    }

    /// Reconcile an incoming snapshot with the pending pick: a snapshot that confirms it
    /// clears the pending state (the dispatcher is the source of truth again); a stale
    /// one is patched to the user's scope so the choice cannot flicker back.
    fn reconcile(&mut self, snapshot: &mut UiSnapshot) {
        match self.0 {
            Some(scope) if snapshot.scope == scope => self.0 = None,
            Some(scope) => snapshot.scope = scope,
            None => {}
        }
    }
}

/// Resolved changed-tree selection sent to the dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectionTarget {
    Empty,
    Directory(String),
    File {
        path: String,
        symbol: Option<(String, u32, u32)>,
    },
}

/// Tracks what the dispatcher was last told is selected, so [`Action::SelectionChanged`]
/// is sent only when the selection actually moves (a selection send per keypress would be
/// coalesced by the dispatcher anyway, but silence is cheaper). The dispatcher treats each
/// send as the new source of truth for the diff + relations panes.
#[derive(Debug, Default)]
struct SelectionTracker(Option<SelectionTarget>);

impl SelectionTracker {
    /// Send [`Action::SelectionChanged`] when the resolved files-pane selection differs
    /// from what the dispatcher was last told. The files-pane selection is well-defined
    /// regardless of focus, so this also runs after snapshot updates (a refresh can clamp
    /// or re-expand the row under the cursor).
    async fn sync(&mut self, app: &App, tx: &mpsc::Sender<Action>) {
        let target = selection_target(app);
        if self.0.as_ref() == Some(&target) {
            return;
        }
        // The dispatcher starts with no selection; the first sync only reports once a real
        // row exists. (An empty list AFTER a real selection IS reported, so the dispatcher
        // can drop the now-gone target.)
        if self.0.is_none() && target == SelectionTarget::Empty {
            self.0 = Some(target);
            return;
        }
        self.0 = Some(target.clone());
        codescope_telemetry::record(
            "ui.selection",
            json!({ "selection": selection_target_value(&target) }),
        );
        let action = match target {
            SelectionTarget::Empty => Action::SelectionChanged {
                file: None,
                symbol: None,
            },
            SelectionTarget::Directory(directory) => {
                Action::DirectorySelectionChanged { directory }
            }
            SelectionTarget::File { path, symbol } => Action::SelectionChanged {
                file: Some(path),
                symbol,
            },
        };
        let _ = tx.send(action).await;
    }
}

fn selection_target_value(target: &SelectionTarget) -> Value {
    match target {
        SelectionTarget::Empty => Value::Null,
        SelectionTarget::Directory(path) => json!({ "kind": "directory", "path": path }),
        SelectionTarget::File { path, symbol } => json!({
            "kind": if symbol.is_some() { "symbol" } else { "file" },
            "path": path,
            "symbol": symbol.as_ref().map(|(name, line, column)| json!({
                "name": name,
                "line": line,
                "column": column,
            })),
        }),
    }
}

fn telemetry_state(app: &App) -> Value {
    let review_progress = app.review_progress();
    let selected_review = match app.selected_summary_key() {
        Some(crate::snapshot::AiSummaryKey::Directory(path)) => {
            Some(app.review_state(&crate::review::ReviewTarget::Directory(path)))
        }
        Some(crate::snapshot::AiSummaryKey::File(path)) => {
            Some(app.review_state(&crate::review::ReviewTarget::File(path)))
        }
        Some(crate::snapshot::AiSummaryKey::Symbol {
            file,
            name,
            position,
        }) => Some(app.review_state(&crate::review::ReviewTarget::Symbol {
            file,
            name,
            position,
        })),
        None => None,
    };
    json!({
        "epoch": app.snapshot.epoch.get(),
        "scope": format!("{:?}", app.snapshot.scope).to_ascii_lowercase(),
        "focused_pane": format!("{:?}", app.focused).to_ascii_lowercase(),
        "selection": selection_target_value(&selection_target(app)),
        "focused_diff": {
            "file": app.snapshot.diff.title,
            "symbol": app.snapshot.diff.focused_symbol,
            "hunk": app.current_hunk,
        },
        "review": {
            "available": review_progress.available,
            "reviewed_files": review_progress.reviewed,
            "total_files": review_progress.total,
            "selected_state": selected_review.map(crate::review::ReviewState::as_str),
        },
        "scroll": {
            "files": app.files_scroll,
            "diff_vertical": app.diff_scroll,
            "diff_vertical_alignment": format!("{:?}", app.diff_scroll_alignment).to_ascii_lowercase(),
            "diff_horizontal": app.diff_hscroll,
            "callers": app.callers_scroll,
            "downstream": app.downstream_scroll,
            "generated_impact": app.ai_plan_scroll,
            "generated_impact_follow_tail": app.ai_activity_follows_tail(),
        },
        "view": {
            "zoomed": app.zoomed,
            "diff_wrap": app.diff_wrap,
            "help_open": app.show_help,
            "status_detail_open": app.status_detail.is_some(),
            "model_picker_open": app.show_model_picker,
            "base_picker_open": app.show_base_picker,
        },
        "user_input": {
            "model_query": app.model_query,
            "base_query": app.base_query,
            "diff_selection": app.diff_selection.map(|selection| json!({
                "start": { "row": selection.start.row, "column": selection.start.column },
                "end": { "row": selection.end.row, "column": selection.end.column },
            })),
        },
    })
}

/// Resolve the flattened files-pane selection to its `(file, symbol)` target: a file row
/// yields `(Some(path), None)`; a symbol row with a position yields the symbol too (an
/// unmapped symbol row degrades to its file). `(None, None)` when the file list is empty.
fn selection_target(app: &App) -> SelectionTarget {
    match app.selected_summary_key() {
        Some(crate::snapshot::AiSummaryKey::Directory(path)) => SelectionTarget::Directory(path),
        Some(crate::snapshot::AiSummaryKey::File(path)) => {
            SelectionTarget::File { path, symbol: None }
        }
        Some(crate::snapshot::AiSummaryKey::Symbol {
            file,
            name,
            position,
        }) => SelectionTarget::File {
            path: file,
            symbol: position.map(|(line, col)| (name, line, col)),
        },
        None => SelectionTarget::Empty,
    }
}

/// Apply view-only actions locally and forward work actions to the dispatcher.
async fn dispatch(
    app: &mut App,
    action: Action,
    tx: &mpsc::Sender<Action>,
    pending_scope: &mut PendingScope,
    selection: &mut SelectionTracker,
) -> bool {
    let before = app.preferences();
    let drag_preview = matches!(&action, Action::ResizeDivider { .. });
    match action {
        Action::Activate => {
            // If the files-pane selection is a symbol row with a position, forward a
            // SelectSymbol so the dispatcher lazily expands its callers/callees.
            if let Some(sel) = selected_symbol(app) {
                let _ = tx.send(sel).await;
            }
            app.apply(Action::Activate);
        }
        Action::AiSettingsSelected {
            model,
            reasoning_effort,
        } => {
            // The modal sends an empty name; resolve it from the selection in the
            // filtered (visible) list.
            let model = if model.is_empty() {
                app.filtered_models()
                    .get(app.model_sel)
                    .map(|s| (*s).to_string())
                    .or_else(|| {
                        let typed = app.model_query.trim();
                        (!typed.is_empty()).then(|| typed.to_string())
                    })
                    .unwrap_or_else(|| app.snapshot.ai_model.clone())
            } else {
                model
            };
            let reasoning_effort = if reasoning_effort.is_empty() {
                app.selected_reasoning_effort().to_string()
            } else {
                reasoning_effort
            };
            if !model.is_empty() {
                let _ = tx
                    .send(Action::AiSettingsSelected {
                        model,
                        reasoning_effort,
                    })
                    .await;
            }
            app.show_model_picker = false;
            app.model_query.clear();
        }
        Action::ReasoningEffortPrevious | Action::ReasoningEffortNext => {
            app.apply(action);
        }
        Action::RefreshGit | Action::GenerateAi | Action::ToggleAiGenerationMode => {
            let _ = tx.send(action).await;
        }
        Action::AgentFocus(target) => {
            // Symbol rows only exist in the projection while their file is expanded.
            // Keep the dispatcher-owned expansion bit aligned with the optimistic local
            // tree update before the ordinary selection tracker reports the new target.
            if let crate::snapshot::AiSummaryKey::Symbol { file, .. } = &target {
                let _ = tx
                    .send(Action::SetFileExpanded {
                        path: file.clone(),
                        expanded: true,
                    })
                    .await;
            }
            app.apply(Action::AgentFocus(target));
        }
        Action::AgentDiagramInspect { .. }
        | Action::AgentDiagram { .. }
        | Action::AgentDiagramRejected { .. } => {
            let _ = tx.send(action).await;
        }
        Action::ClearDiffSelection => {
            app.apply(Action::ClearDiffSelection);
            let _ = tx.send(Action::SetAgentDiffSelection(None)).await;
        }
        Action::CommitDiffSelection {
            selection,
            ref text,
        } => {
            app.apply(Action::CommitDiffSelection {
                selection,
                text: text.clone(),
            });
            copy_osc52(text);
            let context = crate::snapshot::SelectedDiffContext {
                file: app.snapshot.diff.title.clone(),
                text: text.clone(),
                truncated: false,
            };
            let _ = tx.send(Action::SetAgentDiffSelection(Some(context))).await;
        }
        Action::ToggleWrap | Action::ResetHScroll => {
            app.apply(action);
            let _ = tx.send(Action::SetAgentDiffSelection(None)).await;
        }
        Action::ScrollDiffHorizontal { .. } => {
            let previous = app.diff_hscroll;
            app.apply(action);
            if app.diff_hscroll != previous {
                let _ = tx.send(Action::SetAgentDiffSelection(None)).await;
            }
        }
        Action::SetFileExpanded { .. } => {
            // Optimistic local apply (responsive expand/collapse), then the dispatcher
            // reconciles: it owns expansion state. The path is part of
            // the command, so a coalesced SelectionChanged cannot retarget it.
            app.apply(action.clone());
            let _ = tx.send(action).await;
        }
        Action::SetDirectoryExpanded { .. } => {
            // Directory disclosure is pure local view state and must not start work.
            app.apply(action);
        }
        // Space/Left/Right are expansion aliases and resolve the same targeted command.
        Action::ToggleExpand | Action::Collapse | Action::Expand if app.focused == Pane::Files => {
            if let Some(cmd) = app.tree_toggle_action() {
                let forward = matches!(cmd, Action::SetFileExpanded { .. });
                app.apply(cmd.clone());
                if forward {
                    let _ = tx.send(cmd).await;
                }
            }
        }
        Action::ModelPicker => {
            // Toggle locally, and ask the dispatcher to fetch the model list on open.
            app.apply(Action::ModelPicker);
            if app.show_model_picker {
                let _ = tx.send(Action::ModelPicker).await;
            }
        }
        Action::BaseSelected(name) => {
            // The modal sends an empty name; resolve it from the selection in the
            // filtered (visible) list.
            let name = if name.is_empty() {
                app.filtered_bases()
                    .get(app.base_sel)
                    .map(|s| (*s).to_string())
                    .unwrap_or_default()
            } else {
                name
            };
            if !name.is_empty() {
                let _ = tx.send(Action::BaseSelected(name)).await;
            }
            app.show_base_picker = false;
            app.base_query.clear();
        }
        Action::BasePicker => {
            // Toggle locally, and ask the dispatcher to fetch base candidates on open.
            app.apply(Action::BasePicker);
            if app.show_base_picker {
                let _ = tx.send(Action::BasePicker).await;
            }
        }
        // The dispatcher owns the scope: apply locally for instant feedback (selection
        // reset + label), remember the pick until a snapshot confirms it, and forward so
        // every future publish carries the user's scope.
        scope @ (Action::ScopeStaged
        | Action::ScopeUnstaged
        | Action::ScopeBranch
        | Action::ScopeBranchWorking
        | Action::ScopeWorking
        | Action::ScopeCycle
        | Action::ScopeCycleReverse) => {
            app.apply(scope.clone());
            pending_scope.record(app.snapshot.scope);
            let _ = tx.send(scope).await;
        }
        other => app.apply(other),
    }
    let preferences_changed = app.preferences() != before;
    if preferences_changed && !drag_preview {
        persist_preferences(app, tx).await;
    }
    // Navigation-driven panes: whatever the action did, tell the dispatcher where the
    // files-pane selection landed (sends only on change).
    selection.sync(app, tx).await;
    preferences_changed && drag_preview
}

/// Copy without a platform dependency. OSC 52 is understood by modern local terminals
/// and multiplexers; unsupported terminals safely ignore the control sequence.
fn copy_osc52(text: &str) {
    use std::io::Write as _;
    let encoded = base64(text.as_bytes());
    let _ = write!(std::io::stdout(), "\x1b]52;c;{encoded}\x07");
    let _ = std::io::stdout().flush();
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[usize::from(a >> 2)] as char);
        out.push(TABLE[usize::from((a & 0x03) << 4 | b >> 4)] as char);
        out.push(if chunk.len() > 1 {
            TABLE[usize::from((b & 0x0f) << 2 | c >> 6)] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[usize::from(c & 0x3f)] as char
        } else {
            '='
        });
    }
    out
}

/// Forward one coalesced global-preference snapshot to the dispatcher/config writer.
async fn persist_preferences(app: &App, tx: &mpsc::Sender<Action>) {
    let _ = tx
        .send(Action::PersistUiPreferences(app.preferences()))
        .await;
}

/// Resolve the currently selected symbol row to a [`Action::SelectSymbol`], if the files-pane
/// selection is on an expandable symbol (one with a position).
fn selected_symbol(app: &App) -> Option<Action> {
    if app.focused != crate::app::Pane::Files {
        return None;
    }
    let crate::snapshot::AiSummaryKey::Symbol {
        file,
        name,
        position: Some((line, col)),
    } = app.selected_summary_key()?
    else {
        return None;
    };
    Some(Action::SelectSymbol {
        file,
        name,
        line,
        col,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_payload_uses_standard_base64() {
        assert_eq!(base64(b"copy this"), "Y29weSB0aGlz");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
    }

    #[test]
    fn telemetry_state_names_selection_input_and_every_scroll_axis() {
        let mut app = App::new();
        app.update(UiSnapshot {
            epoch: codescope_core::Epoch(7),
            files: vec![crate::snapshot::FileRow {
                semantic: crate::snapshot::FileSemanticLoad::Ready,
                path: "src/api.rs".into(),
                status: "M",
                changed_symbol_count: 0,
                added_lines: 1,
                removed_lines: 0,
                symbols: Vec::new(),
                expanded: false,
            }],
            diff: crate::snapshot::DiffPane {
                title: "src/api.rs".into(),
                ..crate::snapshot::DiffPane::default()
            },
            ..UiSnapshot::default()
        });
        app.file_sel = 1; // synthesized `src` directory is row zero
        app.files_scroll = 1;
        app.diff_scroll = 2;
        app.diff_hscroll = 3;
        app.callers_scroll = 4;
        app.downstream_scroll = 5;
        app.ai_plan_scroll = 6;
        app.model_query = "typed-model".into();

        let state = telemetry_state(&app);
        assert_eq!(state["epoch"], 7);
        assert_eq!(state["selection"]["path"], "src/api.rs");
        assert_eq!(state["focused_diff"]["file"], "src/api.rs");
        assert_eq!(state["review"]["available"], false);
        assert_eq!(state["review"]["reviewed_files"], 0);
        assert_eq!(state["review"]["total_files"], 1);
        assert_eq!(state["review"]["selected_state"], "unreviewed");
        assert_eq!(state["scroll"]["files"], 1);
        assert_eq!(state["scroll"]["diff_vertical"], 2);
        assert_eq!(state["scroll"]["diff_horizontal"], 3);
        assert_eq!(state["scroll"]["callers"], 4);
        assert_eq!(state["scroll"]["downstream"], 5);
        assert_eq!(state["scroll"]["generated_impact"], 6);
        assert_eq!(state["scroll"]["generated_impact_follow_tail"], false);
        assert_eq!(state["user_input"]["model_query"], "typed-model");
    }

    #[tokio::test]
    async fn ai_generation_controls_are_forwarded_to_the_dispatcher() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut app = App::new();
        let mut pending = PendingScope::default();
        let mut selection = SelectionTracker::default();

        dispatch(
            &mut app,
            Action::GenerateAi,
            &tx,
            &mut pending,
            &mut selection,
        )
        .await;
        assert_eq!(rx.recv().await, Some(Action::GenerateAi));

        dispatch(
            &mut app,
            Action::ToggleAiGenerationMode,
            &tx,
            &mut pending,
            &mut selection,
        )
        .await;
        assert_eq!(rx.recv().await, Some(Action::ToggleAiGenerationMode));
    }

    #[tokio::test]
    async fn committed_diff_selection_is_published_for_agent_context() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut app = App::new();
        app.update(UiSnapshot {
            diff: crate::snapshot::DiffPane {
                title: "src/api.rs".to_string(),
                ..crate::snapshot::DiffPane::default()
            },
            ..UiSnapshot::default()
        });
        let mut pending = PendingScope::default();
        let mut selection = SelectionTracker::default();
        let selected = crate::action::DiffTextSelection::default();

        dispatch(
            &mut app,
            Action::CommitDiffSelection {
                selection: selected,
                text: "queue.push(request);".to_string(),
            },
            &tx,
            &mut pending,
            &mut selection,
        )
        .await;
        assert_eq!(
            rx.recv().await,
            Some(Action::SetAgentDiffSelection(Some(
                crate::snapshot::SelectedDiffContext {
                    file: "src/api.rs".to_string(),
                    text: "queue.push(request);".to_string(),
                    truncated: false,
                }
            )))
        );
        assert_eq!(app.diff_selection, Some(selected));

        dispatch(
            &mut app,
            Action::ClearDiffSelection,
            &tx,
            &mut pending,
            &mut selection,
        )
        .await;
        assert_eq!(rx.recv().await, Some(Action::SetAgentDiffSelection(None)));
        assert!(app.diff_selection.is_none());
    }

    /// Bug 2 regression: a scope set via action must persist across refresh snapshots,
    /// including one published before the dispatcher processed the forwarded action.
    #[tokio::test]
    async fn user_scope_persists_across_refresh_snapshots() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = App::new();
        let mut pending = PendingScope::default();

        // Set the scope via action: applied locally AND forwarded to the dispatcher.
        dispatch(
            &mut app,
            Action::ScopeStaged,
            &tx,
            &mut pending,
            &mut SelectionTracker::default(),
        )
        .await;
        assert_eq!(app.snapshot.scope, ChangeScope::Staged);
        assert_eq!(
            rx.recv().await,
            Some(Action::ScopeStaged),
            "scope actions must be forwarded to the dispatcher (it owns the scope)"
        );

        // A snapshot published before the dispatcher saw the action still carries the old
        // scope; applying it must not reset the user's pick (the flicker/reset bug).
        let mut stale = UiSnapshot {
            scope: ChangeScope::Branch,
            ..UiSnapshot::default()
        };
        pending.reconcile(&mut stale);
        app.update(stale);
        assert_eq!(app.snapshot.scope, ChangeScope::Staged);

        // The dispatcher's confirming snapshot clears the pending state; from then on the
        // published scope is the source of truth again.
        let mut confirmed = UiSnapshot {
            scope: ChangeScope::Staged,
            ..UiSnapshot::default()
        };
        pending.reconcile(&mut confirmed);
        app.update(confirmed);
        assert_eq!(app.snapshot.scope, ChangeScope::Staged);
        assert!(
            pending.0.is_none(),
            "a confirming snapshot clears the pending scope"
        );
    }

    /// Enter in a picker resolves the selection against the FILTERED list (not the raw
    /// candidate list), sends the name to the dispatcher, and closes + clears the query.
    #[tokio::test]
    async fn enter_on_filtered_model_picker_resolves_filtered_name() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = App::new();
        let mut pending = PendingScope::default();
        app.update(UiSnapshot {
            available_models: vec![
                "openai/gpt-5".to_string(),
                "anthropic/claude-fable-5".to_string(),
            ],
            ..UiSnapshot::default()
        });
        app.apply(Action::ModelPicker);
        for c in "claude".chars() {
            app.apply(Action::PickerInput(c));
        }
        dispatch(
            &mut app,
            Action::AiSettingsSelected {
                model: String::new(),
                reasoning_effort: String::new(),
            },
            &tx,
            &mut pending,
            &mut SelectionTracker::default(),
        )
        .await;
        assert_eq!(
            rx.recv().await,
            Some(Action::AiSettingsSelected {
                model: "anthropic/claude-fable-5".to_string(),
                reasoning_effort: "default".to_string(),
            }),
            "the filtered entry under the selection is dispatched"
        );
        assert!(!app.show_model_picker);
        assert!(app.model_query.is_empty(), "Enter clears the query");
    }

    #[tokio::test]
    async fn enter_uses_typed_model_when_discovery_has_no_match() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = App::new();
        let mut pending = PendingScope::default();
        app.update(UiSnapshot {
            ai_model: "current/model".to_string(),
            ai_provider: "custom".to_string(),
            available_models: vec!["current/model".to_string()],
            model_list_error: Some("provider has no models endpoint".to_string()),
            ..UiSnapshot::default()
        });
        app.apply(Action::ModelPicker);
        for character in "new/model-id".chars() {
            app.apply(Action::PickerInput(character));
        }

        dispatch(
            &mut app,
            Action::AiSettingsSelected {
                model: String::new(),
                reasoning_effort: String::new(),
            },
            &tx,
            &mut pending,
            &mut SelectionTracker::default(),
        )
        .await;
        assert_eq!(
            rx.recv().await,
            Some(Action::AiSettingsSelected {
                model: "new/model-id".to_string(),
                reasoning_effort: "default".to_string(),
            })
        );
        assert!(!app.show_model_picker);
        assert!(app.model_query.is_empty());
    }

    #[tokio::test]
    async fn model_picker_stages_reasoning_then_publishes_both_settings_once() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = App::new();
        let mut pending = PendingScope::default();
        app.update(UiSnapshot {
            ai_provider: "anthropic".to_string(),
            ai_model: "claude-test".to_string(),
            available_models: vec!["claude-test".to_string()],
            ai_reasoning_effort: "low".to_string(),
            available_reasoning_efforts: ["default", "low", "medium", "high"]
                .map(str::to_string)
                .to_vec(),
            ..UiSnapshot::default()
        });
        app.apply(Action::ModelPicker);

        dispatch(
            &mut app,
            Action::ReasoningEffortNext,
            &tx,
            &mut pending,
            &mut SelectionTracker::default(),
        )
        .await;

        assert_eq!(app.selected_reasoning_effort(), "medium");
        assert!(
            rx.try_recv().is_err(),
            "cycling is local and cannot spam AI"
        );

        dispatch(
            &mut app,
            Action::AiSettingsSelected {
                model: String::new(),
                reasoning_effort: String::new(),
            },
            &tx,
            &mut pending,
            &mut SelectionTracker::default(),
        )
        .await;
        assert_eq!(
            rx.recv().await,
            Some(Action::AiSettingsSelected {
                model: "claude-test".to_string(),
                reasoning_effort: "medium".to_string(),
            })
        );
        assert!(
            !app.show_model_picker,
            "Enter applies and closes the picker"
        );
    }

    #[tokio::test]
    async fn drag_previews_coalesce_until_one_explicit_persist() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = App::new();
        let mut pending = PendingScope::default();
        let mut selection = SelectionTracker::default();

        let dirty = dispatch(
            &mut app,
            Action::ResizeDivider {
                divider: crate::divider::DividerId::FilesDiff,
                extent: 47,
            },
            &tx,
            &mut pending,
            &mut selection,
        )
        .await;
        assert!(dirty, "drag preview changed a stable preference");
        assert!(rx.try_recv().is_err(), "preview must not write config");

        let dirty_again = dispatch(
            &mut app,
            Action::ResizeDivider {
                divider: crate::divider::DividerId::FilesDiff,
                extent: 51,
            },
            &tx,
            &mut pending,
            &mut selection,
        )
        .await;
        assert!(dirty_again);
        assert!(rx.try_recv().is_err(), "many samples still write nothing");

        persist_preferences(&app, &tx).await;
        let persisted = rx.recv().await;
        let Some(Action::PersistUiPreferences(preferences)) = persisted else {
            panic!("expected one preferences update, got {persisted:?}");
        };
        assert_eq!(
            preferences
                .dividers
                .get(crate::divider::DividerId::FilesDiff),
            51
        );
        assert!(
            rx.try_recv().is_err(),
            "release flush is exactly one action"
        );
    }

    /// Navigation moves within the filtered list: j/k indices address filtered entries.
    #[tokio::test]
    async fn base_picker_selection_indexes_the_filtered_list() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = App::new();
        let mut pending = PendingScope::default();
        app.update(UiSnapshot {
            available_bases: vec![
                "main".to_string(),
                "origin/main".to_string(),
                "develop".to_string(),
            ],
            ..UiSnapshot::default()
        });
        app.apply(Action::BasePicker);
        for c in "main".chars() {
            app.apply(Action::PickerInput(c));
        }
        app.apply(Action::Down); // second filtered entry: origin/main
        dispatch(
            &mut app,
            Action::BaseSelected(String::new()),
            &tx,
            &mut pending,
            &mut SelectionTracker::default(),
        )
        .await;
        assert_eq!(
            rx.recv().await,
            Some(Action::BaseSelected("origin/main".to_string()))
        );
        assert!(!app.show_base_picker);
        assert!(app.base_query.is_empty());
    }

    /// Files fixture: `a.go` (expanded, two positioned symbols) and `b.go` (no symbols);
    /// the flattened rows are `a.go`, `sym0`, `sym1`, `b.go`.
    fn app_with_files() -> App {
        use crate::snapshot::{FileRow, SymbolRow};
        let symbol = |name: &str, position: Option<(u32, u32)>| SymbolRow {
            name: name.to_string(),
            change: "modified",
            confidence: "",
            has_diagnostic: false,
            position,
        };
        let mut app = App::new();
        app.update(UiSnapshot {
            files: vec![
                FileRow {
                    path: "a.go".to_string(),
                    status: "M",
                    changed_symbol_count: 2,
                    added_lines: 0,
                    removed_lines: 0,
                    symbols: vec![symbol("sym0", Some((10, 4))), symbol("sym1", Some((20, 4)))],
                    expanded: true,
                    semantic: crate::snapshot::FileSemanticLoad::Ready,
                },
                FileRow {
                    path: "b.go".to_string(),
                    status: "M",
                    changed_symbol_count: 0,
                    added_lines: 0,
                    removed_lines: 0,
                    symbols: Vec::new(),
                    expanded: false,
                    semantic: Default::default(),
                },
            ],
            ..UiSnapshot::default()
        });
        app
    }

    /// j/k navigation drives the dispatcher: every move to a new row sends one
    /// SelectionChanged carrying the file (and the symbol, on symbol rows).
    #[tokio::test]
    async fn selection_changed_fires_on_jk_moves() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = app_with_files();
        let mut pending = PendingScope::default();
        let mut selection = SelectionTracker::default();

        // Row 0 -> 1: symbol row sym0 of a.go.
        dispatch(&mut app, Action::Down, &tx, &mut pending, &mut selection).await;
        assert_eq!(
            rx.recv().await,
            Some(Action::SelectionChanged {
                file: Some("a.go".to_string()),
                symbol: Some(("sym0".to_string(), 10, 4)),
            })
        );
        // Row 1 -> 2: the other symbol.
        dispatch(&mut app, Action::Down, &tx, &mut pending, &mut selection).await;
        assert_eq!(
            rx.recv().await,
            Some(Action::SelectionChanged {
                file: Some("a.go".to_string()),
                symbol: Some(("sym1".to_string(), 20, 4)),
            })
        );
        // Row 2 -> 3: the b.go file row.
        dispatch(&mut app, Action::Down, &tx, &mut pending, &mut selection).await;
        assert_eq!(
            rx.recv().await,
            Some(Action::SelectionChanged {
                file: Some("b.go".to_string()),
                symbol: None,
            })
        );
        // Row 3 -> 1 (Up twice) reports each landing row.
        dispatch(&mut app, Action::Up, &tx, &mut pending, &mut selection).await;
        assert_eq!(
            rx.recv().await,
            Some(Action::SelectionChanged {
                file: Some("a.go".to_string()),
                symbol: Some(("sym1".to_string(), 20, 4)),
            })
        );
    }

    /// A keypress that does not move the selection (clamped at a boundary, or a
    /// selection-neutral action) sends nothing.
    #[tokio::test]
    async fn selection_changed_not_fired_when_selection_does_not_move() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = app_with_files();
        let mut pending = PendingScope::default();
        let mut selection = SelectionTracker::default();

        // Establish the baseline (row 0: the a.go file row).
        selection.sync(&app, &tx).await;
        assert_eq!(
            rx.recv().await,
            Some(Action::SelectionChanged {
                file: Some("a.go".to_string()),
                symbol: None,
            })
        );

        // Up at the top boundary clamps to row 0: no send. Neither does an unmapped key.
        dispatch(&mut app, Action::Up, &tx, &mut pending, &mut selection).await;
        dispatch(&mut app, Action::None, &tx, &mut pending, &mut selection).await;
        assert!(
            rx.try_recv().is_err(),
            "no SelectionChanged when the selection does not move"
        );
    }

    /// The selection can also move because a new snapshot clamped it (here: `b.go`
    /// disappears, so the selection falls back onto `sym1` of `a.go`); the run loop's
    /// snapshot arm syncs that to the dispatcher too.
    #[tokio::test]
    async fn selection_changed_fires_when_a_snapshot_moves_the_selection() {
        use crate::snapshot::{FileRow, SymbolRow};
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = app_with_files();
        app.file_sel = 3; // on b.go
        let mut selection = SelectionTracker::default();

        selection.sync(&app, &tx).await;
        assert_eq!(
            rx.recv().await,
            Some(Action::SelectionChanged {
                file: Some("b.go".to_string()),
                symbol: None,
            })
        );

        app.update(UiSnapshot {
            files: vec![FileRow {
                path: "a.go".to_string(),
                status: "M",
                changed_symbol_count: 2,
                added_lines: 0,
                removed_lines: 0,
                semantic: crate::snapshot::FileSemanticLoad::Ready,
                symbols: vec![
                    SymbolRow {
                        name: "sym0".to_string(),
                        change: "modified",
                        confidence: "",
                        has_diagnostic: false,
                        position: Some((10, 4)),
                    },
                    SymbolRow {
                        name: "sym1".to_string(),
                        change: "modified",
                        confidence: "",
                        has_diagnostic: false,
                        position: Some((20, 4)),
                    },
                ],
                expanded: true,
            }],
            ..UiSnapshot::default()
        });
        selection.sync(&app, &tx).await;
        assert_eq!(app.file_sel, 2, "clamped onto the last remaining row");
        assert_eq!(
            rx.recv().await,
            Some(Action::SelectionChanged {
                file: Some("a.go".to_string()),
                symbol: Some(("sym1".to_string(), 20, 4)),
            })
        );
    }

    /// Enter keeps working: it still forwards SelectSymbol for the symbol under the
    /// selection, and — because Enter does not move the selection — sends nothing else.
    #[tokio::test]
    async fn enter_still_sends_select_symbol() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = app_with_files();
        let mut pending = PendingScope::default();
        let mut selection = SelectionTracker::default();
        app.apply(Action::Down); // row 1: sym0 of a.go

        // Baseline mirrors what navigation already told the dispatcher.
        selection.sync(&app, &tx).await;
        let _ = rx.recv().await;

        dispatch(
            &mut app,
            Action::Activate,
            &tx,
            &mut pending,
            &mut selection,
        )
        .await;
        assert_eq!(
            rx.recv().await,
            Some(Action::SelectSymbol {
                file: "a.go".to_string(),
                name: "sym0".to_string(),
                line: 10,
                col: 4,
            }),
            "Enter still forwards SelectSymbol"
        );
        assert!(
            rx.try_recv().is_err(),
            "Enter does not move the selection; no SelectionChanged follows"
        );
    }

    /// ScopeCycle forwards as-is (the dispatcher cycles its own scope); the guard records
    /// the concrete scope the app cycled to, so stale snapshots cannot undo it.
    #[tokio::test]
    async fn scope_cycle_is_forwarded_and_guarded() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = App::new();
        let mut pending = PendingScope::default();

        dispatch(
            &mut app,
            Action::ScopeCycle,
            &tx,
            &mut pending,
            &mut SelectionTracker::default(),
        )
        .await;
        assert_eq!(app.snapshot.scope, ChangeScope::BranchWorking);
        assert_eq!(rx.recv().await, Some(Action::ScopeCycle));
        assert_eq!(pending.0, Some(ChangeScope::BranchWorking));

        let mut stale = UiSnapshot::default(); // scope: Branch
        pending.reconcile(&mut stale);
        app.update(stale);
        assert_eq!(app.snapshot.scope, ChangeScope::BranchWorking);

        dispatch(
            &mut app,
            Action::ScopeCycleReverse,
            &tx,
            &mut pending,
            &mut SelectionTracker::default(),
        )
        .await;
        assert_eq!(app.snapshot.scope, ChangeScope::Branch);
        assert_eq!(rx.recv().await, Some(Action::ScopeCycleReverse));
        assert_eq!(pending.0, Some(ChangeScope::Branch));
    }
}
