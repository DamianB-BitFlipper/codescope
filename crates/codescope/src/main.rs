//! codescope: understand what your current code changes do to the broader system.
//!
//! Read-only: never edits files, stages, commits, or changes branches. AI is opt-in
//! (`CODESCOPE_AI_*` / an API key); the app is fully functional without it.

mod backend;
mod dispatcher;
mod terminal;
mod watcher;

use std::path::PathBuf;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::Parser;
use codescope_ai::{AiConfig, AiService};
use codescope_analysis::AnalysisEngine;
use codescope_git::GitRepo;
use codescope_lsp::LanguageService;
use codescope_tui::{Action, App, UiSnapshot};
use dispatcher::{DispatchEvent, Dispatcher};
use tokio::sync::{mpsc, watch};
use watcher::RepoWatchers;

/// Understand what your current code changes do to the system.
///
/// With no subcommand, the interactive TUI starts. The subcommands are the
/// non-interactive JSON backend (for scripts and LLM/tool consumers); they never
/// start the TUI.
#[derive(Parser, Debug)]
#[command(name = "codescope", version, about)]
struct Cli {
    /// Non-interactive JSON backend subcommand (scan/changeset/analyze/digest/bases).
    #[command(subcommand)]
    command: Option<backend::BackendCommand>,

    /// Repository path (defaults to the current directory). TUI mode only; the
    /// backend subcommands take their own PATH argument.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Disable AI even if an API key is configured.
    #[arg(long)]
    no_ai: bool,

    /// Write tracing logs to this file (off by default; never logs secrets).
    #[arg(long)]
    log_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.log_file.as_deref());

    // Non-interactive JSON backend: print JSON to stdout and exit, never starting the TUI.
    if let Some(command) = &cli.command {
        let code = backend::run(command).await;
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }

    // Test hook (terminal-restore proof): initialize the terminal then panic before any repo
    // work, so the panic-restoration path is exercised without needing a repository.
    if std::env::var_os("CODESCOPE_TEST_PANIC").is_some() {
        let _term = ratatui::init();
        panic!("codescope test panic");
    }

    let start = Utf8PathBuf::from_path_buf(cli.path.clone())
        .map_err(|p| anyhow::anyhow!("non-UTF-8 path: {}", p.display()))?;
    let start = std::fs::canonicalize(&start)
        .and_then(|p| {
            Utf8PathBuf::from_path_buf(p)
                .map_err(|e| std::io::Error::other(e.display().to_string()))
        })
        .unwrap_or(start);
    let repo = GitRepo::discover(&start)
        .await
        .context("not a git repository (codescope needs one)")?;
    let root = repo.toplevel().to_path_buf();
    tracing::info!(root = %root, "discovered repository");

    // Start the language server in the BACKGROUND: the TUI and git view come up immediately
    // (LsStatus::Starting); when gopls finishes initializing, the engine is handed to the
    // dispatcher via DispatchEvent::EngineReady and semantic analysis begins (rv-perf H2).

    let ai = if cli.no_ai {
        None
    } else {
        match AiConfig::from_env() {
            Ok(config) if config.enabled => match AiService::new(config, root.clone()) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(%e, "AI misconfigured; running without AI");
                    None
                }
            },
            _ => None,
        }
    };

    // Channels: dispatcher publishes snapshots (watch = latest-wins); the TUI sends back
    // work actions; watchers send change events; spawned jobs report back on the same queue.
    let (snapshot_tx, snapshot_rx) = watch::channel(UiSnapshot::placeholder());
    let (action_tx, action_rx) = mpsc::channel::<Action>(64);
    let (event_tx, event_rx) = mpsc::channel::<DispatchEvent>(64);
    let job_tx = event_tx.clone();

    // Language server initializes in the background; the dispatcher starts git-only.
    {
        let repo = repo.clone();
        let root = root.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move {
            match LanguageService::start(root.as_path()).await {
                Ok(svc) => {
                    let engine = AnalysisEngine::new(svc, repo);
                    let _ = tx.send(DispatchEvent::EngineReady(Box::new(engine))).await;
                }
                Err(e) => {
                    // Expected when no supported language is detected. Not an error for the user:
                    // the app simply runs in git-only mode. The TUI surfaces this in the status bar.
                    tracing::info!(%e, "no language server for this repo; git-only mode");
                    let _ = tx
                        .send(DispatchEvent::EngineUnavailable(e.to_string()))
                        .await;
                }
            }
        });
    }

    let _watchers = RepoWatchers::start(&repo, event_tx.clone())?;

    let disp = Dispatcher::new(repo.clone(), None, ai, snapshot_tx, job_tx);
    let disp_handle = tokio::spawn(dispatcher::run(disp, event_rx, action_rx));

    // TUI owns the terminal; restore it no matter how we exit.
    let tui_result = terminal::run_with_terminal(|mut term| async move {
        codescope_tui::run::run(&mut term, App::new(), snapshot_rx, action_tx).await
    })
    .await;

    // TUI exited: close the event channels so the dispatcher loop ends, then give the
    // dispatcher a bounded window to finish (it shuts the language server down gracefully).
    drop(event_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(8), disp_handle).await;
    tui_result?;
    Ok(())
}

fn init_tracing(log_file: Option<&std::path::Path>) {
    use tracing_subscriber::EnvFilter;
    let Some(path) = log_file else { return };
    if let Ok(file) = std::fs::File::create(path) {
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("codescope=info"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .try_init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn bare_path_stays_on_the_tui() {
        let cli = Cli::try_parse_from(["codescope"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.path, PathBuf::from("."));
        let cli = Cli::try_parse_from(["codescope", "/some/repo"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.path, PathBuf::from("/some/repo"));
        let cli = Cli::try_parse_from(["codescope", "--no-ai", "/some/repo"]).unwrap();
        assert!(cli.command.is_none());
        assert!(cli.no_ai);
    }

    #[test]
    fn subcommand_names_route_to_the_backend() {
        for name in ["scan", "changeset", "analyze", "digest", "bases"] {
            let cli = Cli::try_parse_from(["codescope", name]).unwrap();
            assert!(cli.command.is_some(), "{name} must be a subcommand");
        }
        let cli = Cli::try_parse_from(["codescope", "analyze", "/repo", "--scope", "staged"])
            .unwrap();
        match cli.command {
            Some(backend::BackendCommand::Analyze(args)) => {
                assert_eq!(args.path, camino::Utf8PathBuf::from("/repo"));
            }
            other => panic!("expected analyze, got {other:?}"),
        }
    }
}
