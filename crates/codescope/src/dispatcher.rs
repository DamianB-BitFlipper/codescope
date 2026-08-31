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
    /// An AI plan job completed (spawned; epoch-tagged).
    AiDone {
        /// Epoch the plan was requested against.
        epoch: Epoch,
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

/// Lazily-expanded relations of the currently selected symbol.
#[derive(Debug, Clone)]
struct SelectedRelations {
    /// Display label of the selected symbol (the legacy semantic pane's title).
    label: String,
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
    /// the impact pane can show both columns (the legacy semantic pane flattens them).
    selected_relations: Option<SelectedRelations>,
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
            DispatchEvent::AiDone { epoch, outcome } => self.on_ai_done(epoch, outcome),
            DispatchEvent::EngineReady(engine) => {
                self.ls_status = LsStatus::Ready;
                self.engine = Some(std::sync::Arc::new(*engine));
                // Re-run the pipeline now that semantics are available.
                self.spawn_refresh();
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
                self.selected_relations = Some(SelectedRelations {
                    label: name,
                    callers,
                    callees,
                });
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
        // Publish immediately: show the git-level view with a refreshing marker.
        self.repo_ctx = None;
        self.changeset = None;
        self.publish_refreshing();
        let engine = self.engine.clone();
        let tx = self.job_tx.clone();
        // The engine is Arc-shared; the job runs the full git+analysis pipeline without
        // blocking the dispatcher. Result is epoch-gated at apply time (on_analysis_done).
        tokio::spawn(async move {
            let result = run_pipeline(repo, scope, engine, epoch, base_override).await;
            let _ = tx
                .send(DispatchEvent::AnalysisDone {
                    epoch,
                    result: result.map(Box::new),
                })
                .await;
        });
        // Re-fetch the selected symbol's relations against the new state: an in-flight
        // pre-refresh fetch is epoch-gated and dropped, and navigation does not re-send a
        // selection that did not move.
        if let Some((file, name, line, col)) = self.selected_symbol.clone() {
            self.spawn_expand(file, name, line, col);
        }
    }

    fn spawn_ai(&mut self) {
        let (Some(_ai), Some(analysis)) = (&self.ai, &self.analysis) else {
            return;
        };
        if !self.ai_enabled {
            return;
        }
        let epoch = self.epoch;
        self.ai_status = AiStatus::Loading { since_epoch: epoch };
        self.publish();
        let digest = analysis.digest().render();
        let facts = SnapshotFacts::new(analysis);
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
            let _ = tx.send(DispatchEvent::AiDone { epoch, outcome }).await;
        });
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
                self.ls_status = LsStatus::Ready;
                self.status = StatusMessage::default();
                self.analysis = Some(*snap);
            }
            Err(e) => {
                self.set_status(format!("analysis failed: {e}"), StatusLevel::Error);
            }
        }
        self.publish();
    }

    fn on_ai_done(&mut self, epoch: Epoch, outcome: AiOutcome) {
        if epoch != self.epoch {
            // A newer state superseded this plan; do not apply.
            return;
        }
        match outcome {
            AiOutcome::Plan(plan, report) if report.is_renderable() => {
                let rows = plan_rows(&plan);
                let title = plan
                    .forms
                    .first()
                    .map(|f| f.title.clone())
                    .unwrap_or_default();
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
        let files = self.analysis.as_ref().map(file_rows).unwrap_or_default();
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
        // A selected symbol's lazily-expanded callers/callees take precedence over the
        // shallow impact graph (the "who calls this" view; restored after the perf split).
        if let Some(relations) = &self.selected_relations {
            let mut rows = Vec::new();
            rows.push(SemRow {
                depth: 0,
                label: relations.label.clone(),
                relation: "selected",
                changed: true,
                has_diagnostic: false,
            });
            for c in &relations.callers.rows {
                rows.push(SemRow {
                    depth: 1,
                    label: c.label.clone(),
                    relation: "called by",
                    changed: c.changed,
                    has_diagnostic: c.has_diagnostic,
                });
            }
            for c in &relations.callees.rows {
                rows.push(SemRow {
                    depth: 1,
                    label: c.label.clone(),
                    relation: "calls",
                    changed: c.changed,
                    has_diagnostic: c.has_diagnostic,
                });
            }
            let semantic = SemanticPane {
                title: format!("relations of {}", relations.label),
                rows,
                note: String::new(),
                ai_generated: false,
            };
            return (diff, semantic);
        }
        // AI rows render only while their epoch matches the current repo state (H3).
        let semantic = match (&self.ai_rows, &self.analysis) {
            (Some((ep, rows, title)), _) if *ep == self.epoch => SemanticPane {
                title: title.clone(),
                rows: rows.clone(),
                note: String::new(),
                ai_generated: true,
            },
            (Some(_), _) => SemanticPane {
                title: "impact".to_string(),
                rows: Vec::new(),
                note: "AI view stale (repo changed); regenerating…".to_string(),
                ai_generated: false,
            },
            (None, Some(a)) => impact_pane(a),
            (None, None) => SemanticPane::default(),
        };
        (diff, semantic)
    }

    /// Assemble the impact pane (spec §5.3–§5.7): the deterministic selected change plus
    /// the callers/downstream columns. Lazy LSP relations and the one-hop impact graph
    /// merge into both lists; AI plan rows never replace this pane.
    fn build_impact(&self) -> ImpactPane {
        let mut impact = ImpactPane::default();
        let Some(analysis) = &self.analysis else {
            return impact;
        };
        impact.selected_change = selected_change(
            analysis,
            self.selected_file.as_deref(),
            self.selected_symbol.as_ref(),
        );
        // A file row (or no selection) leaves both lists Idle with the file-level
        // selected-change fallback; only a symbol row fetches relations.
        let Some((file, name, _, _)) = &self.selected_symbol else {
            return impact;
        };
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
                // A fetch is in flight only when an engine exists to serve it; in
                // git-only mode the lists are Unavailable, not forever-Loading.
                let state = if self.engine.is_some() {
                    ImpactLoadState::Loading
                } else {
                    ImpactLoadState::Unavailable
                };
                impact.callers.state = state;
                impact.downstream.state = state;
            }
        }
        merge_graph_neighbors(analysis, file, name, &mut impact.callers, &mut impact.downstream);
        if impact.callers.partial || impact.downstream.partial || !analysis.graph.is_complete() {
            impact.note = "partial: some relationships unavailable".to_string();
        }
        impact
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
) -> anyhow::Result<AnalysisSnapshot> {
    let ctx = repo
        .repo_context_with_base(base_override.as_deref())
        .await?;
    let changeset = match (scope, &base_override) {
        (ChangeScope::Branch, Some(base)) => repo.branch_changeset_with_base(base).await?,
        _ => repo.changeset(scope).await?,
    };
    let Some(engine) = engine else {
        // No language service: fabricate an analysis carrying just git state.
        return Ok(git_only_snapshot(epoch, ctx, changeset));
    };
    // The override-aware context rides along so the UI reports the chosen base.
    engine
        .refresh_with_ctx(&changeset, epoch, ctx)
        .await
        .map_err(Into::into)
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

fn file_rows(a: &AnalysisSnapshot) -> Vec<FileRow> {
    use std::collections::BTreeMap;
    let mut by_file: BTreeMap<String, Vec<SymbolRow>> = BTreeMap::new();
    for c in &a.changed {
        let change = change_label(c.record.change_kind);
        let confidence = match &c.record.confidence {
            codescope_core::MappingConfidence::Exact => "",
            codescope_core::MappingConfidence::Approximate(_) => "~",
            codescope_core::MappingConfidence::Unmapped => "?",
        };
        // Diagnostic badge when any diagnostic touches this symbol's range.
        let has_diag = a.diagnostics.iter().any(|d| {
            d.file == c.file
                && d.range.start_line <= c.range.end_line
                && d.range.end_line >= c.range.start_line
        });
        by_file
            .entry(c.file.to_string())
            .or_default()
            .push(SymbolRow {
                name: c.name.clone(),
                change,
                confidence,
                has_diagnostic: has_diag,
                position: Some((c.selection.start_line, c.selection.start_col)),
            });
    }
    a.changeset
        .files
        .iter()
        .map(|f| {
            let symbols = by_file.remove(&f.path.to_string()).unwrap_or_default();
            FileRow {
                path: f.path.to_string(),
                status: status_badge(&f.status),
                changed_symbol_count: symbols.len(),
                symbols,
                expanded: true,
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

fn impact_pane(a: &AnalysisSnapshot) -> SemanticPane {
    let graph = &a.graph.value;
    let mut rows = Vec::new();
    for node in &graph.nodes {
        let label = node
            .entity
            .symbol
            .clone()
            .unwrap_or_else(|| node.entity.file.to_string());
        rows.push(SemRow {
            depth: 0,
            label,
            relation: if node.change.is_some() { "changed" } else { "" },
            changed: node.change.is_some(),
            has_diagnostic: node.diagnostic_severity.is_some(),
        });
        for e in graph.edges_from(&node.id) {
            if let Some(target) = graph.node(&e.to) {
                let label = target
                    .entity
                    .symbol
                    .clone()
                    .unwrap_or_else(|| target.entity.file.to_string());
                rows.push(SemRow {
                    depth: 1,
                    label,
                    relation: relation_label(e.kind),
                    changed: target.change.is_some(),
                    has_diagnostic: target.diagnostic_severity.is_some(),
                });
            }
        }
    }
    let note = if a.graph.is_complete() {
        String::new()
    } else {
        "partial: some relationships unavailable".to_string()
    };
    SemanticPane {
        title: "impact".to_string(),
        rows,
        note,
        ai_generated: false,
    }
}

fn relation_label(kind: codescope_core::RelationKind) -> &'static str {
    use codescope_core::RelationKind as R;
    match kind {
        R::Calls => "calls",
        R::CalledBy => "called by",
        R::Implements => "implements",
        R::ImplementedBy => "implemented by",
        R::References => "references",
        R::Contains => "contains",
        R::SubtypeOf => "subtype of",
        R::SupertypeOf => "supertype of",
    }
}

/// The deterministic `SelectedChange` for the current selection (spec §5.3/§5.6).
///
/// A symbol row resolves the exact [`ChangedSymbolInfo`] and builds the interpretation
/// sentence from it; a file row gets the file-level fallback ("N changed symbols …").
/// `None` only when nothing is selected at all.
fn selected_change(
    analysis: &AnalysisSnapshot,
    selected_file: Option<&str>,
    selected_symbol: Option<&(String, String, u32, u32)>,
) -> Option<SelectedChange> {
    match selected_symbol {
        Some((file, name, line, col)) => {
            let info = find_changed_symbol(analysis, file, name, *line, *col);
            let (change, interpretation) = match info {
                Some(info) => (change_label(info.record.change_kind), interpret_change(info)),
                // The selection predates the current analysis (a refresh is in flight):
                // keep the identity, but do not invent a change kind or a sentence.
                None => ("modified", String::new()),
            };
            Some(SelectedChange {
                file: file.clone(),
                label: name.clone(),
                change,
                interpretation,
                interpretation_source: InterpretationSource::Deterministic,
            })
        }
        None => {
            let file = selected_file?;
            let changed_in_file = analysis
                .changed
                .iter()
                .filter(|c| c.file.as_path().as_str() == file)
                .count();
            let change = analysis
                .changeset
                .files
                .iter()
                .find(|f| f.path.as_str() == file)
                .map(|f| file_change_label(&f.status))
                .unwrap_or("modified");
            Some(SelectedChange {
                file: file.to_string(),
                label: file.to_string(),
                change,
                interpretation: format!(
                    "{changed_in_file} changed symbol{} in this file; select one to inspect impact.",
                    if changed_in_file == 1 { "" } else { "s" }
                ),
                interpretation_source: InterpretationSource::Deterministic,
            })
        }
    }
}

/// The exact [`ChangedSymbolInfo`] behind a symbol selection: file + qualified name +
/// the identifier position the row carries (spec §5.6). Falls back to file + name when
/// the position drifted (e.g. the row was rendered before the latest refresh landed).
fn find_changed_symbol<'a>(
    analysis: &'a AnalysisSnapshot,
    file: &str,
    name: &str,
    line: u32,
    col: u32,
) -> Option<&'a codescope_analysis::ChangedSymbolInfo> {
    analysis
        .changed
        .iter()
        .find(|c| {
            c.file.as_path().as_str() == file
                && c.name == name
                && c.selection.start_line == line
                && c.selection.start_col == col
        })
        .or_else(|| {
            analysis
                .changed
                .iter()
                .find(|c| c.file.as_path().as_str() == file && c.name == name)
        })
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

/// Merge the selected node's one-hop impact-graph neighbors into the impact lists
/// (spec §5.5): incoming `Calls` neighbors are callers, every outgoing neighbor is
/// downstream. Rows deduplicate by `(label, relation)` in stable order; the lazy LSP
/// rows are already in the lists and win on duplicates (graph rows merge their badges).
fn merge_graph_neighbors(
    analysis: &AnalysisSnapshot,
    file: &str,
    name: &str,
    callers: &mut ImpactList,
    downstream: &mut ImpactList,
) {
    let graph = &analysis.graph.value;
    let Some(node) = graph.nodes.iter().find(|n| {
        n.entity.file.as_path().as_str() == file && n.entity.symbol.as_deref() == Some(name)
    }) else {
        return;
    };
    for e in graph.edges_to(&node.id) {
        if e.kind != codescope_core::RelationKind::Calls {
            continue;
        }
        if let Some(source) = graph.node(&e.from) {
            push_graph_row(callers, graph_row(source, e.kind));
        }
    }
    for e in graph.edges_from(&node.id) {
        if let Some(target) = graph.node(&e.to) {
            push_graph_row(downstream, graph_row(target, e.kind));
        }
    }
}

/// An impact row for one graph neighbor of the selection.
fn graph_row(
    node: &codescope_core::ImpactNode,
    kind: codescope_core::RelationKind,
) -> ImpactRow {
    ImpactRow {
        label: node
            .entity
            .symbol
            .clone()
            .unwrap_or_else(|| node.entity.file.to_string()),
        relation: relation_label(kind),
        changed: node.change.is_some(),
        has_diagnostic: node.diagnostic_severity.is_some(),
    }
}

/// Push a graph row, deduplicating by `(label, relation)`: an existing (lazy LSP) row
/// keeps its identity and position, and absorbs the graph row's badges.
fn push_graph_row(list: &mut ImpactList, row: ImpactRow) {
    match list
        .rows
        .iter_mut()
        .find(|r| r.label == row.label && r.relation == row.relation)
    {
        Some(existing) => {
            existing.changed |= row.changed;
            existing.has_diagnostic |= row.has_diagnostic;
        }
        None => list.rows.push(row),
    }
}

fn plan_rows(plan: &codescope_core::VisualizationPlan) -> Vec<SemRow> {
    let mut rows = Vec::new();
    if let Some(form) = plan.forms.first() {
        let by_id: std::collections::HashMap<&str, &codescope_core::PlanNode> =
            form.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let is_child: HashSet<&str> = form
            .nodes
            .iter()
            .flat_map(|n| n.children.iter().map(String::as_str))
            .collect();
        for n in &form.nodes {
            if !is_child.contains(n.id.as_str()) {
                push_plan_node(n, &by_id, 0, &mut rows);
            }
        }
    }
    rows
}

fn push_plan_node(
    n: &codescope_core::PlanNode,
    by_id: &std::collections::HashMap<&str, &codescope_core::PlanNode>,
    depth: u16,
    rows: &mut Vec<SemRow>,
) {
    rows.push(SemRow {
        depth,
        label: n.label.clone(),
        relation: "",
        changed: !matches!(n.change, codescope_core::PlanNodeChange::Unchanged),
        has_diagnostic: n.severity.is_some(),
    });
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
    fn new(a: &AnalysisSnapshot) -> Self {
        let mut facts = SnapshotFacts {
            files: HashSet::new(),
            symbols: std::collections::HashMap::new(),
            edges: HashSet::new(),
            hunks: std::collections::HashMap::new(),
        };
        for f in &a.changeset.files {
            facts.files.insert(f.path.to_string());
            facts.hunks.insert(f.path.to_string(), f.hunks.len());
        }
        for c in &a.changed {
            facts.files.insert(c.file.to_string());
            facts
                .symbols
                .insert((c.file.to_string(), c.name.clone()), c.range);
        }
        let graph = &a.graph.value;
        for e in &graph.edges {
            if let (Some(f), Some(t)) = (graph.node(&e.from), graph.node(&e.to)) {
                facts.edges.insert((
                    entity_key(&f.entity),
                    entity_key(&t.entity),
                    plan_edge_kind(e.kind),
                ));
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

fn plan_edge_kind(kind: codescope_core::RelationKind) -> PlanEdgeKind {
    use codescope_core::RelationKind as R;
    match kind {
        R::Calls => PlanEdgeKind::Calls,
        R::Implements => PlanEdgeKind::Implements,
        R::Contains => PlanEdgeKind::Contains,
        _ => PlanEdgeKind::Reads,
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
    /// lazy expand; the result then populates the relations pane. This fixture has no
    /// engine, so the spawn is skipped — the recorded target is what gates the fetch
    /// result below.
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

        // The fetch result for the CURRENT symbol applies: relations view + title.
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
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.semantic.title, "relations of sym0");
        let labels: Vec<&str> = snap
            .semantic
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(labels, ["sym0", "caller_fn", "callee_fn"]);
        assert_eq!(snap.semantic.rows[1].relation, "called by");
        assert_eq!(snap.semantic.rows[2].relation, "calls");

        // Navigating back to a file row clears the relations view immediately.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: None,
        }))
        .await;
        assert!(disp.selected_symbol.is_none());
        let snap = snapshot_rx.borrow().clone();
        assert_ne!(snap.semantic.title, "relations of sym0");
        assert!(snap.semantic.rows.is_empty());

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
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
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
        assert_eq!(snapshot_rx.borrow().semantic.title, "relations of sym0");

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

    /// An analysis snapshot carrying `changed` symbols and a complete impact `graph`.
    fn analysis_with(
        changed: Vec<codescope_analysis::ChangedSymbolInfo>,
        graph: codescope_core::ImpactGraph,
    ) -> AnalysisSnapshot {
        AnalysisSnapshot {
            epoch: Epoch::ZERO,
            repo_ctx: codescope_core::RepoContext {
                toplevel: "/tmp/codescope-test".into(),
                head: codescope_core::HeadState::Branch("feature".to_string()),
                upstream: None,
                base: None,
            },
            changeset: two_file_changeset(),
            files: Vec::new(),
            changed,
            graph: codescope_core::Evidence::complete(graph),
            diagnostics: Vec::new(),
        }
    }

    /// The deterministic interpretation sentence for each change kind (spec §3.5).
    #[test]
    fn deterministic_interpretation_sentences_cover_each_change_kind() {
        use codescope_core::{ChangeKind, SymbolKind};
        let added = changed_symbol("a.go", "NewHandler", SymbolKind::Function, ChangeKind::Added, 3, false);
        assert_eq!(interpret_change(&added), "Added function across 3 hunks.");

        let modified =
            changed_symbol("a.go", "Handle", SymbolKind::Method, ChangeKind::Modified, 2, false);
        assert_eq!(
            interpret_change(&modified),
            "Modified implementation across 2 hunks."
        );

        let signature =
            changed_symbol("a.go", "Handle", SymbolKind::Method, ChangeKind::Modified, 1, true);
        assert_eq!(
            interpret_change(&signature),
            "Modified signature and implementation across 1 hunks."
        );

        let removed =
            changed_symbol("a.go", "Legacy", SymbolKind::Function, ChangeKind::Deleted, 1, false);
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

        disp.handle(DispatchEvent::AiDone {
            epoch: disp.epoch,
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
        assert_eq!(snap.status.text, "git-only (no supported language detected)");

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
        disp.analysis = Some(analysis_with(
            vec![changed_symbol(
                "a.txt",
                "sym0",
                codescope_core::SymbolKind::Function,
                codescope_core::ChangeKind::Modified,
                2,
                false,
            )],
            codescope_core::ImpactGraph::new(),
        ));

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
            selected.interpretation,
            "0 changed symbols in this file; select one to inspect impact."
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

    /// RelationsLoaded stores callers and callees as separate impact columns, keeps the
    /// evidence honesty flag (spec §5.4), and leaves the legacy semantic pane intact.
    #[tokio::test]
    async fn relations_loaded_populates_impact_columns() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        disp.analysis = Some(analysis_with(
            vec![changed_symbol(
                "a.txt",
                "sym0",
                codescope_core::SymbolKind::Function,
                codescope_core::ChangeKind::Modified,
                1,
                false,
            )],
            codescope_core::ImpactGraph::new(),
        ));
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
        // The legacy semantic pane still flattens both directions.
        assert_eq!(snap.semantic.title, "relations of sym0");

        std::fs::remove_dir_all(&root).ok();
    }

    /// The one-hop impact graph merges into the columns (spec §5.5): incoming `Calls`
    /// neighbors are callers, every outgoing neighbor is downstream, and lazy LSP rows
    /// win duplicates while absorbing the graph row's badges.
    #[tokio::test]
    async fn impact_merges_graph_neighbors_with_lsp_relations() {
        use codescope_core::{
            ChangeKind, EntityRef, FileId, ImpactEdge, ImpactGraph, ImpactNode, RelationKind,
            SymbolKind,
        };
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        let node = |file: &str, symbol: &str, changed: bool| ImpactNode {
            id: format!("{file}:{symbol}"),
            entity: EntityRef::for_symbol(FileId::new_unchecked(file), symbol, None),
            change: changed.then_some(ChangeKind::Modified),
            diagnostic_severity: None,
        };
        let mut graph = ImpactGraph::new();
        graph.add_node(node("a.txt", "sym0", true));
        graph.add_node(node("a.txt", "graph_caller", false));
        graph.add_node(node("b.txt", "callee_fn", true)); // the LSP fetch returns it too
        graph.add_node(node("b.txt", "graph_iface", false));
        graph.add_edge(ImpactEdge {
            from: "a.txt:graph_caller".into(),
            to: "a.txt:sym0".into(),
            kind: RelationKind::Calls,
        });
        graph.add_edge(ImpactEdge {
            from: "a.txt:sym0".into(),
            to: "b.txt:callee_fn".into(),
            kind: RelationKind::Calls,
        });
        graph.add_edge(ImpactEdge {
            from: "a.txt:sym0".into(),
            to: "b.txt:graph_iface".into(),
            kind: RelationKind::Implements,
        });
        disp.changeset = Some(two_file_changeset());
        disp.analysis = Some(analysis_with(
            vec![changed_symbol(
                "a.txt",
                "sym0",
                SymbolKind::Function,
                ChangeKind::Modified,
                1,
                false,
            )],
            graph,
        ));
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: Some(("sym0".to_string(), 2, 4)),
        }))
        .await;

        // The graph rows are visible before the lazy fetch lands (no engine here, so
        // the lists are Unavailable rather than Loading — the rows are still real).
        let snap = snapshot_rx.borrow().clone();
        let callers: Vec<&str> = snap
            .impact
            .callers
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(callers, ["graph_caller"]);

        disp.handle(DispatchEvent::RelationsLoaded {
            epoch: disp.epoch,
            file: "a.txt".to_string(),
            name: "sym0".to_string(),
            line: 2,
            col: 4,
            callers: relation_rows(&["lsp_caller"]),
            callees: relation_rows(&["callee_fn"]),
        })
        .await;

        let snap = snapshot_rx.borrow().clone();
        let callers: Vec<&str> = snap
            .impact
            .callers
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(
            callers,
            ["lsp_caller", "graph_caller"],
            "stable order: the lazy LSP rows lead, graph neighbors follow"
        );
        let downstream: Vec<(&str, &str, bool)> = snap
            .impact
            .downstream
            .rows
            .iter()
            .map(|r| (r.label.as_str(), r.relation, r.changed))
            .collect();
        assert_eq!(
            downstream,
            [("callee_fn", "calls", true), ("graph_iface", "implements", false)],
            "the duplicate keeps the LSP row and absorbs the graph row's changed badge"
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
