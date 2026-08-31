//! Terminal lifecycle: raw mode + alternate screen + mouse capture, restored on every
//! exit path (normal, error, panic, cancellation). Uses ratatui's init/restore so its
//! panic hook stays intact; mouse capture is layered on top with its own guard.

use std::future::Future;
use std::io;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use ratatui::DefaultTerminal;

/// RAII guard: disable mouse capture, then restore the terminal. Armed before
/// `EnableMouseCapture` so a partially-initialized session still restores. `Drop` never
/// short-circuits the restore on a disable error.
struct MouseSessionGuard;

impl Drop for MouseSessionGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        ratatui::restore();
    }
}

/// Run `f` with an initialized terminal plus mouse capture, restoring both regardless of
/// how `f` completes (normal, error, or panic). The panic path: ratatui installs a hook
/// that restores raw mode + leaves the alternate screen; we chain it so mouse capture is
/// also disabled first.
pub async fn run_with_terminal<F, Fut>(f: F) -> io::Result<()>
where
    F: FnOnce(DefaultTerminal) -> Fut,
    Fut: Future<Output = io::Result<()>>,
{
    let terminal = ratatui::init();
    // Chain ratatui's panic hook: disable mouse capture, then run ratatui's restore.
    let prior = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        prior(info);
    }));
    // Arm the guard BEFORE enabling capture so a failed enable still restores.
    let guard = MouseSessionGuard;
    execute!(io::stdout(), EnableMouseCapture)?;
    let result = f(terminal).await;
    drop(guard);
    result
}
