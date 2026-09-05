//! Read-only filesystem + git-state watching (research 06).
//!
//! Native watcher callbacks only record dirty signals. A single async reconciler applies
//! the appropriate quiet window and compares one repository fingerprint before asking the
//! dispatcher to refresh. Filesystem noise that does not alter git-visible state therefore
//! never invalidates semantic or AI work.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use codescope_git::GitRepo;
use notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, DebouncedEventKind, new_debouncer};
use tokio::sync::{Notify, mpsc};

use crate::dispatcher::DispatchEvent;

const TREE_DIRTY: u8 = 1;
const GIT_DIRTY: u8 = 2;
const TREE_QUIET: Duration = Duration::from_millis(500);
const GIT_QUIET: Duration = Duration::from_millis(100);

#[derive(Clone)]
struct WatchSignals {
    dirty: Arc<AtomicU8>,
    wake: Arc<Notify>,
}

impl WatchSignals {
    fn new() -> Self {
        Self {
            dirty: Arc::new(AtomicU8::new(0)),
            wake: Arc::new(Notify::new()),
        }
    }

    fn mark(&self, bit: u8) {
        self.dirty.fetch_or(bit, Ordering::Release);
        self.wake.notify_one();
    }
}

/// Holds the native watchers and fingerprint reconciler; dropping it stops all three.
pub struct RepoWatchers {
    _tree: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
    _git: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
    reconcile: tokio::task::JoinHandle<()>,
}

impl Drop for RepoWatchers {
    fn drop(&mut self) {
        self.reconcile.abort();
    }
}

impl RepoWatchers {
    /// Start watching the repo's working tree and git dir. Raw notifications are coalesced
    /// and only produce `RepoChanged` when the repository fingerprint changes.
    pub async fn start(repo: &GitRepo, tx: mpsc::Sender<DispatchEvent>) -> anyhow::Result<Self> {
        let signals = WatchSignals::new();
        let git_dir = repo.git_dir().as_std_path().to_path_buf();
        let tree = watch_path(
            repo.toplevel().as_std_path(),
            Duration::from_millis(100),
            signals.clone(),
            WatchKind::Tree {
                git_dir: git_dir.clone(),
            },
        )?;
        let git = watch_path(
            &git_dir,
            Duration::from_millis(50),
            signals.clone(),
            WatchKind::Git,
        )?;
        // Establish the baseline after both native watchers are live, but before the
        // dispatcher starts its initial refresh. Signals received during this query remain
        // in the dirty bit; the initial refresh already sees their resulting state.
        let baseline = match repo.fingerprint().await {
            Ok(fingerprint) => Some(fingerprint),
            Err(error) => {
                tracing::warn!(%error, "initial repository fingerprint failed");
                None
            }
        };
        let reconcile = tokio::spawn(reconcile_repo(repo.clone(), signals, tx, baseline));
        Ok(Self {
            _tree: tree,
            _git: git,
            reconcile,
        })
    }
}

enum WatchKind {
    Tree { git_dir: PathBuf },
    Git,
}

fn watch_path(
    path: &Path,
    debounce: Duration,
    signals: WatchSignals,
    kind: WatchKind,
) -> anyhow::Result<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>> {
    let path = path.to_path_buf();
    let mut debouncer = new_debouncer(debounce, move |result: DebounceEventResult| match result {
        Ok(events) => {
            if events
                .iter()
                .any(|event| is_relevant(&event.path, &kind, &event.kind))
            {
                signals.mark(match kind {
                    WatchKind::Tree { .. } => TREE_DIRTY,
                    WatchKind::Git => GIT_DIRTY,
                });
            }
        }
        Err(error) => tracing::warn!(?error, "repository watcher error"),
    })?;
    debouncer.watcher().watch(&path, RecursiveMode::Recursive)?;
    Ok(debouncer)
}

async fn reconcile_repo(
    repo: GitRepo,
    signals: WatchSignals,
    tx: mpsc::Sender<DispatchEvent>,
    mut last_fingerprint: Option<String>,
) {
    loop {
        while signals.dirty.load(Ordering::Acquire) == 0 {
            signals.wake.notified().await;
        }
        let mut dirty = signals.dirty.swap(0, Ordering::AcqRel);

        // Trailing-edge quiet window. Newly observed tree traffic resets the longer
        // timer; a pure git-state event settles quickly.
        loop {
            let quiet = if dirty & TREE_DIRTY != 0 {
                TREE_QUIET
            } else {
                GIT_QUIET
            };
            tokio::time::sleep(quiet).await;
            let added = signals.dirty.swap(0, Ordering::AcqRel);
            if added == 0 {
                break;
            }
            dirty |= added;
        }

        let fingerprint = match repo.fingerprint().await {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                tracing::warn!(%error, dirty, "repository fingerprint failed after watcher signal");
                continue;
            }
        };
        if last_fingerprint.as_ref() == Some(&fingerprint) {
            tracing::debug!(
                dirty,
                "ignored watcher noise; repository fingerprint unchanged"
            );
            continue;
        }
        tracing::debug!(dirty, "repository fingerprint changed; scheduling refresh");
        last_fingerprint = Some(fingerprint);
        if tx.send(DispatchEvent::RepoChanged).await.is_err() {
            break;
        }
    }
}

/// Filter out noise. The worktree watcher ignores the git directory it recursively
/// encloses; the dedicated git watcher ignores object/log traffic and keeps state refs.
fn is_relevant(path: &Path, kind: &WatchKind, _event_kind: &DebouncedEventKind) -> bool {
    match kind {
        WatchKind::Tree { git_dir } => !path.starts_with(git_dir),
        WatchKind::Git => {
            let s = path.to_string_lossy();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn tree_watcher_excludes_the_resolved_git_directory() {
        let kind = WatchKind::Tree {
            git_dir: PathBuf::from("/repo/.git/worktrees/topic"),
        };
        assert!(!is_relevant(
            Path::new("/repo/.git/worktrees/topic/index"),
            &kind,
            &DebouncedEventKind::Any,
        ));
        assert!(is_relevant(
            Path::new("/repo/src/main.rs"),
            &kind,
            &DebouncedEventKind::Any,
        ));
    }

    #[test]
    fn git_watcher_ignores_objects_but_accepts_state_files() {
        assert!(!is_relevant(
            Path::new("/repo/.git/objects/aa/bb"),
            &WatchKind::Git,
            &DebouncedEventKind::Any,
        ));
        assert!(is_relevant(
            Path::new("/repo/.git/index"),
            &WatchKind::Git,
            &DebouncedEventKind::Any,
        ));
    }

    #[tokio::test]
    async fn fingerprint_gate_ignores_noise_and_emits_one_real_change() {
        let temp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .output()
                .unwrap();
            assert!(output.status.success());
        };
        git(&["init", "--quiet", "-b", "main"]);
        std::fs::write(temp.path().join("a.txt"), "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "--quiet", "-m", "base"]);

        let root = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let repo = GitRepo::discover(&root).await.unwrap();
        let baseline = Some(repo.fingerprint().await.unwrap());
        let signals = WatchSignals::new();
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(reconcile_repo(repo, signals.clone(), tx, baseline));

        signals.mark(TREE_DIRTY);
        assert!(
            tokio::time::timeout(Duration::from_millis(700), rx.recv())
                .await
                .is_err(),
            "unchanged fingerprint must not refresh"
        );

        std::fs::write(temp.path().join("a.txt"), "two-two\n").unwrap();
        signals.mark(TREE_DIRTY);
        let event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("changed fingerprint should refresh")
            .expect("reconciler channel closed");
        assert!(matches!(event, DispatchEvent::RepoChanged));
        task.abort();
    }
}
