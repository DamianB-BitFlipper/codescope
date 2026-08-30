//! The tokio run loop: terminal events + snapshot updates, with a biased select so keys
//! always beat ticks. Terminal init/restore is the CALLER's responsibility
//! (`ratatui::init()` / `ratatui::restore()`); this only drives the loop.

use std::time::Duration;

use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::{mpsc, watch};

use crate::action::{map_key, Action};
use crate::app::App;
use crate::render::render;
use crate::snapshot::UiSnapshot;

/// Run the TUI until the user quits.
///
/// - `rx` carries new snapshots from the dispatcher (watch = latest-wins).
/// - `tx` receives Actions that require work the TUI cannot do itself
///   (RefreshGit, AiToggle, AiRefresh); view-only actions are applied locally.
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
                        dispatch(&mut app, action, &tx).await;
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
                let snapshot = rx.borrow_and_update().clone();
                app.update(snapshot);
            }
            // Spinner/redraw heartbeat.
            _ = tick.tick() => {}
        }
    }
}

/// Apply view-only actions locally and forward work actions to the dispatcher.
async fn dispatch(app: &mut App, action: Action, tx: &mpsc::Sender<Action>) {
    match action {
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
        other => app.apply(other),
    }
}
