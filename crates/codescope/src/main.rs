//! codescope: understand what your current code changes do to the broader system.
//!
//! Read-only: never edits files, stages, commits, or changes branches. The interactive
//! application requires an AI provider credential.

mod agent_protocol;
mod backend;
mod config;
mod dispatcher;
mod request_coordinator;
mod research_tools;
mod skills;
mod telemetry_diff;
mod terminal;
mod watcher;

use std::path::PathBuf;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::Parser;
use codescope_ai::{AiService, ReasoningEffort};
use codescope_analysis::AnalysisEngine;
use codescope_git::GitRepo;
use codescope_lsp::LanguageService;
use codescope_tui::{Action, App, UiSnapshot};
use dispatcher::{DispatchEvent, Dispatcher};
use tokio::sync::{mpsc, watch};
use watcher::RepoWatchers;

const DEFAULT_DEBUG_LOG_FILE: &str = "codescope-debug.log";

/// Understand what your current code changes do to the system.
///
/// With no subcommand, the interactive TUI starts. The subcommands are the
/// non-interactive JSON backend or a client for an already-running TUI; they never start a
/// second TUI.
#[derive(Parser, Debug)]
#[command(name = "codescope", version, about)]
struct Cli {
    /// Non-interactive backend subcommand (including headless AI debugging).
    #[command(subcommand)]
    command: Option<backend::BackendCommand>,

    /// Repository path (defaults to the current directory). TUI mode only; the
    /// backend subcommands take their own PATH argument.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// AI model for this run. Overrides the remembered/global model without persisting it.
    #[arg(short = 'm', long, value_name = "MODEL_NAME")]
    model: Option<String>,

    /// Reasoning budget for this run. Use `default` to let the provider/model decide.
    #[arg(short = 'r', long, value_name = "LEVEL")]
    reasoning_effort: Option<ReasoningEffort>,

    /// Watch the working tree and Git state, refreshing automatically after changes.
    /// Off by default; without this flag, press g to refresh manually.
    #[arg(long)]
    watch: bool,

    /// Write tracing logs to this file (off by default; never logs secrets).
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,

    /// Save verbose application traces and scrubbed AI wire data to a debug log file.
    #[arg(long, global = true)]
    debug: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    initialize_telemetry();
    codescope_telemetry::record(
        "session.start",
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "args": telemetry_args(),
        }),
    );
    let cli = Cli::parse();
    codescope_telemetry::record(
        "session.mode",
        serde_json::json!({
            "mode": if cli.command.is_some() { "command" } else { "tui" },
        }),
    );
    let debug_ai = matches!(&cli.command, Some(backend::BackendCommand::DebugAi(_)));
    let log_file = tracing_log_file(&cli);
    init_tracing(log_file.as_deref(), debug_ai, cli.debug)?;
    if cli.debug {
        if let Some(path) = &log_file {
            tracing::info!(
                path = %path.display(),
                "debug tracing enabled"
            );
        }
    }

    // Non-interactive command: run and exit without starting the TUI.
    if let Some(command) = &cli.command {
        let code = backend::run(command).await;
        codescope_telemetry::record("command.complete", serde_json::json!({ "exit_code": code }));
        codescope_telemetry::record("session.end", serde_json::json!({ "exit_code": code }));
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
    codescope_telemetry::set_repository(root.to_string());
    tracing::info!(root = %root, "discovered repository");

    // Global-only v1 configuration. Backend commands returned above and therefore never
    // read or create user config as a side effect.
    let config_store = std::sync::Arc::new(config::ConfigStore::load());
    let config_warning = config_store.warning().map(str::to_owned);
    if let Some(warning) = &config_warning {
        tracing::warn!(%warning, "global config warning");
    }

    // Start the language server in the BACKGROUND: the TUI and git view come up immediately
    // (LsStatus::Starting); when gopls finishes initializing, the engine is handed to the
    // dispatcher via DispatchEvent::EngineReady and semantic analysis begins (rv-perf H2).

    let ai_config = config_store
        .resolve_ai_config(cli.model.as_deref(), cli.reasoning_effort)
        .context("AI configuration is required for interactive Codescope")?;
    let ai = AiService::new(ai_config, root.clone())
        .context("invalid AI configuration for interactive Codescope")?;

    // Channels: dispatcher publishes snapshots (watch = latest-wins); the TUI sends back
    // work actions; optional watchers send change events; spawned jobs report back on the same queue.
    let (snapshot_tx, snapshot_rx) = watch::channel(UiSnapshot::placeholder());
    let (action_tx, action_rx) = mpsc::channel::<Action>(64);
    let (control_tx, control_rx) = mpsc::channel::<Action>(32);
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

    // Watching is opt-in. The dispatcher still performs one initial load and explicit
    // scope/base changes or `g` use the same refresh path regardless of this mode.
    let _watchers = if cli.watch {
        tracing::info!("watch mode enabled");
        Some(RepoWatchers::start(&repo, event_tx.clone()).await?)
    } else {
        tracing::info!("watch mode disabled; repository updates require manual refresh");
        None
    };

    let mut disp = Dispatcher::new(repo.clone(), None, ai, snapshot_tx, job_tx)
        .with_config_persistence(config_store.clone());
    if let Some(warning) = config_warning {
        disp = disp.with_startup_warning(warning);
    }
    let disp_handle = tokio::spawn(dispatcher::run(disp, event_rx, action_rx));

    // A repo-specific, owner-only Unix socket lets local CLI/agent clients inspect the
    // exact published snapshot and feed typed controls through the visible TUI cursor.
    let _agent_server =
        agent_protocol::AgentServer::start(root.clone(), snapshot_rx.clone(), control_tx)
            .await
            .context("cannot start the local agent control protocol")?;

    // TUI owns the terminal; restore it no matter how we exit.
    let tui_result = terminal::run_with_terminal(|mut term| async move {
        let app = App::with_preferences(config_store.ui_preferences());
        codescope_tui::run::run(&mut term, app, snapshot_rx, action_tx, control_rx).await
    })
    .await;

    // TUI exited: close the event channels so the dispatcher loop ends, then give the
    // dispatcher a bounded window to finish (it shuts the language server down gracefully).
    drop(event_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(8), disp_handle).await;
    codescope_telemetry::record(
        "session.end",
        serde_json::json!({ "exit_code": if tui_result.is_ok() { 0 } else { 1 } }),
    );
    tui_result?;
    Ok(())
}

fn telemetry_args() -> Vec<String> {
    std::env::args_os().map(|arg| telemetry_arg(&arg)).collect()
}

fn telemetry_arg(arg: &std::ffi::OsStr) -> String {
    let path = std::path::Path::new(arg);
    if path.is_absolute() {
        return "[absolute-path]".to_string();
    }
    let text = arg.to_string_lossy();
    if let Some((name, value)) = text.split_once('=') {
        if std::path::Path::new(value).is_absolute() {
            return format!("{name}=[absolute-path]");
        }
    }
    text.into_owned()
}

fn initialize_telemetry() {
    let preferred = config::telemetry_dir();
    if codescope_telemetry::init(&preferred).is_err() {
        let fallback = std::env::temp_dir().join("codescope").join("telemetry");
        if fallback == preferred {
            return;
        }
        match codescope_telemetry::init(&fallback) {
            Ok(_) => codescope_telemetry::record(
                "telemetry.fallback",
                serde_json::json!({ "reason": "preferred_directory_unavailable" }),
            ),
            Err(_fallback_error) => {}
        }
    }
}

fn tracing_log_file(cli: &Cli) -> Option<PathBuf> {
    cli.log_file
        .clone()
        .or_else(|| cli.debug.then(|| PathBuf::from(DEFAULT_DEBUG_LOG_FILE)))
}

fn init_tracing(log_file: Option<&std::path::Path>, debug_ai: bool, debug: bool) -> Result<()> {
    use std::io::IsTerminal as _;
    use tracing_subscriber::EnvFilter;
    let filter = if debug {
        EnvFilter::new(concat!(
            "codescope=trace,",
            "codescope_ai=trace,",
            "codescope_analysis=trace,",
            "codescope_core=trace,",
            "codescope_git=trace,",
            "codescope_lsp=trace,",
            "codescope_testutil=trace,",
            "codescope_tui=trace"
        ))
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            if debug_ai {
                EnvFilter::new("codescope::backend=debug,codescope_ai=debug")
            } else {
                EnvFilter::new("codescope=info")
            }
        })
    };
    if let Some(path) = log_file {
        let file = std::fs::File::create(path)
            .with_context(|| format!("cannot create tracing log {}", path.display()))?;
        let builder = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_target(debug)
            .with_file(debug)
            .with_line_number(debug);
        if debug || debug_ai {
            builder
                .with_span_events(
                    tracing_subscriber::fmt::format::FmtSpan::NEW
                        | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
                )
                .try_init()
                .map_err(|error| anyhow::anyhow!("cannot initialize tracing: {error}"))?;
        } else {
            builder
                .try_init()
                .map_err(|error| anyhow::anyhow!("cannot initialize tracing: {error}"))?;
        }
    } else if debug_ai && std::io::stderr().is_terminal() {
        // Headless AI debugging must explain apparent stalls without requiring a second
        // flag when run by a person. Keep piped/captured invocations machine-readable by
        // only sending progress and span timings to an attached stderr terminal.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .with_span_events(
                tracing_subscriber::fmt::format::FmtSpan::NEW
                    | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
            )
            .try_init();
    }
    Ok(())
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
        assert!(!cli.watch, "watch mode is opt-in");
        let cli = Cli::try_parse_from(["codescope", "/some/repo"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.path, PathBuf::from("/some/repo"));
        let cli = Cli::try_parse_from(["codescope", "-m", "z-ai/glm-5.3", "/some/repo"]).unwrap();
        assert_eq!(cli.model.as_deref(), Some("z-ai/glm-5.3"));
        let cli =
            Cli::try_parse_from(["codescope", "--reasoning-effort", "high", "/some/repo"]).unwrap();
        assert_eq!(cli.reasoning_effort, Some(ReasoningEffort::High));
        let cli = Cli::try_parse_from(["codescope", "--watch", "/some/repo"]).unwrap();
        assert!(cli.watch);
        assert_eq!(cli.path, PathBuf::from("/some/repo"));
    }

    #[test]
    fn debug_flag_uses_a_default_log_and_log_file_overrides_it() {
        let cli = Cli::try_parse_from(["codescope", "--debug", "/some/repo"]).unwrap();
        assert!(cli.debug);
        assert_eq!(
            tracing_log_file(&cli),
            Some(PathBuf::from(DEFAULT_DEBUG_LOG_FILE))
        );

        let cli = Cli::try_parse_from([
            "codescope",
            "debug-ai",
            "/some/repo",
            "--debug",
            "--log-file",
            "/tmp/custom-codescope.log",
        ])
        .unwrap();
        assert!(cli.debug);
        assert_eq!(
            tracing_log_file(&cli),
            Some(PathBuf::from("/tmp/custom-codescope.log"))
        );
    }

    #[test]
    fn telemetry_arguments_hide_absolute_paths_in_both_cli_forms() {
        assert_eq!(
            telemetry_arg(std::ffi::OsStr::new("/private/repository")),
            "[absolute-path]"
        );
        assert_eq!(
            telemetry_arg(std::ffi::OsStr::new(
                "--log-file=/private/repository/debug.log"
            )),
            "--log-file=[absolute-path]"
        );
        assert_eq!(
            telemetry_arg(std::ffi::OsStr::new("src/lib.rs")),
            "src/lib.rs"
        );
    }

    #[test]
    fn no_ai_flag_is_rejected() {
        assert!(Cli::try_parse_from(["codescope", "--no-ai"]).is_err());
    }

    #[test]
    fn subcommand_names_route_to_the_backend() {
        for name in [
            "scan",
            "changeset",
            "analyze",
            "digest",
            "bases",
            "debug-ai",
        ] {
            let cli = Cli::try_parse_from(["codescope", name]).unwrap();
            assert!(cli.command.is_some(), "{name} must be a subcommand");
        }
        let cli = Cli::try_parse_from(["codescope", "agent", ".", "context"]).unwrap();
        assert!(
            matches!(cli.command, Some(backend::BackendCommand::Agent(_))),
            "agent must route to the live protocol client"
        );
        for args in [
            vec!["codescope", "agent", ".", "diff", "--hunk", "0"],
            vec!["codescope", "agent", ".", "diagram", "inspect"],
            vec!["codescope", "agent", ".", "diagram", "schema"],
            vec![
                "codescope",
                "agent",
                ".",
                "diagram",
                "edit",
                r#"{"op":"set_intent","intent":"Explain the change."}"#,
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        assert!(Cli::try_parse_from(["codescope", "agent", ".", "ask", "why?"]).is_err());
        assert!(Cli::try_parse_from(["codescope", "agent", ".", "feedback", "revise it"]).is_err());
        let cli = Cli::try_parse_from(["codescope", "skills", "show"]).unwrap();
        assert!(
            matches!(cli.command, Some(backend::BackendCommand::Skills(_))),
            "skills must route to the bundled-skill commands"
        );
        assert!(Cli::try_parse_from([
            "codescope",
            "skills",
            "install",
            "--global",
            "--yes",
            "--claude",
        ])
        .is_ok());
        let cli =
            Cli::try_parse_from(["codescope", "analyze", "/repo", "--scope", "staged"]).unwrap();
        match cli.command {
            Some(backend::BackendCommand::Analyze(args)) => {
                assert_eq!(args.path, camino::Utf8PathBuf::from("/repo"));
            }
            other => panic!("expected analyze, got {other:?}"),
        }
        let cli = Cli::try_parse_from([
            "codescope",
            "debug-ai",
            "/repo",
            "--scope",
            "working",
            "--file",
            "src/main.rs",
            "--symbol",
            "run",
            "--model",
            "z-ai/glm-5.3",
            "--reasoning-effort",
            "minimal",
        ])
        .unwrap();
        match cli.command {
            Some(backend::BackendCommand::DebugAi(args)) => {
                assert_eq!(args.path, camino::Utf8PathBuf::from("/repo"));
                assert!(matches!(args.scope, backend::Scope::Working));
                assert_eq!(args.file.as_deref(), Some("src/main.rs"));
                assert_eq!(args.symbol.as_deref(), Some("run"));
                assert_eq!(args.model.as_deref(), Some("z-ai/glm-5.3"));
                assert_eq!(args.reasoning_effort, Some(ReasoningEffort::Minimal));
            }
            other => panic!("expected debug-ai, got {other:?}"),
        }
    }

    #[test]
    fn debug_ai_symbol_requires_a_file() {
        let error = Cli::try_parse_from(["codescope", "debug-ai", "--symbol", "run"])
            .expect_err("symbol-only debug selection must be rejected");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }
}
