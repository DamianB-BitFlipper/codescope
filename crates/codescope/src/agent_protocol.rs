//! Local control protocol for a running codescope TUI.
//!
//! One owner-only Unix socket is derived from the repository root. Requests are bounded,
//! JSON encoded, and translated into the same typed actions used by the terminal UI; the
//! protocol deliberately exposes neither a shell nor an unvalidated rendering primitive.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use std::path::Path;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Subcommand};
use codescope_core::DiagramCommand;
use codescope_git::GitRepo;
use codescope_tui::snapshot::{
    AiSummaryKey, AiSummaryState, AiToolCallActivityState, DiffRow, FileSemanticLoad,
    ImpactLoadState, UiSnapshot,
};
use codescope_tui::Action;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

const PROTOCOL_VERSION: u8 = 3;
#[cfg(unix)]
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const DEFAULT_DIFF_LINES: usize = 160;
const MAX_DIFF_LINES: usize = 500;
const MAX_TREE_FILES: usize = 500;
const DIAGRAM_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
static AGENT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

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
    /// Read authoritative hunk ids and line coordinates from the captured change-set.
    Diff(DiffArgs),
    /// Move the visible changed-tree cursor to a directory, file, or symbol.
    Focus(FocusArgs),
    /// Inspect or mutate the live renderer-native diagram draft.
    Diagram(DiagramArgs),
    /// Refresh Git and analysis state in the running application.
    Refresh,
    /// Print the socket path for this repository without connecting.
    Socket,
}

/// Authoritative focused-diff lookup for validator-compatible code references.
#[derive(Args, Debug)]
pub(crate) struct DiffArgs {
    /// Repo-relative changed file; defaults to the focused file.
    #[arg(long)]
    file: Option<String>,
    /// Zero-based hunk to read. Omit to list all hunk headers.
    #[arg(long)]
    hunk: Option<usize>,
    /// Diff-body row offset within the selected hunk.
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Maximum diff-body rows returned (20-500).
    #[arg(long, default_value_t = DEFAULT_DIFF_LINES, value_parser = parse_diff_limit)]
    max_lines: usize,
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
    #[command(alias = "show")]
    Inspect,
    /// Apply one shared editor command encoded as JSON.
    #[command(alias = "apply")]
    Edit {
        /// JSON object such as `{"op":"set_intent","intent":"Explain the new flow."}`.
        command: String,
    },
    /// Print the shared edit and inspection tool schemas without connecting to a TUI.
    Schema,
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
    Diff {
        file: Option<String>,
        hunk: Option<usize>,
        offset: usize,
        max_lines: usize,
    },
    Focus {
        directory: Option<String>,
        file: Option<String>,
        symbol: Option<String>,
        line: Option<u32>,
    },
    DiagramGet,
    DiagramApply {
        request_id: u64,
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
    let diagram_lock = Arc::new(tokio::sync::Mutex::new(()));
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::warn!(%error, "agent socket stopped accepting connections");
                    break;
                }
            };
            let snapshots = snapshots.clone();
            let controls = controls.clone();
            let repo_root = repo_root.clone();
            let diagram_lock = diagram_lock.clone();
            tokio::spawn(async move {
                if let Err(error) =
                    serve_connection(stream, &repo_root, snapshots, &controls, &diagram_lock).await
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
    mut snapshots: watch::Receiver<UiSnapshot>,
    controls: &mpsc::Sender<Action>,
    diagram_lock: &tokio::sync::Mutex<()>,
) -> Result<()> {
    let response = match read_request(&mut stream).await {
        Ok(request) => {
            match handle_request(request, repo_root, &mut snapshots, controls, diagram_lock).await {
                Ok(result) => response(true, result),
                Err(error) => response(false, json!({ "error": format!("{error:#}") })),
            }
        }
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
    snapshots: &mut watch::Receiver<UiSnapshot>,
    controls: &mpsc::Sender<Action>,
    diagram_lock: &tokio::sync::Mutex<()>,
) -> Result<Value> {
    match request {
        AgentRequest::Context { max_diff_lines } => {
            let snapshot = snapshots.borrow().clone();
            Ok(context_view(
                repo_root,
                &snapshot,
                max_diff_lines.clamp(20, MAX_DIFF_LINES),
            ))
        }
        AgentRequest::Diff {
            file,
            hunk,
            offset,
            max_lines,
        } => {
            let snapshot = snapshots.borrow().clone();
            diff_view(
                &snapshot,
                file.as_deref(),
                hunk,
                offset,
                max_lines.clamp(20, MAX_DIFF_LINES),
            )
        }
        AgentRequest::Focus {
            directory,
            file,
            symbol,
            line,
        } => {
            let snapshot = snapshots.borrow().clone();
            let target = resolve_focus(&snapshot, directory, file, symbol, line)?;
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
        AgentRequest::DiagramGet => {
            let snapshot = snapshots.borrow().clone();
            Ok(diagram_view(&snapshot))
        }
        AgentRequest::DiagramApply {
            request_id,
            command,
        } => {
            // Serialize external writers so the latest-value snapshot cannot skip over an
            // acknowledgement before its caller observes it.
            let _guard = diagram_lock.lock().await;
            let previous_revision = snapshots
                .borrow()
                .agent_diagram_result
                .as_ref()
                .map_or(0, |result| result.revision);
            controls
                .send(Action::AgentDiagram {
                    request_id,
                    command,
                })
                .await
                .context("the TUI control loop has stopped")?;
            let snapshot =
                wait_for_diagram_result(snapshots, request_id, previous_revision).await?;
            let result = snapshot
                .agent_diagram_result
                .as_ref()
                .context("dispatcher omitted the diagram command result")?;
            Ok(json!({
                "accepted": result.accepted,
                "published": result.published,
                "revision": result.revision,
                "summary": result.summary,
                "error": result.error,
                "selection": selected_view(&snapshot),
                "draft": snapshot.diagram_draft,
                "published_plan": snapshot.semantic.plan,
                "validation": snapshot.semantic.report,
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

fn diagram_view(snapshot: &UiSnapshot) -> Value {
    json!({
        "selection": selected_view(snapshot),
        "revision": snapshot.agent_diagram_result.as_ref().map(|result| result.revision).unwrap_or(0),
        "draft": snapshot.diagram_draft,
        "published_plan": snapshot.semantic.plan,
        "validation": snapshot.semantic.report,
    })
}

async fn wait_for_diagram_result(
    snapshots: &mut watch::Receiver<UiSnapshot>,
    request_id: u64,
    previous_revision: u64,
) -> Result<UiSnapshot> {
    tokio::time::timeout(DIAGRAM_RESPONSE_TIMEOUT, async {
        loop {
            let snapshot = snapshots.borrow_and_update().clone();
            if snapshot
                .agent_diagram_result
                .as_ref()
                .is_some_and(|result| {
                    result.request_id == request_id && result.revision > previous_revision
                })
            {
                return Ok(snapshot);
            }
            snapshots
                .changed()
                .await
                .context("the TUI snapshot stream has stopped")?;
        }
    })
    .await
    .context("timed out waiting for the diagram edit to reach the dispatcher")?
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
        let exists =
            codescope_tui::file_rows::project(&snapshot.files, &std::collections::HashSet::new())
                .iter()
                .any(|row| {
                    matches!(
                        row,
                        codescope_tui::file_rows::ProjectedRow::Directory { path, .. }
                            if path == directory
                    )
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

fn diff_view(
    snapshot: &UiSnapshot,
    requested_file: Option<&str>,
    requested_hunk: Option<usize>,
    offset: usize,
    max_lines: usize,
) -> Result<Value> {
    anyhow::ensure!(
        snapshot.agent_changeset_epoch == snapshot.epoch,
        "the captured Git facts are refreshing; wait for context.live.epoch to become current"
    );
    let changeset = snapshot
        .agent_changeset
        .as_deref()
        .context("the current Git snapshot is not ready")?;
    let default_file =
        (!snapshot.diff.title.ends_with('/')).then_some(snapshot.diff.title.as_str());
    let file = requested_file.or(default_file).context(
        "the current selection is a directory; pass --file with a changed path from context",
    )?;
    anyhow::ensure!(!file.is_empty(), "there is no focused changed file");
    let change = changeset
        .files
        .iter()
        .find(|change| change.path.as_str() == file)
        .with_context(|| {
            format!(
                "{file:?} is not a changed file in the live {:?} scope",
                snapshot.scope
            )
        })?;

    let Some(hunk_index) = requested_hunk else {
        anyhow::ensure!(offset == 0, "--offset requires --hunk");
        return Ok(json!({
            "epoch": snapshot.epoch,
            "scope": snapshot.scope,
            "selection": selected_view(snapshot),
            "file": file,
            "status": change.status,
            "old_path": change.old_path,
            "binary": change.binary,
            "hunk_count": change.hunks.len(),
            "hunks": change.hunks.iter().enumerate().map(|(index, hunk)| json!({
                "hunk": index,
                "old_start": hunk.old_start,
                "old_len": hunk.old_len,
                "new_start": hunk.new_start,
                "new_len": hunk.new_len,
                "section": hunk.section,
                "rows": hunk.lines.len(),
                "added": hunk.count_added(),
                "removed": hunk.count_deleted(),
            })).collect::<Vec<_>>(),
            "note": "pass --hunk N to read exact old/new line coordinates for code_refs"
        }));
    };

    let hunk = change
        .hunks
        .get(hunk_index)
        .with_context(|| format!("hunk {hunk_index} does not exist in {file:?}"))?;
    let page = hunk
        .lines
        .iter()
        .skip(offset)
        .take(max_lines)
        .map(|line| {
            json!({
                "kind": match line.kind {
                    codescope_core::DiffLineKind::Add => "add",
                    codescope_core::DiffLineKind::Del => "delete",
                    codescope_core::DiffLineKind::Context => "context",
                },
                "hunk": hunk_index,
                "old_line": line.old_ln,
                "new_line": line.new_ln,
                "text": line.text,
            })
        })
        .collect::<Vec<_>>();
    let next_offset = (offset + page.len() < hunk.lines.len()).then_some(offset + page.len());
    Ok(json!({
        "epoch": snapshot.epoch,
        "scope": snapshot.scope,
        "selection": selected_view(snapshot),
        "file": file,
        "hunk": hunk_index,
        "header": {
            "old_start": hunk.old_start,
            "old_len": hunk.old_len,
            "new_start": hunk.new_start,
            "new_len": hunk.new_len,
            "section": hunk.section,
        },
        "offset": offset,
        "rows": page,
        "next_offset": next_offset,
        "truncated": next_offset.is_some(),
        "code_ref": {
            "file": file,
            "hunk": hunk_index,
            "side": "old for delete rows; new for add or post-change context rows",
            "line_numbers": "one-based and inclusive"
        }
    }))
}

fn context_view(repo_root: &Utf8Path, snapshot: &UiSnapshot, max_diff_lines: usize) -> Value {
    let mut current_hunk = None;
    let diff_rows = snapshot
        .diff
        .rows
        .iter()
        .take(max_diff_lines)
        .map(|row| {
            if matches!(row, DiffRow::HunkHeader(_)) {
                current_hunk = Some(current_hunk.map_or(0, |index| index + 1));
            }
            diff_row_view(row, current_hunk)
        })
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
            "selected": snapshot.selected_diff.as_ref().filter(|selected| selected.file == snapshot.diff.title).map(|selected| json!({
                "file": selected.file,
                "text": selected.text,
                "truncated": selected.truncated,
            })),
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
            "activity": {
                "active": snapshot.ai_activity.active,
                "waiting_for_model": snapshot.ai_activity.waiting_for_model,
                "calls": snapshot.ai_activity.calls.iter().map(|call| json!({
                    "name": call.name,
                    "detail": call.detail,
                    "error": call.error,
                    "state": match call.state {
                        AiToolCallActivityState::Running => "running",
                        AiToolCallActivityState::Succeeded => "succeeded",
                        AiToolCallActivityState::Failed => "failed",
                    },
                })).collect::<Vec<_>>(),
            },
            "plan": snapshot.semantic.plan,
            "validation": snapshot.semantic.report,
            "draft": snapshot.diagram_draft,
        },
        "capabilities": {
            "context": "read this live, bounded view",
            "focus": "focus exactly one changed directory, file, or loaded symbol",
            "diff": "read authoritative zero-based hunk ids and exact old/new line coordinates for any changed file",
            "selected": "focused_diff.selected is the exact human-highlighted diff excerpt",
            "diagram": "inspect and synchronously create/update/delete the same boxes and relationships used by the internal AI",
            "refresh": "refresh Git and analysis state",
            "workflow": [
                "codescope agent . context",
                "codescope agent . focus --file path/to/file.rs --symbol symbol_name",
                "codescope agent . context",
                "codescope agent . diff --file path/to/file.rs",
                "codescope agent . diff --file path/to/file.rs --hunk 0",
                "codescope agent . diagram inspect",
                "codescope agent . diagram edit '{\"op\":\"update_edge\",\"form_id\":\"main\",\"from\":\"n1\",\"to\":\"n2\",\"patch\":{\"label\":\"passes parsed request\"}}'",
                "codescope agent . diagram finish"
            ],
            "constraints": [
                "local owner-only Unix socket",
                "read-only repository access",
                "no shell execution",
                "the external agent researches with its own code, Git, and language tools",
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

fn diff_row_view(row: &DiffRow, hunk: Option<usize>) -> Value {
    match row {
        DiffRow::HunkHeader(text) => json!({ "kind": "hunk", "hunk": hunk, "text": text }),
        DiffRow::Add { new_ln, text } => {
            json!({ "kind": "add", "hunk": hunk, "new_line": new_ln, "text": text })
        }
        DiffRow::Del { old_ln, text } => {
            json!({ "kind": "delete", "hunk": hunk, "old_line": old_ln, "text": text })
        }
        DiffRow::Context {
            old_ln,
            new_ln,
            text,
        } => {
            json!({ "kind": "context", "hunk": hunk, "old_line": old_ln, "new_line": new_ln, "text": text })
        }
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
    if matches!(
        &args.operation,
        AgentOperation::Diagram(DiagramArgs {
            operation: DiagramOperation::Schema
        })
    ) {
        return emit(
            &json!({
                "tools": codescope_ai::diagram_tools(),
                "finish": {
                    "command": "codescope agent . diagram finish",
                    "description": "Validate and publish the current draft after all edits succeed."
                }
            }),
            args.compact,
        );
    }
    let repo = GitRepo::discover(&args.path)
        .await
        .context("not a git repository (cannot locate a running codescope session)")?;
    codescope_telemetry::set_repository(repo.toplevel().to_string());
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
            AgentOperation::Diff(diff) => AgentRequest::Diff {
                file: diff.file.clone(),
                hunk: diff.hunk,
                offset: diff.offset,
                max_lines: diff.max_lines,
            },
            AgentOperation::Focus(focus) => AgentRequest::Focus {
                directory: focus.directory.clone(),
                file: focus.file.clone(),
                symbol: focus.symbol.clone(),
                line: focus.line,
            },
            AgentOperation::Diagram(diagram) => match &diagram.operation {
                DiagramOperation::Inspect => AgentRequest::DiagramGet,
                DiagramOperation::Edit { command } => AgentRequest::DiagramApply {
                    request_id: AGENT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
                    command: serde_json::from_str(command)
                        .context("diagram command is not valid shared editor JSON")?,
                },
                DiagramOperation::Finish => AgentRequest::DiagramApply {
                    request_id: AGENT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
                    command: DiagramCommand::Finish,
                },
                DiagramOperation::Schema => unreachable!("handled before repository discovery"),
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
    use codescope_tui::snapshot::{
        AgentDiagramResult, DiffPane, FileRow, SelectedDiffContext, SymbolRow,
    };

    fn live_snapshot() -> UiSnapshot {
        UiSnapshot {
            agent_changeset: Some(std::sync::Arc::new(codescope_core::ChangeSet::new(
                codescope_core::ChangeScope::Branch,
                vec![codescope_core::FileChange {
                    path: Utf8PathBuf::from("src/api.rs"),
                    old_path: None,
                    status: codescope_core::FileStatus::Modified,
                    hunks: vec![codescope_core::Hunk {
                        old_start: 12,
                        old_len: 1,
                        new_start: 12,
                        new_len: 2,
                        section: Some("serve".to_string()),
                        lines: vec![
                            codescope_core::DiffLine::context(12, 12, "fn serve() {"),
                            codescope_core::DiffLine::add(13, "listen();"),
                        ],
                    }],
                    binary: false,
                }],
            ))),
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
                rows: vec![
                    DiffRow::HunkHeader("@@ -12,1 +12,2 @@ serve".to_string()),
                    DiffRow::Context {
                        old_ln: 12,
                        new_ln: 12,
                        text: "fn serve() {".to_string(),
                    },
                    DiffRow::Add {
                        new_ln: 13,
                        text: "listen();".to_string(),
                    },
                ],
                current_hunk: 1,
                total_hunks: 1,
                syntax: std::sync::Arc::default(),
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
    fn resolves_only_the_visible_combined_directory_identity() {
        let mut snapshot = live_snapshot();
        let mut first = snapshot.files[0].clone();
        first.path = "sandbox/vm/pkg/internal/a.rs".to_string();
        let mut second = first.clone();
        second.path = "sandbox/vm/pkg/worker/b.rs".to_string();
        snapshot.files = vec![first, second];

        let target = resolve_focus(
            &snapshot,
            Some("sandbox/vm/pkg".to_string()),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            target,
            AiSummaryKey::Directory("sandbox/vm/pkg".to_string())
        );
        assert!(resolve_focus(&snapshot, Some("sandbox".to_string()), None, None, None,).is_err());
    }

    #[test]
    fn context_is_live_bounded_and_documents_agent_workflow() {
        let mut snapshot = live_snapshot();
        snapshot.selected_diff = Some(SelectedDiffContext {
            file: "src/api.rs".to_string(),
            text: "listen();".to_string(),
            truncated: false,
        });
        let view = context_view(camino::Utf8Path::new("/tmp/example/repo"), &snapshot, 20);
        assert_eq!(view["live"]["selection"]["kind"], "symbol");
        assert_eq!(view["focused_diff"]["rows"][0]["kind"], "hunk");
        assert_eq!(view["focused_diff"]["rows"][1]["hunk"], 0);
        assert_eq!(view["ai"]["activity"]["active"], false);
        assert!(view["ai"]["activity"]["calls"].is_array());
        assert_eq!(view["focused_diff"]["selected"]["text"], "listen();");
        assert!(view["capabilities"]["workflow"].as_array().unwrap().len() >= 4);
    }

    #[test]
    fn context_exposes_the_scrubbed_tool_failure_reason() {
        let mut snapshot = live_snapshot();
        snapshot
            .ai_activity
            .calls
            .push(codescope_tui::snapshot::AiToolCallActivity {
                id: "call-1".to_string(),
                name: "git_status_file".to_string(),
                detail: "api.rs".to_string(),
                error: Some("not a changed file".to_string()),
                state: AiToolCallActivityState::Failed,
            });
        let view = context_view(camino::Utf8Path::new("/tmp/example/repo"), &snapshot, 20);
        assert_eq!(
            view["ai"]["activity"]["calls"][0]["error"],
            "not a changed file"
        );
    }

    #[test]
    fn diff_lists_hunks_then_returns_exact_code_reference_coordinates() {
        let snapshot = live_snapshot();
        let overview = diff_view(&snapshot, Some("src/api.rs"), None, 0, 20).unwrap();
        assert_eq!(overview["hunks"][0]["hunk"], 0);
        assert_eq!(overview["hunks"][0]["added"], 1);

        let hunk = diff_view(&snapshot, Some("src/api.rs"), Some(0), 0, 20).unwrap();
        assert_eq!(hunk["rows"][1]["kind"], "add");
        assert_eq!(hunk["rows"][1]["new_line"], 13);
        assert_eq!(hunk["code_ref"]["hunk"], 0);

        let mut refreshing = snapshot;
        refreshing.epoch = refreshing.epoch.next();
        assert!(diff_view(&refreshing, Some("src/api.rs"), None, 0, 20)
            .unwrap_err()
            .to_string()
            .contains("refreshing"));
    }

    #[tokio::test]
    async fn diagram_command_is_forwarded_without_translation() {
        let mut snapshot = live_snapshot();
        snapshot.agent_diagram_result = Some(AgentDiagramResult {
            request_id: 42,
            revision: 6,
            accepted: false,
            published: false,
            summary: None,
            error: Some("older process reused its local request id".to_string()),
        });
        let (tx, mut rx) = mpsc::channel(1);
        let (snapshot_tx, mut snapshots) = watch::channel(snapshot);
        let command = DiagramCommand::SetIntent {
            intent: "Show the request entering the bounded queue.".to_string(),
        };
        let expected = command.clone();
        let responder = tokio::spawn(async move {
            let Some(Action::AgentDiagram {
                request_id,
                command,
            }) = rx.recv().await
            else {
                panic!("expected an agent diagram command");
            };
            assert_eq!(command, expected);
            snapshot_tx.send_modify(|snapshot| {
                snapshot.agent_diagram_result = Some(AgentDiagramResult {
                    request_id,
                    revision: 7,
                    accepted: true,
                    published: false,
                    summary: Some("updated the diagram intent".to_string()),
                    error: None,
                });
            });
        });
        let result = handle_request(
            AgentRequest::DiagramApply {
                request_id: 42,
                command: command.clone(),
            },
            &Utf8PathBuf::from("/tmp/example/repo"),
            &mut snapshots,
            &tx,
            &tokio::sync::Mutex::new(()),
        )
        .await
        .unwrap();
        assert_eq!(result["accepted"], true);
        assert_eq!(result["revision"], 7);
        assert_eq!(result["summary"], "updated the diagram intent");
        responder.await.unwrap();
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
