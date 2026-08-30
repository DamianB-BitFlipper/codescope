//! The non-interactive JSON backend: `codescope <scan|changeset|analyze|digest|bases>`.
//!
//! This module is wiring, not new analysis: every subcommand reuses the existing
//! [`GitRepo`] / [`AnalysisEngine`] / [`codescope_analysis::ChangeDigest`] APIs and
//! serializes their results. The backend never starts the TUI (see `main.rs`).
//!
//! Contract (stable for LLM/tool consumers):
//!
//! - JSON on **stdout**, pretty-printed by default, single-line with `--compact`.
//! - `{"error": ...}` on **stderr** and exit code 1 on failure (e.g. a non-git path).
//! - Exit code 0 on success — including git-only `analyze`/`digest` runs.
//! - Deterministic output: repo-relative paths only (the absolute repo toplevel is
//!   stripped from [`RepoContext`]), no timestamps.

use std::io::Write as _;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Subcommand, ValueEnum};
use codescope_analysis::{
    AnalysisEngine, AnalysisSnapshot, ChangedSymbolInfo, FileAnalysis, MappedHunk, SemanticSource,
};
use codescope_core::{
    BaseInfo, ChangeScope, Diagnostic, Epoch, Evidence, FeatureSet, FileId, FileStatus, HeadState,
    HunkMapping, ImpactGraph, Location, Position, RepoContext, SymbolRef, SymbolTree, Upstream,
};
use codescope_git::GitRepo;
use codescope_lsp::{detect_languages, Language, LanguageService, LspError, SemanticError};
use serde::Serialize;

/// Non-interactive backend subcommands. With none given, the TUI starts instead.
#[derive(Subcommand, Debug)]
pub enum BackendCommand {
    /// Repo summary as JSON: context (branch/base/ahead-behind), per-scope change
    /// counts, detected languages, and language-server availability.
    Scan(BackendArgs),
    /// The change-set for one scope as JSON (files, statuses, hunks with line numbers).
    Changeset(ScopeArgs),
    /// The full analysis snapshot plus the change digest as JSON.
    Analyze(ScopeArgs),
    /// The change digest (the AI prompt payload) as JSON, or rendered text with `--text`.
    Digest(DigestArgs),
    /// Base-ref candidates for the branch scope, as JSON (for pickers / LLM base selection).
    Bases(BackendArgs),
}

/// Shared arguments for subcommands that only read git state.
#[derive(Args, Debug)]
pub struct BackendArgs {
    /// Repository path (any directory inside the worktree).
    #[arg(default_value = ".")]
    pub path: Utf8PathBuf,
    /// Single-line JSON instead of pretty-printed.
    #[arg(long)]
    pub compact: bool,
}

/// Arguments for subcommands that read one change scope.
#[derive(Args, Debug)]
pub struct ScopeArgs {
    /// Repository path (any directory inside the worktree).
    #[arg(default_value = ".")]
    pub path: Utf8PathBuf,
    /// Which change-set to read.
    #[arg(long, value_enum, default_value = "branch")]
    pub scope: Scope,
    /// Single-line JSON instead of pretty-printed.
    #[arg(long)]
    pub compact: bool,
}

/// Arguments for the digest subcommand.
#[derive(Args, Debug)]
pub struct DigestArgs {
    /// Repository path (any directory inside the worktree).
    #[arg(default_value = ".")]
    pub path: Utf8PathBuf,
    /// Which change-set to digest.
    #[arg(long, value_enum, default_value = "branch")]
    pub scope: Scope,
    /// Single-line JSON instead of pretty-printed.
    #[arg(long)]
    pub compact: bool,
    /// Render the digest as prompt text instead of JSON.
    #[arg(long)]
    pub text: bool,
}

/// CLI spelling of [`ChangeScope`].
#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Scope {
    /// Committed on this branch (`merge-base...HEAD`).
    Branch,
    /// Index vs HEAD.
    Staged,
    /// Worktree vs index, plus untracked files.
    Unstaged,
    /// All uncommitted changes (HEAD vs worktree, incl. untracked).
    Working,
}

impl From<Scope> for ChangeScope {
    fn from(scope: Scope) -> ChangeScope {
        match scope {
            Scope::Branch => ChangeScope::Branch,
            Scope::Staged => ChangeScope::Staged,
            Scope::Unstaged => ChangeScope::Unstaged,
            Scope::Working => ChangeScope::Working,
        }
    }
}

impl Scope {
    /// The CLI / JSON spelling of the scope.
    fn as_str(self) -> &'static str {
        match self {
            Scope::Branch => "branch",
            Scope::Staged => "staged",
            Scope::Unstaged => "unstaged",
            Scope::Working => "working",
        }
    }
}

/// Run one backend subcommand; returns the process exit code (0 = success, 1 = error).
///
/// Errors are reported as a single-line `{"error": ...}` JSON object on stderr.
pub async fn run(cmd: &BackendCommand) -> i32 {
    match run_inner(cmd).await {
        Ok(()) => 0,
        Err(err) => {
            let payload = serde_json::json!({ "error": format!("{err:#}") });
            eprintln!("{payload}");
            1
        }
    }
}

async fn run_inner(cmd: &BackendCommand) -> Result<()> {
    match cmd {
        BackendCommand::Scan(args) => scan(args).await,
        BackendCommand::Changeset(args) => changeset(args).await,
        BackendCommand::Analyze(args) => analyze(args).await,
        BackendCommand::Digest(args) => digest(args).await,
        BackendCommand::Bases(args) => bases(args).await,
    }
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

async fn scan(args: &BackendArgs) -> Result<()> {
    let repo = discover(&args.path).await?;
    let ctx = repo
        .repo_context()
        .await
        .context("cannot read repo context")?;

    let mut counts = ScopeCounts::default();
    let mut notes = Vec::new();
    for scope in [
        ChangeScope::Branch,
        ChangeScope::Staged,
        ChangeScope::Unstaged,
        ChangeScope::Working,
    ] {
        match repo.changeset(scope).await {
            Ok(set) => counts.set(scope, set.len()),
            // A scope that cannot be computed (e.g. branch without an inferable base)
            // is reported as null with a note; the summary itself must not fail.
            Err(err) => notes.push(format!("{scope:?} scope unavailable: {err}")),
        }
    }

    let languages = detect_languages(repo.toplevel());
    let out = ScanOut {
        repo: RepoView::from(&ctx),
        scopes: counts,
        languages: languages.iter().map(Language::as_str).collect(),
        language_server: language_server_view(&languages),
        notes,
    };
    emit(&out, args.compact)
}

async fn changeset(args: &ScopeArgs) -> Result<()> {
    let repo = discover(&args.path).await?;
    let scope = args.scope;
    let set = repo
        .changeset(scope.into())
        .await
        .with_context(|| format!("cannot compute the {} change-set", scope.as_str()))?;
    emit(&set, args.compact)
}

async fn analyze(args: &ScopeArgs) -> Result<()> {
    let (snap, lsp, notes) = snapshot_for(&args.path, args.scope).await?;
    let out = SnapshotView {
        lsp,
        epoch: snap.epoch,
        repo: RepoView::from(&snap.repo_ctx),
        changeset: &snap.changeset,
        files: snap.files.iter().map(FileAnalysisView::from).collect(),
        changed: &snap.changed,
        graph: &snap.graph,
        diagnostics: &snap.diagnostics,
        digest: snap.digest(),
        notes,
    };
    emit(&out, args.compact)
}

async fn digest(args: &DigestArgs) -> Result<()> {
    let (snap, _lsp, notes) = snapshot_for(&args.path, args.scope).await?;
    let mut digest = snap.digest();
    digest.notes.extend(notes);
    if args.text {
        print!("{}", digest.render());
        return Ok(());
    }
    emit(&digest, args.compact)
}

async fn bases(args: &BackendArgs) -> Result<()> {
    let repo = discover(&args.path).await?;
    let bases = repo
        .base_candidates()
        .await
        .context("cannot list base candidates")?;
    emit(&BasesOut { bases }, args.compact)
}

// ---------------------------------------------------------------------------
// Shared pipeline
// ---------------------------------------------------------------------------

/// Discover the repository containing `path` (user-supplied; reported verbatim in errors).
async fn discover(path: &Utf8Path) -> Result<GitRepo> {
    GitRepo::discover(path)
        .await
        .with_context(|| format!("not a git repository: {path}"))
}

/// The shared `analyze`/`digest` pipeline: change-set in, snapshot out. When no language
/// server is available for the repo, analysis still runs git-only ([`GitOnlySource`]) and
/// the result carries `lsp: None` plus a note instead of failing.
async fn snapshot_for(
    path: &Utf8Path,
    scope: Scope,
) -> Result<(AnalysisSnapshot, Option<LspView>, Vec<String>)> {
    let repo = discover(path).await?;
    let set = repo
        .changeset(scope.into())
        .await
        .with_context(|| format!("cannot compute the {} change-set", scope.as_str()))?;
    match LanguageService::start(repo.toplevel()).await {
        Ok(svc) => {
            let lsp = LspView {
                language: svc.language_name(),
            };
            let engine = AnalysisEngine::new(svc, repo);
            let result = engine.refresh(&set, Epoch::ZERO).await;
            // Shut the server down gracefully even when analysis failed.
            engine.into_service().shutdown().await;
            let snap = result.context("analysis failed")?;
            Ok((snap, Some(lsp), Vec::new()))
        }
        Err(err) => {
            tracing::info!(%err, "no language server for this repo; git-only analysis");
            let notes = vec![format!(
                "no language server available; git-only analysis ({err})"
            )];
            let engine = AnalysisEngine::new(GitOnlySource::new(err.to_string()), repo);
            let snap = engine.refresh(&set, Epoch::ZERO).await?;
            Ok((snap, None, notes))
        }
    }
}

/// Write `value` as JSON to stdout (pretty by default, single-line when `compact`).
fn emit<T: Serialize>(value: &T, compact: bool) -> Result<()> {
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

// ---------------------------------------------------------------------------
// Serializable output views
//
// `AnalysisSnapshot` / `FileAnalysis` / `MappedHunk` carry no serde derives (they are
// internal pipeline types), so the backend projects them into thin borrowing views.
// This is also where the absolute repo toplevel is stripped: [`RepoView`] omits
// `RepoContext::toplevel`, and every other path in these types is already repo-relative.
// ---------------------------------------------------------------------------

/// `RepoContext` without the absolute `toplevel` path.
#[derive(Serialize)]
struct RepoView<'a> {
    head: &'a HeadState,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<&'a Upstream>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<&'a BaseInfo>,
}

impl<'a> From<&'a RepoContext> for RepoView<'a> {
    fn from(ctx: &'a RepoContext) -> Self {
        RepoView {
            head: &ctx.head,
            upstream: ctx.upstream.as_ref(),
            base: ctx.base.as_ref(),
        }
    }
}

/// Per-scope changed-file counts; a scope is `null` when it cannot be computed
/// (e.g. `branch` without an inferable base).
#[derive(Serialize, Default)]
struct ScopeCounts {
    branch: Option<usize>,
    staged: Option<usize>,
    unstaged: Option<usize>,
    working: Option<usize>,
}

impl ScopeCounts {
    fn set(&mut self, scope: ChangeScope, count: usize) {
        match scope {
            ChangeScope::Branch => self.branch = Some(count),
            ChangeScope::Staged => self.staged = Some(count),
            ChangeScope::Unstaged => self.unstaged = Some(count),
            ChangeScope::Working => self.working = Some(count),
        }
    }
}

/// Language-server availability summary (a cheap binary probe, not a full session).
#[derive(Serialize)]
struct LanguageServerView {
    /// The language a session would serve (Go wins ties, then Rust — the same
    /// precedence as [`LanguageService::start`]). TypeScript/Python are detected but
    /// have no adapter, so a repo with only those reports `language_server: null`.
    language: &'static str,
    /// The server binary that was probed (env override honored).
    server: String,
    /// `true` when the server binary runs successfully.
    available: bool,
}

/// `scan` output.
#[derive(Serialize)]
struct ScanOut<'a> {
    repo: RepoView<'a>,
    scopes: ScopeCounts,
    languages: Vec<&'static str>,
    language_server: Option<LanguageServerView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
}

/// Language-server session info for `analyze` (`null` in git-only mode).
#[derive(Serialize)]
struct LspView {
    language: &'static str,
}

/// [`HunkMapping`] plus the analysis-level signature-touch flag.
#[derive(Serialize)]
struct MappedHunkView<'a> {
    #[serde(flatten)]
    mapping: &'a HunkMapping,
    signature_touch: bool,
}

impl<'a> From<&'a MappedHunk> for MappedHunkView<'a> {
    fn from(m: &'a MappedHunk) -> Self {
        MappedHunkView {
            mapping: &m.mapping,
            signature_touch: m.signature_touch,
        }
    }
}

/// Per-file analysis artifacts (trees, mappings, degradation notes).
#[derive(Serialize)]
struct FileAnalysisView<'a> {
    file: &'a FileId,
    status: FileStatus,
    worktree: Option<&'a SymbolTree>,
    base: Option<&'a SymbolTree>,
    mappings: Vec<MappedHunkView<'a>>,
    notes: &'a [String],
}

impl<'a> From<&'a FileAnalysis> for FileAnalysisView<'a> {
    fn from(f: &'a FileAnalysis) -> Self {
        FileAnalysisView {
            file: &f.file,
            status: f.status,
            worktree: f.worktree.as_ref(),
            base: f.base.as_ref(),
            mappings: f.mappings.iter().map(MappedHunkView::from).collect(),
            notes: &f.notes,
        }
    }
}

/// `analyze` output: the full [`AnalysisSnapshot`] (repo-relative paths only) plus the
/// change digest.
#[derive(Serialize)]
struct SnapshotView<'a> {
    lsp: Option<LspView>,
    epoch: Epoch,
    repo: RepoView<'a>,
    changeset: &'a codescope_core::ChangeSet,
    files: Vec<FileAnalysisView<'a>>,
    changed: &'a [ChangedSymbolInfo],
    graph: &'a Evidence<ImpactGraph>,
    diagnostics: &'a [Diagnostic],
    digest: codescope_analysis::ChangeDigest,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
}

/// `bases` output.
#[derive(Serialize)]
struct BasesOut {
    bases: Vec<BaseInfo>,
}

// ---------------------------------------------------------------------------
// Language-server availability probe
// ---------------------------------------------------------------------------

/// The language server a session would start for these languages, with a cheap
/// binary probe (`<server> version`-style) instead of a full initialize handshake.
/// `None` when no detected language has an adapter (matching [`LanguageService::start`]).
fn language_server_view(languages: &[Language]) -> Option<LanguageServerView> {
    let (language, program, probe_args): (Language, String, &[&str]) =
        if languages.contains(&Language::Go) {
            (
                Language::Go,
                std::env::var("CODESCOPE_GOPLS").unwrap_or_else(|_| "gopls".to_string()),
                &["version"],
            )
        } else if languages.contains(&Language::Rust) {
            (
                Language::Rust,
                std::env::var("CODESCOPE_RUST_ANALYZER")
                    .unwrap_or_else(|_| "rust-analyzer".to_string()),
                &["--version"],
            )
        } else {
            return None;
        };
    let available = binary_runs(&program, probe_args);
    Some(LanguageServerView {
        language: language.as_str(),
        server: program,
        available,
    })
}

/// `true` when `program args...` spawns and exits successfully (stdio discarded).
fn binary_runs(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Git-only semantic source
// ---------------------------------------------------------------------------

/// [`SemanticSource`] used when no language server is available: it owns no files, so
/// every file degrades to a per-file git-only note in the engine, and the (never
/// reached) semantic queries fail fast with the recorded reason.
#[derive(Debug)]
struct GitOnlySource {
    features: FeatureSet,
    reason: String,
}

impl GitOnlySource {
    fn new(reason: String) -> Self {
        GitOnlySource {
            features: FeatureSet::default(),
            reason,
        }
    }

    fn unavailable(&self) -> SemanticError {
        SemanticError::Client(LspError::Protocol(format!(
            "no language server available: {}",
            self.reason
        )))
    }
}

impl SemanticSource for GitOnlySource {
    fn features(&self) -> &FeatureSet {
        &self.features
    }

    fn diagnostics(&self, _file: &FileId) -> Vec<Diagnostic> {
        Vec::new()
    }

    fn handles(&self, _file: &FileId) -> bool {
        false
    }

    async fn document_symbols(&self, _file: &FileId) -> Result<Evidence<SymbolTree>, SemanticError> {
        Err(self.unavailable())
    }

    async fn base_document_symbols(
        &self,
        _file: &FileId,
        _content: &str,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        Err(self.unavailable())
    }

    async fn references(
        &self,
        _file: &FileId,
        _pos: Position,
    ) -> Result<Evidence<Vec<Location>>, SemanticError> {
        Err(self.unavailable())
    }

    async fn incoming_calls(
        &self,
        _file: &FileId,
        _pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        Err(self.unavailable())
    }

    async fn outgoing_calls(
        &self,
        _file: &FileId,
        _pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        Err(self.unavailable())
    }

    async fn implementations(
        &self,
        _file: &FileId,
        _pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        Err(self.unavailable())
    }

    async fn type_subtypes(
        &self,
        _file: &FileId,
        _pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        Err(self.unavailable())
    }
}
