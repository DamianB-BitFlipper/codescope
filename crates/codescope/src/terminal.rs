//! Terminal lifecycle: raw mode + alternate screen, restored on every exit path
//! (normal, error, panic). Uses ratatui's own init/restore so its panic hook stays intact.

use std::future::Future;
use std::io;

use ratatui::DefaultTerminal;

/// Run `f` with an initialized terminal, restoring the terminal afterwards regardless of
/// how `f` completes. `ratatui::init()` installs a panic hook that restores on panic, and
/// we restore here for the normal/error paths.
pub async fn run_with_terminal<F, Fut>(f: F) -> io::Result<()>
where
    F: FnOnce(DefaultTerminal) -> Fut,
    Fut: Future<Output = io::Result<()>>,
{
    let terminal = ratatui::init();
    let result = f(terminal).await;
    ratatui::restore();
    result
}
