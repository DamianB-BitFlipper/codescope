//! Local control protocol for a running codescope TUI.
//!
//! One owner-only Unix socket is derived from the repository root. Requests are bounded,
//! JSON encoded, and translated into the same typed actions used by the terminal UI; the
//! protocol deliberately exposes neither a shell nor an unvalidated rendering primitive.

use std::io::Write as _;
use std::path::PathBuf;

#[cfg(unix)]
use std::path::Path;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Subcommand};
use codescope_core::DiagramCommand;
use codescope_git::GitRepo;
use codescope_tui::snapshot::{
    AiSummaryKey, AiSummaryState, DiffRow, FileSemanticLoad, ImpactLoadState, UiSnapshot,
};
use codescope_tui::Action;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

const PROTOCOL_VERSION: u8 = 2;
#[cfg(unix)]
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const DEFAULT_DIFF_LINES: usize = 160;
const MAX_DIFF_LINES: usize = 500;
const MAX_TREE_FILES: usize = 500;

fn parse_diff_limit(value: &str) -> std::result::Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| "diff line limit must be an integer".to_string())?;
    if (20..=MAX_DIFF_LINES).contains(&limit) {
        Ok(limit)
    } else {
        Err(format!(
            "diff line limit must be between 20 and {MAX_DIFF_LINES}"
        ))
    }
}

/// CLI arguments for controlling the live TUI associated with a repository.
#[derive(Args, Debug)]
pub(crate) struct AgentArgs {
    /// Repository path (any directory inside the running TUI's worktree).
    #[arg(default_value = ".")]
    pub path: Utf8PathBuf,
    /// Override the derived Unix-socket path.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Emit single-line JSON.
    #[arg(long)]
    pub compact: bool,
    /// Operation to perform against the running TUI.
    #[command(subcommand)]
    pub operation: AgentOperation,
}

/// Operations understood by the local codescope control protocol.
#[derive(Subcommand, Debug)]
pub(crate) enum AgentOperation {
    /// Read the live selection, changed tree, focused diff, relationships, and AI result.
    Context {
        /// Maximum focused diff rows returned (20-500).
        #[arg(long, default_value_t = DEFAULT_DIFF_LINES, value_parser = parse_diff_limit)]
        max_diff_lines: usize,
    },
    /// Move the visible changed-tree cursor to a directory, file, or symbol.
    Focus(FocusArgs),
    /// Ask a question about the visible selection and regenerate its validated explanation.
    Ask {
        /// Question the generated intent and diagram should answer.
        question: String,
    },
    /// Revise the current generated explanation while retaining its validated prior design.
    Feedback {
        /// Feedback for the current selection's next generated explanation.
        feedback: String,
    },
    /// Inspect or mutate the live renderer-native diagram draft.
    Diagram(DiagramArgs),
    /// Refresh Git and analysis state in the running application.
    Refresh,
    /// Print the socket path for this repository without connecting.
    Socket,
}

/// Incremental diagram editor commands.
#[derive(Args, Debug)]
pub(crate) struct DiagramArgs {
    /// Diagram operation.
    #[command(subcommand)]
    operation: DiagramOperation,
}

/// The controller and internal AI use the same serialized [`DiagramCommand`] operations.
#[derive(Subcommand, Debug)]
enum DiagramOperation {
    /// Return the complete current draft for the visible selection.
    Show,
    /// Apply one shared editor command encoded as JSON.
    Apply {
        /// JSON object such as `{"op":"set_intent","intent":"Explain the new flow."}`.
        command: String,
    },
    /// Clear all forms, boxes, relationships, intent, and evidence in the current draft.
    Reset,
    /// Validate and publish the current draft as the visible AI summary.
    Finish,
}

/// Stable focus selectors. A directory is exclusive with a file; a symbol requires a file.
#[derive(Args, Debug)]
pub(crate) struct FocusArgs {
    /// Repo-relative changed directory to summarize.
    #[arg(long, conflicts_with = "file")]
    directory: Option<String>,
    /// Repo-relative changed file to inspect.
    #[arg(long, required_unless_present = "directory")]
    file: Option<String>,
    /// Changed symbol name inside `--file`.
    #[arg(long, requires = "file")]
    symbol: Option<String>,
    /// Optional zero-based symbol line to disambiguate duplicate names.
    #[arg(long, requires = "symbol")]
    line: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum AgentRequest {
    Context {
        max_diff_lines: usize,
    },
    Focus {
        directory: Option<String>,
        file: Option<String>,
        symbol: Option<String>,
        line: Option<u32>,
    },
    Ask {
        question: String,
    },
    Feedback {
        feedback: String,
    },
    DiagramGet,
    DiagramApply {
        command: DiagramCommand,
    },
    Refresh,
}

/// Keeps the listener task and its filesystem entry alive for exactly one TUI session.
pub(crate) struct AgentServer {
    path: Option<PathBuf>,
    task: tokio::task::JoinHandle<()>,
}

impl AgentServer {
    pub(crate) async fn start(
        repo_root: Utf8PathBuf,
        snapshots: watch::Receiver<UiSnapshot>,
        controls: mpsc::Sender<Action>,
    ) -> Result<Self> {
        #[cfg(unix)]
        {
            let path = socket_path(&repo_root);
            bind_server(path, repo_root, snapshots, controls).await
        }
        #[cfg(not(unix))]
        {
            let _ = (repo_root, snapshots, controls);
            tracing::warn!("live agent control is unavailable: Unix sockets are not supported");
            Ok(Self {
                path: None,
                task: tokio::spawn(async {}),
            })
        }
    }
}

impl Drop for AgentServer {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(path) = &self.path {
            if let Err(error) = std::fs::remove_file(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), %error, "could not remove agent socket");
                }
            }
        }
    }
}

#[cfg(unix)]
async fn bind_server(
    path: PathBuf,
    repo_root: Utf8PathBuf,
    snapshots: watch::Receiver<UiSnapshot>,
    controls: mpsc::Sender<Action>,
) -> Result<AgentServer> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create socket directory {}", parent.display()))?;
    }
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(&path).await.is_ok() {
                anyhow::bail!(
                    "another codescope session is already listening at {}",
                    path.display()
                );
            }
            std::fs::remove_file(&path).with_context(|| {
                format!("cannot remove stale codescope socket {}", path.display())
            })?;
            UnixListener::bind(&path)
                .with_context(|| format!("cannot bind agent socket {}", path.display()))?
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot bind agent socket {}", path.display()))
        }
    };
    set_owner_only(&path)?;
    let task_path = path.clone();
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::warn!(%error, "agent socket stopped accepting connections");
                    break;
                }
            };
            let snapshot = snapshots.borrow().clone();
            let controls = controls.clone();
            let repo_root = repo_root.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_connection(stream, &repo_root, &snapshot, &controls).await
                {
                    tracing::debug!(%error, "agent protocol request failed");
                }
            });
        }
        let _ = std::fs::remove_file(task_path);
    });
    tracing::info!(path = %path.display(), "agent control socket ready");
    Ok(AgentServer {
        path: Some(path),
        task,
    })
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot secure agent socket {}", path.display()))
}

#[cfg(unix)]
async fn serve_connection(
    mut stream: UnixStream,
    repo_root: &Utf8PathBuf,
    snapshot: &UiSnapshot,
    controls: &mpsc::Sender<Action>,
) -> Result<()> {
    let response = match read_request(&mut stream).await {
        Ok(request) => match handle_request(request, repo_root, snapshot, controls).await {
            Ok(result) => response(true, result),
            Err(error) => response(false, json!({ "error": format!("{error:#}") })),
        },
        Err(error) => response(false, json!({ "error": format!("{error:#}") })),
    };
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
async fn read_request(stream: &mut UnixStream) -> Result<AgentRequest> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let end = chunk[..read]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(read, |index| index);
        bytes.extend_from_slice(&chunk[..end]);
        anyhow::ensure!(
            bytes.len() <= MAX_REQUEST_BYTES,
            "request exceeds the {MAX_REQUEST_BYTES}-byte limit"
        );
        if end < read {
            break;
        }
    }
    anyhow::ensure!(!bytes.is_empty(), "empty agent protocol request");
    serde_json::from_slice(&bytes).context("invalid agent protocol JSON")
}

async fn handle_request(
    request: AgentRequest,
    repo_root: &Utf8PathBuf,
    snapshot: &UiSnapshot,
    controls: &mpsc::Sender<Action>,
) -> Result<Value> {
    match request {
        AgentRequest::Context { max_diff_lines } => Ok(context_view(
            repo_root,
            snapshot,
            max_diff_lines.clamp(20, MAX_DIFF_LINES),
        )),
        AgentRequest::Focus {
            directory,
            file,
            symbol,
            line,
        } => {
            let target = resolve_focus(snapshot, directory, file, symbol, line)?;
            controls
                .send(Action::AgentFocus(target.clone()))
                .await
                .context("the TUI control loop has stopped")?;
            Ok(json!({
                "accepted": true,
                "target": summary_key_view(&target),
                "note": "focus is applied asynchronously; call context to observe the resulting snapshot"
            }))
        }
        AgentRequest::Ask { question } => {
            validate_guidance(&question)?;
            controls
                .send(Action::AgentAsk(question))
                .await
                .context("the TUI control loop has stopped")?;
            Ok(accepted_generation(snapshot, "question"))
        }
        AgentRequest::Feedback { feedback } => {
            validate_guidance(&feedback)?;
            controls
                .send(Action::AgentFeedback(feedback))
                .await
                .context("the TUI control loop has stopped")?;
            Ok(accepted_generation(snapshot, "feedback"))
        }
        AgentRequest::DiagramGet => Ok(json!({
            "selection": selected_view(snapshot),
            "draft": snapshot.diagram_draft,
            "published_plan": snapshot.semantic.plan,
            "validation": snapshot.semantic.report,
        })),
        AgentRequest::DiagramApply { command } => {
            controls
                .send(Action::AgentDiagram(command))
                .await
                .context("the TUI control loop has stopped")?;
            Ok(json!({
                "accepted": true,
                "selection": selected_view(snapshot),
                "note": "the edit is applied asynchronously; call diagram show to inspect the resulting draft"
            }))
        }
        AgentRequest::Refresh => {
            controls
                .send(Action::RefreshGit)
                .await
                .context("the TUI control loop has stopped")?;
            Ok(json!({
                "accepted": true,
                "note": "refresh is asynchronous; call context until refreshing is false and epoch advances"
            }))
        }
    }
}

fn accepted_generation(snapshot: &UiSnapshot, kind: &str) -> Value {
    json!({
        "accepted": true,
        "kind": kind,
        "selection": selected_view(snapshot),
        "note": "generation is asynchronous; poll context.ai.status and context.ai.plan"
    })
}

fn validate_guidance(text: &str) -> Result<()> {
    anyhow::ensure!(!text.trim().is_empty(), "guidance must not be empty");
    anyhow::ensure!(
        text.chars().count() <= 2_000,
        "guidance exceeds the 2000-character limit"
    );
    Ok(())
}

fn resolve_focus(
    snapshot: &UiSnapshot,
    directory: Option<String>,
    file: Option<String>,
    symbol: Option<String>,
    line: Option<u32>,
) -> Result<AiSummaryKey> {
    if let Some(directory) = directory {
        let directory = directory.trim().trim_end_matches('/');
        anyhow::ensure!(!directory.is_empty(), "directory must not be empty");
        let exists = snapshot.files.iter().any(|file| {
            codescope_tui::file_rows::directory_prefixes(&file.path)
                .iter()
                .any(|candidate| candidate == directory)
        });
        anyhow::ensure!(
            exists,
            "{directory:?} is not a changed directory in the live tree"
        );
        return Ok(AiSummaryKey::Directory(directory.to_string()));
    }
    let file = file.context("focus requires --directory or --file")?;
    let row = snapshot
        .files
        .iter()
        .find(|row| row.path == file)
        .with_context(|| format!("{file:?} is not a changed file in the live tree"))?;
    let Some(symbol) = symbol else {
        return Ok(AiSummaryKey::File(file));
    };
    let matches = row
        .symbols
        .iter()
        .filter(|candidate| {
            candidate.name == symbol
                && line.is_none_or(|line| candidate.position.is_some_and(|pos| pos.0 == line))
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !matches.is_empty(),
        "symbol {symbol:?} is not available in {file:?}; wait for semantic loading or inspect context"
    );
    anyhow::ensure!(
        matches.len() == 1,
        "symbol {symbol:?} is ambiguous in {file:?}; pass --line"
    );
    let position = matches[0]
        .position
        .context("the symbol has no selectable source position")?;
    Ok(AiSummaryKey::Symbol {
        file,
        name: symbol,
        position: Some(position),
    })
}

fn context_view(repo_root: &Utf8Path, snapshot: &UiSnapshot, max_diff_lines: usize) -> Value {
    let diff_rows = snapshot
        .diff
        .rows
        .iter()
        .take(max_diff_lines)
        .map(diff_row_view)
        .collect::<Vec<_>>();
    let files = snapshot
        .files
        .iter()
        .take(MAX_TREE_FILES)
        .map(|file| {
            let key = AiSummaryKey::File(file.path.clone());
            json!({
                "path": file.path,
                "status": file.status,
                "lines": { "added": file.added_lines, "removed": file.removed_lines },
                "semantic_state": semantic_state(file.semantic),
                "ai_summary": ai_summary_state(snapshot.ai_summary_state(&key)),
                "symbols": file.symbols.iter().map(|symbol| {
                    let key = AiSummaryKey::Symbol {
                        file: file.path.clone(),
                        name: symbol.name.clone(),
                        position: symbol.position,
                    };
                    json!({
                        "name": symbol.name,
                        "change": symbol.change,
                        "position": symbol.position.map(|(line, column)| json!({ "line": line, "column": column })),
                        "ai_summary": ai_summary_state(snapshot.ai_summary_state(&key)),
                    })
                }).collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    let impact = &snapshot.impact;
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "repository": {
            "root": repo_root,
            "name": snapshot.repo.repo_name,
            "branch": snapshot.repo.branch,
            "base": snapshot.base_ref,
            "ahead": snapshot.repo.ahead,
            "behind": snapshot.repo.behind,
        },
        "live": {
            "epoch": snapshot.epoch,
            "scope": snapshot.scope,
            "refreshing": snapshot.refreshing,
            "selection": selected_view(snapshot),
            "status": { "text": snapshot.status.text, "level": format!("{:?}", snapshot.status.level).to_lowercase() },
        },
        "changed_tree": {
            "files": files,
            "truncated": snapshot.files.len() > MAX_TREE_FILES,
        },
        "focused_diff": {
            "file": snapshot.diff.title,
            "symbol": snapshot.diff.focused_symbol,
            "total_hunks": snapshot.diff.total_hunks,
            "rows": diff_rows,
            "truncated": snapshot.diff.rows.len() > max_diff_lines,
        },
        "impact": {
            "selected_change": impact.selected_change.as_ref().map(|change| json!({
                "file": change.file,
                "label": change.label,
                "change": change.change,
                "interpretation": change.interpretation,
            })),
            "callers": impact_list_view(&impact.callers),
            "downstream": impact_list_view(&impact.downstream),
            "note": impact.note,
        },
        "ai": {
            "status": snapshot.ai,
            "provider": snapshot.ai_provider,
            "model": snapshot.ai_model,
            "active_agent_guidance": snapshot.semantic.note.strip_prefix("Agent ").map(|_| snapshot.semantic.note.as_str()),
            "plan": snapshot.semantic.plan,
            "validation": snapshot.semantic.report,
            "draft": snapshot.diagram_draft,
        },
        "capabilities": {
            "context": "read this live, bounded view",
            "focus": "focus exactly one changed directory, file, or loaded symbol",
            "ask": "generate a validated answer for the visible selection",
            "feedback": "revise that selection's prior validated plan",
            "diagram": "inspect and incrementally create/update/delete the same boxes and relationships used by the internal AI",
            "refresh": "refresh Git and analysis state",
            "workflow": [
                "codescope agent . context",
                "codescope agent . focus --file path/to/file.rs --symbol symbol_name",
                "codescope agent . ask 'What is the failure path introduced here?'",
                "codescope agent . context",
                "codescope agent . feedback 'Emphasize the boundary with the cache module'",
                "codescope agent . diagram show",
                "codescope agent . diagram apply '{\"op\":\"update_edge\",\"form_id\":\"main\",\"from\":\"n1\",\"to\":\"n2\",\"patch\":{\"label\":\"passes parsed request\"}}'",
                "codescope agent . diagram finish"
            ],
            "constraints": [
                "local owner-only Unix socket",
                "read-only repository access",
                "no shell execution",
                "questions and feedback are guidance, not evidence",
                "draft edits use the shared typed diagram API",
                "finish validates AI/controller output before publication"
            ]
        }
    })
}

fn selected_view(snapshot: &UiSnapshot) -> Value {
    if snapshot.diff.title.ends_with('/') {
        return json!({ "kind": "directory", "path": snapshot.diff.title.trim_end_matches('/') });
    }
    if let Some(symbol) = &snapshot.diff.focused_symbol {
        return json!({ "kind": "symbol", "file": snapshot.diff.title, "symbol": symbol });
    }
    if snapshot.diff.title.is_empty() {
        Value::Null
    } else {
        json!({ "kind": "file", "path": snapshot.diff.title })
    }
}

fn summary_key_view(key: &AiSummaryKey) -> Value {
    match key {
        AiSummaryKey::Directory(path) => json!({ "kind": "directory", "path": path }),
        AiSummaryKey::File(path) => json!({ "kind": "file", "path": path }),
        AiSummaryKey::Symbol {
            file,
            name,
            position,
        } => json!({
            "kind": "symbol",
            "file": file,
            "symbol": name,
            "position": position.map(|(line, column)| json!({ "line": line, "column": column })),
        }),
    }
}

fn diff_row_view(row: &DiffRow) -> Value {
    match row {
        DiffRow::HunkHeader(text) => json!({ "kind": "hunk", "text": text }),
        DiffRow::Add { new_ln, text } => {
            json!({ "kind": "add", "new_line": new_ln, "text": text })
        }
        DiffRow::Del { old_ln, text } => {
            json!({ "kind": "delete", "old_line": old_ln, "text": text })
        }
        DiffRow::Context {
            old_ln,
            new_ln,
            text,
        } => json!({ "kind": "context", "old_line": old_ln, "new_line": new_ln, "text": text }),
    }
}

fn impact_list_view(list: &codescope_tui::snapshot::ImpactList) -> Value {
    json!({
        "state": impact_state(list.state),
        "partial": list.partial,
        "rows": list.rows.iter().map(|row| json!({
            "label": row.label,
            "relation": row.relation,
            "changed": row.changed,
            "has_diagnostic": row.has_diagnostic,
        })).collect::<Vec<_>>()
    })
}

fn ai_summary_state(state: AiSummaryState) -> &'static str {
    match state {
        AiSummaryState::NotGenerated => "not_generated",
        AiSummaryState::Generating => "generating",
        AiSummaryState::Ready => "ready",
        AiSummaryState::Failed => "failed",
    }
}

fn semantic_state(state: FileSemanticLoad) -> &'static str {
    match state {
        FileSemanticLoad::Unloaded => "unloaded",
        FileSemanticLoad::Loading => "loading",
        FileSemanticLoad::Ready => "ready",
        FileSemanticLoad::Unsupported => "unsupported",
        FileSemanticLoad::Failed => "failed",
    }
}

fn impact_state(state: ImpactLoadState) -> &'static str {
    match state {
        ImpactLoadState::Idle => "idle",
        ImpactLoadState::Loading => "loading",
        ImpactLoadState::Ready => "ready",
        ImpactLoadState::Unavailable => "unavailable",
    }
}

#[cfg(unix)]
fn response(ok: bool, result: Value) -> Value {
    json!({ "protocol_version": PROTOCOL_VERSION, "ok": ok, "result": result })
}

/// Stable, dependency-free FNV-1a naming keeps socket paths short enough for Unix limits.
pub(crate) fn socket_path(repo_root: &camino::Utf8Path) -> PathBuf {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in repo_root.as_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    std::env::temp_dir().join(format!("codescope-agent-{hash:016x}.sock"))
}

pub(crate) async fn run_client(args: &AgentArgs) -> Result<()> {
    let repo = GitRepo::discover(&args.path)
        .await
        .context("not a git repository (cannot locate a running codescope session)")?;
    let path = args
        .socket
        .clone()
        .unwrap_or_else(|| socket_path(repo.toplevel()));
    if matches!(args.operation, AgentOperation::Socket) {
        return emit(&json!({ "socket": path }), args.compact);
    }
    #[cfg(not(unix))]
    anyhow::bail!("live agent control requires Unix-socket support");
    #[cfg(unix)]
    {
        let request = match &args.operation {
            AgentOperation::Context { max_diff_lines } => AgentRequest::Context {
                max_diff_lines: *max_diff_lines,
            },
            AgentOperation::Focus(focus) => AgentRequest::Focus {
                directory: focus.directory.clone(),
                file: focus.file.clone(),
                symbol: focus.symbol.clone(),
                line: focus.line,
            },
            AgentOperation::Ask { question } => AgentRequest::Ask {
                question: question.clone(),
            },
            AgentOperation::Feedback { feedback } => AgentRequest::Feedback {
                feedback: feedback.clone(),
            },
            AgentOperation::Diagram(diagram) => match &diagram.operation {
                DiagramOperation::Show => AgentRequest::DiagramGet,
                DiagramOperation::Apply { command } => AgentRequest::DiagramApply {
                    command: serde_json::from_str(command)
                        .context("diagram command is not valid shared editor JSON")?,
                },
                DiagramOperation::Reset => AgentRequest::DiagramApply {
                    command: DiagramCommand::Reset,
                },
                DiagramOperation::Finish => AgentRequest::DiagramApply {
                    command: DiagramCommand::Finish,
                },
            },
            AgentOperation::Refresh => AgentRequest::Refresh,
            AgentOperation::Socket => unreachable!(),
        };
        let mut stream = UnixStream::connect(&path).await.with_context(|| {
            format!(
                "cannot connect to {}; start codescope in this repository first",
                path.display()
            )
        })?;
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        stream.write_all(&bytes).await?;
        stream.shutdown().await?;
        let mut response_bytes = Vec::new();
        stream.read_to_end(&mut response_bytes).await?;
        let response: Value = serde_json::from_slice(&response_bytes)
            .context("running codescope returned invalid protocol JSON")?;
        anyhow::ensure!(
            response.get("ok").and_then(Value::as_bool) == Some(true),
            "agent request rejected: {}",
            response
                .pointer("/result/error")
                .and_then(Value::as_str)
                .unwrap_or("unknown protocol error")
        );
        emit(&response, args.compact)
    }
}

fn emit(value: &Value, compact: bool) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if compact {
        serde_json::to_writer(&mut out, value)?;
    } else {
        serde_json::to_writer_pretty(&mut out, value)?;
    }
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codescope_tui::snapshot::{DiffPane, FileRow, SymbolRow};

    fn live_snapshot() -> UiSnapshot {
        UiSnapshot {
            files: vec![FileRow {
                semantic: FileSemanticLoad::Ready,
                path: "src/api.rs".to_string(),
                status: "M",
                changed_symbol_count: 1,
                added_lines: 3,
                removed_lines: 1,
                symbols: vec![SymbolRow {
                    name: "serve".to_string(),
                    change: "modified",
                    confidence: "",
                    has_diagnostic: false,
                    position: Some((12, 4)),
                }],
                expanded: true,
            }],
            diff: DiffPane {
                title: "src/api.rs".to_string(),
                focused_symbol: Some("serve".to_string()),
                rows: vec![DiffRow::Add {
                    new_ln: 13,
                    text: "listen();".to_string(),
                }],
                current_hunk: 1,
                total_hunks: 1,
            },
            ..UiSnapshot::default()
        }
    }

    #[test]
    fn socket_name_is_stable_and_short() {
        let first = socket_path(camino::Utf8Path::new("/tmp/example/repo"));
        let second = socket_path(camino::Utf8Path::new("/tmp/example/repo"));
        assert_eq!(first, second);
        assert!(first.to_string_lossy().len() < 100);
    }

    #[test]
    fn resolves_loaded_symbol_to_exact_tree_identity() {
        let target = resolve_focus(
            &live_snapshot(),
            None,
            Some("src/api.rs".to_string()),
            Some("serve".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(
            target,
            AiSummaryKey::Symbol {
                file: "src/api.rs".to_string(),
                name: "serve".to_string(),
                position: Some((12, 4)),
            }
        );
    }

    #[test]
    fn context_is_live_bounded_and_documents_agent_workflow() {
        let view = context_view(
            camino::Utf8Path::new("/tmp/example/repo"),
            &live_snapshot(),
            20,
        );
        assert_eq!(view["live"]["selection"]["kind"], "symbol");
        assert_eq!(view["focused_diff"]["rows"][0]["kind"], "add");
        assert!(view["capabilities"]["workflow"].as_array().unwrap().len() >= 4);
    }

    #[tokio::test]
    async fn ask_is_forwarded_as_a_typed_ui_action() {
        let snapshot = live_snapshot();
        let (tx, mut rx) = mpsc::channel(1);
        let result = handle_request(
            AgentRequest::Ask {
                question: "What changed?".to_string(),
            },
            &Utf8PathBuf::from("/tmp/example/repo"),
            &snapshot,
            &tx,
        )
        .await
        .unwrap();
        assert_eq!(result["accepted"], true);
        assert_eq!(
            rx.recv().await,
            Some(Action::AgentAsk("What changed?".to_string()))
        );
    }

    #[tokio::test]
    async fn diagram_command_is_forwarded_without_translation() {
        let snapshot = live_snapshot();
        let (tx, mut rx) = mpsc::channel(1);
        let command = DiagramCommand::SetIntent {
            intent: "Show the request entering the bounded queue.".to_string(),
        };
        let result = handle_request(
            AgentRequest::DiagramApply {
                command: command.clone(),
            },
            &Utf8PathBuf::from("/tmp/example/repo"),
            &snapshot,
            &tx,
        )
        .await
        .unwrap();
        assert_eq!(result["accepted"], true);
        assert_eq!(rx.recv().await, Some(Action::AgentDiagram(command)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_round_trip_returns_context_and_cleans_up() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("agent.sock");
        let (_snapshot_tx, snapshot_rx) = watch::channel(live_snapshot());
        let (control_tx, _control_rx) = mpsc::channel(1);
        let server = bind_server(
            path.clone(),
            Utf8PathBuf::from("/tmp/example/repo"),
            snapshot_rx,
            control_tx,
        )
        .await
        .unwrap();

        let mut stream = UnixStream::connect(&path).await.unwrap();
        let request = serde_json::to_vec(&AgentRequest::Context { max_diff_lines: 20 }).unwrap();
        stream.write_all(&request).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        stream.shutdown().await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        let reply: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["result"]["live"]["selection"]["kind"], "symbol");

        drop(server);
        assert!(!path.exists());
    }
}
