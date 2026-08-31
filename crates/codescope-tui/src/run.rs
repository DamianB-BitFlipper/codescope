//! The tokio run loop: terminal events + snapshot updates, with a biased select so keys
//! always beat ticks. Terminal init/restore is the CALLER's responsibility
//! (`ratatui::init()` / `ratatui::restore()`); this only drives the loop.

use std::time::Duration;

use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::{mpsc, watch};

use codescope_core::ChangeScope;

use crate::action::{map_key, Action};
use crate::app::App;
use crate::render::render;
use crate::snapshot::UiSnapshot;

/// Run the TUI until the user quits.
///
/// - `rx` carries new snapshots from the dispatcher (watch = latest-wins).
/// - `tx` receives Actions that require work the TUI cannot do itself
///   (RefreshGit, AiToggle, AiRefresh, scope changes); view-only actions are applied
///   locally.
pub async fn run(
    terminal: &mut DefaultTerminal,
    mut app: App,
    mut rx: watch::Receiver<UiSnapshot>,
    tx: mpsc::Sender<Action>,
) -> std::io::Result<()> {
    // Defensive: ensure the terminal is in raw mode so key events are delivered. Idempotent;
    // ratatui::init already does this, but a missed enable leaves the app unresponsive.
    let _ = crossterm::terminal::enable_raw_mode();
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    let mut pending_scope = PendingScope::default();
    let mut selection = SelectionTracker::default();

    loop {
        terminal.draw(|frame| render(frame, &app, &app.snapshot.clone()))?;
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            biased;
            // Keyboard/mouse input first — never starve interactivity.
            maybe = events.next() => {
                match maybe {
                    Some(Ok(Event::Key(key))) => {
                        let action = map_key(key, &app);
                        dispatch(&mut app, action, &tx, &mut pending_scope, &mut selection)
                            .await;
                    }
                    Some(Ok(Event::Resize(_, _))) => { /* redrawn next pass */ }
                    Some(Ok(_)) | Some(Err(_)) | None => {}
                }
            }
            // A new repository/analysis state arrived.
            changed = rx.changed() => {
                if changed.is_err() {
                    // Dispatcher dropped the sender: keep rendering the last state.
                    continue;
                }
                let mut snapshot = rx.borrow_and_update().clone();
                pending_scope.reconcile(&mut snapshot);
                app.update(snapshot);
                // The new state may have moved the selection (clamp / re-expanded rows):
                // keep the dispatcher's diff + relations aimed at it.
                selection.sync(&app, &tx).await;
            }
            // Spinner/redraw heartbeat.
            _ = tick.tick() => {}
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

/// The resolved files-pane selection target, as sent to the dispatcher.
type SelectionTarget = (Option<String>, Option<(String, u32, u32)>);

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
        if self.0.is_none() && target == (None, None) {
            self.0 = Some(target);
            return;
        }
        self.0 = Some(target.clone());
        let _ = tx
            .send(Action::SelectionChanged {
                file: target.0,
                symbol: target.1,
            })
            .await;
    }
}

/// Resolve the flattened files-pane selection to its `(file, symbol)` target: a file row
/// yields `(Some(path), None)`; a symbol row with a position yields the symbol too (an
/// unmapped symbol row degrades to its file). `(None, None)` when the file list is empty.
fn selection_target(app: &App) -> SelectionTarget {
    let mut idx = app.file_sel;
    for f in &app.snapshot.files {
        if idx == 0 {
            return (Some(f.path.clone()), None);
        }
        idx -= 1;
        if f.expanded {
            if idx < f.symbols.len() {
                let s = &f.symbols[idx];
                let symbol = s.position.map(|(line, col)| (s.name.clone(), line, col));
                return (Some(f.path.clone()), symbol);
            }
            idx -= f.symbols.len();
        }
    }
    (None, None)
}

/// Apply view-only actions locally and forward work actions to the dispatcher.
async fn dispatch(
    app: &mut App,
    action: Action,
    tx: &mpsc::Sender<Action>,
    pending_scope: &mut PendingScope,
    selection: &mut SelectionTracker,
) {
    match action {
        Action::Activate => {
            // If the files-pane selection is a symbol row with a position, forward a
            // SelectSymbol so the dispatcher lazily expands its callers/callees.
            if let Some(sel) = selected_symbol(app) {
                let _ = tx.send(sel).await;
            }
            app.apply(Action::Activate);
        }
        Action::ModelSelected(name) => {
            // The modal sends an empty name; resolve it from the selection in the
            // filtered (visible) list.
            let name = if name.is_empty() {
                app.filtered_models()
                    .get(app.model_sel)
                    .map(|s| (*s).to_string())
                    .unwrap_or_default()
            } else {
                name
            };
            if !name.is_empty() {
                let _ = tx.send(Action::ModelSelected(name)).await;
            }
            app.show_model_picker = false;
            app.model_query.clear();
        }
        Action::RefreshGit | Action::AiToggle | Action::AiRefresh => {
            let _ = tx.send(action).await;
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
        | Action::ScopeWorking
        | Action::ScopeCycle) => {
            app.apply(scope.clone());
            pending_scope.record(app.snapshot.scope);
            let _ = tx.send(scope).await;
        }
        other => app.apply(other),
    }
    // Navigation-driven panes: whatever the action did, tell the dispatcher where the
    // files-pane selection landed (sends only on change).
    selection.sync(app, tx).await;
}

/// Resolve the currently selected symbol row to a [`Action::SelectSymbol`], if the files-pane
/// selection is on an expandable symbol (one with a position).
fn selected_symbol(app: &App) -> Option<Action> {
    if app.focused != crate::app::Pane::Files {
        return None;
    }
    let mut idx = app.file_sel;
    for f in &app.snapshot.files {
        if idx == 0 {
            return None; // a file row, not a symbol
        }
        idx -= 1;
        if f.expanded {
            if idx < f.symbols.len() {
                let s = &f.symbols[idx];
                return s.position.map(|(line, col)| Action::SelectSymbol {
                    file: f.path.clone(),
                    name: s.name.clone(),
                    line,
                    col,
                });
            }
            idx -= f.symbols.len();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Action::ModelSelected(String::new()),
            &tx,
            &mut pending,
            &mut SelectionTracker::default(),
        )
        .await;
        assert_eq!(
            rx.recv().await,
            Some(Action::ModelSelected(
                "anthropic/claude-fable-5".to_string()
            )),
            "the filtered entry under the selection is dispatched"
        );
        assert!(!app.show_model_picker);
        assert!(app.model_query.is_empty(), "Enter clears the query");
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
                    symbols: vec![symbol("sym0", Some((10, 4))), symbol("sym1", Some((20, 4)))],
                    expanded: true,
                },
                FileRow {
                    path: "b.go".to_string(),
                    status: "M",
                    symbols: Vec::new(),
                    expanded: false,
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
        assert_eq!(app.snapshot.scope, ChangeScope::Staged); // Branch -> Staged
        assert_eq!(rx.recv().await, Some(Action::ScopeCycle));
        assert_eq!(pending.0, Some(ChangeScope::Staged));

        let mut stale = UiSnapshot::default(); // scope: Branch
        pending.reconcile(&mut stale);
        app.update(stale);
        assert_eq!(app.snapshot.scope, ChangeScope::Staged);
    }
}
