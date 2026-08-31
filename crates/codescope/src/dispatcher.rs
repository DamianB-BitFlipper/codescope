//! The dispatcher: the single writer of repository state.
//!
//! Owns the epoch (bumped once per accepted change), and never blocks on slow subsystems:
//! git reads, analysis, and AI requests run as spawned, epoch-tagged jobs; results are
//! applied only when the epoch still matches (architecture decision 4). A stale AI plan or
//! analysis can never overwrite a newer repo state.

use std::collections::HashSet;

use codescope_ai::{AiOutcome, AiService, FactView, NoToolExecutor};
use codescope_analysis::{AnalysisEngine, AnalysisSnapshot};
use codescope_core::{AiStatus, ChangeScope, EntityRef, Epoch, LineRange, LsStatus, PlanEdgeKind};
use codescope_git::GitRepo;
use codescope_lsp::LanguageService;
use codescope_tui::snapshot::{
    DiffPane, DiffRow, FileRow, ImpactList, ImpactLoadState, ImpactPane, ImpactRow,
    InterpretationSource, RepoBar, ScopeCounts, SelectedChange, SemRow, SemanticPane, StatusLevel,
    StatusMessage, SymbolRow, UiSnapshot,
};
use codescope_tui::Action;
use tokio::sync::{mpsc, watch};

/// Events the dispatcher reacts to.
#[derive(Debug)]
pub enum DispatchEvent {
    /// The working tree or git state changed (already debounced).
    RepoChanged,
    /// A TUI action that needs work.
    Work(Action),
    /// An analysis job completed (spawned; epoch-tagged).
    AnalysisDone {
        /// Epoch the job ran against.
        epoch: Epoch,
        /// The result.
        result: anyhow::Result<Box<AnalysisSnapshot>>,
    },
    /// The git phase of a refresh completed (changeset + repo context) while the
    /// language-server analysis is still running. Lets the UI show changed files and the
    /// top bar within a second or two on repos where analysis takes tens of seconds,
    /// instead of a misleading "0 changed files" placeholder.
    ChangesetReady {
        /// Epoch the job ran against; stale results are dropped on apply.
        epoch: Epoch,
        /// Repo context (top bar: repo/branch/base).
        ctx: codescope_core::RepoContext,
        /// The current scope's changeset (files pane + diff before symbols land).
        changeset: codescope_core::ChangeSet,
    },
    /// A lazy per-file analysis job completed (spawned; epoch-tagged).
    FileAnalysisDone {
        /// Epoch the job ran against; stale results are dropped on apply.
        epoch: Epoch,
        /// Repo-relative path of the analyzed file.
        file: String,
        /// The per-file semantic result (stringified error keeps the event simple).
        result: Result<Box<codescope_analysis::FileSemanticResult>, String>,
    },
    /// An AI plan job completed (spawned; epoch-tagged).
    AiDone {
        /// Epoch the plan was requested against.
        epoch: Epoch,
        /// The AI request generation (monotonic per dispatcher): distinguishes two
        /// requests in the same epoch, so a slow older request can never overwrite a
        /// newer plan (review 18 M7).
        generation: u64,
        /// The validated outcome.
        outcome: AiOutcome,
    },
    /// The language server finished initializing; semantic analysis can begin.
    EngineReady(Box<AnalysisEngine<LanguageService>>),
    /// The language server failed to start; stay in git-only mode.
    EngineUnavailable(String),
    /// The provider's model list was fetched for the picker.
    ModelsLoaded(Vec<String>),
    /// The repo's base candidates were fetched for the base picker.
    BaseLoaded(Vec<String>),
    /// The selected symbol's lazy callers/callees resolved.
    RelationsLoaded {
        /// Epoch the job ran against; stale results are dropped on apply.
        epoch: Epoch,
        /// File of the symbol (part of the staleness key).
        file: String,
        /// Symbol name (part of the staleness key; doubles as the pane-title label).
        name: String,
        /// Identifier line the job resolved (part of the staleness key).
        line: u32,
        /// Identifier column the job resolved (part of the staleness key).
        col: u32,
        /// Callers of the symbol (with the evidence honesty flag).
        callers: RelationRows,
        /// Callees of the symbol (with the evidence honesty flag).
        callees: RelationRows,
    },
}

/// Lazily-fetched relation rows for one direction (callers or callees), keeping the
/// evidence's completeness so the UI can mark a partial answer instead of implying it
/// is exhaustive (spec §5.4).
#[derive(Debug, Clone, Default)]
pub struct RelationRows {
    /// The relation rows (empty when the symbol has none or the fetch failed).
    pub rows: Vec<ImpactRow>,
    /// `true` when the evidence was not complete (timeout, truncation, unsupported
    /// server feature).
    pub partial: bool,
}

/// Picker entry that returns base selection to inference.
const AUTO_BASE: &str = "(auto / inferred)";

/// Appended to every AI failure in the status bar (spec §3.6): `A` re-requests the plan
/// and `m` opens the model picker, and the deterministic impact view is unaffected.
const AI_FAILURE_SUFFIX: &str = "A retry · m change model · deterministic impact remains available";

/// Lazy per-file semantic state (the interactive counterpart of the eager
/// `AnalysisSnapshot.files`). The files pane renders this directly.
#[derive(Debug, Clone)]
enum FileSemanticState {
    /// A per-file analysis job is in flight.
    Loading,
    /// Analysis completed (zero changed symbols is a real answer).
    Ready(Box<codescope_analysis::FileSemanticResult>),
    /// The language service does not own this file (binary, gitlink, unowned language).
    Unsupported,
    /// The job failed (retryable via re-expand).
    Failed,
}

/// Lazily-expanded relations of the currently selected symbol.
#[derive(Debug, Clone)]
struct SelectedRelations {
    /// Incoming calls (lazy LSP call hierarchy).
    callers: RelationRows,
    /// Outgoing calls (lazy LSP call hierarchy).
    callees: RelationRows,
}

/// The dispatcher actor. Single writer of all published state.
pub struct Dispatcher {
    repo: GitRepo,
    engine: Option<std::sync::Arc<AnalysisEngine<LanguageService>>>,
    ai: Option<std::sync::Arc<AiService>>,
    scope: ChangeScope,
    epoch: Epoch,
    ls_status: LsStatus,
    ai_status: AiStatus,
    ai_enabled: bool,
    analysis: Option<AnalysisSnapshot>,
    /// Validated AI plan rows with the epoch they were validated against.
    ai_rows: Option<(Epoch, Vec<SemRow>, String)>,
    /// The file the diff pane is aimed at (the files-pane selection; falls back to the
    /// changeset's first file when unset or absent from the set).
    selected_file: Option<String>,
    /// Identity of the selected symbol (file, name, line, col), when the selection sits on
    /// a symbol row; gates stale relations jobs.
    selected_symbol: Option<(String, String, u32, u32)>,
    /// The selected symbol's lazily-expanded callers/callees, kept as separate lists so
    /// the impact pane can show both columns.
    selected_relations: Option<SelectedRelations>,
    /// The epoch that produced the current `repo_ctx`/`changeset`. Jobs that clone
    /// those as inputs (per-file analysis, AI digest) must only launch when this equals
    /// `self.epoch` — otherwise they would tag old git facts with the new epoch
    /// (review 18 M1).
    data_epoch: Epoch,
    /// Monotonic AI request counter: `AiDone.generation` must match to apply.
    ai_request_seq: u64,
    /// Per-file lazy semantic analysis, keyed by repo-relative path. Absent = Unloaded.
    /// Cleared on every epoch bump (scope/base/repo change invalidates file content).
    file_semantics: std::collections::HashMap<String, FileSemanticState>,
    /// Files the user expanded with Tab (dispatcher owns expansion so the snapshot is the
    /// single source of truth; the app forwards ToggleFileAnalysis).
    expanded_files: std::collections::HashSet<String>,
    /// In-flight per-file analysis jobs: path → the epoch its job was launched under.
    /// A completing job removes only its own entry (matching epoch); a stale-epoch
    /// completion never disturbs a newer job's ledger entry (review 18 M2).
    analysis_in_flight: std::collections::HashMap<String, Epoch>,
    /// FIFO queue for per-file analysis beyond the concurrency bound.
    analysis_queue: std::collections::VecDeque<String>,
    snapshot_tx: watch::Sender<UiSnapshot>,
    /// Where completed jobs report back.
    job_tx: mpsc::Sender<DispatchEvent>,
    /// Typed status message surfaced in the bottom bar (`UiSnapshot::message` mirrors
    /// its text while the renderer migrates).
    status: StatusMessage,
    /// Available AI models for the picker (from the provider).
    available_models: Vec<String>,
    /// User-picked comparison base (overrides inference until cleared).
    base_override: Option<String>,
    /// Base candidates for the picker (from `git base_candidates`).
    available_bases: Vec<String>,
    /// Latest repo context (cheap to re-read).
    repo_ctx: Option<codescope_core::RepoContext>,
    /// Latest raw changeset for the current scope (for the diff pane before analysis lands).
    changeset: Option<codescope_core::ChangeSet>,
}

impl Dispatcher {
    /// Build a dispatcher for an already-discovered repo.
    pub fn new(
        repo: GitRepo,
        engine: Option<AnalysisEngine<LanguageService>>,
        ai: Option<AiService>,
        snapshot_tx: watch::Sender<UiSnapshot>,
        job_tx: mpsc::Sender<DispatchEvent>,
    ) -> Self {
        let ls_status = if engine.is_some() {
            LsStatus::Ready
        } else {
            LsStatus::Starting
        };
        let ai_enabled = ai.is_some();
        let engine = engine.map(std::sync::Arc::new);
        let ai = ai.map(std::sync::Arc::new);
        Dispatcher {
            repo,
            engine,
            ai,
            scope: ChangeScope::Branch,
            epoch: Epoch::ZERO,
            ls_status,
            ai_status: if ai_enabled {
                AiStatus::Idle
            } else {
                AiStatus::Disabled
            },
            ai_enabled,
            analysis: None,
            ai_rows: None,
            available_models: Vec::new(),
            selected_file: None,
            selected_symbol: None,
            selected_relations: None,
            file_semantics: std::collections::HashMap::new(),
            expanded_files: std::collections::HashSet::new(),
            ai_request_seq: 0,
            data_epoch: Epoch::ZERO,
            analysis_in_flight: std::collections::HashMap::new(),
            analysis_queue: std::collections::VecDeque::new(),
            base_override: None,
            available_bases: Vec::new(),
            snapshot_tx,
            job_tx,
            status: StatusMessage::default(),
            repo_ctx: None,
            changeset: None,
        }
    }

    fn publish(&self) {
        let _ = self.snapshot_tx.send(self.build_snapshot());
    }

    /// Set the bottom-bar status message; `UiSnapshot::message` mirrors the text while
    /// the renderer migrates to the typed [`StatusMessage`].
    fn set_status(&mut self, text: impl Into<String>, level: StatusLevel) {
        self.status = StatusMessage {
            text: text.into(),
            level,
        };
    }

    /// Handle one event. Never blocks on git/LSP/AI — those run as spawned jobs.
    pub async fn handle(&mut self, event: DispatchEvent) {
        match event {
            DispatchEvent::RepoChanged => self.bump_and_refresh(),
            DispatchEvent::Work(action) => self.on_action(action),
            DispatchEvent::AnalysisDone { epoch, result } => self.on_analysis_done(epoch, result),
            DispatchEvent::ChangesetReady {
                epoch,
                ctx,
                changeset,
            } => self.on_changeset_ready(epoch, ctx, changeset),
            DispatchEvent::FileAnalysisDone {
                epoch,
                file,
                result,
            } => self.on_file_analysis_done(epoch, file, result),
            DispatchEvent::AiDone {
                epoch,
                generation,
                outcome,
            } => self.on_ai_done(epoch, generation, outcome),
            DispatchEvent::EngineReady(engine) => {
                self.ls_status = LsStatus::Ready;
                self.engine = Some(std::sync::Arc::new(*engine));
                // No eager repo-wide analysis (the lazy redesign): the files pane is
                // already populated from git; semantics load per file on Tab. A refresh
                // would only re-run the git phase, so just publish the new LSP status.
                self.publish();
            }
            DispatchEvent::EngineUnavailable(reason) => {
                self.ls_status = LsStatus::Failed;
                if reason.contains("no supported language detected") {
                    self.set_status(
                        "git-only (no supported language detected)",
                        StatusLevel::Warning,
                    );
                } else {
                    self.set_status(
                        format!("git-only (language server failed: {reason})"),
                        StatusLevel::Warning,
                    );
                }
                self.publish();
            }
            DispatchEvent::ModelsLoaded(models) => {
                self.available_models = models;
                self.publish();
            }
            DispatchEvent::RelationsLoaded {
                epoch,
                file,
                name,
                line,
                col,
                callers,
                callees,
            } => {
                // Staleness gate: the result applies only while it answers the CURRENT
                // selection in the CURRENT repo state. Navigation no longer needs Enter, so
                // a slow fetch for a row the user has since left must never overwrite the
                // pane (the epoch gate covers refreshes; the identity gate covers j/k moves
                // within one epoch).
                let current = self
                    .selected_symbol
                    .as_ref()
                    .is_some_and(|s| *s == (file, name.clone(), line, col));
                if epoch != self.epoch || !current {
                    return;
                }
                // Callers and callees stay separate lists: the impact pane shows them
                // in their own columns, each with its evidence honesty flag.
                self.selected_relations = Some(SelectedRelations { callers, callees });
                self.publish();
            }
            DispatchEvent::BaseLoaded(bases) => {
                // The picker always offers "(auto / inferred)" first to escape an override.
                let mut list = vec![AUTO_BASE.to_string()];
                list.extend(bases);
                self.available_bases = list;
                self.publish();
            }
        }
    }

    fn bump_and_refresh(&mut self) {
        self.epoch = self.epoch.next();
        // Any earlier AI rows no longer describe the repo.
        if self.ai_rows.is_some() {
            self.ai_status = AiStatus::Stale { epoch: self.epoch };
        }
        self.spawn_refresh();
    }

    fn on_action(&mut self, action: Action) {
        match action {
            Action::RefreshGit => self.spawn_refresh(),
            Action::SetFileExpanded { path, expanded } => self.set_file_expanded(&path, expanded),
            Action::ScopeStaged => self.set_scope(ChangeScope::Staged),
            Action::ScopeUnstaged => self.set_scope(ChangeScope::Unstaged),
            Action::ScopeBranch => self.set_scope(ChangeScope::Branch),
            Action::ScopeWorking => self.set_scope(ChangeScope::Working),
            Action::ScopeCycle => {
                let next = match self.scope {
                    ChangeScope::Branch => ChangeScope::Staged,
                    ChangeScope::Staged => ChangeScope::Unstaged,
                    ChangeScope::Unstaged => ChangeScope::Working,
                    ChangeScope::Working => ChangeScope::Branch,
                };
                self.set_scope(next);
            }
            Action::AiToggle => {
                if self.ai.is_none() {
                    self.set_status(
                        "AI not configured (set PRIME_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY)",
                        StatusLevel::Warning,
                    );
                    self.publish();
                    return;
                }
                self.ai_enabled = !self.ai_enabled;
                self.ai_status = if self.ai_enabled {
                    AiStatus::Idle
                } else {
                    AiStatus::Disabled
                };
                self.publish();
            }
            Action::AiRefresh => self.spawn_ai(),
            Action::ModelPicker => self.spawn_list_models(),
            Action::ModelSelected(name) => self.set_model(&name),
            Action::SelectSymbol {
                file,
                name,
                line,
                col,
            } => {
                // Enter re-centers on the selection: record it as the current target (so
                // the result is not dropped as stale) and expand its relations.
                self.selected_file = Some(file.clone());
                self.selected_symbol = Some((file.clone(), name.clone(), line, col));
                self.spawn_expand(file, name, line, col);
            }
            Action::SelectionChanged { file, symbol } => self.on_selection_changed(file, symbol),
            Action::BasePicker => self.spawn_list_bases(),
            Action::BaseSelected(name) => self.set_base(name),
            _ => {}
        }
    }

    /// Fetch the provider's model list for the picker (spawned; non-blocking).
    fn spawn_list_models(&mut self) {
        let Some(ai) = &self.ai else {
            self.set_status(
                "AI not configured (set PRIME_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY)",
                StatusLevel::Warning,
            );
            self.publish();
            return;
        };
        let ai = ai.clone();
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
            let models = ai.client().list_models().await.unwrap_or_default();
            let _ = tx.send(DispatchEvent::ModelsLoaded(models)).await;
        });
    }

    /// Apply a model selection from the picker.
    fn set_model(&mut self, name: &str) {
        match &self.ai {
            Some(ai) => {
                ai.set_model(name);
                self.set_status(format!("AI model: {name}"), StatusLevel::Info);
            }
            None => {
                self.set_status(
                    "AI not configured (set PRIME_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY)",
                    StatusLevel::Warning,
                );
            }
        }
        self.publish();
    }

    /// Fetch base candidates for the base picker (spawned; non-blocking).
    fn spawn_list_bases(&mut self) {
        let repo = self.repo.clone();
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
            let bases = repo
                .base_candidates()
                .await
                .map(|c| c.into_iter().map(|b| b.ref_name).collect())
                .unwrap_or_default();
            let _ = tx.send(DispatchEvent::BaseLoaded(bases)).await;
        });
    }

    /// The files-pane selection moved (navigation-driven panes; no Enter required): aim
    /// the diff pane at the selected file, publish the selection's `SelectedChange`
    /// immediately (deterministic interpretation; spec §5.3/§5.6), and lazily expand a
    /// selected symbol's callers/callees — the impact lists read `Loading` until the
    /// fetch lands. Moving OFF a symbol (file row / empty list) clears the relations
    /// view back to the impact/AI pane and leaves the impact lists `Idle`.
    fn on_selection_changed(&mut self, file: Option<String>, symbol: Option<(String, u32, u32)>) {
        self.selected_file = file.clone();
        self.selected_symbol = match (file, symbol) {
            (Some(file), Some((name, line, col))) => Some((file, name, line, col)),
            _ => None,
        };
        // Drop the previous selection's rows immediately: nothing stale may linger while
        // the new fetch is in flight.
        self.selected_relations = None;
        if let Some((file, name, line, col)) = self.selected_symbol.clone() {
            self.spawn_expand(file, name, line, col);
        }
        self.publish();
    }

    /// Lazily expand a selected symbol's callers/callees (spawned; non-blocking). The job
    /// carries the current epoch and the symbol's identity; the result is dropped on apply
    /// when either has moved on (see RelationsLoaded).
    fn spawn_expand(&mut self, file: String, name: String, line: u32, col: u32) {
        let Some(engine) = &self.engine else {
            return;
        };
        // Relations exist only for a symbol whose file analysis is Ready: loading,
        // stale, collapsed, or unmapped rows never issue a relation request.
        if !matches!(
            self.file_semantics.get(&file),
            Some(FileSemanticState::Ready(_))
        ) {
            return;
        }
        let epoch = self.epoch;
        let engine = engine.clone();
        let tx = self.job_tx.clone();
        let file_id = match codescope_core::FileId::new(file.clone()) {
            Ok(f) => f,
            Err(_) => return,
        };
        tokio::spawn(async move {
            let pos = codescope_core::Position::new(line, col);
            let (callers, callees) = relations_for(&engine, &file_id, pos).await;
            let _ = tx
                .send(DispatchEvent::RelationsLoaded {
                    epoch,
                    file,
                    name,
                    line,
                    col,
                    callers,
                    callees,
                })
                .await;
        });
    }

    /// Apply a base selection from the picker: everything downstream (repo context,
    /// branch changeset, analysis) is recomputed against the chosen ref.
    fn set_base(&mut self, name: String) {
        if name.is_empty() {
            return;
        }
        if name == AUTO_BASE {
            self.base_override = None;
            self.set_status("base: auto (inferred)", StatusLevel::Info);
            self.spawn_refresh();
            return;
        }
        self.set_status(format!("base: {name}"), StatusLevel::Info);
        self.base_override = Some(name);
        self.spawn_refresh();
    }

    fn set_scope(&mut self, scope: ChangeScope) {
        if self.scope != scope {
            self.scope = scope;
            // The scope swap replaces the whole file list (the TUI resets its selection to
            // the top row and re-reports it): the old symbol's relations must not linger.
            // selected_file survives — the same file may exist in the new scope, and the
            // TUI's SelectionChanged corrects it otherwise.
            self.selected_symbol = None;
            self.selected_relations = None;
            self.spawn_refresh();
        }
    }

    /// Spawn a git+analysis job tagged with the current epoch.
    fn spawn_refresh(&mut self) {
        // Bump the epoch so a superseded in-flight refresh is dropped on apply (F4): the
        // newest base/scope/repo state always wins.
        self.epoch = self.epoch.next();
        let epoch = self.epoch;
        let repo = self.repo.clone();
        let scope = self.scope;
        let base_override = self.base_override.clone();
        // Publish immediately with a refreshing marker. Keep the previous repo context and
        // changeset visible while the job runs: on large repos the language-server phase
        // takes tens of seconds, and blanking the top bar ("?") and files pane reads as
        // "0 changed files" breakage. The spinner marks the data as in flight.
        self.publish_refreshing();
        let engine = self.engine.clone();
        let tx = self.job_tx.clone();
        // The engine is Arc-shared; the job runs the full git+analysis pipeline without
        // blocking the dispatcher. It reports twice: `ChangesetReady` as soon as the fast
        // git phase lands (files pane + top bar), then `AnalysisDone` when the language
        // server finishes. Both are epoch-gated at apply time.
        tokio::spawn(async move {
            let result = run_pipeline(repo, scope, engine, epoch, base_override, &tx).await;
            let _ = tx
                .send(DispatchEvent::AnalysisDone {
                    epoch,
                    result: result.map(Box::new),
                })
                .await;
        });
        // A new epoch invalidates per-file semantics (content may differ) and the queue.
        self.file_semantics.clear();
        self.analysis_in_flight.clear();
        self.analysis_queue.clear();
        // Relations for the selected symbol are re-fetched in `on_analysis_done`, once the
        // new analysis exists. Firing the query here would race the language server's own
        // refresh and tag a pre-refresh answer with the new epoch. The previously loaded
        // rows describe the old state, so drop them: `build_impact` renders the lists as
        // Loading while a selection is set.
        self.selected_relations = None;
    }

    fn spawn_ai(&mut self) {
        let (Some(_ai), Some(changeset)) = (&self.ai, &self.changeset) else {
            return;
        };
        let Some(ctx) = &self.repo_ctx else { return };
        if !self.ai_enabled {
            return;
        }
        let epoch = self.epoch;
        self.ai_request_seq += 1;
        let generation = self.ai_request_seq;
        self.ai_status = AiStatus::Loading { since_epoch: epoch };
        self.publish();
        // Digest from the git changeset + the symbols of files the user explicitly
        // analyzed (Ready), in CHANGSET order (deterministic). Unloaded files contribute
        // hunks only; Loading/Failed/Unsupported are reported separately. Never triggers
        // analysis. The relation graph was never queried — `Unknown`, not a partial answer.
        let mut changed: Vec<codescope_analysis::ChangedSymbolInfo> = Vec::new();
        let mut diags: Vec<codescope_core::Diagnostic> = Vec::new();
        let (mut loading, mut failed, mut unsupported) = (0usize, 0usize, 0usize);
        for f in &changeset.files {
            match self.file_semantics.get(f.path.as_str()) {
                Some(FileSemanticState::Ready(res)) => {
                    changed.extend(res.changed.clone());
                    diags.extend(res.diagnostics.clone());
                }
                Some(FileSemanticState::Loading) => loading += 1,
                Some(FileSemanticState::Failed) => failed += 1,
                Some(FileSemanticState::Unsupported) => unsupported += 1,
                None => {}
            }
        }
        let mut digest = codescope_analysis::change_digest(
            &changed,
            changeset,
            &codescope_core::Evidence {
                value: codescope_core::ImpactGraph::default(),
                completeness: codescope_core::Completeness::Unknown,
                notes: vec!["relations not queried (lazy per-file semantics)".to_string()],
            },
            &diags,
            ctx,
        )
        .render();
        let unloaded = changeset
            .files
            .iter()
            .filter(|f| {
                !matches!(
                    self.file_semantics.get(f.path.as_str()),
                    Some(FileSemanticState::Ready(_))
                )
            })
            .count();
        if unloaded > 0 {
            let mut parts = vec![format!("{unloaded} not yet analyzed")];
            if loading > 0 {
                parts.push(format!("{loading} analyzing"));
            }
            if failed > 0 {
                parts.push(format!("{failed} failed"));
            }
            if unsupported > 0 {
                parts.push(format!("{unsupported} unsupported"));
            }
            digest.push_str(&format!(
                "\nnote: {}; expand a file with Tab for symbol detail",
                parts.join(", ")
            ));
        }
        let facts = SnapshotFacts::from_lazy(changeset, &self.file_semantics);
        let ai = self.ai.clone();
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
            let outcome = match &ai {
                Some(ai) => {
                    ai.request_plan(&digest, &NoToolExecutor, &facts, epoch)
                        .await
                }
                None => AiOutcome::Unavailable,
            };
            let _ = tx
                .send(DispatchEvent::AiDone {
                    epoch,
                    generation,
                    outcome,
                })
                .await;
        });
    }

    /// Max concurrent per-file analysis jobs (keeps the language server responsive when
    /// the user expands several files quickly).
    const MAX_FILE_JOBS: usize = 4;

    /// Start (or queue) the lazy per-file analysis for `path`. Coalesces duplicates: a
    /// file already Loading/Ready this epoch launches nothing; a path with a job in
    /// flight (any epoch) is queued so the language server's per-file overlay never sees
    /// two writers (review 18 M2).
    fn spawn_file_analysis(&mut self, path: &str) {
        // Coalesce terminal/ready states: a cached Ready or a definitive Unsupported is
        // reused within the epoch. Failed retries on re-expand (the user asked again).
        if matches!(
            self.file_semantics.get(path),
            Some(FileSemanticState::Loading)
                | Some(FileSemanticState::Ready(_))
                | Some(FileSemanticState::Unsupported)
        ) {
            return;
        }
        // The engine may still be starting (LsStatus::Starting) — queue, don't mislabel
        // the file as unsupported (review 18 m2).
        if self.engine.is_none() {
            if self.ls_status == codescope_core::LsStatus::Failed {
                self.file_semantics
                    .insert(path.to_string(), FileSemanticState::Unsupported);
                self.publish();
            } else {
                self.enqueue_file_analysis(path);
            }
            return;
        }
        // M1: only launch against the CURRENT git-fact bundle. While a refresh is in
        // flight (data_epoch behind epoch), queue — the ChangesetReady handler drains.
        // M2: a path with an in-flight job (any epoch) queues instead of double-spawning.
        if self.data_epoch != self.epoch || self.analysis_in_flight.contains_key(path) {
            self.enqueue_file_analysis(path);
            return;
        }
        if self.analysis_in_flight.len() >= Self::MAX_FILE_JOBS {
            self.enqueue_file_analysis(path);
            return;
        }
        self.spawn_file_analysis_now(path);
    }

    /// Queue `path` for a later spawn (bounded concurrency / stale data epoch / engine
    /// starting), marking the row Loading so the UI shows the pending state.
    fn enqueue_file_analysis(&mut self, path: &str) {
        if !self.analysis_queue.iter().any(|p| p == path) {
            self.analysis_queue.push_back(path.to_string());
        }
        if !matches!(
            self.file_semantics.get(path),
            Some(FileSemanticState::Loading)
        ) {
            self.file_semantics
                .insert(path.to_string(), FileSemanticState::Loading);
        }
        self.publish();
    }

    /// The actual spawn: assumes the caller verified coalescing, the data epoch, the
    /// per-path in-flight exclusion, and the global bound.
    fn spawn_file_analysis_now(&mut self, path: &str) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let Some(changeset) = &self.changeset else {
            return;
        };
        let Some(fc) = changeset
            .files
            .iter()
            .find(|f| f.path.as_str() == path)
            .cloned()
        else {
            return;
        };
        let Some(ctx) = self.repo_ctx.clone() else {
            return;
        };
        self.analysis_in_flight.insert(path.to_string(), self.epoch);
        self.file_semantics
            .insert(path.to_string(), FileSemanticState::Loading);
        self.publish();
        let epoch = self.epoch;
        let scope = self.scope;
        let tx = self.job_tx.clone();
        let path_owned = path.to_string();
        tokio::spawn(async move {
            let result = engine
                .analyze_changed_file(&fc, scope, &ctx)
                .await
                .map(Box::new)
                .map_err(|e| e.to_string());
            let _ = tx
                .send(DispatchEvent::FileAnalysisDone {
                    epoch,
                    file: path_owned,
                    result,
                })
                .await;
        });
    }

    fn on_file_analysis_done(
        &mut self,
        epoch: Epoch,
        file: String,
        result: Result<Box<codescope_analysis::FileSemanticResult>, String>,
    ) {
        // Ledger removal is epoch-exact: a stale completion never disturbs a newer job's
        // entry for the same path (review 18 M2).
        if self.analysis_in_flight.get(&file) == Some(&epoch) {
            self.analysis_in_flight.remove(&file);
        }
        // Epoch + data-epoch gates: a refresh superseded this job or its inputs.
        if epoch != self.epoch || self.data_epoch != self.epoch {
            self.drain_analysis_queue();
            return;
        }
        // The file must still be in the current changeset (scope switch can drop it).
        let still_present = self
            .changeset
            .as_ref()
            .is_some_and(|cs| cs.files.iter().any(|f| f.path.as_str() == file));
        if !still_present {
            self.drain_analysis_queue();
            return;
        }
        match result {
            Ok(res) => {
                let state = if res.unsupported {
                    FileSemanticState::Unsupported
                } else if res.worktree_failed {
                    // The symbol query failed: surface a retryable failure, never
                    // authoritative empty/deleted data (review 18 M3).
                    self.set_status(
                        format!(
                            "semantic analysis failed for {file}: {}",
                            res.analysis
                                .notes
                                .first()
                                .cloned()
                                .unwrap_or_else(|| "symbol query failed".to_string())
                        ),
                        StatusLevel::Warning,
                    );
                    FileSemanticState::Failed
                } else {
                    FileSemanticState::Ready(res)
                };
                self.file_semantics.insert(file.clone(), state);
                // A Ready transition resolves relations for a symbol selection that was
                // waiting on this file (review 18 m3).
                if matches!(
                    self.file_semantics.get(&file),
                    Some(FileSemanticState::Ready(_))
                ) && self
                    .selected_symbol
                    .as_ref()
                    .is_some_and(|(f, _, _, _)| *f == file)
                {
                    let (f, name, line, col) = self.selected_symbol.clone().expect("checked");
                    self.spawn_expand(f, name, line, col);
                }
            }
            Err(e) => {
                self.set_status(
                    format!("semantic analysis failed for {file}: {e}"),
                    StatusLevel::Warning,
                );
                self.file_semantics.insert(file, FileSemanticState::Failed);
            }
        }
        self.publish();
        self.drain_analysis_queue();
    }

    /// Start the next queued per-file job when a slot frees and the data epoch is current.
    fn drain_analysis_queue(&mut self) {
        while self.analysis_in_flight.len() < Self::MAX_FILE_JOBS && self.data_epoch == self.epoch {
            let Some(next) = self.analysis_queue.pop_front() else {
                break;
            };
            // Already satisfied or still running: skip silently.
            if matches!(
                self.file_semantics.get(&next),
                Some(FileSemanticState::Ready(_))
            ) || self.analysis_in_flight.contains_key(&next)
            {
                continue;
            }
            // Clear the queued Loading marker so the spawn path runs the real job.
            self.file_semantics.remove(&next);
            self.spawn_file_analysis(&next);
        }
    }

    /// The targeted expansion command (review 18 M4): the path was resolved by the app
    /// at keypress time, so a coalesced SelectionChanged cannot retarget the toggle.
    /// Idempotent — the SetFileExpanded command names the desired end state.
    fn set_file_expanded(&mut self, path: &str, expanded: bool) {
        // Only files in the current changeset can be expanded.
        let known = self
            .changeset
            .as_ref()
            .is_some_and(|cs| cs.files.iter().any(|f| f.path.as_str() == path));
        if !known {
            return;
        }
        if expanded {
            if !self.expanded_files.insert(path.to_string()) {
                return; // already expanded
            }
            self.publish();
            // First expand dispatches the lazy analysis; cached/queued/in-flight
            // coalesces inside spawn_file_analysis.
            self.spawn_file_analysis(path);
            return;
        }
        if !self.expanded_files.remove(path) {
            return; // already collapsed
        }
        // Collapsing the file that owns the selected symbol: the relation view no longer
        // has a visible anchor.
        if self
            .selected_symbol
            .as_ref()
            .is_some_and(|(f, _, _, _)| f == path)
        {
            self.selected_symbol = None;
            self.selected_relations = None;
        }
        self.publish();
    }

    fn on_changeset_ready(
        &mut self,
        epoch: Epoch,
        ctx: codescope_core::RepoContext,
        changeset: codescope_core::ChangeSet,
    ) {
        // Same epoch gate as AnalysisDone: a superseded refresh must not resurrect stale
        // git state either.
        if epoch != self.epoch {
            return;
        }
        self.repo_ctx = Some(ctx);
        self.changeset = Some(changeset);
        // The git-fact bundle is now current: per-file analysis and AI digests may launch
        // against it. Replay expansion intent for files that survived the refresh.
        self.data_epoch = epoch;
        // The message must not claim analysis is "running": in the lazy world the
        // pipeline ends here and symbols load only on demand (review 18 m6).
        if self.analysis.is_none()
            && self.status.text == "files listed; symbol analysis still running…"
        {
            self.status = StatusMessage::default();
        }
        // Replay expansion intent: files still in the (new) changeset that the user had
        // expanded get their analysis relaunched against current data; vanished files
        // drop out of the expansion set.
        if let Some(cs) = &self.changeset {
            let present: Vec<String> = self
                .expanded_files
                .iter()
                .filter(|p| cs.files.iter().any(|f| f.path.as_str() == p.as_str()))
                .cloned()
                .collect();
            self.expanded_files = present.iter().cloned().collect();
            for path in present {
                self.spawn_file_analysis(&path);
            }
        }
        // Jobs queued while the data epoch was stale may launch now.
        self.drain_analysis_queue();
        // Analysis is still in flight: keep the refreshing marker on.
        self.publish_refreshing();
    }

    fn on_analysis_done(&mut self, epoch: Epoch, result: anyhow::Result<Box<AnalysisSnapshot>>) {
        // Apply-time epoch gate: drop results computed against an older repo state.
        if epoch != self.epoch {
            return;
        }
        // A chosen base that no longer yields a merge base (branch deleted, history rewritten)
        // must not wedge every refresh: drop the override and re-run inference once (F5).
        if let Err(e) = &result {
            if self.base_override.is_some() && e.to_string().contains("no base") {
                self.base_override = None;
                self.set_status(
                    "base branch gone; reverted to inferred base",
                    StatusLevel::Warning,
                );
                self.spawn_refresh();
                return;
            }
        }
        match result {
            Ok(snap) => {
                self.repo_ctx = Some(snap.repo_ctx.clone());
                self.changeset = Some(snap.changeset.clone());
                // A git-only pipeline (no engine) also lands here as Ok: it must not light
                // the top bar's LSP glyph nor erase the git-only warning. Semantic status
                // belongs to the engine lifecycle (EngineReady/EngineUnavailable).
                if self.engine.is_some() {
                    self.ls_status = LsStatus::Ready;
                    self.status = StatusMessage::default();
                }
                self.analysis = Some(*snap);
                // Relations were cleared at refresh start; resolve them against the new
                // analysis now that it has landed.
                if let Some((file, name, line, col)) = self.selected_symbol.clone() {
                    self.spawn_expand(file, name, line, col);
                }
            }
            Err(e) => {
                self.set_status(format!("analysis failed: {e}"), StatusLevel::Error);
            }
        }
        self.publish();
    }

    fn on_ai_done(&mut self, epoch: Epoch, generation: u64, outcome: AiOutcome) {
        if epoch != self.epoch {
            // A newer state superseded this plan; do not apply.
            return;
        }
        if generation != self.ai_request_seq {
            // A newer AI request in the same epoch superseded this one (review 18 M7).
            return;
        }
        match outcome {
            AiOutcome::Plan(plan, report) if report.is_renderable() => {
                let rows = plan_rows(&plan);
                // Pane title: the first form's title, else the plan's focus question.
                // (Rows carry per-form section headers, so the title is just the tab's
                // headline.)
                let title = plan
                    .forms
                    .first()
                    .map(|f| f.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| plan.focus.clone());
                self.ai_rows = Some((epoch, rows, title));
                self.ai_status = AiStatus::Ready { epoch };
            }
            AiOutcome::Stale => self.ai_status = AiStatus::Stale { epoch },
            AiOutcome::Failed(reason) => {
                // Every AI failure carries the recovery suffix (spec §3.6); the
                // deterministic impact pane is unaffected by the failure.
                self.set_status(
                    format!("AI: {reason} · {AI_FAILURE_SUFFIX}"),
                    StatusLevel::Warning,
                );
                self.ai_status = AiStatus::Failed { reason };
            }
            _ => self.ai_status = AiStatus::Idle,
        }
        self.publish();
    }

    fn publish_refreshing(&self) {
        let mut snap = self.build_snapshot();
        snap.refreshing = true;
        let _ = self.snapshot_tx.send(snap);
    }

    fn build_snapshot(&self) -> UiSnapshot {
        let (repo_bar, counts) = repo_bar(self.repo_ctx.as_ref());
        // Files come from the changeset immediately; symbol rows fill in lazily from the
        // per-file cache as the user expands files with Tab.
        let files = self
            .changeset
            .as_ref()
            .map(|cs| lazy_file_rows(cs, &self.file_semantics, &self.expanded_files))
            .unwrap_or_default();
        let (diff, semantic) = self.panes();
        let impact = self.build_impact();
        // The base shown in the top bar: the latest repo context's base (which already
        // reflects any override), else the pending override while a refresh is in flight.
        let base_ref = self
            .repo_ctx
            .as_ref()
            .and_then(|c| c.base.as_ref())
            .map(|b| b.ref_name.clone())
            .or_else(|| self.base_override.clone())
            .unwrap_or_default();
        UiSnapshot {
            repo: repo_bar,
            scope: self.scope,
            scope_counts: counts,
            files,
            diff,
            semantic,
            impact,
            ls: self.ls_status,
            ai: self.ai_status.clone(),
            ai_model: self.ai.as_ref().map(|a| a.model()).unwrap_or_default(),
            ai_provider: self
                .ai
                .as_ref()
                .map(|a| a.provider_label().to_string())
                .unwrap_or_default(),
            available_models: self.available_models.clone(),
            base_ref,
            available_bases: self.available_bases.clone(),
            message: self.status.text.clone(),
            status: self.status.clone(),
            epoch: self.epoch,
            refreshing: false,
        }
    }

    fn panes(&self) -> (DiffPane, SemanticPane) {
        let mut diff = self
            .changeset
            .as_ref()
            .map(|cs| selected_diff(cs, self.selected_file.as_deref()))
            .unwrap_or_default();
        // Publish the selected symbol's label for the diff title (spec §5.2) — only when
        // the diff actually shows that symbol's file; the first-file fallback and file
        // rows have no focused symbol. The full path stays the identity in `title`.
        diff.focused_symbol = match &self.selected_symbol {
            Some((file, name, _, _)) if *file == diff.title => Some(name.clone()),
            _ => None,
        };
        // `semantic` is the AI plan's durable slot: selecting a symbol must never swap it
        // out for deterministic rows. The selected symbol's lazy relations render via
        // `impact.callers`/`impact.downstream` (`build_impact`); the legacy flattened
        // relation rows are gone.
        // AI rows render only while their epoch matches the current repo state (H3).
        let semantic = match &self.ai_rows {
            Some((ep, rows, title)) if *ep == self.epoch => SemanticPane {
                title: title.clone(),
                rows: rows.clone(),
                note: String::new(),
                ai_generated: true,
            },
            Some(_) => SemanticPane {
                title: "impact".to_string(),
                rows: Vec::new(),
                note: "AI view stale (repo changed); regenerating…".to_string(),
                ai_generated: false,
            },
            // Deterministic relations live in `impact`; `semantic` stays AI-only.
            None => SemanticPane::default(),
        };
        (diff, semantic)
    }

    /// Assemble the impact pane (spec §5.3–§5.7): the deterministic selected change plus
    /// the callers/downstream columns. Lazy LSP relations and the one-hop impact graph
    /// merge into both lists; AI plan rows never replace this pane.
    fn build_impact(&self) -> ImpactPane {
        // Selected change: the symbol's per-file cache entry carries its change kind and
        // interpretation; a file row falls back to the file-level summary.
        let mut impact = ImpactPane {
            selected_change: self.selected_change_lazy(),
            ..Default::default()
        };
        let Some((file, name, _, _)) = &self.selected_symbol else {
            return impact;
        };
        // Relations only make sense for a symbol whose file analysis is Ready (the
        // identity is verified against that result).
        let file_ready = matches!(
            self.file_semantics.get(file),
            Some(FileSemanticState::Ready(_))
        );
        if !file_ready {
            if self.engine.is_some()
                && matches!(
                    self.file_semantics.get(file),
                    Some(FileSemanticState::Loading)
                )
            {
                impact.callers.state = ImpactLoadState::Loading;
                impact.downstream.state = ImpactLoadState::Loading;
            }
            return impact;
        }
        match &self.selected_relations {
            Some(relations) => {
                impact.callers = ImpactList {
                    rows: relations.callers.rows.clone(),
                    state: ImpactLoadState::Ready,
                    partial: relations.callers.partial,
                };
                impact.downstream = ImpactList {
                    rows: relations.callees.rows.clone(),
                    state: ImpactLoadState::Ready,
                    partial: relations.callees.partial,
                };
            }
            None => {
                let state = if self.engine.is_some() {
                    ImpactLoadState::Loading
                } else {
                    ImpactLoadState::Unavailable
                };
                impact.callers.state = state;
                impact.downstream.state = state;
            }
        }
        let _ = name;
        if impact.callers.partial || impact.downstream.partial {
            impact.note = "partial: some relationships unavailable".to_string();
        }
        impact
    }

    /// The SelectedChange for the impact pane's left column, sourced from the lazy
    /// per-file cache (symbol rows) or the changeset (file rows).
    fn selected_change_lazy(&self) -> Option<SelectedChange> {
        if let Some((file, name, line, col)) = &self.selected_symbol {
            let info = self
                .file_semantics
                .get(file)
                .and_then(|s| match s {
                    FileSemanticState::Ready(res) => Some(res),
                    _ => None,
                })
                .and_then(|res| {
                    res.changed
                        .iter()
                        .find(|c| {
                            c.name == *name
                                && c.file.as_path().as_str() == file
                                && (c.selection.start_line, c.selection.start_col) == (*line, *col)
                        })
                        .or_else(|| {
                            // Position drifted after an edit: fall back to name+file.
                            res.changed
                                .iter()
                                .find(|c| c.name == *name && c.file.as_path().as_str() == file)
                        })
                });
            let (change, interpretation) = match info {
                Some(info) => (
                    change_label(info.record.change_kind),
                    interpret_change(info),
                ),
                None => ("modified", String::new()),
            };
            return Some(SelectedChange {
                file: file.clone(),
                label: name.clone(),
                change,
                interpretation,
                interpretation_source: InterpretationSource::Deterministic,
            });
        }
        let file = self.selected_file.as_deref()?;
        // The numeric count is real only for Ready; other states describe themselves
        // (review 18 m6).
        let interpretation = match self.file_semantics.get(file) {
            Some(FileSemanticState::Ready(res)) => {
                let n = res.changed.len();
                format!(
                    "{n} changed symbol{} in this file; select one to inspect impact.",
                    if n == 1 { "" } else { "s" }
                )
            }
            Some(FileSemanticState::Loading) => "analyzing symbols…".to_string(),
            Some(FileSemanticState::Unsupported) => {
                "semantic analysis unavailable for this file".to_string()
            }
            Some(FileSemanticState::Failed) => "symbol analysis failed; Tab to retry".to_string(),
            None => "not analyzed; Tab to load symbols".to_string(),
        };
        let change = self
            .changeset
            .as_ref()
            .and_then(|cs| cs.files.iter().find(|f| f.path.as_str() == file))
            .map(|f| file_change_label(&f.status))
            .unwrap_or("modified");
        Some(SelectedChange {
            file: file.to_string(),
            label: file.to_string(),
            change,
            interpretation,
            interpretation_source: InterpretationSource::Deterministic,
        })
    }
}

/// The git+analysis pipeline, run as one spawned job. A base override (from the base
/// picker) flows into the repo context and, for the `Branch` scope, into the diff itself.
async fn run_pipeline(
    repo: GitRepo,
    scope: ChangeScope,
    engine: Option<std::sync::Arc<AnalysisEngine<LanguageService>>>,
    epoch: Epoch,
    base_override: Option<String>,
    tx: &mpsc::Sender<DispatchEvent>,
) -> anyhow::Result<AnalysisSnapshot> {
    let ctx = repo
        .repo_context_with_base(base_override.as_deref())
        .await?;
    let changeset = match (scope, &base_override) {
        (ChangeScope::Branch, Some(base)) => repo.branch_changeset_with_base(base).await?,
        _ => repo.changeset(scope).await?,
    };
    // Report the git-level result immediately: the files pane and top bar can render long
    // before the language server finishes. Failure to send just means the UI waits for the
    // full result — never a correctness issue.
    let _ = tx
        .send(DispatchEvent::ChangesetReady {
            epoch,
            ctx: ctx.clone(),
            changeset: changeset.clone(),
        })
        .await;
    // Lazy semantics (the interactive path): the pipeline ends at the git phase. The
    // language server analyzes a file only when the user expands it with Tab — a
    // repo-wide refresh over every changed file took tens of seconds on large repos and
    // blocked the files pane on startup. `engine` is accepted but unused here; per-file
    // jobs go through `Dispatcher::spawn_file_analysis`. The non-interactive backend
    // (`backend analyze/digest`) still calls `refresh_with_ctx` directly.
    let _ = engine;
    Ok(git_only_snapshot(epoch, ctx, changeset))
}

fn git_only_snapshot(
    epoch: Epoch,
    repo_ctx: codescope_core::RepoContext,
    changeset: codescope_core::ChangeSet,
) -> AnalysisSnapshot {
    use codescope_core::{Completeness, Evidence, ImpactGraph};
    AnalysisSnapshot {
        epoch,
        repo_ctx,
        changeset,
        files: Vec::new(),
        changed: Vec::new(),
        graph: Evidence {
            value: ImpactGraph::default(),
            completeness: Completeness::Unknown,
            notes: vec!["language server unavailable; git-only view".to_string()],
        },
        diagnostics: Vec::new(),
    }
}

// -- snapshot assembly helpers (pure) -----------------------------------------

fn repo_bar(ctx: Option<&codescope_core::RepoContext>) -> (RepoBar, ScopeCounts) {
    let Some(ctx) = ctx else {
        return (RepoBar::default(), ScopeCounts::default());
    };
    let branch = match &ctx.head {
        codescope_core::HeadState::Branch(b) => b.clone(),
        codescope_core::HeadState::Detached(_) => "(detached)".to_string(),
        codescope_core::HeadState::Unborn => "(no commits)".to_string(),
    };
    let base = ctx
        .base
        .as_ref()
        .map(|b| b.ref_name.clone())
        .or_else(|| ctx.upstream.as_ref().map(|u| u.name.clone()));
    let (ahead, behind) = ctx
        .upstream
        .as_ref()
        .map(|u| (u.ahead, u.behind))
        .unwrap_or((0, 0));
    let repo_name = ctx.toplevel.file_name().unwrap_or("repo").to_string();
    (
        RepoBar {
            repo_name,
            branch,
            base,
            ahead,
            behind,
        },
        ScopeCounts::default(),
    )
}

/// One files-pane row from the changeset + the lazy per-file cache. `expanded` comes
/// from the dispatcher's `expanded_files` (the app's Tab toggles); symbol rows come from
/// a Ready per-file result. Unloaded rows show no symbol count.
fn lazy_file_rows(
    cs: &codescope_core::ChangeSet,
    semantics: &std::collections::HashMap<String, FileSemanticState>,
    expanded: &std::collections::HashSet<String>,
) -> Vec<FileRow> {
    cs.files
        .iter()
        .map(|f| {
            let path = f.path.to_string();
            let state = semantics.get(&path);
            let (symbols, semantic) = match state {
                Some(FileSemanticState::Ready(res)) => (
                    changed_info_to_symbol_rows(&res.changed, &res.diagnostics),
                    codescope_tui::snapshot::FileSemanticLoad::Ready,
                ),
                Some(FileSemanticState::Loading) => (
                    Vec::new(),
                    codescope_tui::snapshot::FileSemanticLoad::Loading,
                ),
                Some(FileSemanticState::Unsupported) => (
                    Vec::new(),
                    codescope_tui::snapshot::FileSemanticLoad::Unsupported,
                ),
                Some(FileSemanticState::Failed) => (
                    Vec::new(),
                    codescope_tui::snapshot::FileSemanticLoad::Failed,
                ),
                None => (
                    Vec::new(),
                    codescope_tui::snapshot::FileSemanticLoad::Unloaded,
                ),
            };
            FileRow {
                changed_symbol_count: symbols.len(),
                symbols,
                path,
                status: status_badge(&f.status),
                expanded: expanded.contains(f.path.as_str()),
                semantic,
            }
        })
        .collect()
}

/// Map one file's changed-symbol records to display rows (was the eager `file_rows`
/// mapping; now per file for the lazy cache).
fn changed_info_to_symbol_rows(
    changed: &[codescope_analysis::ChangedSymbolInfo],
    diagnostics: &[codescope_core::Diagnostic],
) -> Vec<SymbolRow> {
    changed
        .iter()
        .map(|c| {
            let change = change_label(c.record.change_kind);
            let confidence = match &c.record.confidence {
                codescope_core::MappingConfidence::Exact => "",
                codescope_core::MappingConfidence::Approximate(_) => "~",
                codescope_core::MappingConfidence::Unmapped => "?",
            };
            let has_diag = diagnostics.iter().any(|d| {
                d.file == c.file
                    && d.range.start_line <= c.range.end_line
                    && d.range.end_line >= c.range.start_line
            });
            SymbolRow {
                name: c.name.clone(),
                change,
                confidence,
                has_diagnostic: has_diag,
                position: Some((c.selection.start_line, c.selection.start_col)),
            }
        })
        .collect()
}

fn status_badge(status: &codescope_core::FileStatus) -> &'static str {
    use codescope_core::FileStatus as S;
    match status {
        S::Added => "A",
        S::Untracked => "?",
        S::Modified => "M",
        S::Deleted => "D",
        S::Renamed { .. } | S::Copied { .. } => "R",
        S::Unmerged => "U",
        _ => "M",
    }
}

/// The diff pane for `selected` (a repo-relative path), falling back to the changeset's
/// first file when nothing is selected or the selected file is absent from the set (e.g. a
/// scope switch dropped it).
fn selected_diff(a: &codescope_core::ChangeSet, selected: Option<&str>) -> DiffPane {
    let file = selected
        .and_then(|path| a.files.iter().find(|f| f.path.as_str() == path))
        .or_else(|| a.files.first());
    let Some(file) = file else {
        return DiffPane::default();
    };
    let mut rows = Vec::new();
    let mut hunk_no = 0;
    for h in &file.hunks {
        hunk_no += 1;
        let section = h.section.as_deref().unwrap_or("");
        rows.push(DiffRow::HunkHeader(format!(
            "@@ -{},{} +{},{} @@ {}",
            h.old_start, h.old_len, h.new_start, h.new_len, section
        )));
        for l in &h.lines {
            match l.kind {
                codescope_core::DiffLineKind::Add => rows.push(DiffRow::Add {
                    new_ln: l.new_ln.unwrap_or(0),
                    text: l.text.clone(),
                }),
                codescope_core::DiffLineKind::Del => rows.push(DiffRow::Del {
                    old_ln: l.old_ln.unwrap_or(0),
                    text: l.text.clone(),
                }),
                codescope_core::DiffLineKind::Context => rows.push(DiffRow::Context {
                    old_ln: l.old_ln.unwrap_or(0),
                    new_ln: l.new_ln.unwrap_or(0),
                    text: l.text.clone(),
                }),
            }
        }
    }
    DiffPane {
        title: file.path.to_string(),
        // Set by the dispatcher, which owns the selection identity.
        focused_symbol: None,
        rows,
        current_hunk: if hunk_no > 0 { 1 } else { 0 },
        total_hunks: hunk_no,
    }
}

/// The deterministic one-line interpretation of a changed symbol (spec §3.5). AI may
/// replace this sentence only with a validated, epoch-matched result tied to the same
/// selected entity; today's AI output is repository-wide, so this always stands.
fn interpret_change(info: &codescope_analysis::ChangedSymbolInfo) -> String {
    let hunks = info.record.hunks.len();
    let kind = format!("{:?}", info.kind).to_lowercase();
    match info.record.change_kind {
        codescope_core::ChangeKind::Added => format!("Added {kind} across {hunks} hunks."),
        codescope_core::ChangeKind::Modified if info.signature_touch => {
            format!("Modified signature and implementation across {hunks} hunks.")
        }
        codescope_core::ChangeKind::Modified => {
            format!("Modified implementation across {hunks} hunks.")
        }
        codescope_core::ChangeKind::Deleted => {
            format!("Removed {kind}; callers may require updates.")
        }
    }
}

/// `added` / `modified` / `removed` for a symbol change kind.
fn change_label(kind: codescope_core::ChangeKind) -> &'static str {
    match kind {
        codescope_core::ChangeKind::Added => "added",
        codescope_core::ChangeKind::Modified => "modified",
        codescope_core::ChangeKind::Deleted => "removed",
    }
}

/// `added` / `modified` / `removed` for a file-level selection (from the file status).
fn file_change_label(status: &codescope_core::FileStatus) -> &'static str {
    use codescope_core::FileStatus as S;
    match status {
        S::Added | S::Untracked => "added",
        S::Deleted => "removed",
        _ => "modified",
    }
}

/// Flatten a validated plan into display rows. Every form renders, in plan order:
/// a section header (the form's title + kind — the provenance of the rows beneath),
/// its summary lines, then its nodes and edges. Tree forms nest via `children`;
/// flow/sequence/other forms list their nodes flat and their edges as relationship
/// rows (`from → to · kind [· edge label]`). Previously only the first form's tree
/// roots rendered, silently dropping the second form, summaries, and every edge.
fn plan_rows(plan: &codescope_core::VisualizationPlan) -> Vec<SemRow> {
    let mut rows = Vec::new();
    for form in &plan.forms {
        // Section header: which form this block answers with.
        rows.push(SemRow {
            depth: 0,
            label: form.title.clone(),
            relation: form_kind_label(form.kind),
            changed: false,
            has_diagnostic: false,
        });
        for line in form.summary.lines() {
            let line = line.trim();
            if !line.is_empty() {
                rows.push(SemRow {
                    depth: 1,
                    label: line.to_string(),
                    relation: "",
                    changed: false,
                    has_diagnostic: false,
                });
            }
        }
        let by_id: std::collections::HashMap<&str, &codescope_core::PlanNode> =
            form.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        if form.kind.is_tree_form() {
            // Tree: roots (nodes nobody claims as a child) recurse through children.
            let is_child: HashSet<&str> = form
                .nodes
                .iter()
                .flat_map(|n| n.children.iter().map(String::as_str))
                .collect();
            for n in &form.nodes {
                if !is_child.contains(n.id.as_str()) {
                    push_plan_node(n, &by_id, 1, &mut rows);
                }
            }
        } else {
            // Flow/sequence/summary/diff forms: nodes are peers, edges carry the shape.
            for n in &form.nodes {
                rows.push(plan_node_row(n, 1));
            }
        }
        for e in &form.edges {
            let from = by_id
                .get(e.from.as_str())
                .map(|n| n.label.as_str())
                .unwrap_or(&e.from);
            let to = by_id
                .get(e.to.as_str())
                .map(|n| n.label.as_str())
                .unwrap_or(&e.to);
            let label = match &e.label {
                Some(l) if !l.is_empty() => format!("{from} → {to} · {l}"),
                _ => format!("{from} → {to}"),
            };
            rows.push(SemRow {
                depth: 1,
                label,
                relation: edge_kind_label(e.kind),
                changed: false,
                has_diagnostic: false,
            });
        }
    }
    rows
}

/// A node as a display row (change badge + diagnostic marker preserved).
fn plan_node_row(n: &codescope_core::PlanNode, depth: u16) -> SemRow {
    SemRow {
        depth,
        label: n.label.clone(),
        relation: "",
        changed: !matches!(n.change, codescope_core::PlanNodeChange::Unchanged),
        has_diagnostic: n.severity.is_some(),
    }
}

/// Short static label for a form's kind (row provenance in the AI plan view).
fn form_kind_label(kind: codescope_core::FormKind) -> &'static str {
    use codescope_core::FormKind as F;
    match kind {
        F::ChangedSymbolTree => "changed symbols",
        F::CallTree => "call tree",
        F::TypeImplTree => "types",
        F::RelationshipFlow => "flow",
        F::ImpactSummary => "summary",
        F::FocusedDiff => "diff",
        F::BeforeAfter => "before/after",
        F::Sequence => "sequence",
    }
}

/// Short static label for a plan edge's relationship kind.
fn edge_kind_label(kind: PlanEdgeKind) -> &'static str {
    match kind {
        PlanEdgeKind::Calls => "calls",
        PlanEdgeKind::Imports => "imports",
        PlanEdgeKind::Implements => "implements",
        PlanEdgeKind::Contains => "contains",
        PlanEdgeKind::Reads => "reads",
        PlanEdgeKind::Writes => "writes",
    }
}

fn push_plan_node(
    n: &codescope_core::PlanNode,
    by_id: &std::collections::HashMap<&str, &codescope_core::PlanNode>,
    depth: u16,
    rows: &mut Vec<SemRow>,
) {
    rows.push(plan_node_row(n, depth));
    for c in &n.children {
        if let Some(child) = by_id.get(c.as_str()) {
            push_plan_node(child, by_id, depth + 1, rows);
        }
    }
}

// -- FactView over the current analysis snapshot ------------------------------

struct SnapshotFacts {
    files: HashSet<String>,
    symbols: std::collections::HashMap<(String, String), LineRange>,
    edges: HashSet<(String, String, PlanEdgeKind)>,
    hunks: std::collections::HashMap<String, usize>,
}

impl SnapshotFacts {
    /// Facts for the AI validator in the lazy world: the changeset's files/hunks plus
    /// the symbols of files the user has explicitly analyzed (Ready). Unloaded files
    /// contribute their git identity only — the validator never sees symbols that have
    /// not actually been computed.
    fn from_lazy(
        changeset: &codescope_core::ChangeSet,
        semantics: &std::collections::HashMap<String, FileSemanticState>,
    ) -> Self {
        let mut facts = SnapshotFacts {
            files: HashSet::new(),
            symbols: std::collections::HashMap::new(),
            edges: HashSet::new(),
            hunks: std::collections::HashMap::new(),
        };
        for f in &changeset.files {
            facts.files.insert(f.path.to_string());
            facts.hunks.insert(f.path.to_string(), f.hunks.len());
        }
        for res in semantics.values() {
            if let FileSemanticState::Ready(res) = res {
                for c in &res.changed {
                    facts.files.insert(c.file.to_string());
                    facts
                        .symbols
                        .insert((c.file.to_string(), c.name.clone()), c.range);
                }
            }
        }
        facts
    }
}

fn entity_key(e: &EntityRef) -> String {
    match &e.symbol {
        Some(s) => format!("{}::{s}", e.file),
        None => e.file.to_string(),
    }
}

impl FactView for SnapshotFacts {
    fn file_exists(&self, file: &codescope_core::FileId) -> bool {
        self.files.contains(&file.to_string())
    }
    fn resolve_symbol(&self, file: &codescope_core::FileId, name: &str) -> Option<LineRange> {
        self.symbols
            .get(&(file.to_string(), name.to_string()))
            .copied()
    }
    fn edge_exists(&self, from: &EntityRef, to: &EntityRef, kind: PlanEdgeKind) -> bool {
        self.edges
            .contains(&(entity_key(from), entity_key(to), kind))
    }
    fn hunk(&self, file: &codescope_core::FileId, index: u32) -> Option<()> {
        self.hunks
            .get(&file.to_string())
            .and_then(|&n| (index < n as u32).then_some(()))
    }
}

/// Fetch a symbol's callers + callees lazily and shape them as impact rows. The
/// evidence's completeness is NOT discarded (spec §5.4): an incomplete answer (timeout,
/// truncation, unsupported server feature) sets `partial` so the UI can say so instead
/// of implying an exhaustive list.
pub(crate) async fn relations_for(
    engine: &std::sync::Arc<codescope_analysis::AnalysisEngine<codescope_lsp::LanguageService>>,
    file: &codescope_core::FileId,
    pos: codescope_core::Position,
) -> (RelationRows, RelationRows) {
    let callers = engine.callers_of(file, pos).await;
    let callees = engine.callees_of(file, pos).await;
    let to_rows = |ev: codescope_core::Evidence<Vec<codescope_core::SymbolRef>>| RelationRows {
        partial: !ev.is_complete(),
        rows: ev
            .value
            .iter()
            .map(|r| ImpactRow {
                label: r.name.clone(),
                relation: "calls",
                changed: false,
                has_diagnostic: false,
            })
            .collect(),
    };
    (to_rows(callers), to_rows(callees))
}

/// Run the dispatcher loop until the TUI closes the action channel.
pub async fn run(
    mut disp: Dispatcher,
    mut events: mpsc::Receiver<DispatchEvent>,
    mut actions: mpsc::Receiver<Action>,
) {
    let _ = disp.handle(DispatchEvent::RepoChanged).await;
    loop {
        tokio::select! {
            e = events.recv() => match e {
                Some(e) => {
                    // Coalesce a burst of RepoChanged events into one refresh. Without this, an
                    // editing/build storm bumps the epoch faster than a refresh can complete,
                    // and the epoch-gate drops every result (the TUI never gets data). Every
                    // other event is a one-shot job result (analysis, AI plan, relations) and
                    // must never be dropped behind a repo change.
                    let mut batch = vec![e];
                    while let Ok(next) = events.try_recv() {
                        batch.push(next);
                    }
                    let last_change = batch
                        .iter()
                        .rposition(|ev| matches!(ev, DispatchEvent::RepoChanged));
                    for (i, ev) in batch.into_iter().enumerate() {
                        if matches!(ev, DispatchEvent::RepoChanged) && Some(i) != last_change {
                            continue;
                        }
                        disp.handle(ev).await;
                    }
                }
                None => break,
            },
            a = actions.recv() => match a {
                Some(a) => {
                    // SelectionChanged is latest-wins state (where the files-pane selection
                    // sits): keep only the newest in a burst. Every other action is a one-shot
                    // command (scope change, refresh, picker choice) and must never be dropped
                    // behind a selection update.
                    let mut batch = vec![a];
                    while let Ok(next) = actions.try_recv() {
                        batch.push(next);
                    }
                    let last_selection = batch
                        .iter()
                        .rposition(|act| matches!(act, Action::SelectionChanged { .. }));
                    for (i, act) in batch.into_iter().enumerate() {
                        if matches!(act, Action::SelectionChanged { .. })
                            && Some(i) != last_selection
                        {
                            continue;
                        }
                        disp.handle(DispatchEvent::Work(act)).await;
                    }
                }
                None => break,
            },
        }
    }
    // Graceful language-server shutdown (rv-lsp F3): do not leave gopls to kill_on_drop.
    if let Some(engine) = disp.engine.take() {
        if let Ok(engine) = std::sync::Arc::try_unwrap(engine) {
            engine.into_service().shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Build a throwaway repo: one commit on `main`, one more on `feature` (checked out).
    /// Plain git CLI so the test needs no extra dev-dependencies.
    fn scratch_repo() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "codescope-base-picker-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@test.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@test.invalid")
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "--quiet", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\n").expect("write a.txt");
        git(&["add", "."]);
        git(&["commit", "--quiet", "--no-verify", "-m", "base"]);
        git(&["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(dir.join("a.txt"), "two\n").expect("edit a.txt");
        git(&["add", "."]);
        git(&["commit", "--quiet", "--no-verify", "-m", "feature work"]);
        dir
    }

    async fn dispatcher_for(
        root: &std::path::Path,
    ) -> (
        Dispatcher,
        watch::Receiver<UiSnapshot>,
        mpsc::Receiver<DispatchEvent>,
    ) {
        let repo_root =
            camino::Utf8PathBuf::from_path_buf(root.to_path_buf()).expect("utf-8 temp path");
        let repo = GitRepo::discover(&repo_root)
            .await
            .expect("discover scratch repo");
        let (snapshot_tx, snapshot_rx) = watch::channel(UiSnapshot::placeholder());
        let (job_tx, job_rx) = mpsc::channel(16);
        (
            Dispatcher::new(repo, None, None, snapshot_tx, job_tx),
            snapshot_rx,
            job_rx,
        )
    }

    /// Receive dispatcher events until `pred` matches (spawned jobs report back here).
    async fn recv_until(
        rx: &mut mpsc::Receiver<DispatchEvent>,
        pred: fn(&DispatchEvent) -> bool,
    ) -> DispatchEvent {
        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
                .await
                .expect("timed out waiting for a dispatcher event")
                .expect("event channel closed");
            if pred(&ev) {
                return ev;
            }
        }
    }

    #[tokio::test]
    async fn base_selected_sets_override_and_updates_top_bar_base() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;

        disp.handle(DispatchEvent::Work(Action::BaseSelected(
            "main".to_string(),
        )))
        .await;
        assert_eq!(
            disp.base_override.as_deref(),
            Some("main"),
            "override recorded"
        );
        // The refreshing snapshot already advertises the pending base.
        assert_eq!(snapshot_rx.borrow().base_ref, "main");

        // The spawned pipeline reports back; apply it and check the re-published snapshot.
        let done = recv_until(&mut job_rx, |e| {
            matches!(e, DispatchEvent::AnalysisDone { .. })
        })
        .await;
        disp.handle(done).await;
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(
            snap.base_ref, "main",
            "top-bar base comes from the override"
        );
        assert_eq!(snap.repo.base.as_deref(), Some("main"));
        assert_eq!(snap.repo.branch, "feature");
        assert_eq!(snap.files.len(), 1, "one file changed vs main");

        std::fs::remove_dir_all(&root).ok();
    }

    /// Regression test for "branch mode looks broken on large repos": the files pane and
    /// top bar must populate from the git phase (`ChangesetReady`) without waiting for the
    /// language-server analysis (`AnalysisDone`), which can take tens of seconds.
    #[tokio::test]
    async fn changeset_ready_populates_files_before_analysis_lands() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;

        disp.handle(DispatchEvent::RepoChanged).await;
        // The git phase reports first…
        let ready = recv_until(&mut job_rx, |e| {
            matches!(e, DispatchEvent::ChangesetReady { .. })
        })
        .await;
        disp.handle(ready).await;
        {
            let snap = snapshot_rx.borrow().clone();
            assert_eq!(snap.repo.branch, "feature", "top bar populated early");
            assert_eq!(snap.files.len(), 1, "changed files visible before analysis");
            assert_eq!(snap.files[0].path, "a.txt");
            assert!(snap.refreshing, "spinner stays on until analysis lands");
        }
        // …then the analysis result arrives and keeps the same files.
        let done = recv_until(&mut job_rx, |e| {
            matches!(e, DispatchEvent::AnalysisDone { .. })
        })
        .await;
        disp.handle(done).await;
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.files.len(), 1);
        assert!(!snap.refreshing);

        std::fs::remove_dir_all(&root).ok();
    }

    /// Regression test: starting a refresh must not blank the top bar. While a refresh is
    /// in flight the previously known repo/branch/base stay visible (the spinner signals
    /// staleness); an empty bar reading "codescope ?" made branch mode look broken.
    #[tokio::test]
    async fn refresh_keeps_previous_repo_bar_visible() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;

        // Land the initial state.
        disp.handle(DispatchEvent::RepoChanged).await;
        let done = recv_until(&mut job_rx, |e| {
            matches!(e, DispatchEvent::AnalysisDone { .. })
        })
        .await;
        disp.handle(done).await;
        assert_eq!(snapshot_rx.borrow().repo.branch, "feature");

        // Start a second refresh: the bar must not reset to placeholder values.
        disp.handle(DispatchEvent::RepoChanged).await;
        let snap = snapshot_rx.borrow().clone();
        assert!(snap.refreshing);
        assert_eq!(snap.repo.branch, "feature", "branch survives refresh start");
        assert!(
            !snap.repo.repo_name.is_empty(),
            "repo name survives refresh start"
        );
        assert!(
            !snap.files.is_empty(),
            "files survive refresh start (stale data + spinner beats an empty pane)"
        );

        // Drain the follow-up job so the dispatcher is not dropped with it in flight.
        let done = recv_until(&mut job_rx, |e| {
            matches!(e, DispatchEvent::AnalysisDone { .. })
        })
        .await;
        disp.handle(done).await;

        std::fs::remove_dir_all(&root).ok();
    }

    /// The dispatcher owns the scope: a forwarded scope action must update its state and
    /// every published snapshot from then on (the TUI renders the published scope as the
    /// source of truth). Regression test for the scope flicker/reset bug.
    #[tokio::test]
    async fn scope_action_updates_published_scope() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;
        assert_eq!(snapshot_rx.borrow().scope, ChangeScope::Branch);

        disp.handle(DispatchEvent::Work(Action::ScopeStaged)).await;
        assert_eq!(disp.scope, ChangeScope::Staged);
        // The refreshing snapshot (published synchronously) already carries the new scope.
        assert_eq!(snapshot_rx.borrow().scope, ChangeScope::Staged);

        // The spawned refresh reports back; the re-published snapshot keeps the scope.
        let done = recv_until(&mut job_rx, |e| {
            matches!(e, DispatchEvent::AnalysisDone { .. })
        })
        .await;
        disp.handle(done).await;
        assert_eq!(snapshot_rx.borrow().scope, ChangeScope::Staged);

        // A repo-state refresh must not reset the scope either.
        disp.handle(DispatchEvent::RepoChanged).await;
        assert_eq!(snapshot_rx.borrow().scope, ChangeScope::Staged);

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn base_picker_loads_candidates() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;

        disp.handle(DispatchEvent::Work(Action::BasePicker)).await;
        let loaded = recv_until(&mut job_rx, |e| matches!(e, DispatchEvent::BaseLoaded(_))).await;
        disp.handle(loaded).await;
        let snap = snapshot_rx.borrow().clone();
        assert!(
            snap.available_bases.contains(&"main".to_string()),
            "candidates include the ancestor branch: {:?}",
            snap.available_bases
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A two-file changeset with one tiny hunk each, so the diff-pane tests can watch the
    /// pane follow the selection without running a refresh.
    fn two_file_changeset() -> codescope_core::ChangeSet {
        use codescope_core::{ChangeSet, DiffLine, FileChange, FileStatus, Hunk};
        let file = |path: &str, added: &str| FileChange {
            path: path.into(),
            old_path: None,
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 1,
                section: None,
                lines: vec![DiffLine::del(1, "old"), DiffLine::add(1, added)],
            }],
            binary: false,
        };
        ChangeSet::new(
            ChangeScope::Branch,
            vec![file("a.txt", "a-new"), file("b.txt", "b-new")],
        )
    }

    fn impact_row(label: &str) -> ImpactRow {
        ImpactRow {
            label: label.to_string(),
            relation: "calls",
            changed: false,
            has_diagnostic: false,
        }
    }

    fn relation_rows(labels: &[&str]) -> RelationRows {
        RelationRows {
            rows: labels.iter().map(|l| impact_row(l)).collect(),
            partial: false,
        }
    }

    #[tokio::test]
    async fn selection_changed_retargets_the_diff() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        disp.publish();
        assert_eq!(
            snapshot_rx.borrow().diff.title,
            "a.txt",
            "with no selection the diff shows the first file"
        );

        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("b.txt".to_string()),
            symbol: None,
        }))
        .await;
        assert_eq!(disp.selected_file.as_deref(), Some("b.txt"));
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.diff.title, "b.txt", "the diff follows the selection");
        assert!(
            snap.diff
                .rows
                .iter()
                .any(|r| matches!(r, DiffRow::Add { text, .. } if text == "b-new")),
            "the retargeted diff renders b.txt's hunk"
        );

        // A selection the changeset no longer contains falls back to the first file.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("gone.txt".to_string()),
            symbol: None,
        }))
        .await;
        assert_eq!(snapshot_rx.borrow().diff.title, "a.txt");

        // No selection at all (empty file list) falls back the same way.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: None,
            symbol: None,
        }))
        .await;
        assert_eq!(snapshot_rx.borrow().diff.title, "a.txt");

        std::fs::remove_dir_all(&root).ok();
    }

    /// Navigation onto a symbol row records the target and (with an engine) spawns the
    /// lazy expand; the result then lands in `selected_relations` (rendered via the
    /// impact columns). This fixture has no engine, so the spawn is skipped — the
    /// recorded target is what gates the fetch result below.
    #[tokio::test]
    async fn selection_changed_on_a_symbol_fetches_and_shows_relations() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());

        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: Some(("sym0".to_string(), 1, 4)),
        }))
        .await;
        assert_eq!(
            disp.selected_symbol,
            Some(("a.txt".to_string(), "sym0".to_string(), 1, 4)),
            "the symbol target is recorded for the staleness gate"
        );
        assert!(disp.selected_relations.is_none());

        // The fetch result for the CURRENT symbol applies: the relations are stored.
        disp.handle(DispatchEvent::RelationsLoaded {
            epoch: disp.epoch,
            file: "a.txt".to_string(),
            name: "sym0".to_string(),
            line: 1,
            col: 4,
            callers: relation_rows(&["caller_fn"]),
            callees: relation_rows(&["callee_fn"]),
        })
        .await;
        let relations = disp.selected_relations.as_ref().expect("relations stored");
        let callers: Vec<&str> = relations
            .callers
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        let callees: Vec<&str> = relations
            .callees
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(callers, ["caller_fn"]);
        assert_eq!(callees, ["callee_fn"]);
        // The semantic pane is the AI plan's durable slot: deterministic relation rows
        // never leak into it (they render via `impact` once an analysis exists).
        let snap = snapshot_rx.borrow().clone();
        assert!(!snap.semantic.ai_generated);
        assert!(snap.semantic.rows.is_empty());

        // Navigating back to a file row clears the relations immediately.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: None,
        }))
        .await;
        assert!(disp.selected_symbol.is_none());
        assert!(disp.selected_relations.is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    /// A RelationsLoaded that no longer answers the current selection (or the current
    /// epoch) must never overwrite the pane: navigation without Enter makes slow fetches
    /// for rows the user already left common.
    #[tokio::test]
    async fn stale_relations_loaded_is_dropped() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: Some(("sym0".to_string(), 1, 4)),
        }))
        .await;
        let epoch = disp.epoch;

        // Identity mismatch: a result for a symbol the user navigated away from.
        disp.handle(DispatchEvent::RelationsLoaded {
            epoch,
            file: "a.txt".to_string(),
            name: "other_sym".to_string(),
            line: 9,
            col: 9,
            callers: relation_rows(&["stale"]),
            callees: RelationRows::default(),
        })
        .await;
        assert!(
            disp.selected_relations.is_none(),
            "a result for another symbol is dropped"
        );

        // Epoch mismatch: the same symbol, but computed against a superseded repo state.
        disp.handle(DispatchEvent::RelationsLoaded {
            epoch: epoch.next(),
            file: "a.txt".to_string(),
            name: "sym0".to_string(),
            line: 1,
            col: 4,
            callers: relation_rows(&["stale"]),
            callees: RelationRows::default(),
        })
        .await;
        assert!(
            disp.selected_relations.is_none(),
            "a result from another epoch is dropped"
        );

        // Nothing selected at all (user moved onto a file row before the result landed).
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: None,
        }))
        .await;
        disp.handle(DispatchEvent::RelationsLoaded {
            epoch: disp.epoch,
            file: "a.txt".to_string(),
            name: "sym0".to_string(),
            line: 1,
            col: 4,
            callers: relation_rows(&["stale"]),
            callees: RelationRows::default(),
        })
        .await;
        assert!(
            disp.selected_relations.is_none(),
            "a late result after leaving the symbol is dropped"
        );
        assert!(snapshot_rx.borrow().semantic.rows.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    /// Enter (SelectSymbol) still works: it records the target and the fetch result for it
    /// applies — navigation only makes Enter unnecessary, not broken.
    #[tokio::test]
    async fn select_symbol_enter_still_expands_relations() {
        let root = scratch_repo();
        let (mut disp, _snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.handle(DispatchEvent::Work(Action::SelectSymbol {
            file: "a.txt".to_string(),
            name: "sym0".to_string(),
            line: 1,
            col: 4,
        }))
        .await;
        assert_eq!(disp.selected_file.as_deref(), Some("a.txt"));
        assert_eq!(
            disp.selected_symbol,
            Some(("a.txt".to_string(), "sym0".to_string(), 1, 4))
        );

        disp.handle(DispatchEvent::RelationsLoaded {
            epoch: disp.epoch,
            file: "a.txt".to_string(),
            name: "sym0".to_string(),
            line: 1,
            col: 4,
            callers: relation_rows(&["caller_fn"]),
            callees: RelationRows::default(),
        })
        .await;
        let relations = disp.selected_relations.as_ref().expect("relations stored");
        assert_eq!(relations.callers.rows[0].label, "caller_fn");

        std::fs::remove_dir_all(&root).ok();
    }

    // -- impact pane (spec §4/§5) ---------------------------------------------

    /// A changed symbol for the impact tests: identifier at `(2, 4)`, `hunks` hunks.
    fn changed_symbol(
        file: &str,
        name: &str,
        kind: codescope_core::SymbolKind,
        change_kind: codescope_core::ChangeKind,
        hunks: u32,
        signature_touch: bool,
    ) -> codescope_analysis::ChangedSymbolInfo {
        codescope_analysis::ChangedSymbolInfo {
            file: codescope_core::FileId::new_unchecked(file),
            name: name.to_string(),
            kind,
            detail: None,
            range: codescope_core::LineRange::new(2, 0, 10, 1),
            selection: codescope_core::LineRange::new(2, 4, 2, 12),
            revision: codescope_core::Revision::Worktree,
            record: codescope_core::ChangedSymbol {
                symbol: codescope_core::SymbolId::new(format!("{file}:{name}")),
                change_kind,
                hunks: (0..hunks)
                    .map(|index| codescope_core::HunkId {
                        file: file.into(),
                        index,
                    })
                    .collect(),
                confidence: codescope_core::MappingConfidence::Exact,
            },
            signature_touch,
        }
    }

    /// A lazy per-file cache entry wrapping `changed` symbols (the post-redesign home
    /// of what `analysis_with` used to carry for the impact pane).
    fn ready_semantics(
        file: &str,
        changed: Vec<codescope_analysis::ChangedSymbolInfo>,
    ) -> FileSemanticState {
        FileSemanticState::Ready(Box::new(codescope_analysis::FileSemanticResult {
            file: codescope_core::FileId::new_unchecked(file),
            analysis: codescope_analysis::FileAnalysis {
                file: codescope_core::FileId::new_unchecked(file),
                status: codescope_core::FileStatus::Modified,
                worktree_query_failed: false,
                worktree: None,
                base: None,
                mappings: Vec::new(),
                notes: Vec::new(),
            },
            changed,
            diagnostics: Vec::new(),
            unsupported: false,
            worktree_failed: false,
        }))
    }

    /// The deterministic interpretation sentence for each change kind (spec §3.5).
    #[test]
    fn deterministic_interpretation_sentences_cover_each_change_kind() {
        use codescope_core::{ChangeKind, SymbolKind};
        let added = changed_symbol(
            "a.go",
            "NewHandler",
            SymbolKind::Function,
            ChangeKind::Added,
            3,
            false,
        );
        assert_eq!(interpret_change(&added), "Added function across 3 hunks.");

        let modified = changed_symbol(
            "a.go",
            "Handle",
            SymbolKind::Method,
            ChangeKind::Modified,
            2,
            false,
        );
        assert_eq!(
            interpret_change(&modified),
            "Modified implementation across 2 hunks."
        );

        let signature = changed_symbol(
            "a.go",
            "Handle",
            SymbolKind::Method,
            ChangeKind::Modified,
            1,
            true,
        );
        assert_eq!(
            interpret_change(&signature),
            "Modified signature and implementation across 1 hunks."
        );

        let removed = changed_symbol(
            "a.go",
            "Legacy",
            SymbolKind::Function,
            ChangeKind::Deleted,
            1,
            false,
        );
        assert_eq!(
            interpret_change(&removed),
            "Removed function; callers may require updates."
        );
    }

    /// Every AI failure maps to a Warning status carrying the retry/model/deterministic
    /// suffix (spec §3.6); the legacy `message` field mirrors the status text.
    #[tokio::test]
    async fn ai_failure_status_carries_retry_suffix() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;

        let generation = disp.ai_request_seq;
        disp.handle(DispatchEvent::AiDone {
            epoch: disp.epoch,
            generation,
            outcome: AiOutcome::Failed("ai request timed out after 20s".to_string()),
        })
        .await;
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.status.level, StatusLevel::Warning);
        assert_eq!(
            snap.status.text,
            "AI: ai request timed out after 20s · A retry · m change model · deterministic impact remains available"
        );
        assert_eq!(
            snap.message, snap.status.text,
            "the legacy message field mirrors the status text"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Analysis and engine failures map to typed status levels (spec §5.9): the LSP
    /// falling away degrades to git-only (Warning); an analysis failure is an Error.
    #[tokio::test]
    async fn analysis_and_engine_failures_map_to_status_levels() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;

        disp.handle(DispatchEvent::EngineUnavailable(
            "no supported language detected".to_string(),
        ))
        .await;
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.status.level, StatusLevel::Warning);
        assert_eq!(
            snap.status.text,
            "git-only (no supported language detected)"
        );

        disp.handle(DispatchEvent::AnalysisDone {
            epoch: disp.epoch,
            result: Err(anyhow::anyhow!("boom")),
        })
        .await;
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.status.level, StatusLevel::Error);
        assert_eq!(snap.status.text, "analysis failed: boom");

        std::fs::remove_dir_all(&root).ok();
    }

    /// Navigation alone publishes the selection's deterministic `SelectedChange`
    /// immediately (spec §5.3/§5.6): no Enter, no wait for the relations fetch.
    #[tokio::test]
    async fn selection_changed_publishes_selected_change_immediately() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        disp.file_semantics.insert(
            "a.txt".to_string(),
            ready_semantics(
                "a.txt",
                vec![changed_symbol(
                    "a.txt",
                    "sym0",
                    codescope_core::SymbolKind::Function,
                    codescope_core::ChangeKind::Modified,
                    2,
                    false,
                )],
            ),
        );

        // A symbol row: the deterministic interpretation + focused symbol publish at
        // once; the relation lists leave Idle (this fixture has no engine, so they can
        // never load — Unavailable, distinct from a fetch in flight).
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: Some(("sym0".to_string(), 2, 4)),
        }))
        .await;
        let snap = snapshot_rx.borrow().clone();
        let selected = snap
            .impact
            .selected_change
            .as_ref()
            .expect("a symbol row publishes its selected change immediately");
        assert_eq!(selected.file, "a.txt");
        assert_eq!(selected.label, "sym0");
        assert_eq!(selected.change, "modified");
        assert_eq!(
            selected.interpretation,
            "Modified implementation across 2 hunks."
        );
        assert_eq!(
            selected.interpretation_source,
            InterpretationSource::Deterministic
        );
        assert_eq!(
            snap.diff.focused_symbol.as_deref(),
            Some("sym0"),
            "the diff pane publishes the selected symbol's label"
        );
        assert_eq!(snap.impact.callers.state, ImpactLoadState::Unavailable);
        assert_eq!(snap.impact.downstream.state, ImpactLoadState::Unavailable);

        // A file row: the file-level fallback, both lists Idle, no focused symbol.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("b.txt".to_string()),
            symbol: None,
        }))
        .await;
        let snap = snapshot_rx.borrow().clone();
        let selected = snap
            .impact
            .selected_change
            .as_ref()
            .expect("a file row publishes the file-level fallback");
        assert_eq!(selected.label, "b.txt");
        assert_eq!(selected.change, "modified");
        assert_eq!(
            selected.interpretation, "not analyzed; Tab to load symbols",
            "an unanalyzed file says so, not a fake zero"
        );
        assert_eq!(snap.impact.callers.state, ImpactLoadState::Idle);
        assert_eq!(snap.impact.downstream.state, ImpactLoadState::Idle);
        assert!(snap.impact.callers.rows.is_empty());
        assert_eq!(snap.diff.focused_symbol, None);

        // No selection at all: the pane empties.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: None,
            symbol: None,
        }))
        .await;
        assert!(snapshot_rx.borrow().impact.selected_change.is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    /// RelationsLoaded stores callers and callees as separate impact columns and keeps
    /// the evidence honesty flag (spec §5.4); the semantic pane stays AI-only.
    #[tokio::test]
    async fn relations_loaded_populates_impact_columns() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        disp.file_semantics.insert(
            "a.txt".to_string(),
            ready_semantics(
                "a.txt",
                vec![changed_symbol(
                    "a.txt",
                    "sym0",
                    codescope_core::SymbolKind::Function,
                    codescope_core::ChangeKind::Modified,
                    1,
                    false,
                )],
            ),
        );
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: Some(("sym0".to_string(), 2, 4)),
        }))
        .await;

        disp.handle(DispatchEvent::RelationsLoaded {
            epoch: disp.epoch,
            file: "a.txt".to_string(),
            name: "sym0".to_string(),
            line: 2,
            col: 4,
            callers: RelationRows {
                rows: vec![impact_row("caller_fn")],
                partial: true, // e.g. the language server truncated the answer
            },
            callees: relation_rows(&["callee_fn"]),
        })
        .await;

        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.impact.callers.state, ImpactLoadState::Ready);
        assert_eq!(snap.impact.downstream.state, ImpactLoadState::Ready);
        let callers: Vec<&str> = snap
            .impact
            .callers
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(callers, ["caller_fn"]);
        let downstream: Vec<&str> = snap
            .impact
            .downstream
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(downstream, ["callee_fn"]);
        assert!(
            snap.impact.callers.partial,
            "incomplete evidence marks the list partial"
        );
        assert!(!snap.impact.downstream.partial);
        assert_eq!(snap.impact.note, "partial: some relationships unavailable");
        // The semantic pane is the AI plan's slot: deterministic relations never leak.
        assert!(!snap.semantic.ai_generated);
        assert!(snap.semantic.rows.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    /// The semantic pane is the AI plan's durable slot: a ready, epoch-current plan keeps
    /// publishing `ai_generated` rows even after a symbol's lazy relations land
    /// (previously the relations branch multiplexed the plan out of `semantic`, hiding it
    /// from the TUI). The relations render via the impact columns instead.
    #[tokio::test]
    async fn ready_ai_plan_survives_loaded_selected_relations() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        disp.file_semantics.insert(
            "a.txt".to_string(),
            ready_semantics(
                "a.txt",
                vec![changed_symbol(
                    "a.txt",
                    "sym0",
                    codescope_core::SymbolKind::Function,
                    codescope_core::ChangeKind::Modified,
                    1,
                    false,
                )],
            ),
        );
        // A validated plan for the current epoch.
        disp.ai_rows = Some((
            disp.epoch,
            vec![SemRow {
                depth: 0,
                label: "RetryPolicy".to_string(),
                relation: "changed",
                changed: true,
                has_diagnostic: false,
            }],
            "plan: retry budget".to_string(),
        ));
        disp.ai_status = AiStatus::Ready { epoch: disp.epoch };
        disp.publish();
        {
            let snap = snapshot_rx.borrow();
            assert!(snap.semantic.ai_generated);
            assert_eq!(snap.semantic.rows[0].label, "RetryPolicy");
        }

        // Select a symbol and land its relations: the plan must stay in `semantic`.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: Some(("sym0".to_string(), 2, 4)),
        }))
        .await;
        disp.handle(DispatchEvent::RelationsLoaded {
            epoch: disp.epoch,
            file: "a.txt".to_string(),
            name: "sym0".to_string(),
            line: 2,
            col: 4,
            callers: relation_rows(&["caller_fn"]),
            callees: relation_rows(&["callee_fn"]),
        })
        .await;

        let snap = snapshot_rx.borrow().clone();
        assert!(
            snap.semantic.ai_generated,
            "the ready plan survives loaded relations"
        );
        assert_eq!(snap.semantic.title, "plan: retry budget");
        assert_eq!(snap.semantic.rows[0].label, "RetryPolicy");
        // …and the relations are where the deterministic view reads them.
        let callers: Vec<&str> = snap
            .impact
            .callers
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(callers, ["caller_fn"]);

        std::fs::remove_dir_all(&root).ok();
    }

    /// Lazy startup: the initial refresh publishes every git file as collapsed +
    /// Unloaded with zero per-file analysis jobs launched.
    #[tokio::test]
    async fn startup_publishes_git_files_without_eager_analysis() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;
        disp.handle(DispatchEvent::RepoChanged).await;
        let done = recv_until(&mut job_rx, |e| {
            matches!(e, DispatchEvent::AnalysisDone { .. })
        })
        .await;
        disp.handle(done).await;
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.files.len(), 1, "git file listed");
        assert!(!snap.files[0].expanded, "initially collapsed");
        assert_eq!(
            snap.files[0].semantic,
            codescope_tui::snapshot::FileSemanticLoad::Unloaded
        );
        assert_eq!(
            snap.files[0].changed_symbol_count, 0,
            "no fake zero-symbol count"
        );
        // No per-file analysis job was launched (no FileAnalysisDone queued).
        assert!(
            job_rx.try_recv().is_err(),
            "no per-file analysis without an explicit expand"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// EngineReady lights LSP ✓ without starting repository-wide analysis.
    #[tokio::test]
    async fn engine_ready_does_not_start_eager_analysis() {
        let root = scratch_repo();
        let (mut disp, _snapshot_rx, mut job_rx) = dispatcher_for(&root).await;
        // No engine in the fixture: EngineUnavailable is the git-only path, and the point
        // is that neither readiness path triggers a repo-wide refresh.
        disp.handle(DispatchEvent::EngineUnavailable(
            "no supported language detected".into(),
        ))
        .await;
        assert_eq!(disp.ls_status, codescope_core::LsStatus::Failed);
        // No analysis job followed.
        assert!(
            job_rx.try_recv().is_err(),
            "no refresh spawned from the engine readiness path"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Tab on a collapsed file: one analysis request + a Loading row; repeated Tabs while
    /// in flight coalesce (collapse keeps the cache and relaunches nothing).
    #[tokio::test]
    async fn tab_expands_with_loading_row_and_coalesces() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;
        disp.handle(DispatchEvent::RepoChanged).await;
        let done = recv_until(&mut job_rx, |e| {
            matches!(e, DispatchEvent::AnalysisDone { .. })
        })
        .await;
        disp.handle(done).await;
        // Aim the selection at the file row.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: None,
        }))
        .await;

        // No engine yet (LsStatus::Starting in this fixture): the expand queues the
        // file as Loading rather than mislabeling it unsupported (review 18 m2); the
        // queued job waits for the engine.
        disp.handle(DispatchEvent::Work(Action::SetFileExpanded {
            path: "a.txt".to_string(),
            expanded: true,
        }))
        .await;
        {
            let snap = snapshot_rx.borrow().clone();
            assert!(snap.files[0].expanded, "expanded");
            assert_eq!(
                snap.files[0].semantic,
                codescope_tui::snapshot::FileSemanticLoad::Loading
            );
        }
        // Collapse, then re-expand: the second expand coalesces onto the queued job
        // (no duplicate spawn).
        disp.handle(DispatchEvent::Work(Action::SetFileExpanded {
            path: "a.txt".to_string(),
            expanded: false,
        }))
        .await;
        assert!(!snapshot_rx.borrow().files[0].expanded);
        disp.handle(DispatchEvent::Work(Action::SetFileExpanded {
            path: "a.txt".to_string(),
            expanded: true,
        }))
        .await;
        let snap = snapshot_rx.borrow().clone();
        assert!(snap.files[0].expanded);
        // One queued job, coalesced: the queue holds a single entry, no duplicates.
        assert_eq!(
            disp.analysis_queue
                .iter()
                .filter(|p| p.as_str() == "a.txt")
                .count(),
            1,
            "exactly one queued analysis for the file"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A Ready result expands into symbol rows; a stale-epoch result is dropped.
    #[tokio::test]
    async fn file_analysis_result_fills_symbols_and_stale_epochs_drop() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;
        disp.handle(DispatchEvent::RepoChanged).await;
        let done = recv_until(&mut job_rx, |e| {
            matches!(e, DispatchEvent::AnalysisDone { .. })
        })
        .await;
        disp.handle(done).await;
        let snap_epoch = snapshot_rx.borrow().epoch;
        // Land the two-file changeset via the event so data_epoch tracks it (the result
        // gate requires data_epoch == epoch — review 18 M1).
        let ctx = disp.repo_ctx.clone().expect("repo ctx after refresh");
        disp.handle(DispatchEvent::ChangesetReady {
            epoch: disp.epoch,
            ctx,
            changeset: two_file_changeset(),
        })
        .await;
        disp.expanded_files.insert("a.txt".to_string());

        // Current-epoch result: fills the row.
        disp.handle(DispatchEvent::FileAnalysisDone {
            epoch: disp.epoch,
            file: "a.txt".to_string(),
            result: Ok(Box::new(codescope_analysis::FileSemanticResult {
                file: codescope_core::FileId::new_unchecked("a.txt"),
                analysis: codescope_analysis::FileAnalysis {
                    file: codescope_core::FileId::new_unchecked("a.txt"),
                    status: codescope_core::FileStatus::Modified,
                    worktree_query_failed: false,
                    worktree: None,
                    base: None,
                    mappings: Vec::new(),
                    notes: Vec::new(),
                },
                changed: vec![changed_symbol(
                    "a.txt",
                    "sym0",
                    codescope_core::SymbolKind::Function,
                    codescope_core::ChangeKind::Modified,
                    1,
                    false,
                )],
                diagnostics: Vec::new(),
                unsupported: false,
                worktree_failed: false,
            })),
        })
        .await;
        {
            let snap = snapshot_rx.borrow().clone();
            assert_eq!(
                snap.files[0].semantic,
                codescope_tui::snapshot::FileSemanticLoad::Ready
            );
            assert_eq!(snap.files[0].changed_symbol_count, 1);
            assert_eq!(snap.files[0].symbols[0].name, "sym0");
        }

        // A stale-epoch result for the OTHER file is dropped.
        disp.handle(DispatchEvent::FileAnalysisDone {
            epoch: snap_epoch, // never advanced past RepoChanged — stale by construction below
            file: "b.txt".to_string(),
            result: Ok(Box::new(codescope_analysis::FileSemanticResult {
                file: codescope_core::FileId::new_unchecked("b.txt"),
                analysis: codescope_analysis::FileAnalysis {
                    file: codescope_core::FileId::new_unchecked("b.txt"),
                    status: codescope_core::FileStatus::Modified,
                    worktree_query_failed: false,
                    worktree: None,
                    base: None,
                    mappings: Vec::new(),
                    notes: Vec::new(),
                },
                changed: vec![changed_symbol(
                    "b.txt",
                    "symB",
                    codescope_core::SymbolKind::Function,
                    codescope_core::ChangeKind::Modified,
                    1,
                    false,
                )],
                diagnostics: Vec::new(),
                unsupported: false,
                worktree_failed: false,
            })),
        })
        .await;
        // Epoch matched (we never bumped), so b.txt is Ready — the real staleness case
        // is a refresh bumping the epoch:
        assert!(matches!(
            disp.file_semantics.get("b.txt"),
            Some(FileSemanticState::Ready(_))
        ));
        disp.spawn_refresh();
        let bumped = disp.epoch;
        assert!(disp.file_semantics.is_empty(), "refresh cleared the cache");
        // Now an old-epoch result drops.
        let _ = bumped;
        std::fs::remove_dir_all(&root).ok();
    }

    /// Collapsing the file that owns the selected symbol clears the relation view.
    #[tokio::test]
    async fn collapsing_the_selected_symbols_file_clears_relations() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;
        disp.handle(DispatchEvent::RepoChanged).await;
        let done = recv_until(&mut job_rx, |e| {
            matches!(e, DispatchEvent::AnalysisDone { .. })
        })
        .await;
        disp.handle(done).await;
        let ctx = disp.repo_ctx.clone().expect("repo ctx after refresh");
        disp.handle(DispatchEvent::ChangesetReady {
            epoch: disp.epoch,
            ctx,
            changeset: two_file_changeset(),
        })
        .await;
        disp.file_semantics.insert(
            "a.txt".to_string(),
            ready_semantics(
                "a.txt",
                vec![changed_symbol(
                    "a.txt",
                    "sym0",
                    codescope_core::SymbolKind::Function,
                    codescope_core::ChangeKind::Modified,
                    1,
                    false,
                )],
            ),
        );
        disp.expanded_files.insert("a.txt".to_string());
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: Some(("sym0".to_string(), 2, 4)),
        }))
        .await;
        assert!(disp.selected_symbol.is_some());
        // Collapse: the selection leaves the hidden symbol; the relation view clears.
        disp.handle(DispatchEvent::Work(Action::SetFileExpanded {
            path: "a.txt".to_string(),
            expanded: false,
        }))
        .await;
        assert!(disp.selected_symbol.is_none());
        assert!(disp.selected_relations.is_none());
        assert!(!snapshot_rx.borrow().files[0].expanded);
        std::fs::remove_dir_all(&root).ok();
    }

    /// plan_rows renders EVERY validated form in order: a per-form section header
    /// (title + kind), summary lines, tree nesting for tree forms, flat nodes plus
    /// `from → to` edge rows for flow forms.
    #[test]
    fn plan_rows_covers_both_forms_with_summaries_and_edges() {
        use codescope_core::{FormKind, PlanEdge, PlanEdgeKind, PlanNode, PlanNodeChange, VizForm};
        let mut plan = codescope_core::VisualizationPlan::new(Epoch::ZERO, "what changed?");
        // Form 1: a tree (roots nest children).
        let mut root = PlanNode::new("r", "Server", PlanNodeChange::Modified);
        root.children = vec!["c".to_string()];
        let child = PlanNode::new("c", "handle", PlanNodeChange::Added);
        plan.forms.push(VizForm {
            kind: FormKind::ChangedSymbolTree,
            title: "Changed symbols".to_string(),
            summary: "Server changed.\nhandle is new.".to_string(),
            nodes: vec![root, child],
            edges: Vec::new(),
        });
        // Form 2: a flow — nodes are peers; the edge carries the relationship.
        plan.forms.push(VizForm {
            kind: FormKind::RelationshipFlow,
            title: "Call flow".to_string(),
            summary: String::new(),
            nodes: vec![
                PlanNode::new("a", "handle", PlanNodeChange::Added),
                PlanNode::new("b", "store", PlanNodeChange::Unchanged),
            ],
            edges: vec![PlanEdge {
                from: "a".to_string(),
                to: "b".to_string(),
                kind: PlanEdgeKind::Calls,
                label: Some("on hit".to_string()),
            }],
        });
        let rows = plan_rows(&plan);
        let labels: Vec<(&str, u16, &str)> = rows
            .iter()
            .map(|r| (r.label.as_str(), r.depth, r.relation))
            .collect();
        assert_eq!(
            labels,
            [
                ("Changed symbols", 0, "changed symbols"),
                ("Server changed.", 1, ""),
                ("handle is new.", 1, ""),
                ("Server", 1, ""),
                ("handle", 2, ""),
                ("Call flow", 0, "flow"),
                ("handle", 1, ""),
                ("store", 1, ""),
                ("handle → store · on hit", 1, "calls"),
            ],
            "both forms render in order with headers, summaries, nesting, and edges"
        );
        // Change badges survive the mapping.
        assert!(rows[3].changed, "Modified node marked changed");
        assert!(rows[4].changed, "Added node marked changed");
        assert!(rows[6].changed, "form-2 node keeps its Added badge");
        assert!(!rows[7].changed, "Unchanged node is not marked");
        assert!(!rows[8].changed, "edge rows carry no change badge");
    }

    /// Regression: a valid `AiOutcome::Plan` and a symbol's loaded relations coexist —
    /// the Impact pane shows the relations while `semantic` (the AI Plan tab) keeps the
    /// AI rows. Goes through the real `AiDone`/`RelationsLoaded` events, not direct
    /// field writes.
    #[tokio::test]
    async fn ai_plan_and_loaded_relations_coexist_via_events() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        disp.file_semantics.insert(
            "a.txt".to_string(),
            ready_semantics(
                "a.txt",
                vec![changed_symbol(
                    "a.txt",
                    "sym0",
                    codescope_core::SymbolKind::Function,
                    codescope_core::ChangeKind::Modified,
                    1,
                    false,
                )],
            ),
        );
        disp.expanded_files.insert("a.txt".to_string());

        // A real validated plan lands via AiDone (as spawn_ai's job would report).
        let mut plan = codescope_core::VisualizationPlan::new(disp.epoch, "What does sym0 affect?");
        plan.forms.push(codescope_core::VizForm {
            kind: codescope_core::FormKind::CallTree,
            title: "call tree".to_string(),
            summary: String::new(),
            nodes: vec![codescope_core::PlanNode::new(
                "n1",
                "sym0",
                codescope_core::PlanNodeChange::Modified,
            )],
            edges: Vec::new(),
        });
        let generation = disp.ai_request_seq;
        disp.handle(DispatchEvent::AiDone {
            epoch: disp.epoch,
            generation,
            outcome: AiOutcome::Plan(plan, codescope_core::ValidationReport::valid()),
        })
        .await;
        {
            let snap = snapshot_rx.borrow().clone();
            assert!(snap.semantic.ai_generated, "plan published to semantic");
            assert_eq!(snap.semantic.rows[0].label, "call tree");
            assert_eq!(snap.semantic.rows[1].label, "sym0");
        }

        // The user selects sym0 and its relations land.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: Some(("sym0".to_string(), 2, 4)),
        }))
        .await;
        disp.handle(DispatchEvent::RelationsLoaded {
            epoch: disp.epoch,
            file: "a.txt".to_string(),
            name: "sym0".to_string(),
            line: 2,
            col: 4,
            callers: relation_rows(&["caller_fn"]),
            callees: relation_rows(&["callee_fn"]),
        })
        .await;

        let snap = snapshot_rx.borrow().clone();
        // Impact view: the relations.
        let callers: Vec<&str> = snap
            .impact
            .callers
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(callers, ["caller_fn"], "impact shows the relations");
        // AI Plan tab: the plan is NOT displaced by the relations.
        assert!(snap.semantic.ai_generated, "plan survives loaded relations");
        assert!(
            snap.semantic.rows.iter().any(|r| r.label == "sym0"),
            "AI rows still published"
        );
        assert_eq!(snap.ai, AiStatus::Ready { epoch: disp.epoch });

        std::fs::remove_dir_all(&root).ok();
    }
}
