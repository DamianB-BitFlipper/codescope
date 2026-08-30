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
                        dispatch(&mut app, action, &tx, &mut pending_scope).await;
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

/// Apply view-only actions locally and forward work actions to the dispatcher.
async fn dispatch(
    app: &mut App,
    action: Action,
    tx: &mpsc::Sender<Action>,
    pending_scope: &mut PendingScope,
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
            // The modal sends an empty name; resolve it from the current selection.
            let name = if name.is_empty() {
                app.snapshot
                    .available_models
                    .get(app.model_sel)
                    .cloned()
                    .unwrap_or_default()
            } else {
                name
            };
            if !name.is_empty() {
                let _ = tx.send(Action::ModelSelected(name)).await;
            }
            app.show_model_picker = false;
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
            // The modal sends an empty name; resolve it from the current selection.
            let name = if name.is_empty() {
                app.snapshot
                    .available_bases
                    .get(app.base_sel)
                    .cloned()
                    .unwrap_or_default()
            } else {
                name
            };
            if !name.is_empty() {
                let _ = tx.send(Action::BaseSelected(name)).await;
            }
            app.show_base_picker = false;
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
        dispatch(&mut app, Action::ScopeStaged, &tx, &mut pending).await;
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

    /// ScopeCycle forwards as-is (the dispatcher cycles its own scope); the guard records
    /// the concrete scope the app cycled to, so stale snapshots cannot undo it.
    #[tokio::test]
    async fn scope_cycle_is_forwarded_and_guarded() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = App::new();
        let mut pending = PendingScope::default();

        dispatch(&mut app, Action::ScopeCycle, &tx, &mut pending).await;
        assert_eq!(app.snapshot.scope, ChangeScope::Staged); // Branch -> Staged
        assert_eq!(rx.recv().await, Some(Action::ScopeCycle));
        assert_eq!(pending.0, Some(ChangeScope::Staged));

        let mut stale = UiSnapshot::default(); // scope: Branch
        pending.reconcile(&mut stale);
        app.update(stale);
        assert_eq!(app.snapshot.scope, ChangeScope::Staged);
    }
}
