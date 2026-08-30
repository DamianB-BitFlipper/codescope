//! Read-only filesystem + git-state watching (research 06).
//!
//! Two debounced watchers feed one `DispatchEvent::RepoChanged` stream: the working tree
//! (300 ms) and the resolved git dir (100 ms). Watching only one side misses real changes:
//! plain edits don't touch `.git`, and `git add`/`commit`/`checkout` don't touch the tree.

use std::path::{Path, PathBuf};
use std::time::Duration;

use codescope_git::GitRepo;
use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, DebouncedEventKind};
use tokio::sync::mpsc;

use crate::dispatcher::DispatchEvent;

/// Holds the watchers; dropping it stops watching.
pub struct RepoWatchers {
    _tree: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
    _git: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
}

impl RepoWatchers {
    /// Start watching the repo's working tree and git dir, forwarding change events.
    pub fn start(repo: &GitRepo, tx: mpsc::Sender<DispatchEvent>) -> anyhow::Result<Self> {
        let tree = watch_path(
            repo.toplevel().as_std_path(),
            Duration::from_millis(300),
            tx.clone(),
            WatchKind::Tree,
        )?;
        let git = watch_path(
            repo.git_dir().as_std_path(),
            Duration::from_millis(100),
            tx,
            WatchKind::Git,
        )?;
        Ok(RepoWatchers {
            _tree: tree,
            _git: git,
        })
    }
}

enum WatchKind {
    Tree,
    Git,
}

fn watch_path(
    path: &Path,
    debounce: Duration,
    tx: mpsc::Sender<DispatchEvent>,
    kind: WatchKind,
) -> anyhow::Result<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>> {
    let path = path.to_path_buf();
    let mut debouncer = new_debouncer(debounce, move |result: DebounceEventResult| {
        let Ok(events) = result else { return };
        let relevant = events
            .iter()
            .any(|e| is_relevant(e.path.as_path(), &kind, &e.kind));
        if relevant {
            let _ = tx.try_send(DispatchEvent::RepoChanged);
        }
    })?;
    debouncer.watcher().watch(&path, RecursiveMode::Recursive)?;
    Ok(debouncer)
}

/// Filter out noise: inside the git dir we only care about state-changing files
/// (HEAD/index/refs/MERGE_HEAD/…), not `objects/` or `logs/`.
fn is_relevant(path: &Path, kind: &WatchKind, _event_kind: &DebouncedEventKind) -> bool {
    match kind {
        WatchKind::Tree => true,
        WatchKind::Git => {
            let name: PathBuf = path.to_path_buf();
            let s = name.to_string_lossy();
            if s.contains("/objects/") || s.contains("/logs/") {
                return false;
            }
            s.ends_with("HEAD")
                || s.ends_with("index")
                || s.contains("/refs/")
                || s.ends_with("packed-refs")
                || s.ends_with("MERGE_HEAD")
                || s.ends_with("FETCH_HEAD")
                || s.ends_with("ORIG_HEAD")
        }
    }
}
