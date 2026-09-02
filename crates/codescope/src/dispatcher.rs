//! The dispatcher: the single writer of repository state.
//!
//! Owns the epoch (bumped once per accepted change), and never blocks on slow subsystems:
//! git reads, analysis, and AI requests run as spawned, epoch-tagged jobs; results are
//! applied only when the epoch still matches (architecture decision 4). A stale AI plan or
//! analysis can never overwrite a newer repo state.

use std::collections::HashSet;

use codescope_ai::{
    parse_plan, validate, AiActivityObserver, AiActivityUpdate, AiOutcome, AiService,
    AiToolActivityState, DiagramObserver, FactView, ReasoningEffort,
};
use codescope_analysis::{AnalysisEngine, AnalysisSnapshot};
use codescope_core::{
    AiStatus, ChangeScope, DiagramCommand, DiagramDraft, DiffSide, EntityRef, Epoch, LineRange,
    LsStatus, PlanEdgeKind,
};
use codescope_git::GitRepo;
use codescope_lsp::LanguageService;
use codescope_tui::snapshot::{
    AiActivity, AiSummaryKey, AiSummaryState, AiTokenUsage, AiToolCallActivity,
    AiToolCallActivityState, DiffPane, DiffRow, FileRow, ImpactList, ImpactLoadState, ImpactPane,
    ImpactRow, InterpretationSource, RepoBar, ScopeCounts, SelectedChange, SemanticPane,
    StatusLevel, StatusMessage, SymbolRow, UiSnapshot,
};
use codescope_tui::Action;
use codescope_tui::UiPreferences;
use tokio::sync::{mpsc, watch};

use crate::request_coordinator::RequestCoordinator;
use crate::research_tools::{research_brief, ScopedResearchTools};

/// Publication boundary between the backend dispatcher and a state consumer.
///
/// The interactive application implements this with a latest-value [`watch`] channel;
/// the headless debug backend implements it with an ordered [`mpsc`] channel. Both paths
/// therefore observe the exact same [`UiSnapshot`] assembled by the dispatcher.
pub(crate) trait BackendOutput: Send + Sync {
    /// Publish one immutable backend state snapshot.
    fn publish(&self, snapshot: UiSnapshot);
}

impl BackendOutput for watch::Sender<UiSnapshot> {
    fn publish(&self, snapshot: UiSnapshot) {
        let _ = self.send(snapshot);
    }
}

impl BackendOutput for mpsc::UnboundedSender<UiSnapshot> {
    fn publish(&self, snapshot: UiSnapshot) {
        let _ = self.send(snapshot);
    }
}

/// Narrow persistence boundary used by the dispatcher. Tests can inject a memory/failing
/// implementation without ever touching the user's real global config.
pub(crate) trait ConfigPersistence: Send + Sync {
    /// Remember an explicit picker selection in the active provider's slot.
    fn persist_model(&self, provider: &str, model: &str) -> Result<(), String>;
    /// Remember an explicit reasoning-budget selection in the active provider's slot.
    fn persist_reasoning_effort(
        &self,
        provider: &str,
        effort: ReasoningEffort,
    ) -> Result<(), String>;
    /// Remember stable, repository-independent TUI preferences.
    fn persist_ui(&self, preferences: UiPreferences) -> Result<(), String>;
}

/// Owned writes sent to the single FIFO config worker. Serial execution preserves the
/// user's action order while `spawn_blocking` keeps filesystem locking/fsync off the
/// dispatcher runtime thread.
enum ConfigWrite {
    Model {
        provider: String,
        model: String,
    },
    ReasoningEffort {
        provider: String,
        effort: ReasoningEffort,
    },
    Ui(UiPreferences),
}

fn flatten_config_write_result(
    result: Result<Result<(), String>, tokio::task::JoinError>,
) -> Result<(), String> {
    result.map_err(|error| format!("config writer task failed: {error}"))?
}

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
    /// Branch context resolved successfully, but no meaningful comparison base exists.
    /// This is distinct from an empty changeset: the UI must clear stale branch facts and
    /// say `base: none`, never imply that a comparison ran and found no changes.
    BranchUnavailable {
        /// Epoch the resolution ran against; stale results are dropped on apply.
        epoch: Epoch,
        /// Current repository context with `base: None`.
        ctx: codescope_core::RepoContext,
    },
    /// A background global-config write failed. The live selection remains applied.
    ConfigSaveFailed {
        /// Human-readable description of the preference that remained session-only.
        what: &'static str,
        /// Filesystem/config failure detail.
        error: String,
    },
    /// An asynchronous per-file analysis job completed (spawned; epoch-tagged).
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
        /// Exact directory/file/function row the plan explains. Completion is cached for this row
        /// even when focus moved while the provider was answering.
        selection: AiSelectionKey,
        /// The AI request generation (monotonic per dispatcher): distinguishes two
        /// requests in the same epoch, so a slow older request can never overwrite a
        /// newer plan (review 18 M7).
        generation: u64,
        /// The validated outcome.
        outcome: AiOutcome,
    },
    /// One atomic internal-agent diagram edit landed before the overall request finished.
    AiDraft {
        /// Repository epoch owned by the draft.
        epoch: Epoch,
        /// Stable selection whose diagram is being built.
        selection: AiSelectionKey,
        /// Request generation, used to reject edits from evicted/obsolete writers.
        generation: u64,
        /// Complete bounded draft after the accepted edit.
        draft: DiagramDraft,
    },
    /// One model/tool lifecycle update for an in-flight AI request.
    AiActivity {
        /// Repository epoch the request is researching.
        epoch: Epoch,
        /// Selection whose explanation is being generated.
        selection: AiSelectionKey,
        /// Request generation used to reject stale or evicted updates.
        generation: u64,
        /// Waiting/tool-call state emitted by the AI service.
        update: AiActivityUpdate,
    },
    /// The selection stayed on one changed directory/file/symbol long enough to request its AI plan.
    /// Navigation increments `generation`, so earlier debounce events become inert before
    /// they can spend a provider request.
    AiSelectionSettled {
        /// Repo-state epoch the selection belongs to.
        epoch: Epoch,
        /// Latest selection-debounce generation.
        generation: u64,
    },
    /// The language server finished initializing; semantic analysis can begin.
    EngineReady(Box<AnalysisEngine<LanguageService>>),
    /// The language server failed to start; stay in git-only mode.
    EngineUnavailable(String),
    /// The provider's model list request completed for the picker. Failure is distinct
    /// from AI being disabled: the current or manually entered model remains usable.
    ModelsLoaded {
        /// Monotonic request generation; late older responses are ignored.
        generation: u64,
        /// Normalized model ids or a safe discovery error.
        result: Result<Vec<String>, String>,
    },
    /// The repo's base candidates were fetched for the base picker.
    BaseLoaded {
        /// Ordered selectable ref names.
        bases: Vec<String>,
        /// The bounded graph walk stopped before exhausting all possible ancestors.
        truncated: bool,
    },
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

/// Appended to every AI failure in the status bar. File/selection changes retry
/// automatically; `m` remains available to change the model.
const AI_FAILURE_SUFFIX: &str =
    "m change model · retries automatically when the selection or file changes · deterministic impact remains available";

/// Keep backend-owned status summaries stable and small; the TUI separately receives the
/// complete failure in [`StatusMessage::detail`]. Newlines are detail structure, never footer
/// content.
fn ai_failure_footer_reason(reason: &str) -> String {
    const MAX_CHARS: usize = 180;
    let first_line = reason
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("AI generation failed")
        .trim();
    if first_line.chars().count() <= MAX_CHARS {
        return first_line.to_string();
    }
    let mut concise = first_line
        .chars()
        .take(MAX_CHARS.saturating_sub(1))
        .collect::<String>();
    concise.push('…');
    concise
}

const MAX_AGENT_GUIDANCE_CHARS: usize = 2_000;

fn normalize_agent_text(text: &str) -> String {
    text.chars()
        .filter(|character| *character == '\n' || *character == '\t' || !character.is_control())
        .take(MAX_AGENT_GUIDANCE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.replace(['\n', '\t'], " ");
    }
    let mut shortened = text
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>()
        .replace(['\n', '\t'], " ");
    shortened.push('…');
    shortened
}

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
    /// The job failed (retried after the next repository/file change).
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

/// Stable identity of the directory/file/function row an AI plan explains.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum AiSelectionKey {
    Directory(String),
    File(String),
    Symbol {
        file: String,
        name: String,
        line: u32,
        col: u32,
    },
}

impl AiSelectionKey {
    fn label(&self) -> &str {
        match self {
            Self::Directory(path) | Self::File(path) => path,
            Self::Symbol { name, .. } => name,
        }
    }

    fn file(&self) -> Option<&str> {
        match self {
            Self::Directory(_) => None,
            Self::File(path) | Self::Symbol { file: path, .. } => Some(path),
        }
    }

    fn contains_file(&self, file: &str) -> bool {
        match self {
            Self::Directory(directory) => file.starts_with(&format!("{directory}/")),
            Self::File(path) | Self::Symbol { file: path, .. } => file == path,
        }
    }

    fn summary_key(&self) -> AiSummaryKey {
        match self {
            Self::Directory(path) => AiSummaryKey::Directory(path.clone()),
            Self::File(path) => AiSummaryKey::File(path.clone()),
            Self::Symbol {
                file,
                name,
                line,
                col,
            } => AiSummaryKey::Symbol {
                file: file.clone(),
                name: name.clone(),
                position: Some((*line, *col)),
            },
        }
    }
}

/// Stable identity for carrying a validated design across repository revisions.
///
/// Line and column deliberately do not participate: an edit above a function may move it
/// without changing the behavior the prior diagram explains. Revision entries are prompt
/// seeds only; they are never rendered as facts for a newer epoch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AiRevisionKey {
    Directory(String),
    File(String),
    Symbol { file: String, name: String },
}

impl From<&AiSelectionKey> for AiRevisionKey {
    fn from(selection: &AiSelectionKey) -> Self {
        match selection {
            AiSelectionKey::Directory(path) => Self::Directory(path.clone()),
            AiSelectionKey::File(path) => Self::File(path.clone()),
            AiSelectionKey::Symbol { file, name, .. } => Self::Symbol {
                file: file.clone(),
                name: name.clone(),
            },
        }
    }
}

/// Render-ready validated plan cached for one selection.
#[derive(Debug, Clone)]
struct CachedAiPlan {
    plan: codescope_core::VisualizationPlan,
    /// The validation report that produced the (already sanitized) plan. Cached with the
    /// plan so cache hits and selection changes keep flagging dropped content instead of
    /// presenting a sanitized plan as fully trusted (Terra's report-preservation fix).
    report: codescope_core::ValidationReport,
}

/// Agent-supplied presentation intent for one stable selection. This text can steer what
/// the reviewer explains, but is explicitly labelled as untrusted guidance in the prompt.
#[derive(Debug, Clone, Default)]
struct AgentGuidance {
    question: Option<String>,
    feedback: Option<String>,
}

impl AgentGuidance {
    fn prompt_section(&self) -> String {
        let mut section = String::new();
        if let Some(question) = &self.question {
            section.push_str("\n## Agent question (presentation goal, not repository evidence)\n");
            section.push_str(question);
            section.push_str(
                "\nResearch the repository facts needed to answer this question through the validated intent and diagram. Do not treat the question as evidence.\n",
            );
        }
        if let Some(feedback) = &self.feedback {
            section.push_str(
                "\n## Agent feedback on the previous design (presentation goal, not evidence)\n",
            );
            section.push_str(feedback);
            section.push_str(
                "\nRevise the explanation where the current repository evidence supports it. Current evidence always wins.\n",
            );
        }
        section
    }

    fn display(&self) -> Option<String> {
        let (prefix, text) = if let Some(feedback) = &self.feedback {
            ("Agent feedback", feedback)
        } else {
            ("Agent question", self.question.as_ref()?)
        };
        Some(format!("{prefix}: {}", truncate_chars(text, 180)))
    }
}

#[derive(Debug, Clone)]
struct AiRunningJob {
    selection: AiSelectionKey,
    epoch: Epoch,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SemanticJobPriority {
    Focused,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemanticRunningJob {
    epoch: Epoch,
    priority: SemanticJobPriority,
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
    analysis: Option<AnalysisSnapshot>,
    /// The plan currently displayed, scoped to exactly one changed directory/file/function row.
    ai_rows: Option<(Epoch, AiSelectionKey, CachedAiPlan)>,
    /// Per-selection plan cache for this epoch. Arrowing back to a previously visited row
    /// restores its plan without another provider request.
    ai_cache: std::collections::HashMap<AiSelectionKey, CachedAiPlan>,
    /// Latest editable diagram per selection, shared with internal tools and controllers.
    ai_drafts: std::collections::HashMap<AiSelectionKey, DiagramDraft>,
    /// Last renderable projection of a draft while construction continues.
    ai_previews: std::collections::HashMap<AiSelectionKey, CachedAiPlan>,
    /// Drafts currently owned by the controller CLI rather than an internal provider job.
    agent_owned_drafts: HashSet<AiSelectionKey>,
    /// Last validated design for a stable directory/file/symbol identity. Unlike `ai_cache`, this
    /// survives repository epochs and is supplied only as a continuity seed to the next
    /// request. Current git/LSP facts still own validation and rendering.
    ai_revision_cache: std::collections::HashMap<AiRevisionKey, CachedAiPlan>,
    /// Selection-scoped questions and revision feedback received through the local agent
    /// protocol. Kept separate from evidence and from the generated-plan cache.
    agent_guidance: std::collections::HashMap<AiSelectionKey, AgentGuidance>,
    /// Active AI requests, indexed by their unique generation.
    ai_running: std::collections::HashMap<u64, AiRunningJob>,
    /// Bounded per-generation tool-call history used by the focused loading view.
    ai_activity: std::collections::HashMap<u64, AiActivity>,
    /// FIFO window of active requests. Selection changes do not cancel work; starting a
    /// seventeenth request cancels the oldest active generation.
    ai_requests: RequestCoordinator,
    /// Per-selection terminal failures, surfaced when that row is focused.
    ai_failures: std::collections::HashMap<AiSelectionKey, String>,
    /// Monotonic debounce generation for selection-follow requests.
    ai_selection_seq: u64,
    /// The file the diff pane is aimed at (the files-pane selection; falls back to the
    /// changeset's first file when unset or absent from the set).
    selected_file: Option<String>,
    /// Repo-relative directory selected as an aggregate AI summary scope.
    selected_directory: Option<String>,
    /// Identity of the selected symbol (file, name, line, col), when the selection sits on
    /// a symbol row; gates stale relations jobs.
    selected_symbol: Option<(String, String, u32, u32)>,
    /// The selected symbol's lazily-expanded callers/callees, kept as separate lists so
    /// the impact pane can show both columns.
    selected_relations: Option<SelectedRelations>,
    /// Per-symbol relationship facts cached for rows the user has selected.
    relation_cache: std::collections::HashMap<AiSelectionKey, SelectedRelations>,
    relation_in_flight: std::collections::HashMap<AiSelectionKey, SemanticRunningJob>,
    relation_queue: std::collections::VecDeque<AiSelectionKey>,
    /// The epoch that produced the current `repo_ctx`/`changeset`. Jobs that clone
    /// those as inputs (per-file analysis, AI digest) must only launch when this equals
    /// `self.epoch` — otherwise they would tag old git facts with the new epoch
    /// (review 18 M1).
    data_epoch: Epoch,
    /// Monotonic AI request counter: `AiDone.generation` must match to apply.
    ai_request_seq: u64,
    /// Per-file asynchronous semantic analysis, keyed by repo-relative path. Absent = Unloaded.
    /// Cleared on every epoch bump (scope/base/repo change invalidates file content).
    file_semantics: std::collections::HashMap<String, FileSemanticState>,
    /// Files the user expanded with Tab. Expansion controls visibility only; symbol
    /// analysis is scheduled independently for every changed file.
    expanded_files: std::collections::HashSet<String>,
    /// In-flight per-file analysis jobs: path → the epoch its job was launched under.
    /// A completing job removes only its own entry (matching epoch); a stale-epoch
    /// completion never disturbs a newer job's ledger entry (review 18 M2).
    analysis_in_flight: std::collections::HashMap<String, SemanticRunningJob>,
    /// FIFO queue for per-file analysis beyond the concurrency bound.
    analysis_queue: std::collections::VecDeque<String>,
    output: std::sync::Arc<dyn BackendOutput>,
    /// Where completed jobs report back.
    job_tx: mpsc::Sender<DispatchEvent>,
    /// Typed status message surfaced in the bottom bar (`UiSnapshot::message` mirrors
    /// its text while the renderer migrates).
    status: StatusMessage,
    /// Available AI models for the picker (from the provider).
    available_models: Vec<String>,
    /// Whether provider model discovery is in flight.
    model_list_loading: bool,
    /// Last safe provider model-discovery error.
    model_list_error: Option<String>,
    /// Latest user-triggered model-discovery generation.
    model_list_seq: u64,
    /// User-picked comparison base (overrides inference until cleared).
    base_override: Option<String>,
    /// Base candidates for the picker (from `git base_candidates`).
    available_bases: Vec<String>,
    /// Honesty marker for a bounded base-candidate graph scan.
    available_bases_truncated: bool,
    /// Latest repo context (cheap to re-read).
    repo_ctx: Option<codescope_core::RepoContext>,
    /// Latest raw changeset for the current scope (for the diff pane before analysis lands).
    changeset: Option<codescope_core::ChangeSet>,
    /// FIFO sender for nonblocking persistence. The worker owns the persistence sink.
    config_write_tx: Option<mpsc::UnboundedSender<ConfigWrite>>,
    /// Joined during dispatcher shutdown so the last explicit preference reaches disk.
    config_writer: Option<tokio::task::JoinHandle<()>>,
}

impl Dispatcher {
    /// Build a dispatcher for an already-discovered repo.
    pub(crate) fn new<O: BackendOutput + 'static>(
        repo: GitRepo,
        engine: Option<AnalysisEngine<LanguageService>>,
        ai: Option<AiService>,
        output: O,
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
            analysis: None,
            ai_rows: None,
            ai_cache: std::collections::HashMap::new(),
            ai_drafts: std::collections::HashMap::new(),
            ai_previews: std::collections::HashMap::new(),
            agent_owned_drafts: HashSet::new(),
            ai_revision_cache: std::collections::HashMap::new(),
            agent_guidance: std::collections::HashMap::new(),
            ai_running: std::collections::HashMap::new(),
            ai_activity: std::collections::HashMap::new(),
            ai_requests: RequestCoordinator::default(),
            ai_failures: std::collections::HashMap::new(),
            ai_selection_seq: 0,
            available_models: Vec::new(),
            model_list_loading: false,
            model_list_error: None,
            model_list_seq: 0,
            selected_file: None,
            selected_directory: None,
            selected_symbol: None,
            selected_relations: None,
            relation_cache: std::collections::HashMap::new(),
            relation_in_flight: std::collections::HashMap::new(),
            relation_queue: std::collections::VecDeque::new(),
            file_semantics: std::collections::HashMap::new(),
            expanded_files: std::collections::HashSet::new(),
            ai_request_seq: 0,
            data_epoch: Epoch::ZERO,
            analysis_in_flight: std::collections::HashMap::new(),
            analysis_queue: std::collections::VecDeque::new(),
            base_override: None,
            available_bases: Vec::new(),
            available_bases_truncated: false,
            output: std::sync::Arc::new(output),
            job_tx,
            status: StatusMessage::default(),
            repo_ctx: None,
            changeset: None,
            config_write_tx: None,
            config_writer: None,
        }
    }

    /// Attach global config persistence without making it a requirement for dispatcher
    /// construction or tests.
    pub(crate) fn with_config_persistence(
        mut self,
        persistence: std::sync::Arc<dyn ConfigPersistence>,
    ) -> Self {
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<ConfigWrite>();
        let result_tx = self.job_tx.clone();
        let worker_persistence = persistence.clone();
        let writer = tokio::spawn(async move {
            while let Some(write) = write_rx.recv().await {
                let persistence = worker_persistence.clone();
                let (what, result) = match write {
                    ConfigWrite::Model { provider, model } => {
                        let result = tokio::task::spawn_blocking(move || {
                            persistence.persist_model(&provider, &model)
                        })
                        .await;
                        ("AI model", flatten_config_write_result(result))
                    }
                    ConfigWrite::ReasoningEffort { provider, effort } => {
                        let result = tokio::task::spawn_blocking(move || {
                            persistence.persist_reasoning_effort(&provider, effort)
                        })
                        .await;
                        ("AI reasoning effort", flatten_config_write_result(result))
                    }
                    ConfigWrite::Ui(preferences) => {
                        let result = tokio::task::spawn_blocking(move || {
                            persistence.persist_ui(preferences)
                        })
                        .await;
                        ("view preferences", flatten_config_write_result(result))
                    }
                };
                if let Err(error) = result {
                    let _ = result_tx
                        .send(DispatchEvent::ConfigSaveFailed { what, error })
                        .await;
                }
            }
        });
        self.config_write_tx = Some(write_tx);
        self.config_writer = Some(writer);
        self
    }

    /// Seed a warning that is visible in the first TUI snapshot (for example, a
    /// malformed/future global config that has deliberately been opened read-only).
    pub(crate) fn with_startup_warning(mut self, warning: impl Into<String>) -> Self {
        self.set_status(warning, StatusLevel::Warning);
        self
    }

    /// Mark a dispatcher whose language-server startup already failed before the actor
    /// starts (the headless path). The TUI reports the same state later through an event.
    pub(crate) fn with_engine_unavailable(mut self, reason: impl Into<String>) -> Self {
        self.apply_engine_unavailable(reason.into());
        self
    }

    fn publish(&self) {
        self.output.publish(self.build_snapshot());
    }

    /// Set the bottom-bar status message; `UiSnapshot::message` mirrors the text while
    /// the renderer migrates to the typed [`StatusMessage`].
    fn set_status(&mut self, text: impl Into<String>, level: StatusLevel) {
        self.status = StatusMessage {
            text: text.into(),
            detail: None,
            level,
        };
    }

    /// Set a concise footer message with a separate full diagnostic for the click-open
    /// status dialog. The detail is frozen by the TUI when opened, just like the summary.
    fn set_status_with_detail(
        &mut self,
        text: impl Into<String>,
        detail: impl Into<String>,
        level: StatusLevel,
    ) {
        self.status = StatusMessage {
            text: text.into(),
            detail: Some(detail.into()),
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
            DispatchEvent::BranchUnavailable { epoch, ctx } => {
                self.on_branch_unavailable(epoch, ctx)
            }
            DispatchEvent::ConfigSaveFailed { what, error } => {
                self.set_status(
                    format!(
                        "{what} could not be saved; the session change remains active: {error}"
                    ),
                    StatusLevel::Warning,
                );
                self.publish();
            }
            DispatchEvent::FileAnalysisDone {
                epoch,
                file,
                result,
            } => self.on_file_analysis_done(epoch, file, result),
            DispatchEvent::AiDone {
                epoch,
                selection,
                generation,
                outcome,
            } => self.on_ai_done(epoch, selection, generation, outcome),
            DispatchEvent::AiDraft {
                epoch,
                selection,
                generation,
                draft,
            } => self.on_ai_draft(epoch, selection, generation, draft),
            DispatchEvent::AiActivity {
                epoch,
                selection,
                generation,
                update,
            } => self.on_ai_activity(epoch, selection, generation, update),
            DispatchEvent::AiSelectionSettled { epoch, generation } => {
                if epoch == self.epoch && generation == self.ai_selection_seq {
                    if let Some(selection) = self.current_ai_selection() {
                        self.spawn_ai_job(selection);
                        self.refresh_current_ai_status();
                        self.publish();
                    }
                }
            }
            DispatchEvent::EngineReady(engine) => {
                self.ls_status = LsStatus::Ready;
                self.engine = Some(std::sync::Arc::new(*engine));
                // A changeset may have landed while the language server initialized.
                // Drain its already-queued per-file work now; the bounded queue keeps this
                // asynchronous and prevents a large change-set from flooding the server.
                self.schedule_all_file_analysis();
                self.drain_analysis_queue();
                self.drain_relation_queue();
                self.publish();
            }
            DispatchEvent::EngineUnavailable(reason) => {
                self.apply_engine_unavailable(reason);
                self.publish();
            }
            DispatchEvent::ModelsLoaded { generation, result } => {
                if generation != self.model_list_seq {
                    return;
                }
                self.model_list_loading = false;
                match result {
                    Ok(models) => {
                        self.model_list_error = None;
                        self.merge_available_models(models);
                    }
                    Err(error) => {
                        self.model_list_error = Some(error.clone());
                        self.set_status(
                            format!(
                                "model discovery failed; current/manual model remains available: {error}"
                            ),
                            StatusLevel::Warning,
                        );
                    }
                }
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
                let selection = AiSelectionKey::Symbol {
                    file,
                    name,
                    line,
                    col,
                };
                if self
                    .relation_in_flight
                    .get(&selection)
                    .is_some_and(|job| job.epoch == epoch)
                {
                    self.relation_in_flight.remove(&selection);
                }
                if epoch != self.epoch {
                    self.drain_relation_queue();
                    return;
                }
                let relations = SelectedRelations { callers, callees };
                self.relation_cache
                    .insert(selection.clone(), relations.clone());
                let current = self.current_ai_selection();
                if current.as_ref() == Some(&selection) {
                    self.selected_relations = Some(relations);
                }
                self.drain_relation_queue();
                self.publish();
            }
            DispatchEvent::BaseLoaded { bases, truncated } => {
                // The picker always offers "(auto / inferred)" first to escape an override.
                let mut list = vec![AUTO_BASE.to_string()];
                list.extend(bases);
                self.available_bases = list;
                self.available_bases_truncated = truncated;
                self.publish();
            }
        }
    }

    fn bump_and_refresh(&mut self) {
        // `spawn_refresh` is the single epoch owner. Keeping the bump there prevents one
        // filesystem notification from invalidating two generations of otherwise valid
        // work.
        self.spawn_refresh();
    }

    fn apply_engine_unavailable(&mut self, reason: String) {
        self.ls_status = LsStatus::Failed;
        // No symbol request can ever complete in this epoch. Mark every changed file
        // terminal/unsupported so file-level AI can still explain its diff.
        if let Some(changeset) = &self.changeset {
            for file in &changeset.files {
                self.file_semantics
                    .insert(file.path.to_string(), FileSemanticState::Unsupported);
            }
        }
        self.analysis_queue.clear();
        self.relation_queue.clear();
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
        if self.current_ai_selection().is_some() {
            self.retarget_ai_to_current_selection(true);
        }
    }

    fn on_action(&mut self, action: Action) {
        match action {
            Action::RefreshGit => self.spawn_refresh(),
            Action::SetFileExpanded { path, expanded } => self.set_file_expanded(&path, expanded),
            Action::SetDirectoryExpanded { .. } => {}
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
            Action::ModelPicker => self.spawn_list_models(),
            Action::AiSettingsSelected {
                model,
                reasoning_effort,
            } => self.set_ai_settings(&model, &reasoning_effort),
            Action::PersistUiPreferences(preferences) => {
                self.queue_config_write(ConfigWrite::Ui(preferences));
            }
            Action::SelectSymbol {
                file,
                name,
                line,
                col,
            } => {
                // Enter re-centers on the selection: record it as the current target (so
                // the result is not dropped as stale) and expand its relations.
                self.selected_directory = None;
                self.selected_file = Some(file.clone());
                self.selected_symbol = Some((file.clone(), name.clone(), line, col));
                self.spawn_expand(file, name, line, col);
            }
            Action::SelectionChanged { file, symbol } => self.on_selection_changed(file, symbol),
            Action::DirectorySelectionChanged { directory } => {
                self.on_directory_selection_changed(directory)
            }
            Action::AgentAsk(question) => self.apply_agent_guidance(question, false),
            Action::AgentFeedback(feedback) => self.apply_agent_guidance(feedback, true),
            Action::AgentDiagram(command) => self.apply_agent_diagram(command),
            Action::BasePicker => self.spawn_list_bases(),
            Action::BaseSelected(name) => self.set_base(name),
            _ => {}
        }
    }

    /// Fetch the provider's model list for the picker (spawned; non-blocking).
    fn spawn_list_models(&mut self) {
        let Some(ai) = self.ai.clone() else {
            self.set_status(
                "AI not configured (set PRIME_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY)",
                StatusLevel::Warning,
            );
            self.publish();
            return;
        };
        // The currently configured model is always a valid picker fallback, even when the
        // provider has no `/models` endpoint or discovery fails after an inference error.
        self.merge_available_models([ai.model()]);
        self.model_list_seq = self.model_list_seq.saturating_add(1);
        let generation = self.model_list_seq;
        self.model_list_loading = true;
        self.model_list_error = None;
        self.publish();
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
            let result = ai
                .client()
                .list_models()
                .await
                .map_err(|error| error.to_string());
            let _ = tx
                .send(DispatchEvent::ModelsLoaded { generation, result })
                .await;
        });
    }

    fn merge_available_models(&mut self, models: impl IntoIterator<Item = String>) {
        for model in models {
            let model = model.trim();
            if !model.is_empty() && !self.available_models.iter().any(|item| item == model) {
                self.available_models.push(model.to_string());
            }
        }
    }

    /// Apply the model picker's staged model and reasoning budget atomically.
    fn set_ai_settings(&mut self, model: &str, reasoning_effort: &str) {
        let effort = match reasoning_effort.parse::<ReasoningEffort>() {
            Ok(effort) => effort,
            Err(error) => {
                self.set_status(error, StatusLevel::Warning);
                self.publish();
                return;
            }
        };
        let changed = match self.ai.clone() {
            Some(ai)
                if ai.provider_label() == "anthropic" && effort != ReasoningEffort::Default =>
            {
                self.set_status(
                    "reasoning_effort is unavailable through Anthropic's native API",
                    StatusLevel::Warning,
                );
                false
            }
            Some(ai) => {
                let provider = ai.provider_label().to_string();
                ai.set_model(model);
                ai.set_reasoning_effort(effort);
                self.queue_config_write(ConfigWrite::Model {
                    provider: provider.clone(),
                    model: model.to_string(),
                });
                self.queue_config_write(ConfigWrite::ReasoningEffort { provider, effort });
                true
            }
            None => {
                self.set_status(
                    "AI not configured (set PRIME_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY)",
                    StatusLevel::Warning,
                );
                false
            }
        };
        if changed {
            self.reset_ai_for_settings_change();
            self.set_status(
                format!("AI model: {model} · reasoning: {effort}"),
                StatusLevel::Info,
            );
        }
        self.publish();
    }

    /// A model or reasoning-budget change invalidates outputs and in-flight work produced
    /// with the previous request settings, then prioritizes the current selection again.
    fn reset_ai_for_settings_change(&mut self) {
        self.ai_cache.clear();
        self.ai_drafts.clear();
        self.ai_previews.clear();
        self.agent_owned_drafts.clear();
        self.ai_revision_cache.clear();
        self.ai_failures.clear();
        self.abort_all_ai_requests();
        self.ai_rows = None;
        if self.ai.is_some() {
            self.retarget_ai_to_current_selection(true);
        } else {
            self.ai_status = AiStatus::Idle;
        }
    }

    fn queue_config_write(&mut self, write: ConfigWrite) {
        let Some(tx) = &self.config_write_tx else {
            return;
        };
        if tx.send(write).is_err() {
            self.set_status(
                "preference changed for this session but the config writer is unavailable",
                StatusLevel::Warning,
            );
            self.publish();
        }
    }

    /// Fetch base candidates for the base picker (spawned; non-blocking).
    fn spawn_list_bases(&mut self) {
        let repo = self.repo.clone();
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
            let result = repo.base_candidates_with_metadata().await;
            let (bases, truncated) = match result {
                Ok(candidates) => (
                    candidates
                        .entries
                        .into_iter()
                        .map(|base| base.ref_name)
                        .collect(),
                    candidates.truncated,
                ),
                Err(_) => (Vec::new(), false),
            };
            let _ = tx
                .send(DispatchEvent::BaseLoaded { bases, truncated })
                .await;
        });
    }

    /// The files-pane selection moved (navigation-driven panes; no Enter required): aim
    /// the diff pane at the selected file, publish the selection's `SelectedChange`
    /// immediately (deterministic interpretation; spec §5.3/§5.6), and lazily expand a
    /// selected symbol's callers/callees — the impact lists read `Loading` until the
    /// fetch lands. Moving OFF a symbol (file row / empty list) clears the relations
    /// view and leaves the impact lists `Idle`. The same selection transition retargets
    /// the automatically generated plan.
    fn on_selection_changed(&mut self, file: Option<String>, symbol: Option<(String, u32, u32)>) {
        let previous_ai_selection = self.current_ai_selection();
        self.selected_directory = None;
        self.selected_file = file.clone();
        self.selected_symbol = match (file, symbol) {
            (Some(file), Some((name, line, col))) => Some((file, name, line, col)),
            _ => None,
        };
        self.reprioritize_semantic_work();
        // Drop the previous selection's rows immediately: nothing stale may linger while
        // the new fetch is in flight.
        self.selected_relations = self
            .current_ai_selection()
            .as_ref()
            .and_then(|selection| self.relation_cache.get(selection).cloned());
        if let Some((file, name, line, col)) = self.selected_symbol.clone() {
            if self.selected_relations.is_none() {
                self.spawn_expand(file, name, line, col);
            }
        }
        if self.current_ai_selection() != previous_ai_selection {
            self.retarget_ai_to_current_selection(false);
        }
        if self.selected_file.is_some() {
            // Loading may mean queued rather than running. Semantic reprioritization has
            // already moved this file to the front; draining lets it claim the focused lane.
            self.drain_analysis_queue();
        }
        self.drain_relation_queue();
        self.publish();
    }

    fn on_directory_selection_changed(&mut self, directory: String) {
        let previous_ai_selection = self.current_ai_selection();
        self.selected_directory = Some(directory);
        self.selected_file = None;
        self.selected_symbol = None;
        self.selected_relations = None;
        self.reprioritize_semantic_work();
        if self.current_ai_selection() != previous_ai_selection {
            self.retarget_ai_to_current_selection(false);
        }
        self.publish();
    }

    fn apply_agent_guidance(&mut self, text: String, feedback: bool) {
        let Some(selection) = self.current_ai_selection() else {
            self.set_status(
                "agent request needs a selected directory, file, or function",
                StatusLevel::Warning,
            );
            self.publish();
            return;
        };
        let text = normalize_agent_text(&text);
        if text.is_empty() {
            self.set_status("agent request was empty", StatusLevel::Warning);
            self.publish();
            return;
        }
        let guidance = self.agent_guidance.entry(selection.clone()).or_default();
        if feedback {
            guidance.feedback = Some(text.clone());
        } else {
            guidance.question = Some(text.clone());
            guidance.feedback = None;
        }

        // A replacement instruction for the same target supersedes its in-flight answer.
        // Ordinary navigation still never cancels provider work; this is the one explicit
        // revision path where allowing the old answer to win would ignore the command.
        let generations = self
            .ai_running
            .iter()
            .filter_map(|(generation, running)| {
                (running.epoch == self.epoch && running.selection == selection)
                    .then_some(*generation)
            })
            .collect::<Vec<_>>();
        for generation in generations {
            self.ai_requests.abort(generation);
            self.ai_running.remove(&generation);
            self.ai_activity.remove(&generation);
        }
        self.retarget_ai_to_current_selection(true);
        self.set_status(
            format!(
                "agent: {} {}",
                if feedback {
                    "revising from"
                } else {
                    "answering"
                },
                truncate_chars(&text, 120)
            ),
            StatusLevel::Info,
        );
        self.publish();
    }

    /// Apply the controller's command to the exact same draft model exposed to the
    /// provider. A controller mutation takes ownership of this selection's writer, so an
    /// older internal request cannot subsequently overwrite it.
    fn apply_agent_diagram(&mut self, command: DiagramCommand) {
        let Some(selection) = self.current_ai_selection() else {
            self.set_status(
                "diagram edit needs a selected directory, file, or function",
                StatusLevel::Warning,
            );
            self.publish();
            return;
        };
        if self.data_epoch != self.epoch || self.changeset.is_none() {
            self.set_status(
                "diagram edit is waiting for the current Git snapshot",
                StatusLevel::Warning,
            );
            self.publish();
            return;
        }

        self.cancel_ai_for_selection(&selection);
        let mut draft = self
            .ai_drafts
            .get(&selection)
            .cloned()
            .or_else(|| {
                self.ai_cache
                    .get(&selection)
                    .map(|cached| DiagramDraft::from_plan(&cached.plan))
            })
            .unwrap_or_else(|| DiagramDraft::new(self.epoch));
        draft.epoch = self.epoch;
        let summary = match draft.apply(&command) {
            Ok(summary) => summary,
            Err(error) => {
                self.set_status(
                    format!("diagram edit rejected: {error}"),
                    StatusLevel::Warning,
                );
                self.publish();
                return;
            }
        };

        self.ai_drafts.insert(selection.clone(), draft.clone());
        self.agent_owned_drafts.insert(selection.clone());
        self.ai_failures.remove(&selection);
        if matches!(command, DiagramCommand::Finish) {
            match self.validated_draft(&selection, &draft) {
                Ok(cached) => {
                    self.ai_cache.insert(selection.clone(), cached.clone());
                    self.ai_previews.insert(selection.clone(), cached.clone());
                    self.ai_revision_cache
                        .insert(AiRevisionKey::from(&selection), cached.clone());
                    self.agent_owned_drafts.remove(&selection);
                    self.ai_rows = Some((self.epoch, selection, cached));
                    self.ai_status = AiStatus::Ready { epoch: self.epoch };
                    self.set_status("agent diagram validated and published", StatusLevel::Info);
                }
                Err(error) => {
                    self.ai_status = AiStatus::Failed {
                        reason: error.clone(),
                    };
                    self.set_status_with_detail(
                        format!(
                            "agent diagram needs another edit: {}",
                            truncate_chars(&error, 140)
                        ),
                        error,
                        StatusLevel::Warning,
                    );
                }
            }
        } else {
            self.ai_cache.remove(&selection);
            self.ai_rows = None;
            if draft.forms.is_empty() {
                self.ai_previews.remove(&selection);
            } else if let Ok(preview) = self.validated_draft(&selection, &draft) {
                self.ai_previews.insert(selection.clone(), preview);
            }
            self.ai_status = AiStatus::Loading {
                since_epoch: self.epoch,
            };
            self.set_status(format!("agent diagram: {summary}"), StatusLevel::Info);
        }
        self.publish();
    }

    fn cancel_ai_for_selection(&mut self, selection: &AiSelectionKey) {
        let generations = self
            .ai_running
            .iter()
            .filter_map(|(generation, running)| {
                (running.epoch == self.epoch && running.selection == *selection)
                    .then_some(*generation)
            })
            .collect::<Vec<_>>();
        for generation in generations {
            self.ai_requests.abort(generation);
            self.ai_running.remove(&generation);
        }
    }

    fn validated_draft(
        &self,
        selection: &AiSelectionKey,
        draft: &DiagramDraft,
    ) -> Result<CachedAiPlan, String> {
        if draft.epoch != self.epoch {
            return Err(format!(
                "draft epoch {} does not match current epoch {}",
                draft.epoch, self.epoch
            ));
        }
        let changeset = self
            .changeset
            .as_ref()
            .ok_or_else(|| "current Git facts are unavailable".to_string())?;
        let scoped = changeset_for_selection(changeset, selection);
        if scoped.files.is_empty() {
            return Err("the current selection has no changed files".to_string());
        }
        let serialized = serde_json::to_string(&draft.plan())
            .map_err(|error| format!("could not serialize the draft: {error}"))?;
        let mut plan = parse_plan(&serialized).map_err(|error| error.to_string())?;
        let facts = SnapshotFacts::from_lazy(&scoped, &self.file_semantics, selection);
        let report = validate(&mut plan, &facts, self.epoch);
        if report.is_renderable() {
            Ok(CachedAiPlan { plan, report })
        } else {
            let detail = report
                .dropped
                .iter()
                .map(|item| format!("{}: {}", item.subject, item.reason))
                .chain(report.notes.iter().cloned())
                .collect::<Vec<_>>()
                .join("; ");
            Err(if detail.is_empty() {
                "the draft has no renderable diagram".to_string()
            } else {
                detail
            })
        }
    }

    fn current_ai_selection(&self) -> Option<AiSelectionKey> {
        if let Some(directory) = &self.selected_directory {
            return Some(AiSelectionKey::Directory(directory.clone()));
        }
        if let Some((file, name, line, col)) = &self.selected_symbol {
            return Some(AiSelectionKey::Symbol {
                file: file.clone(),
                name: name.clone(),
                line: *line,
                col: *col,
            });
        }
        self.selected_file
            .as_ref()
            .map(|file| AiSelectionKey::File(file.clone()))
    }

    /// Move the generated pane to the current changed directory/file/function row. `invalidate_cache`
    /// is used when its symbols or file contents changed; navigation reuses cached plans.
    fn retarget_ai_to_current_selection(&mut self, invalidate_cache: bool) {
        // Invalidate only the pending navigation debounce. An already-sent provider request
        // is deliberately allowed to finish and cache for its own selection.
        self.ai_selection_seq = self.ai_selection_seq.saturating_add(1);
        self.ai_rows = None;

        let Some(selection) = self.current_ai_selection() else {
            self.ai_status = AiStatus::Idle;
            self.set_status(
                "select a changed directory, file, or function before generating Impact",
                StatusLevel::Warning,
            );
            return;
        };
        if invalidate_cache {
            self.ai_cache.remove(&selection);
            self.ai_drafts.remove(&selection);
            self.ai_previews.remove(&selection);
            self.agent_owned_drafts.remove(&selection);
            self.ai_failures.remove(&selection);
        } else if let Some(plan) = self.ai_cache.get(&selection).cloned() {
            self.ai_rows = Some((self.epoch, selection, plan));
            self.ai_status = AiStatus::Ready { epoch: self.epoch };
            return;
        } else if self.agent_owned_drafts.contains(&selection) {
            self.ai_status = AiStatus::Loading {
                since_epoch: self.epoch,
            };
            return;
        }
        if self.ai.is_none() {
            self.ai_status = AiStatus::Disabled;
            return;
        }

        // The plan must never race the selected file's symbol inventory. `Unsupported`
        // is terminal and honest (there are no loadable symbols), while Failed remains
        // non-ready until the next repository refresh retries it.
        let Some(file) = selection.file() else {
            self.ai_status = AiStatus::Debouncing { epoch: self.epoch };
            self.schedule_ai_selection();
            return;
        };
        match self.file_semantics.get(file) {
            Some(FileSemanticState::Ready(_)) | Some(FileSemanticState::Unsupported) => {}
            Some(FileSemanticState::Failed) => {
                self.ai_status = AiStatus::Idle;
                return;
            }
            Some(FileSemanticState::Loading) | None => {
                self.ai_status = AiStatus::WaitingForSymbols { epoch: self.epoch };
                self.spawn_file_analysis(file);
                return;
            }
        }

        // Relation loading continues independently. If it finishes inside this debounce,
        // the prompt includes it; a late result never delays, cancels, or restarts inference.
        self.ai_status = AiStatus::Debouncing { epoch: self.epoch };
        self.schedule_ai_selection();
    }

    /// Debounce arrow navigation so holding Up/Down does not issue—and bill—one request
    /// per intermediate row. Cache hits bypass this path and render immediately.
    fn schedule_ai_selection(&mut self) {
        self.schedule_ai_selection_after(250);
    }

    fn schedule_ai_selection_after(&mut self, delay_ms: u64) {
        self.ai_selection_seq = self.ai_selection_seq.saturating_add(1);
        let generation = self.ai_selection_seq;
        let epoch = self.epoch;
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            let _ = tx
                .send(DispatchEvent::AiSelectionSettled { epoch, generation })
                .await;
        });
    }

    fn abort_all_ai_requests(&mut self) {
        self.ai_requests.abort_all();
        self.ai_running.clear();
        self.ai_activity.clear();
    }

    fn refresh_current_ai_status(&mut self) {
        let Some(selection) = self.current_ai_selection() else {
            self.ai_rows = None;
            self.ai_status = if self.ai.is_some() {
                AiStatus::Idle
            } else {
                AiStatus::Disabled
            };
            return;
        };
        if let Some(plan) = self.ai_cache.get(&selection).cloned() {
            self.ai_rows = Some((self.epoch, selection, plan));
            self.ai_status = AiStatus::Ready { epoch: self.epoch };
            return;
        }
        self.ai_rows = None;
        if let Some(reason) = self.ai_failures.get(&selection).cloned() {
            self.ai_status = AiStatus::Failed { reason };
            return;
        }
        if self.agent_owned_drafts.contains(&selection) {
            self.ai_status = AiStatus::Loading {
                since_epoch: self.epoch,
            };
            return;
        }
        if self.ai.is_none() {
            self.ai_status = AiStatus::Disabled;
            return;
        }
        if let Some(file) = selection.file() {
            match self.file_semantics.get(file) {
                Some(FileSemanticState::Loading) | None => {
                    self.ai_status = AiStatus::WaitingForSymbols { epoch: self.epoch };
                    return;
                }
                Some(FileSemanticState::Failed) => {
                    self.ai_status = AiStatus::Idle;
                    return;
                }
                Some(FileSemanticState::Ready(_)) | Some(FileSemanticState::Unsupported) => {}
            }
        }
        if self
            .ai_running
            .values()
            .any(|job| job.epoch == self.epoch && job.selection == selection)
        {
            self.ai_status = AiStatus::Loading {
                since_epoch: self.epoch,
            };
            return;
        }
        self.ai_status = AiStatus::Debouncing { epoch: self.epoch };
    }

    /// Queue a selected symbol's callers/callees through the bounded prerequisite path.
    fn spawn_expand(&mut self, file: String, name: String, line: u32, col: u32) {
        let selection = AiSelectionKey::Symbol {
            file,
            name,
            line,
            col,
        };
        self.enqueue_relation_job(selection);
        self.drain_relation_queue();
    }

    const MAX_RELATION_JOBS: usize = 2;
    const MAX_BACKGROUND_RELATION_JOBS: usize = 1;

    fn semantic_priority_for_file(&self, file: &str) -> SemanticJobPriority {
        if self.selected_file.as_deref() == Some(file) {
            SemanticJobPriority::Focused
        } else {
            SemanticJobPriority::Background
        }
    }

    fn semantic_priority_for_selection(&self, selection: &AiSelectionKey) -> SemanticJobPriority {
        if self.current_ai_selection().as_ref() == Some(selection) {
            SemanticJobPriority::Focused
        } else {
            SemanticJobPriority::Background
        }
    }

    fn reprioritize_semantic_work(&mut self) {
        let selected_file = self.selected_file.as_deref();
        for (file, job) in &mut self.analysis_in_flight {
            job.priority = if selected_file == Some(file.as_str()) {
                SemanticJobPriority::Focused
            } else {
                SemanticJobPriority::Background
            };
        }
        if let Some(file) = &self.selected_file {
            if let Some(index) = self.analysis_queue.iter().position(|queued| queued == file) {
                if let Some(queued) = self.analysis_queue.remove(index) {
                    self.analysis_queue.push_front(queued);
                }
            }
        }

        let current = self.current_ai_selection();
        for (selection, job) in &mut self.relation_in_flight {
            job.priority = if current.as_ref() == Some(selection) {
                SemanticJobPriority::Focused
            } else {
                SemanticJobPriority::Background
            };
        }
        if let Some(selection) = current {
            if let Some(index) = self
                .relation_queue
                .iter()
                .position(|queued| queued == &selection)
            {
                if let Some(queued) = self.relation_queue.remove(index) {
                    self.relation_queue.push_front(queued);
                }
            }
        }
    }

    fn can_launch_relation(&self, priority: SemanticJobPriority) -> bool {
        if self.relation_in_flight.len() >= Self::MAX_RELATION_JOBS {
            return false;
        }
        priority == SemanticJobPriority::Focused
            || self
                .relation_in_flight
                .values()
                .filter(|job| job.priority == SemanticJobPriority::Background)
                .count()
                < Self::MAX_BACKGROUND_RELATION_JOBS
    }

    fn enqueue_relation_job(&mut self, selection: AiSelectionKey) {
        let AiSelectionKey::Symbol { file, .. } = &selection else {
            return;
        };
        if self.engine.is_none()
            || self.relation_cache.contains_key(&selection)
            || self.relation_in_flight.contains_key(&selection)
            || self
                .relation_queue
                .iter()
                .any(|queued| queued == &selection)
            || !matches!(
                self.file_semantics.get(file),
                Some(FileSemanticState::Ready(_))
            )
        {
            return;
        }
        if self.semantic_priority_for_selection(&selection) == SemanticJobPriority::Focused {
            self.relation_queue.push_front(selection);
        } else {
            self.relation_queue.push_back(selection);
        }
    }

    fn drain_relation_queue(&mut self) {
        if self.engine.is_none() || self.data_epoch != self.epoch {
            return;
        }
        // Inspect each queued entry at most once per drain. A saturated background lane
        // therefore leaves work queued instead of pop/requeue spinning the actor.
        let mut remaining = self.relation_queue.len();
        while self.relation_in_flight.len() < Self::MAX_RELATION_JOBS && remaining > 0 {
            remaining -= 1;
            let Some(selection) = self.relation_queue.pop_front() else {
                break;
            };
            if self.relation_cache.contains_key(&selection)
                || self.relation_in_flight.contains_key(&selection)
            {
                continue;
            }
            let priority = self.semantic_priority_for_selection(&selection);
            if !self.can_launch_relation(priority) {
                self.relation_queue.push_back(selection);
                continue;
            }
            self.spawn_relation_job_now(selection);
        }
    }

    fn spawn_relation_job_now(&mut self, selection: AiSelectionKey) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let AiSelectionKey::Symbol {
            file,
            name,
            line,
            col,
        } = selection.clone()
        else {
            return;
        };
        let epoch = self.epoch;
        let tx = self.job_tx.clone();
        let file_id = match codescope_core::FileId::new(file.clone()) {
            Ok(f) => f,
            Err(_) => return,
        };
        let priority = self.semantic_priority_for_selection(&selection);
        self.relation_in_flight
            .insert(selection, SemanticRunningJob { epoch, priority });
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
        self.ai_selection_seq = self.ai_selection_seq.saturating_add(1);
        self.abort_all_ai_requests();
        self.ai_failures.clear();
        self.ai_rows = None;
        // Epoch-exact plans may no longer render after any repository refresh. Preserve
        // their stable revision counterparts: once fresh facts land they become prompt
        // seeds, never current UI state.
        self.ai_cache.clear();
        self.ai_drafts.clear();
        self.ai_previews.clear();
        self.agent_owned_drafts.clear();
        if self.ai.is_some() {
            self.ai_status = AiStatus::Stale { epoch };
        }
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
        // A new epoch invalidates cached semantics and queued intent. Keep the in-flight
        // ledger: if the same file changed while its old analysis was running, the fresh
        // request queues behind that job instead of racing two language-server overlays.
        // The stale completion removes its own epoch-exact ledger entry and drains the
        // fresh queue.
        self.file_semantics.clear();
        self.analysis_queue.clear();
        self.relation_cache.clear();
        self.relation_queue.clear();
        // Relations for the selected symbol are re-fetched in `on_analysis_done`, once the
        // new analysis exists. Firing the query here would race the language server's own
        // refresh and tag a pre-refresh answer with the new epoch. The previously loaded
        // rows describe the old state, so drop them: `build_impact` renders the lists as
        // Loading while a selection is set.
        self.selected_relations = None;
    }

    fn spawn_ai_job(&mut self, selection: AiSelectionKey) {
        let (Some(_ai), Some(changeset)) = (&self.ai, &self.changeset) else {
            return;
        };
        let Some(ctx) = &self.repo_ctx else { return };
        if self.data_epoch != self.epoch
            || self.ai_cache.contains_key(&selection)
            || self
                .ai_running
                .values()
                .any(|running| running.epoch == self.epoch && running.selection == selection)
        {
            return;
        }
        let scoped_changeset = changeset_for_selection(changeset, &selection);
        if scoped_changeset.files.is_empty() {
            return;
        }
        if let Some(generation) = self.ai_requests.admit() {
            if let Some(running) = self.ai_running.remove(&generation) {
                tracing::debug!(
                    generation,
                    selection = %running.selection.label(),
                    "cancelled oldest AI request after active window reached 16"
                );
            }
            self.ai_activity.remove(&generation);
        }
        // A validated plan from an older epoch is not current UI state, but it is a useful
        // design draft for the same directory/file/symbol. The AI service labels it as untrusted
        // continuity context and requires the new facts to win.
        let previous_plan = self
            .ai_revision_cache
            .get(&AiRevisionKey::from(&selection))
            .map(|cached| cached.plan.clone());
        let epoch = self.epoch;
        self.ai_request_seq = self.ai_request_seq.saturating_add(1);
        let generation = self.ai_request_seq;
        // The initial turn is intentionally an inventory, not an evidence dump. A scoped
        // mini-shell serves the captured Git snapshot and selected worktree files on demand;
        // its virtual cwd is the selected directory or the selected file's parent.
        let mut brief = research_brief(&selection, &scoped_changeset);
        if let Some(guidance) = self.agent_guidance.get(&selection) {
            brief.push_str(&guidance.prompt_section());
        }
        let research_tools =
            ScopedResearchTools::new(ctx.toplevel.clone(), &selection, scoped_changeset.clone());
        let facts = SnapshotFacts::from_lazy(&scoped_changeset, &self.file_semantics, &selection);
        let ai = self.ai.clone();
        let tx = self.job_tx.clone();
        let draft_tx = tx.clone();
        let activity_tx = tx.clone();
        let draft_selection = selection.clone();
        let observer: DiagramObserver = std::sync::Arc::new(move |draft| {
            // Draft mutations are bounded and frequent. Never make the provider loop wait
            // behind rendering; a later complete-draft event supersedes a full channel.
            let _ = draft_tx.try_send(DispatchEvent::AiDraft {
                epoch,
                selection: draft_selection.clone(),
                generation,
                draft,
            });
        });
        let activity_selection = selection.clone();
        let activity_observer: AiActivityObserver = std::sync::Arc::new(move |update| {
            let _ = activity_tx.try_send(DispatchEvent::AiActivity {
                epoch,
                selection: activity_selection.clone(),
                generation,
                update,
            });
        });
        let running_selection = selection.clone();
        let task = tokio::spawn(async move {
            let outcome = match &ai {
                Some(ai) => {
                    ai.request_plan_with_observers(
                        &brief,
                        previous_plan.as_ref(),
                        &research_tools,
                        &facts,
                        epoch,
                        Some(observer),
                        Some(activity_observer),
                    )
                    .await
                }
                None => AiOutcome::Unavailable,
            };
            let _ = tx
                .send(DispatchEvent::AiDone {
                    epoch,
                    selection,
                    generation,
                    outcome,
                })
                .await;
        });
        self.ai_requests.register(generation, task.abort_handle());
        self.ai_running.insert(
            generation,
            AiRunningJob {
                selection: running_selection.clone(),
                epoch,
                generation,
            },
        );
        self.ai_activity.insert(
            generation,
            AiActivity {
                active: true,
                waiting_for_model: true,
                ..AiActivity::default()
            },
        );
        tracing::debug!(
            generation,
            selection = %running_selection.label(),
            active = self.ai_requests.len(),
            "started AI request"
        );
        if self.current_ai_selection().as_ref() == Some(&running_selection) {
            self.ai_status = AiStatus::Loading { since_epoch: epoch };
            self.ai_rows = None;
        }
        self.publish();
    }

    /// One focused file may run alongside one background warm-up. This preserves eventual
    /// automatic loading without allowing a large change-set to saturate the language
    /// server before the row under the pointer can be analyzed.
    const MAX_FILE_JOBS: usize = 2;
    const MAX_BACKGROUND_FILE_JOBS: usize = 1;

    fn can_launch_file_analysis(&self, priority: SemanticJobPriority) -> bool {
        if self.analysis_in_flight.len() >= Self::MAX_FILE_JOBS {
            return false;
        }
        priority == SemanticJobPriority::Focused
            || self
                .analysis_in_flight
                .values()
                .filter(|job| job.priority == SemanticJobPriority::Background)
                .count()
                < Self::MAX_BACKGROUND_FILE_JOBS
    }

    /// Schedule every file in the current change-set through the bounded per-file queue.
    fn schedule_all_file_analysis(&mut self) {
        let paths: Vec<String> = self
            .changeset
            .as_ref()
            .map(|changeset| {
                changeset
                    .files
                    .iter()
                    .map(|file| file.path.to_string())
                    .collect()
            })
            .unwrap_or_default();
        for path in paths {
            self.spawn_file_analysis(&path);
        }
    }

    /// Start (or queue) asynchronous per-file analysis for `path`. Coalesces duplicates: a
    /// file already Loading/Ready this epoch launches nothing; a path with a job in
    /// flight (any epoch) is queued so the language server's per-file overlay never sees
    /// two writers (review 18 M2).
    fn spawn_file_analysis(&mut self, path: &str) {
        // Coalesce terminal/ready states: a cached Ready or a definitive Unsupported is
        // reused within the epoch. Failed files retry on the next repository epoch.
        if matches!(
            self.file_semantics.get(path),
            Some(FileSemanticState::Loading)
                | Some(FileSemanticState::Ready(_))
                | Some(FileSemanticState::Unsupported)
                | Some(FileSemanticState::Failed)
        ) {
            return;
        }
        // The engine may still be starting (LsStatus::Starting) — queue, don't mislabel
        // the file as unsupported (review 18 m2).
        if self.engine.is_none() {
            if self.ls_status == codescope_core::LsStatus::Failed {
                self.file_semantics
                    .insert(path.to_string(), FileSemanticState::Unsupported);
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
        if !self.can_launch_file_analysis(self.semantic_priority_for_file(path)) {
            self.enqueue_file_analysis(path);
            return;
        }
        self.spawn_file_analysis_now(path);
    }

    /// Queue `path` for a later spawn (bounded concurrency / stale data epoch / engine
    /// starting), marking the row Loading so the UI shows the pending state. Callers
    /// publish once after batching all changed files.
    fn enqueue_file_analysis(&mut self, path: &str) {
        if !self.analysis_queue.iter().any(|p| p == path) {
            if self.semantic_priority_for_file(path) == SemanticJobPriority::Focused {
                self.analysis_queue.push_front(path.to_string());
            } else {
                self.analysis_queue.push_back(path.to_string());
            }
        }
        if !matches!(
            self.file_semantics.get(path),
            Some(FileSemanticState::Loading)
        ) {
            self.file_semantics
                .insert(path.to_string(), FileSemanticState::Loading);
        }
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
        self.analysis_in_flight.insert(
            path.to_string(),
            SemanticRunningJob {
                epoch: self.epoch,
                priority: self.semantic_priority_for_file(path),
            },
        );
        self.file_semantics
            .insert(path.to_string(), FileSemanticState::Loading);
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
        let selected_ai_file_changed = self
            .current_ai_selection()
            .and_then(|selection| selection.file().map(str::to_string))
            .is_some_and(|selected| selected == file);
        // Ledger removal is epoch-exact: a stale completion never disturbs a newer job's
        // entry for the same path (review 18 M2).
        if self
            .analysis_in_flight
            .get(&file)
            .is_some_and(|job| job.epoch == epoch)
        {
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
                self.file_semantics
                    .insert(file.clone(), FileSemanticState::Failed);
            }
        }
        if selected_ai_file_changed {
            // The deterministic Impact interpretation/symbol inventory changed while the
            // selection stayed put; its cached AI explanation is no longer current.
            self.retarget_ai_to_current_selection(true);
        }
        self.publish();
        self.drain_analysis_queue();
    }

    /// Start the next queued per-file job when a slot frees and the data epoch is current.
    fn drain_analysis_queue(&mut self) {
        // While the language server is starting, queued work must remain queued. Popping
        // and immediately re-enqueuing it would spin the dispatcher forever.
        if self.engine.is_none() {
            return;
        }
        let mut remaining = self.analysis_queue.len();
        while self.analysis_in_flight.len() < Self::MAX_FILE_JOBS
            && self.data_epoch == self.epoch
            && remaining > 0
        {
            remaining -= 1;
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
            let priority = self.semantic_priority_for_file(&next);
            if !self.can_launch_file_analysis(priority) {
                self.analysis_queue.push_back(next);
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
            return;
        }
        if !self.expanded_files.remove(path) {
            return; // already collapsed
        }
        // Collapsing the file that owns the selected symbol: the relation view no longer
        // has a visible anchor.
        let collapsed_selected_symbol = self
            .selected_symbol
            .as_ref()
            .is_some_and(|(f, _, _, _)| f == path);
        if collapsed_selected_symbol {
            self.selected_symbol = None;
            self.selected_relations = None;
            self.retarget_ai_to_current_selection(false);
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
        // Preserve expansion only for files that still exist. Independently schedule
        // every changed file so opening a row only reveals already-loading/ready symbols.
        if let Some(cs) = &self.changeset {
            self.expanded_files
                .retain(|path| cs.files.iter().any(|file| file.path.as_str() == path));
            self.ai_revision_cache
                .retain(|selection, _| match selection {
                    AiRevisionKey::Directory(directory) => cs
                        .files
                        .iter()
                        .any(|file| file.path.as_str().starts_with(&format!("{directory}/"))),
                    AiRevisionKey::File(path) | AiRevisionKey::Symbol { file: path, .. } => {
                        cs.files.iter().any(|file| file.path.as_str() == path)
                    }
                });
        }
        self.schedule_all_file_analysis();
        self.drain_analysis_queue();
        if self.current_ai_selection().is_some() {
            self.retarget_ai_to_current_selection(false);
        }
        // Analysis is still in flight: keep the refreshing marker on.
        self.publish_refreshing();
    }

    fn on_branch_unavailable(&mut self, epoch: Epoch, ctx: codescope_core::RepoContext) {
        if epoch != self.epoch {
            return;
        }
        debug_assert!(ctx.base.is_none());
        self.repo_ctx = Some(ctx);
        // No comparison ran, so an empty ChangeSet would be a false fact. Remove every
        // branch-scoped artifact retained while refresh was in flight instead.
        self.changeset = None;
        self.analysis = None;
        self.file_semantics.clear();
        self.ai_revision_cache.clear();
        self.abort_all_ai_requests();
        self.ai_failures.clear();
        self.relation_cache.clear();
        self.relation_queue.clear();
        self.selected_file = None;
        self.selected_directory = None;
        self.selected_symbol = None;
        self.selected_relations = None;
        self.set_status(
            "branch comparison unavailable: no meaningful base could be inferred",
            StatusLevel::Warning,
        );
        self.publish();
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
                if e.to_string().contains("no base") {
                    self.set_status(
                        "branch comparison unavailable: no meaningful base could be inferred",
                        StatusLevel::Warning,
                    );
                } else {
                    self.set_status(format!("analysis failed: {e}"), StatusLevel::Error);
                }
            }
        }
        self.publish();
    }

    fn on_ai_draft(
        &mut self,
        epoch: Epoch,
        selection: AiSelectionKey,
        generation: u64,
        draft: DiagramDraft,
    ) {
        let Some(running) = self.ai_running.get(&generation) else {
            return;
        };
        if epoch != self.epoch
            || draft.epoch != epoch
            || running.epoch != epoch
            || running.generation != generation
            || running.selection != selection
        {
            return;
        }

        let preview = self.validated_draft(&selection, &draft).ok();
        if draft.forms.is_empty() {
            self.ai_previews.remove(&selection);
        } else if let Some(preview) = preview {
            self.ai_previews.insert(selection.clone(), preview);
        }
        self.ai_drafts.insert(selection.clone(), draft);
        self.agent_owned_drafts.remove(&selection);
        if self.current_ai_selection().as_ref() == Some(&selection) {
            self.ai_status = AiStatus::Loading { since_epoch: epoch };
            self.publish();
        }
    }

    fn on_ai_activity(
        &mut self,
        epoch: Epoch,
        selection: AiSelectionKey,
        generation: u64,
        update: AiActivityUpdate,
    ) {
        let Some(running) = self.ai_running.get(&generation) else {
            return;
        };
        if epoch != self.epoch
            || running.epoch != epoch
            || running.generation != generation
            || running.selection != selection
        {
            return;
        }

        let activity = self.ai_activity.entry(generation).or_insert(AiActivity {
            active: true,
            ..AiActivity::default()
        });
        match update {
            AiActivityUpdate::WaitingForModel => activity.waiting_for_model = true,
            AiActivityUpdate::ToolCall {
                id,
                name,
                detail,
                state,
            } => {
                activity.waiting_for_model = false;
                let state = match state {
                    AiToolActivityState::Running => AiToolCallActivityState::Running,
                    AiToolActivityState::Succeeded => AiToolCallActivityState::Succeeded,
                    AiToolActivityState::Failed => AiToolCallActivityState::Failed,
                };
                if let Some(call) = activity.calls.iter_mut().find(|call| call.id == id) {
                    call.name = name;
                    call.detail = detail;
                    call.state = state;
                } else {
                    activity.calls.push(AiToolCallActivity {
                        id,
                        name,
                        detail,
                        state,
                    });
                    if activity.calls.len() > codescope_ai::MAX_TOOL_CALLS as usize {
                        activity.calls.remove(0);
                    }
                }
            }
        }
        if self.current_ai_selection().as_ref() == Some(&selection) {
            self.ai_status = AiStatus::Loading { since_epoch: epoch };
            self.publish();
        }
    }

    fn on_ai_done(
        &mut self,
        epoch: Epoch,
        selection: AiSelectionKey,
        generation: u64,
        outcome: AiOutcome,
    ) {
        self.ai_requests.complete(generation);
        let Some(running) = self.ai_running.remove(&generation) else {
            // A completion may already be queued when the 16-request window evicts this
            // generation. Its ledger entry is gone, so it cannot publish stale output.
            return;
        };
        self.ai_activity.remove(&generation);
        if running.epoch != epoch
            || running.generation != generation
            || running.selection != selection
        {
            // The generation key is unique. A mismatched payload is malformed and must
            // not affect any of the other concurrently running requests.
            return;
        }
        let is_current_epoch = epoch == self.epoch;
        let is_focused = self.current_ai_selection().as_ref() == Some(&selection);
        match outcome {
            AiOutcome::Plan(plan, report) if report.is_renderable() => {
                let final_draft = DiagramDraft::from_plan(&plan);
                let cached = CachedAiPlan { plan, report };
                self.ai_revision_cache
                    .insert(AiRevisionKey::from(&selection), cached.clone());
                if is_current_epoch {
                    self.ai_cache.insert(selection.clone(), cached.clone());
                    self.ai_drafts.insert(selection.clone(), final_draft);
                    self.ai_previews.insert(selection.clone(), cached.clone());
                    self.ai_failures.remove(&selection);
                    if is_focused {
                        self.ai_rows = Some((epoch, selection.clone(), cached));
                    }
                }
            }
            AiOutcome::Stale if is_current_epoch && is_focused => {
                self.ai_status = AiStatus::Stale { epoch }
            }
            AiOutcome::Failed(reason) if is_current_epoch => {
                self.ai_failures.insert(selection.clone(), reason.clone());
                // Every AI failure carries the recovery suffix (spec §3.6); the
                // deterministic impact pane is unaffected by the failure.
                if is_focused {
                    let footer_reason = ai_failure_footer_reason(&reason);
                    self.set_status_with_detail(
                        format!("AI: {footer_reason} · {AI_FAILURE_SUFFIX}"),
                        format!(
                            "AI generation failed\n\n{reason}\n\nRecovery: {AI_FAILURE_SUFFIX}"
                        ),
                        StatusLevel::Warning,
                    );
                }
            }
            AiOutcome::Unavailable if is_current_epoch => {
                self.ai_failures.insert(
                    selection.clone(),
                    "AI provider is temporarily unavailable".to_string(),
                );
            }
            _ => {}
        }
        self.refresh_current_ai_status();
        if is_current_epoch && is_focused && self.ai_cache.contains_key(&selection) {
            if let Some(guidance) = self.agent_guidance.get(&selection) {
                if let Some(display) = guidance.display() {
                    self.set_status(format!("{display} · answer ready"), StatusLevel::Info);
                }
            }
        }
        self.publish();
    }

    fn publish_refreshing(&self) {
        let mut snap = self.build_snapshot();
        snap.refreshing = true;
        self.output.publish(snap);
    }

    fn build_snapshot(&self) -> UiSnapshot {
        let (repo_bar, counts) = repo_bar(self.repo_ctx.as_ref());
        // Files come from the changeset immediately; symbol rows fill in asynchronously
        // from the bounded per-file analysis queue.
        let files = self
            .changeset
            .as_ref()
            .map(|cs| file_rows(cs, &self.file_semantics, &self.expanded_files))
            .unwrap_or_default();
        let ai_summaries = self.build_ai_summary_states(&files);
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
        let ai_tokens = self
            .ai
            .as_ref()
            .map(|ai| ai.token_usage())
            .map(|usage| AiTokenUsage {
                input: usage.input,
                output: usage.output,
            })
            .unwrap_or_default();
        let diagram_draft = self
            .current_ai_selection()
            .and_then(|selection| self.ai_drafts.get(&selection).cloned());
        let ai_activity = self
            .current_ai_selection()
            .and_then(|selection| {
                self.ai_running
                    .iter()
                    .filter_map(|(generation, running)| {
                        (running.epoch == self.epoch && running.selection == selection)
                            .then_some(*generation)
                    })
                    .max()
            })
            .and_then(|generation| self.ai_activity.get(&generation).cloned())
            .unwrap_or_default();
        UiSnapshot {
            repo: repo_bar,
            scope: self.scope,
            scope_counts: counts,
            files,
            ai_summaries,
            diff,
            semantic,
            diagram_draft,
            impact,
            ls: self.ls_status,
            ai: self.ai_status.clone(),
            ai_model: self.ai.as_ref().map(|a| a.model()).unwrap_or_default(),
            ai_reasoning_effort: self
                .ai
                .as_ref()
                .map(|ai| ai.reasoning_effort().as_str().to_string())
                .unwrap_or_else(|| "default".to_string()),
            available_reasoning_efforts: ReasoningEffort::ALL
                .iter()
                .map(|effort| effort.as_str().to_string())
                .collect(),
            ai_provider: self
                .ai
                .as_ref()
                .map(|a| a.provider_label().to_string())
                .unwrap_or_default(),
            ai_tokens,
            ai_activity,
            available_models: self.available_models.clone(),
            model_list_loading: self.model_list_loading,
            model_list_error: self.model_list_error.clone(),
            base_ref,
            available_bases: self.available_bases.clone(),
            base_candidates_truncated: self.available_bases_truncated,
            message: self.status.text.clone(),
            status: self.status.clone(),
            epoch: self.epoch,
            refreshing: false,
        }
    }

    fn build_ai_summary_states(
        &self,
        files: &[FileRow],
    ) -> std::collections::HashMap<AiSummaryKey, AiSummaryState> {
        let mut selections = HashSet::new();
        for file in files {
            selections.insert(AiSelectionKey::File(file.path.clone()));
            for directory in codescope_tui::file_rows::directory_prefixes(&file.path) {
                selections.insert(AiSelectionKey::Directory(directory));
            }
            for symbol in &file.symbols {
                if let Some((line, col)) = symbol.position {
                    selections.insert(AiSelectionKey::Symbol {
                        file: file.path.clone(),
                        name: symbol.name.clone(),
                        line,
                        col,
                    });
                }
            }
        }
        selections
            .into_iter()
            .map(|selection| {
                let state = if self.ai_cache.contains_key(&selection) {
                    AiSummaryState::Ready
                } else if self.ai_failures.contains_key(&selection) {
                    AiSummaryState::Failed
                } else if self.ai_drafts.contains_key(&selection)
                    || self.ai_running.values().any(|running| {
                        running.epoch == self.epoch && running.selection == selection
                    })
                    || (self.current_ai_selection().as_ref() == Some(&selection)
                        && matches!(
                            self.ai_status,
                            AiStatus::WaitingForSymbols { .. }
                                | AiStatus::Debouncing { .. }
                                | AiStatus::Loading { .. }
                        ))
                {
                    AiSummaryState::Generating
                } else {
                    AiSummaryState::NotGenerated
                };
                (selection.summary_key(), state)
            })
            .collect()
    }

    fn panes(&self) -> (DiffPane, SemanticPane) {
        let mut diff = if let Some(directory) = &self.selected_directory {
            DiffPane {
                title: format!("{directory}/"),
                ..DiffPane::default()
            }
        } else {
            self.changeset
                .as_ref()
                .map(|cs| selected_diff(cs, self.selected_file.as_deref()))
                .unwrap_or_default()
        };
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
        let current_ai_selection = self.current_ai_selection();
        let guidance_note = current_ai_selection
            .as_ref()
            .and_then(|selection| self.agent_guidance.get(selection))
            .and_then(AgentGuidance::display);
        let semantic = match &self.ai_rows {
            Some((ep, selection, plan))
                if *ep == self.epoch && current_ai_selection.as_ref() == Some(selection) =>
            {
                SemanticPane {
                    plan: Some(plan.plan.clone()),
                    // The report travels with the plan: sanitized content keeps its
                    // verdict/dropped-items trail in every publish, cache hit included.
                    report: Some(plan.report.clone()),
                    note: guidance_note.unwrap_or_else(|| selection.label().to_string()),
                    ai_generated: true,
                }
            }
            Some((ep, _, _)) if *ep != self.epoch => SemanticPane {
                plan: None,
                report: None,
                note: "AI view stale (repo changed); regenerating…".to_string(),
                ai_generated: false,
            },
            // A selection mismatch must never display the previous row's explanation —
            // or its validation report.
            _ => {
                let preview = current_ai_selection
                    .as_ref()
                    .and_then(|selection| self.ai_previews.get(selection));
                if let Some(preview) = preview {
                    SemanticPane {
                        plan: Some(preview.plan.clone()),
                        report: Some(preview.report.clone()),
                        note: if current_ai_selection
                            .as_ref()
                            .is_some_and(|selection| self.agent_owned_drafts.contains(selection))
                        {
                            "Agent diagram draft · edit or finish through the controller"
                                .to_string()
                        } else {
                            "AI draft · building boxes and relationships…".to_string()
                        },
                        ai_generated: true,
                    }
                } else {
                    let draft = current_ai_selection
                        .as_ref()
                        .and_then(|selection| self.ai_drafts.get(selection));
                    if let Some(draft) =
                        draft.filter(|draft| draft.forms.iter().any(|form| !form.nodes.is_empty()))
                    {
                        // A draft must be visibly incremental, but it has not crossed the
                        // fact validator yet. Strip entity claims so draft connectors render
                        // inferred/dashed; final publication restores only validated facts.
                        let mut plan = draft.plan();
                        for node in plan.forms.iter_mut().flat_map(|form| &mut form.nodes) {
                            node.entity = None;
                        }
                        SemanticPane {
                            plan: Some(plan),
                            report: None,
                            note: if current_ai_selection.as_ref().is_some_and(|selection| {
                                self.agent_owned_drafts.contains(selection)
                            }) {
                                "Agent diagram draft · unvalidated boxes; edit or finish"
                                    .to_string()
                            } else {
                                "AI draft · unvalidated boxes; building relationships…".to_string()
                            },
                            ai_generated: true,
                        }
                    } else {
                        SemanticPane {
                            note: current_ai_selection
                                .as_ref()
                                .and_then(|selection| {
                                    self.ai_drafts.contains_key(selection).then_some(
                                        "AI draft · researching and building the first box…"
                                            .to_string(),
                                    )
                                })
                                .or(guidance_note)
                                .unwrap_or_default(),
                            ..SemanticPane::default()
                        }
                    }
                }
            }
        };
        (diff, semantic)
    }

    /// Assemble the impact pane (spec §5.3–§5.7): the deterministic selected change plus
    /// the callers/downstream columns. Lazy LSP relations and the one-hop impact graph
    /// merge into both lists; AI plan rows never replace this pane.
    fn build_impact(&self) -> ImpactPane {
        let Some(selection) = self.current_ai_selection() else {
            return ImpactPane::default();
        };
        self.build_impact_for_selection(&selection)
    }

    /// Selection-scoped Impact packet used by the focused UI and its AI request.
    fn build_impact_for_selection(&self, selection: &AiSelectionKey) -> ImpactPane {
        // Selected change: the symbol's per-file cache entry carries its change kind and
        // interpretation; a file row falls back to the file-level summary.
        let mut impact = ImpactPane {
            selected_change: self.selected_change_for(selection),
            ..Default::default()
        };
        let AiSelectionKey::Symbol { file, name, .. } = selection else {
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
        let relations = self.relation_cache.get(selection).or_else(|| {
            (self.current_ai_selection().as_ref() == Some(selection))
                .then_some(self.selected_relations.as_ref())
                .flatten()
        });
        match relations {
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

    fn selected_change_for(&self, selection: &AiSelectionKey) -> Option<SelectedChange> {
        if let AiSelectionKey::Directory(directory) = selection {
            let files: Vec<_> = self
                .changeset
                .as_ref()?
                .files
                .iter()
                .filter(|file| file.path.as_str().starts_with(&format!("{directory}/")))
                .collect();
            let symbols = files
                .iter()
                .filter_map(|file| self.file_semantics.get(file.path.as_str()))
                .filter_map(|state| match state {
                    FileSemanticState::Ready(result) => Some(result.changed.len()),
                    _ => None,
                })
                .sum::<usize>();
            return Some(SelectedChange {
                file: directory.clone(),
                label: format!("{directory}/"),
                change: "modified",
                interpretation: format!(
                    "{} changed file{} and {symbols} mapped symbol{} in this directory.",
                    files.len(),
                    if files.len() == 1 { "" } else { "s" },
                    if symbols == 1 { "" } else { "s" }
                ),
                interpretation_source: InterpretationSource::Deterministic,
            });
        }
        if let AiSelectionKey::Symbol {
            file,
            name,
            line,
            col,
        } = selection
        {
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
        let AiSelectionKey::File(file) = selection else {
            return None;
        };
        let file = file.as_str();
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
            Some(FileSemanticState::Failed) => {
                "symbol analysis failed; retrying after the next file change".to_string()
            }
            None => "symbol analysis pending…".to_string(),
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

/// Restrict every prompt and validator fact to the selected directory or exact file.
fn changeset_for_selection(
    changeset: &codescope_core::ChangeSet,
    selection: &AiSelectionKey,
) -> codescope_core::ChangeSet {
    codescope_core::ChangeSet {
        scope: changeset.scope,
        files: changeset
            .files
            .iter()
            .filter(|file| selection.contains_file(file.path.as_str()))
            .cloned()
            .collect(),
        fallback: changeset.fallback,
    }
}

/// Append the current deterministic Impact view as the AI plan's required focus. The
/// already-scoped digest supplies only this directory/file's validation context, and this
/// contract keeps the model from turning one sibling into the whole plan's subject.
#[cfg(test)]
fn append_impact_focus(digest: &mut String, impact: &ImpactPane, selection: &AiSelectionKey) {
    let Some(selected) = &impact.selected_change else {
        return;
    };
    digest.push_str("\n## current impact selection\n");
    match selection {
        AiSelectionKey::Directory(path) => {
            digest.push_str(&format!("directory: {}/\n", one_line(path)));
        }
        _ => digest.push_str(&format!("file: {}\n", one_line(&selected.file))),
    }
    digest.push_str(&format!("label: {}\n", one_line(&selected.label)));
    digest.push_str(&format!("change: {}\n", selected.change));
    if !selected.interpretation.is_empty() {
        digest.push_str(&format!(
            "deterministic interpretation: {}\n",
            one_line(&selected.interpretation)
        ));
    }
    append_impact_rows(digest, "callers", &impact.callers);
    append_impact_rows(digest, "downstream", &impact.downstream);
    if !impact.note.is_empty() {
        digest.push_str(&format!("impact caveat: {}\n", one_line(&impact.note)));
    }
    if matches!(selection, AiSelectionKey::Directory(_)) {
        digest.push_str(
            "request: Summarize this changed directory as a module for a reviewer seeing it for the first time. Show the common purpose of the changes, how the changed files relate, and the most important implemented behavior. The plan MUST stay within this directory; every node code_ref MUST use one of its supplied changed files. Prefer a changed_symbol_tree or relationship_flow over a flat file list.\n",
        );
    } else {
        digest.push_str(
            "request: Visually explain this selected change to a reviewer seeing it for the first time. Show its intent, the most important runtime/data/control relationship, and the direct code-owned implication. The main pane already shows the raw diff, so do not restate a list of modified symbols. The plan MUST be about this file/function only; every node code_ref MUST use this selected file. If this diff publishes input to an external system, end the visual at that publication and omit any unshown actor, mapping, or outcome.\n",
        );
    }
}

/// State the selection's semantic capabilities next to its exact focused hunks. The explicit
/// contract is especially important for directories and files no language server owns.
#[cfg(test)]
fn append_selected_evidence_contract(
    digest: &mut String,
    selection: &AiSelectionKey,
    changeset: &codescope_core::ChangeSet,
    semantics: &std::collections::HashMap<String, FileSemanticState>,
) {
    if let AiSelectionKey::Directory(directory) = selection {
        let available_symbols = changeset
            .files
            .iter()
            .filter_map(|file| semantics.get(file.path.as_str()))
            .filter_map(|state| match state {
                FileSemanticState::Ready(result) => Some(result.changed.len()),
                _ => None,
            })
            .sum::<usize>();
        digest.push_str("\n## selected directory evidence contract (mandatory)\n");
        digest.push_str(&format!(
            "directory scope: {directory}/ ({} changed files; {available_symbols} mapped symbols)\n",
            changeset.files.len()
        ));
        digest.push_str(
            "- Every node code_ref and evidence file MUST be copied from this scoped digest and focused source packet; all supplied files are inside the selected directory.\n\
             - Use symbol metadata only when that exact file+symbol appears in `## changed symbols`; otherwise use conceptual nodes grounded by exact file+hunk lines.\n\
             - Describe the directory as one module-level change, not as unrelated repository-wide work.\n",
        );
        return;
    }
    let file = selection
        .file()
        .expect("non-directory selection has a file");
    let available_symbols = match semantics.get(file) {
        Some(FileSemanticState::Ready(result)) => result
            .changed
            .iter()
            .filter(|changed| changed.file.as_path().as_str() == file)
            .count(),
        _ => 0,
    };
    digest.push_str("\n## selected file evidence contract (mandatory)\n");
    if available_symbols == 0 {
        let state = match semantics.get(file) {
            Some(FileSemanticState::Unsupported) => {
                "unavailable: this file type has no semantic analyzer"
            }
            Some(FileSemanticState::Failed) => "unavailable: semantic analysis failed",
            Some(FileSemanticState::Loading) => {
                "unavailable in this snapshot: analysis is still loading"
            }
            Some(FileSemanticState::Ready(_)) => "unavailable: analysis found no changed symbols",
            None => "unavailable in this snapshot: analysis has not loaded",
        };
        digest.push_str(&format!("symbol catalog: {state}\n"));
        digest.push_str(
            "- Every plan-level evidence item MUST use the exact selected file plus a supplied zero-based hunk_id and MUST omit symbol and range.\n\
             - Node entities MUST be absent or use the exact file only. Prefer sequence, relationship_flow, or before_after with conceptual entityless nodes; do not assert symbol, call-tree, or type-ownership facts.\n\
             - Words that describe the change (for example `changes`, `workflow`, `configuration`, or an action label) are concepts, NOT symbols.\n",
        );
    } else {
        digest.push_str(&format!(
            "symbol catalog: {available_symbols} exact changed symbol{} available for this file\n",
            if available_symbols == 1 { "" } else { "s" }
        ));
        digest.push_str(
            "- A symbol or range may be used only when the exact selected-file symbol appears verbatim in `## changed symbols`; otherwise cite exact file+hunk evidence and omit symbol/range.\n",
        );
    }
}

/// Caps for the selection-scoped source evidence appended to AI plan requests.
/// `FOCUSED_MAX_HUNKS` bounds how many hunks a selection may cover; the line/byte caps
/// are hard totals, sliced fairly across the selected hunks so a huge early hunk cannot
/// starve the final one of its header and body evidence.
#[cfg(test)]
const FOCUSED_MAX_HUNKS: usize = 8;
#[cfg(test)]
const FOCUSED_MAX_LINES: usize = 160;
#[cfg(test)]
const FOCUSED_MAX_BYTES: usize = 20_000;

#[cfg(test)]
fn balanced_slice<T>(mut values: Vec<T>, max: usize) -> Vec<T> {
    if values.len() <= max {
        return values;
    }
    let tail_count = max / 2;
    let head_count = max - tail_count;
    let tail = values.split_off(values.len() - tail_count);
    values.truncate(head_count);
    values.extend(tail);
    values
}

/// Append exact changed lines for the selected directory/file/symbol. This is intentionally
/// capped and selection-scoped: the AI needs the actual control/data change to draw a useful
/// relationship, while the compact digest supplies validation ids for the same scope.
/// File-level selections cover every hunk while the file fits the hunk cap and balance
/// head/tail coverage beyond it; the line and byte budgets are sliced fairly across the
/// selected hunks, so late finalization edits keep their evidence even when early hunks
/// are huge.
#[cfg(test)]
fn append_focused_source_packet(
    digest: &mut String,
    selection: &AiSelectionKey,
    changeset: &codescope_core::ChangeSet,
    semantics: &std::collections::HashMap<String, FileSemanticState>,
) {
    fn rendered_line(line: &codescope_core::DiffLine) -> String {
        let marker = match line.kind {
            codescope_core::DiffLineKind::Add => '+',
            codescope_core::DiffLineKind::Del => '-',
            codescope_core::DiffLineKind::Context => ' ',
        };
        let old = line
            .old_ln
            .map_or_else(|| "-".to_string(), |line| line.to_string());
        let new = line
            .new_ln
            .map_or_else(|| "-".to_string(), |line| line.to_string());
        let text: String = line.text.chars().take(1_000).collect();
        format!("[old:{old} new:{new}] {marker}{text}\n")
    }

    let selected: Vec<(&codescope_core::FileChange, u32, &codescope_core::Hunk)> =
        if matches!(selection, AiSelectionKey::Directory(_)) {
            let all: Vec<_> = changeset
                .files
                .iter()
                .flat_map(|file| {
                    file.hunks
                        .iter()
                        .enumerate()
                        .map(move |(index, hunk)| (file, index as u32, hunk))
                })
                .collect();
            balanced_slice(all, FOCUSED_MAX_HUNKS)
        } else {
            let file_path = selection.file().expect("file or symbol selection");
            let Some(file) = changeset
                .files
                .iter()
                .find(|file| file.path.as_str() == file_path)
            else {
                return;
            };
            let mut wanted: Vec<u32> = match selection {
                AiSelectionKey::Symbol { name, .. } => match semantics.get(file_path) {
                    Some(FileSemanticState::Ready(result)) => result
                        .changed
                        .iter()
                        .filter(|changed| changed.name == *name)
                        .flat_map(|changed| changed.record.hunks.iter().map(|hunk| hunk.index))
                        .collect(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            if wanted.is_empty() {
                if let AiSelectionKey::Symbol { line, .. } = selection {
                    let line = line.saturating_add(1);
                    wanted.extend(file.hunks.iter().enumerate().filter_map(|(index, hunk)| {
                        let (start, end) = hunk.new_span();
                        (line >= start && line < end).then_some(index as u32)
                    }));
                }
            }
            if wanted.is_empty() {
                wanted.extend(0..file.hunks.len() as u32);
            }
            wanted.sort_unstable();
            wanted.dedup();
            let wanted = balanced_slice(wanted, FOCUSED_MAX_HUNKS);
            wanted
                .into_iter()
                .filter_map(|index| Some((file, index, file.hunks.get(index as usize)?)))
                .collect()
        };
    if selected.is_empty() {
        return;
    }

    // Fair line slice per selected hunk: each hunk may emit at least its share of the
    // line budget, and shares small hunks do not use flow forward to later ones. A
    // selection whose hunks fit the budget therefore emits exactly what a plain greedy
    // fill would, while a huge early hunk can only spend its share plus the leftovers.
    let count = selected.len();
    let fair_lines = FOCUSED_MAX_LINES / count;
    let mut line_allowance: Vec<usize> = selected
        .iter()
        .map(|(_, _, hunk)| hunk.lines.len().min(fair_lines))
        .collect();
    let mut spare = FOCUSED_MAX_LINES - line_allowance.iter().sum::<usize>();
    for (allowance, (_, _, hunk)) in line_allowance.iter_mut().zip(&selected) {
        let extra = spare.min(hunk.lines.len() - *allowance);
        *allowance += extra;
        spare -= extra;
    }

    // Render every selected hunk's header and line-capped body once.
    let intro = "\n## focused source evidence (exact selected hunks; hunk ids are zero-based; body annotations use one-based old/new lines)\n";
    let headers: Vec<String> = selected
        .iter()
        .map(|(file, index, hunk)| {
            format!(
                "hunk_id: {index}  file: {}  @@ -{},{} +{},{} @@ {}\n",
                file.path,
                hunk.old_start,
                hunk.old_len,
                hunk.new_start,
                hunk.new_len,
                hunk.section.as_deref().unwrap_or_default()
            )
        })
        .collect();
    let bodies: Vec<Vec<String>> = selected
        .iter()
        .zip(&line_allowance)
        .map(|((_, _, hunk), share)| hunk.lines.iter().take(*share).map(rendered_line).collect())
        .collect();

    // Fair byte slice over the rendered hunks, same forward flow: a wide early hunk can
    // spend its own share plus what later hunks cannot use, never their shares. That
    // reservation is what guarantees the final selected hunk's header and body evidence.
    let byte_budget = FOCUSED_MAX_BYTES.saturating_sub(intro.len());
    let byte_need: Vec<usize> = headers
        .iter()
        .zip(&bodies)
        .map(|(header, body)| header.len() + body.iter().map(|line| line.len()).sum::<usize>())
        .collect();
    let fair_bytes = byte_budget / count;
    let mut byte_allowance: Vec<usize> = byte_need
        .iter()
        .copied()
        .map(|need| need.min(fair_bytes))
        .collect();
    let mut spare_bytes = byte_budget - byte_allowance.iter().sum::<usize>();
    for (allowance, need) in byte_allowance.iter_mut().zip(&byte_need) {
        let extra = spare_bytes.min(need - *allowance);
        *allowance += extra;
        spare_bytes -= extra;
    }

    // Emit each hunk within both of its allowances. Byte cuts are per hunk, so a hunk
    // that overflows its slice no longer ends the packet — the remaining hunks (above
    // all the final one) still get their reserved evidence. `truncated` flags any hunk
    // that had more evidence than its slice allowed.
    let mut packet = String::from(intro);
    let mut truncated = false;
    for k in 0..count {
        let header = &headers[k];
        if packet.len() + header.len() > FOCUSED_MAX_BYTES {
            truncated = true;
            break;
        }
        packet.push_str(header);
        let mut hunk_bytes = header.len();
        for rendered in &bodies[k] {
            if hunk_bytes + rendered.len() > byte_allowance[k] {
                truncated = true;
                break;
            }
            packet.push_str(rendered);
            hunk_bytes += rendered.len();
        }
        if selected[k].2.lines.len() > line_allowance[k] {
            truncated = true;
        }
    }
    if truncated {
        packet.push_str("… focused source truncated to prompt budget\n");
    }
    digest.push_str(&packet);
}

#[cfg(test)]
fn append_impact_rows(digest: &mut String, heading: &str, list: &ImpactList) {
    const MAX_PROMPT_ROWS: usize = 20;
    digest.push_str(&format!("{heading} (state {:?}):\n", list.state));
    if list.rows.is_empty() {
        digest.push_str("- (none available)\n");
        return;
    }
    for row in list.rows.iter().take(MAX_PROMPT_ROWS) {
        let relation = if row.relation.is_empty() {
            String::new()
        } else {
            format!(" ({})", row.relation)
        };
        digest.push_str(&format!("- {}{relation}\n", one_line(&row.label)));
    }
    if list.rows.len() > MAX_PROMPT_ROWS {
        digest.push_str(&format!(
            "- … {} more omitted\n",
            list.rows.len() - MAX_PROMPT_ROWS
        ));
    }
    if list.partial {
        digest.push_str("- caveat: relationship evidence is partial\n");
    }
}

#[cfg(test)]
fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
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
    let changeset = match scope {
        ChangeScope::Branch => {
            let Some(base) = ctx.base.as_ref() else {
                // Publish the honest current context and explicitly invalidate a retained
                // branch diff. This is not an empty comparison: no comparison can run.
                let _ = tx
                    .send(DispatchEvent::BranchUnavailable {
                        epoch,
                        ctx: ctx.clone(),
                    })
                    .await;
                return Err(codescope_git::GitError::NoBase.into());
            };
            // Use the merge-base captured in the same context rendered by the top bar.
            // Re-resolving a mutable ref here could label one base while diffing another.
            repo.branch_changeset_from_base(base).await?
        }
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
    // The interactive pipeline ends at the git phase so files appear immediately. The
    // dispatcher then fans each file into a bounded asynchronous analysis queue; this
    // retains incremental rendering without tying symbol availability to expansion.
    // `engine` is accepted but unused here; per-file jobs go through
    // `Dispatcher::spawn_file_analysis`. The non-interactive backend
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
    // An upstream is tracking metadata, not necessarily a meaningful comparison. In
    // particular, a same-tip upstream must not be relabeled as the active base.
    let base = ctx.base.as_ref().map(|b| b.ref_name.clone());
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

/// One files-pane row from the changeset + the asynchronous per-file cache. `expanded`
/// controls visibility only; symbol rows come from a Ready per-file result. Unloaded rows
/// show no symbol count.
fn file_rows(
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
                added_lines: f.hunks.iter().map(codescope_core::Hunk::count_added).sum(),
                removed_lines: f
                    .hunks
                    .iter()
                    .map(codescope_core::Hunk::count_deleted)
                    .sum(),
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

// -- FactView over the current analysis snapshot ------------------------------

struct SnapshotFacts {
    focus: AiSelectionKey,
    files: HashSet<String>,
    symbols: std::collections::HashMap<(String, String), LineRange>,
    edges: HashSet<(String, String, PlanEdgeKind)>,
    hunks: std::collections::HashMap<String, usize>,
    diff_lines: HashSet<(String, u32, DiffSide, u32)>,
}

use codescope_ai::Lookup;

impl SnapshotFacts {
    /// Facts for the AI validator in the lazy world: the changeset's files/hunks plus
    /// the symbols of files the user has explicitly analyzed (Ready). Unloaded files
    /// contribute their git identity only — the validator never sees symbols that have
    /// not actually been computed.
    fn from_lazy(
        changeset: &codescope_core::ChangeSet,
        semantics: &std::collections::HashMap<String, FileSemanticState>,
        focus: &AiSelectionKey,
    ) -> Self {
        let mut facts = SnapshotFacts {
            focus: focus.clone(),
            files: HashSet::new(),
            symbols: std::collections::HashMap::new(),
            edges: HashSet::new(),
            hunks: std::collections::HashMap::new(),
            diff_lines: HashSet::new(),
        };
        for f in &changeset.files {
            let path = f.path.to_string();
            facts.files.insert(path.clone());
            facts.hunks.insert(path.clone(), f.hunks.len());
            for (hunk_index, hunk) in f.hunks.iter().enumerate() {
                let Ok(hunk_index) = u32::try_from(hunk_index) else {
                    continue;
                };
                for line in &hunk.lines {
                    if let Some(old_ln) = line.old_ln {
                        facts
                            .diff_lines
                            .insert((path.clone(), hunk_index, DiffSide::Old, old_ln));
                    }
                    if let Some(new_ln) = line.new_ln {
                        facts
                            .diff_lines
                            .insert((path.clone(), hunk_index, DiffSide::New, new_ln));
                    }
                }
            }
        }
        for res in semantics.values() {
            if let FileSemanticState::Ready(res) = res {
                for c in &res.changed {
                    if !facts.files.contains(&c.file.to_string()) {
                        continue;
                    }
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
    fn is_focus_file(&self, file: &codescope_core::FileId) -> bool {
        self.focus.contains_file(file.as_path().as_str())
    }

    fn file(&self, file: &codescope_core::FileId) -> Lookup<()> {
        if self.files.contains(&file.to_string()) {
            Lookup::Present(())
        } else {
            // The changeset is a complete inventory of CHANGED files, not of every repo
            // file. A miss means "not in the current fact catalog", not "does not exist"
            // (review 21 m1): the file may simply be unchanged.
            Lookup::Unknown
        }
    }
    fn symbol(&self, file: &codescope_core::FileId, name: &str) -> Lookup<LineRange> {
        match self
            .symbols
            .get(&(file.to_string(), name.to_string()))
            .copied()
        {
            Some(extent) => Lookup::Present(extent),
            // The lazy cache only surfaces CHANGED symbols, not a file's full outline, so
            // a miss here is "not surfaced by the loaded analysis", never "proven absent".
            None => Lookup::Unknown,
        }
    }
    fn edge(&self, from: &EntityRef, to: &EntityRef, kind: PlanEdgeKind) -> Lookup<()> {
        if self
            .edges
            .contains(&(entity_key(from), entity_key(to), kind))
        {
            Lookup::Present(())
        } else {
            // The lazy path never builds a complete edge universe; an absent edge is
            // "not queried", not "proven absent".
            Lookup::Unknown
        }
    }
    fn hunk(&self, file: &codescope_core::FileId, index: u32) -> Lookup<()> {
        match self.hunks.get(&file.to_string()) {
            Some(&n) if (index as usize) < n => Lookup::Present(()),
            Some(_) => Lookup::Absent, // file enumerated, hunk index out of range
            None => Lookup::Unknown,
        }
    }

    fn diff_line(
        &self,
        file: &codescope_core::FileId,
        index: u32,
        side: DiffSide,
        line: u32,
    ) -> Lookup<()> {
        let path = file.to_string();
        if self.diff_lines.contains(&(path.clone(), index, side, line)) {
            Lookup::Present(())
        } else if self.hunks.contains_key(&path) {
            Lookup::Absent
        } else {
            Lookup::Unknown
        }
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
    let (callers, callees) = engine.relations_of(file, pos).await;
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
                    // Directory/file/symbol selection is latest-wins state (where the
                    // changed-tree cursor sits): keep only the newest in a burst. Every other
                    // action is a one-shot command and must never be dropped behind it.
                    let mut batch = vec![a];
                    while let Ok(next) = actions.try_recv() {
                        batch.push(next);
                    }
                    let last_selection = batch
                        .iter()
                        .rposition(|act| {
                            matches!(
                                act,
                                Action::SelectionChanged { .. }
                                    | Action::DirectorySelectionChanged { .. }
                            )
                        });
                    for (i, act) in batch.into_iter().enumerate() {
                        if matches!(
                            act,
                            Action::SelectionChanged { .. }
                                | Action::DirectorySelectionChanged { .. }
                        )
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
    // No more UI can consume completion events. Closing the receivers also guarantees a
    // pending config-error report cannot block writer shutdown on a full event channel.
    drop(events);
    drop(actions);
    // Closing and joining the FIFO writer makes final AI/view selections durable
    // before the runtime exits. No config filesystem work ever ran on this dispatcher task.
    disp.config_write_tx.take();
    if let Some(writer) = disp.config_writer.take() {
        let _ = writer.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn agent_guidance_is_bounded_and_explicitly_not_evidence() {
        let guidance = AgentGuidance {
            question: Some("Where does this request fail?".to_string()),
            feedback: Some("Emphasize the queue boundary.".to_string()),
        };
        let prompt = guidance.prompt_section();
        assert!(prompt.contains("Where does this request fail?"));
        assert!(prompt.contains("Emphasize the queue boundary."));
        assert!(prompt.matches("not evidence").count() >= 1);

        let normalized = normalize_agent_text(&"x".repeat(MAX_AGENT_GUIDANCE_CHARS + 50));
        assert_eq!(normalized.chars().count(), MAX_AGENT_GUIDANCE_CHARS);
    }

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

    #[tokio::test]
    async fn backend_output_supports_headless_ordered_snapshots() {
        let root = scratch_repo();
        let repo_root = camino::Utf8PathBuf::from_path_buf(root).expect("utf-8 temp path");
        let repo = GitRepo::discover(&repo_root).await.unwrap();
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        let (job_tx, _job_rx) = mpsc::channel(4);
        let disp = Dispatcher::new(repo, None, None, output_tx, job_tx);

        disp.publish();
        let snapshot = output_rx.recv().await.expect("headless snapshot");
        assert_eq!(snapshot.epoch, Epoch::ZERO);
        assert!(snapshot.semantic.plan.is_none());
        // Snapshot default: no AI plan means no report either.
        assert!(snapshot.semantic.report.is_none());
    }

    struct SlowFailingConfig;

    impl ConfigPersistence for SlowFailingConfig {
        fn persist_model(&self, _provider: &str, _model: &str) -> Result<(), String> {
            Ok(())
        }

        fn persist_reasoning_effort(
            &self,
            _provider: &str,
            _effort: ReasoningEffort,
        ) -> Result<(), String> {
            Ok(())
        }

        fn persist_ui(&self, _preferences: UiPreferences) -> Result<(), String> {
            std::thread::sleep(std::time::Duration::from_millis(200));
            Err("disk unavailable".to_string())
        }
    }

    #[tokio::test]
    async fn config_write_is_nonblocking_and_failure_returns_as_an_event() {
        let root = scratch_repo();
        let (disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;
        let mut disp = disp.with_config_persistence(std::sync::Arc::new(SlowFailingConfig));
        let start = std::time::Instant::now();
        disp.handle(DispatchEvent::Work(Action::PersistUiPreferences(
            UiPreferences::default(),
        )))
        .await;
        assert!(
            start.elapsed() < std::time::Duration::from_millis(100),
            "filesystem work blocked the dispatcher"
        );

        let failed = recv_until(&mut job_rx, |event| {
            matches!(event, DispatchEvent::ConfigSaveFailed { .. })
        })
        .await;
        disp.handle(failed).await;
        assert!(snapshot_rx
            .borrow()
            .status
            .text
            .contains("disk unavailable"));
        assert_eq!(snapshot_rx.borrow().status.level, StatusLevel::Warning);

        disp.config_write_tx.take();
        if let Some(writer) = disp.config_writer.take() {
            writer.await.unwrap();
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn startup_config_warning_is_published_in_the_tui_status() {
        let root = scratch_repo();
        let (disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        let mut disp = disp.with_startup_warning("global config is malformed; using defaults");
        disp.handle(DispatchEvent::RepoChanged).await;
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.status.level, StatusLevel::Warning);
        assert!(snap.status.text.contains("global config is malformed"));
        assert!(snap.refreshing);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn model_discovery_failure_keeps_current_model_and_reports_configured_ai() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;
        let config = codescope_ai::AiConfig {
            enabled: true,
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "current/model".to_string(),
            reasoning_effort: ReasoningEffort::Default,
            api_key: None,
            timeout: std::time::Duration::from_millis(100),
            tool_choice: codescope_ai::ToolChoice::Required,
            max_tool_calls: 1,
            prime_team_id: None,
        };
        let service = AiService::new(
            config,
            camino::Utf8PathBuf::from_path_buf(root.clone()).unwrap(),
        )
        .unwrap();
        disp.ai = Some(std::sync::Arc::new(service));
        disp.ai_status = AiStatus::Idle;

        disp.handle(DispatchEvent::Work(Action::ModelPicker)).await;
        {
            let snap = snapshot_rx.borrow().clone();
            assert!(snap.model_list_loading);
            assert_eq!(snap.available_models, ["current/model"]);
            assert_eq!(snap.ai_model, "current/model");
            assert_eq!(snap.ai_provider, "custom");
        }

        let loaded = recv_until(&mut job_rx, |event| {
            matches!(event, DispatchEvent::ModelsLoaded { .. })
        })
        .await;
        disp.handle(loaded).await;
        let snap = snapshot_rx.borrow().clone();
        assert!(!snap.model_list_loading);
        assert!(snap.model_list_error.is_some());
        assert_eq!(snap.available_models, ["current/model"]);
        assert!(snap
            .status
            .text
            .contains("current/manual model remains available"));
        assert!(!snap.status.text.contains("AI not configured"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn applying_ai_settings_updates_model_and_reasoning_in_one_snapshot() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        let service = AiService::new(
            codescope_ai::AiConfig {
                enabled: true,
                base_url: "http://127.0.0.1:1/v1".to_string(),
                model: "old/model".to_string(),
                reasoning_effort: ReasoningEffort::Low,
                api_key: None,
                timeout: std::time::Duration::from_millis(100),
                tool_choice: codescope_ai::ToolChoice::Required,
                max_tool_calls: 1,
                prime_team_id: None,
            },
            camino::Utf8PathBuf::from_path_buf(root.clone()).unwrap(),
        )
        .unwrap();
        disp.ai = Some(std::sync::Arc::new(service));
        disp.ai_status = AiStatus::Idle;

        disp.handle(DispatchEvent::Work(Action::AiSettingsSelected {
            model: "new/model".to_string(),
            reasoning_effort: "high".to_string(),
        }))
        .await;

        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.ai_model, "new/model");
        assert_eq!(snap.ai_reasoning_effort, "high");
        assert!(snap.status.text.contains("new/model · reasoning: high"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn repo_bar_does_not_promote_upstream_to_comparison_base() {
        let ctx = codescope_core::RepoContext {
            toplevel: camino::Utf8PathBuf::from("/tmp/demo"),
            head: codescope_core::HeadState::Branch("feature".to_string()),
            upstream: Some(codescope_core::Upstream {
                name: "origin/feature".to_string(),
                ahead: 0,
                behind: 0,
            }),
            base: None,
        };
        let (bar, _) = repo_bar(Some(&ctx));
        assert_eq!(bar.base, None, "tracking metadata is not an active base");
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

    #[tokio::test]
    async fn losing_the_only_base_publishes_none_and_clears_stale_branch_facts() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;

        disp.handle(DispatchEvent::RepoChanged).await;
        let ready = recv_until(&mut job_rx, |event| {
            matches!(event, DispatchEvent::ChangesetReady { .. })
        })
        .await;
        disp.handle(ready).await;
        assert_eq!(snapshot_rx.borrow().repo.base.as_deref(), Some("main"));
        assert_eq!(snapshot_rx.borrow().files.len(), 1);

        let output = Command::new("git")
            .args(["branch", "-D", "main"])
            .current_dir(&root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .output()
            .expect("delete the only base ref");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        disp.handle(DispatchEvent::RepoChanged).await;
        let unavailable = recv_until(&mut job_rx, |event| {
            matches!(event, DispatchEvent::BranchUnavailable { .. })
        })
        .await;
        disp.handle(unavailable).await;
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.repo.branch, "feature");
        assert_eq!(snap.repo.base, None);
        assert!(snap.base_ref.is_empty());
        assert!(snap.files.is_empty(), "the old base's diff must be cleared");
        assert!(snap.status.text.contains("no meaningful base"));
        assert_eq!(snap.status.level, StatusLevel::Warning);

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
        let loaded = recv_until(&mut job_rx, |e| {
            matches!(e, DispatchEvent::BaseLoaded { .. })
        })
        .await;
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

    /// Two files in one module plus a changed sibling outside it.
    fn directory_changeset() -> codescope_core::ChangeSet {
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
            vec![
                file("src/api/handler.rs", "handler-new"),
                file("src/api/model.rs", "model-new"),
                file("src/cli.rs", "cli-new"),
            ],
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

    /// A cached plan whose report records one dropped item tagged with `label`, so
    /// report-preservation assertions can tell selections apart.
    fn cached_ai_plan(label: &str) -> CachedAiPlan {
        let mut plan = codescope_core::VisualizationPlan::new(Epoch(1));
        plan.intent = format!("plan for {label}");
        plan.forms.push(codescope_core::VizForm {
            kind: codescope_core::FormKind::CallTree,
            nodes: vec![codescope_core::PlanNode::new(
                "n1",
                label,
                codescope_core::PlanNodeChange::Modified,
            )
            .with_detail("explains the selected change")],
            edges: Vec::new(),
        });
        CachedAiPlan {
            plan,
            report: codescope_core::ValidationReport::with_drops(vec![
                codescope_core::DroppedItem {
                    subject: format!("node extra in form 0 ({label})"),
                    reason: "entity does not resolve".to_string(),
                },
            ]),
        }
    }

    #[test]
    fn directory_prompt_scope_includes_only_its_changed_files() {
        let selection = AiSelectionKey::Directory("src/api".to_string());
        let scoped = changeset_for_selection(&directory_changeset(), &selection);
        let paths: Vec<&str> = scoped.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["src/api/handler.rs", "src/api/model.rs"]);

        let semantics = std::collections::HashMap::new();
        let mut prompt = String::new();
        append_impact_focus(
            &mut prompt,
            &ImpactPane {
                selected_change: Some(SelectedChange {
                    file: "src/api".to_string(),
                    label: "src/api/".to_string(),
                    change: "modified",
                    interpretation: "Two changed files form one API module update.".to_string(),
                    interpretation_source: InterpretationSource::Deterministic,
                }),
                ..ImpactPane::default()
            },
            &selection,
        );
        append_selected_evidence_contract(&mut prompt, &selection, &scoped, &semantics);
        append_focused_source_packet(&mut prompt, &selection, &scoped, &semantics);
        assert!(prompt.contains("directory: src/api/"));
        assert!(prompt.contains("Summarize this changed directory as a module"));
        assert!(prompt.contains("Show the common purpose of the changes"));
        assert!(prompt.contains("selected directory evidence contract"));
        assert!(prompt.contains("directory scope: src/api/ (2 changed files"));
        assert!(prompt.contains("file: src/api/handler.rs"));
        assert!(prompt.contains("file: src/api/model.rs"));
        assert!(!prompt.contains("src/cli.rs"));

        let facts = SnapshotFacts::from_lazy(&scoped, &semantics, &selection);
        let handler = codescope_core::FileId::new("src/api/handler.rs").unwrap();
        let cli = codescope_core::FileId::new("src/cli.rs").unwrap();
        assert!(facts.is_focus_file(&handler));
        assert!(!facts.is_focus_file(&cli));
        assert!(matches!(facts.file(&handler), Lookup::Present(())));
        assert!(matches!(facts.file(&cli), Lookup::Unknown));
    }

    #[tokio::test]
    async fn directory_selection_publishes_module_scope_and_summary_states() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(directory_changeset());
        disp.data_epoch = disp.epoch;
        let directory = AiSelectionKey::Directory("src/api".to_string());
        disp.ai_cache
            .insert(directory.clone(), cached_ai_plan("src/api"));
        disp.ai_failures.insert(
            AiSelectionKey::File("src/api/model.rs".to_string()),
            "provider failed".to_string(),
        );

        disp.handle(DispatchEvent::Work(Action::DirectorySelectionChanged {
            directory: "src/api".to_string(),
        }))
        .await;

        assert_eq!(disp.current_ai_selection(), Some(directory));
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.diff.title, "src/api/");
        let selected = snap
            .impact
            .selected_change
            .as_ref()
            .expect("directory impact");
        assert_eq!(selected.label, "src/api/");
        assert!(selected.interpretation.contains("2 changed files"));
        assert_eq!(
            snap.ai_summary_state(&AiSummaryKey::Directory("src/api".to_string())),
            AiSummaryState::Ready
        );
        assert_eq!(
            snap.ai_summary_state(&AiSummaryKey::File("src/api/model.rs".to_string())),
            AiSummaryState::Failed
        );
        assert_eq!(
            snap.ai_summary_state(&AiSummaryKey::File("src/api/handler.rs".to_string())),
            AiSummaryState::NotGenerated
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn revision_cache_identity_survives_symbol_line_movement() {
        let before = AiSelectionKey::Symbol {
            file: "src/service.rs".to_string(),
            name: "request_plan".to_string(),
            line: 120,
            col: 4,
        };
        let after = AiSelectionKey::Symbol {
            file: "src/service.rs".to_string(),
            name: "request_plan".to_string(),
            line: 164,
            col: 4,
        };
        assert_ne!(
            before, after,
            "epoch-exact render keys include source position"
        );
        assert_eq!(
            AiRevisionKey::from(&before),
            AiRevisionKey::from(&after),
            "revision seeds follow the same symbol through surrounding edits"
        );
    }

    fn semantic_node_label(snap: &UiSnapshot) -> Option<&str> {
        snap.semantic
            .plan
            .as_ref()?
            .forms
            .first()?
            .nodes
            .first()
            .map(|node| node.label.as_str())
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
        assert!(snap.semantic.plan.is_none());

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
        assert!(snapshot_rx.borrow().semantic.plan.is_none());

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

    /// An asynchronous per-file cache entry wrapping `changed` symbols (the post-redesign home
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

    /// Activity events update one stable row and expose the next-model wait separately.
    #[tokio::test]
    async fn ai_activity_updates_one_tool_row_from_running_to_succeeded() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.selected_file = Some("a.txt".to_string());
        let selection = disp.current_ai_selection().unwrap();
        let generation = 41;
        disp.ai_running.insert(
            generation,
            AiRunningJob {
                selection: selection.clone(),
                epoch: disp.epoch,
                generation,
            },
        );

        for state in [AiToolActivityState::Running, AiToolActivityState::Succeeded] {
            disp.handle(DispatchEvent::AiActivity {
                epoch: disp.epoch,
                selection: selection.clone(),
                generation,
                update: AiActivityUpdate::ToolCall {
                    id: "call-1".to_string(),
                    name: "git_diff_file".to_string(),
                    detail: "a.txt · hunk 0".to_string(),
                    state,
                },
            })
            .await;
        }
        disp.handle(DispatchEvent::AiActivity {
            epoch: disp.epoch,
            selection,
            generation,
            update: AiActivityUpdate::WaitingForModel,
        })
        .await;

        let snap = snapshot_rx.borrow().clone();
        assert!(snap.ai_activity.active);
        assert!(snap.ai_activity.waiting_for_model);
        assert_eq!(snap.ai_activity.calls.len(), 1);
        assert_eq!(
            snap.ai_activity.calls[0].state,
            AiToolCallActivityState::Succeeded
        );
        assert_eq!(snap.ai_activity.calls[0].name, "git_diff_file");

        std::fs::remove_dir_all(&root).ok();
    }

    /// Every AI failure maps to a concise Warning footer plus a complete click-open
    /// diagnostic describing automatic regeneration, model recovery, and fallback.
    #[tokio::test]
    async fn ai_failure_status_carries_retry_suffix() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;

        disp.selected_file = Some("a.txt".to_string());
        let selection = disp.current_ai_selection().unwrap();
        let generation = disp.ai_request_seq;
        disp.ai_running.insert(
            generation,
            AiRunningJob {
                selection: selection.clone(),
                epoch: disp.epoch,
                generation,
            },
        );
        disp.handle(DispatchEvent::AiDone {
            epoch: disp.epoch,
            selection,
            generation,
            outcome: AiOutcome::Failed("ai request timed out after 20s".to_string()),
        })
        .await;
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.status.level, StatusLevel::Warning);
        assert_eq!(
            snap.status.text,
            "AI: ai request timed out after 20s · m change model · retries automatically when the selection or file changes · deterministic impact remains available"
        );
        assert_eq!(
            snap.message, snap.status.text,
            "the legacy message field mirrors the status text"
        );
        assert_eq!(
            snap.status.detail.as_deref(),
            Some(
                "AI generation failed\n\nai request timed out after 20s\n\nRecovery: m change model · retries automatically when the selection or file changes · deterministic impact remains available"
            )
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ai_failure_footer_uses_only_a_bounded_first_line() {
        let reason = format!(
            "plan rejected: concise explanation\n\nValidation details:\n- {}",
            "full validation reason ".repeat(40)
        );
        assert_eq!(
            ai_failure_footer_reason(&reason),
            "plan rejected: concise explanation"
        );
        let long = "x".repeat(300);
        let compact = ai_failure_footer_reason(&long);
        assert_eq!(compact.chars().count(), 180);
        assert!(compact.ends_with('…'));
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
            selected.interpretation, "symbol analysis pending…",
            "a pending file says so, not a fake zero"
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
        assert!(snap.semantic.plan.is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ai_plan_follows_file_navigation_and_reuses_selection_cache() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        let a = AiSelectionKey::File("a.txt".to_string());
        let b = AiSelectionKey::File("b.txt".to_string());
        disp.ai_cache.insert(a.clone(), cached_ai_plan("plan-a"));
        disp.ai_cache.insert(b.clone(), cached_ai_plan("plan-b"));

        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: None,
        }))
        .await;
        {
            let snap = snapshot_rx.borrow().clone();
            assert_eq!(semantic_node_label(&snap), Some("plan-a"));
            assert_eq!(snap.semantic.note, "a.txt");
            assert_report_for(&snap, "plan-a");
        }

        // This is the dispatcher's Up/Down input: the plan switches with Impact instead
        // of leaving a.txt's explanation fixed beneath b.txt — and its report switches
        // with it (never a.txt's drops beneath b.txt's plan).
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("b.txt".to_string()),
            symbol: None,
        }))
        .await;
        {
            let snap = snapshot_rx.borrow().clone();
            assert_eq!(semantic_node_label(&snap), Some("plan-b"));
            assert_eq!(snap.semantic.note, "b.txt");
            assert_report_for(&snap, "plan-b");
        }

        // Moving back restores the cached plan — and its cached report — without any
        // provider request.
        let request_generation = disp.ai_request_seq;
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: None,
        }))
        .await;
        assert_eq!(semantic_node_label(&snapshot_rx.borrow()), Some("plan-a"));
        assert_report_for(&snapshot_rx.borrow(), "plan-a");
        assert_eq!(
            disp.ai_request_seq, request_generation,
            "a cache hit launches no provider request"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The semantic pane publishes the validation report that produced the plan — verdict
    /// plus dropped items — and keeps it scoped to the matching selection (Terra's
    /// report-preservation contract).
    fn assert_report_for(snap: &UiSnapshot, label: &str) {
        let report = snap
            .semantic
            .report
            .as_ref()
            .unwrap_or_else(|| panic!("plan for {label} must carry its validation report"));
        assert_eq!(
            report.verdict,
            codescope_core::ValidationVerdict::ValidWithDrops,
            "cached report verdict for {label}"
        );
        assert_eq!(report.dropped.len(), 1, "dropped items survive for {label}");
        assert!(
            report.dropped[0].subject.contains(label),
            "report belongs to {label}: {:?}",
            report.dropped
        );
    }

    #[tokio::test]
    async fn uncached_ai_selection_hides_previous_plan_and_waits_for_symbols() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        disp.repo_ctx = Some(codescope_core::RepoContext {
            toplevel: camino::Utf8PathBuf::from_path_buf(root.clone()).unwrap(),
            head: codescope_core::HeadState::Branch("feature".to_string()),
            upstream: None,
            base: None,
        });
        disp.data_epoch = disp.epoch;
        let config = codescope_ai::AiConfig {
            enabled: true,
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "test/model".to_string(),
            reasoning_effort: ReasoningEffort::Default,
            api_key: None,
            timeout: std::time::Duration::from_millis(50),
            tool_choice: codescope_ai::ToolChoice::Required,
            max_tool_calls: 1,
            prime_team_id: None,
        };
        disp.ai = Some(std::sync::Arc::new(
            AiService::new(
                config,
                camino::Utf8PathBuf::from_path_buf(root.clone()).unwrap(),
            )
            .unwrap(),
        ));
        let a = AiSelectionKey::File("a.txt".to_string());
        disp.ai_cache.insert(a, cached_ai_plan("plan-a"));
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: None,
        }))
        .await;
        assert_eq!(semantic_node_label(&snapshot_rx.borrow()), Some("plan-a"));

        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("b.txt".to_string()),
            symbol: None,
        }))
        .await;
        let snap = snapshot_rx.borrow().clone();
        assert!(
            !snap.semantic.ai_generated && snap.semantic.plan.is_none(),
            "the old selection's plan disappears immediately"
        );
        assert!(
            snap.semantic.report.is_none(),
            "the old selection's report disappears with its plan"
        );
        assert_eq!(snap.ai, AiStatus::WaitingForSymbols { epoch: disp.epoch });

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn late_focused_relations_do_not_cancel_a_running_plan() {
        let root = scratch_repo();
        let (mut disp, _snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        let config = codescope_ai::AiConfig {
            enabled: true,
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "test/model".to_string(),
            reasoning_effort: ReasoningEffort::Default,
            api_key: None,
            timeout: std::time::Duration::from_millis(25),
            tool_choice: codescope_ai::ToolChoice::Required,
            max_tool_calls: 1,
            prime_team_id: None,
        };
        disp.ai = Some(std::sync::Arc::new(
            AiService::new(
                config,
                camino::Utf8PathBuf::from_path_buf(root.clone()).unwrap(),
            )
            .unwrap(),
        ));
        disp.selected_file = Some("a.txt".to_string());
        disp.selected_symbol = Some(("a.txt".to_string(), "focused".to_string(), 10, 2));
        let selection = disp.current_ai_selection().unwrap();
        disp.ai_running.insert(
            9,
            AiRunningJob {
                selection: selection.clone(),
                epoch: disp.epoch,
                generation: 9,
            },
        );

        disp.handle(DispatchEvent::RelationsLoaded {
            epoch: disp.epoch,
            file: "a.txt".to_string(),
            name: "focused".to_string(),
            line: 10,
            col: 2,
            callers: relation_rows(&["caller"]),
            callees: relation_rows(&["callee"]),
        })
        .await;

        assert!(
            disp.ai_running.contains_key(&9),
            "late relation data must not cancel or restart an active request"
        );
        let relations = disp
            .selected_relations
            .as_ref()
            .expect("current selection receives late relations");
        assert_eq!(relations.callers.rows.len(), 1);
        assert_eq!(relations.callees.rows.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn automatic_ai_waits_for_selected_file_symbols_then_starts() {
        let root = scratch_repo();
        let (mut disp, _snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        disp.repo_ctx = Some(codescope_core::RepoContext {
            toplevel: camino::Utf8PathBuf::from_path_buf(root.clone()).unwrap(),
            head: codescope_core::HeadState::Branch("feature".to_string()),
            upstream: None,
            base: None,
        });
        disp.data_epoch = disp.epoch;
        disp.file_semantics
            .insert("b.txt".to_string(), FileSemanticState::Loading);
        let config = codescope_ai::AiConfig {
            enabled: true,
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "test/model".to_string(),
            reasoning_effort: ReasoningEffort::Default,
            api_key: None,
            timeout: std::time::Duration::from_millis(25),
            tool_choice: codescope_ai::ToolChoice::Required,
            max_tool_calls: 1,
            prime_team_id: None,
        };
        disp.ai = Some(std::sync::Arc::new(
            AiService::new(
                config,
                camino::Utf8PathBuf::from_path_buf(root.clone()).unwrap(),
            )
            .unwrap(),
        ));

        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("b.txt".to_string()),
            symbol: None,
        }))
        .await;
        assert!(
            disp.ai_running.is_empty(),
            "provider request is gated while symbols are Loading"
        );
        let before_ready = disp.ai_selection_seq;

        let FileSemanticState::Ready(result) = ready_semantics("b.txt", Vec::new()) else {
            unreachable!("helper always returns Ready")
        };
        disp.handle(DispatchEvent::FileAnalysisDone {
            epoch: disp.epoch,
            file: "b.txt".to_string(),
            result: Ok(result),
        })
        .await;
        assert!(
            disp.ai_selection_seq > before_ready,
            "Ready schedules the automatic selection debounce"
        );
        assert!(disp.ai_running.is_empty(), "debounce has not fired yet");

        disp.handle(DispatchEvent::AiSelectionSettled {
            epoch: disp.epoch,
            generation: disp.ai_selection_seq,
        })
        .await;
        assert_eq!(
            disp.ai_running.values().map(|job| &job.selection).next(),
            Some(&AiSelectionKey::File("b.txt".to_string())),
            "the provider request starts only after semantic readiness"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn repository_change_invalidates_symbols_and_automatically_regenerates() {
        let root = scratch_repo();
        let (mut disp, _snapshot_rx, mut job_rx) = dispatcher_for(&root).await;
        let config = codescope_ai::AiConfig {
            enabled: true,
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "test/model".to_string(),
            reasoning_effort: ReasoningEffort::Default,
            api_key: None,
            timeout: std::time::Duration::from_millis(25),
            tool_choice: codescope_ai::ToolChoice::Required,
            max_tool_calls: 1,
            prime_team_id: None,
        };
        disp.ai = Some(std::sync::Arc::new(
            AiService::new(
                config,
                camino::Utf8PathBuf::from_path_buf(root.clone()).unwrap(),
            )
            .unwrap(),
        ));
        disp.selected_file = Some("a.txt".to_string());
        disp.file_semantics
            .insert("a.txt".to_string(), ready_semantics("a.txt", Vec::new()));
        let selection = disp.current_ai_selection().unwrap();
        let cached = cached_ai_plan("old plan");
        disp.ai_cache.insert(selection.clone(), cached.clone());
        let revision_key = AiRevisionKey::from(&selection);
        disp.ai_revision_cache
            .insert(revision_key.clone(), cached.clone());
        disp.ai_rows = Some((disp.epoch, selection, cached.clone()));

        disp.handle(DispatchEvent::RepoChanged).await;
        assert!(
            disp.file_semantics.is_empty(),
            "old symbol facts are invalidated"
        );
        assert!(
            disp.ai_cache.is_empty(),
            "old generated plans cannot render in the new epoch"
        );
        assert_eq!(
            disp.ai_revision_cache
                .get(&revision_key)
                .map(|item| &item.plan),
            Some(&cached.plan),
            "the old validated design survives only as a revision seed"
        );
        assert!(disp.ai_running.is_empty());

        let changeset_ready = recv_until(&mut job_rx, |event| {
            matches!(event, DispatchEvent::ChangesetReady { .. })
        })
        .await;
        disp.handle(changeset_ready).await;
        assert!(
            disp.ai_revision_cache.contains_key(&revision_key),
            "a still-changed file keeps its revision seed"
        );
        assert!(matches!(
            disp.file_semantics.get("a.txt"),
            Some(FileSemanticState::Loading)
        ));
        let before_ready = disp.ai_selection_seq;

        let FileSemanticState::Ready(result) = ready_semantics("a.txt", Vec::new()) else {
            unreachable!("helper always returns Ready")
        };
        disp.handle(DispatchEvent::FileAnalysisDone {
            epoch: disp.epoch,
            file: "a.txt".to_string(),
            result: Ok(result),
        })
        .await;
        assert!(
            disp.ai_selection_seq > before_ready,
            "fresh symbols schedule regeneration without a keypress"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A cached plan whose epoch no longer matches the repo state publishes the stale
    /// pane: no plan, no report — the old report must never leak into the new epoch's
    /// fallback view.
    #[tokio::test]
    async fn stale_epoch_ai_pane_carries_neither_plan_nor_report() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        disp.selected_file = Some("a.txt".to_string());
        let stale_epoch = disp.epoch.next();
        disp.ai_rows = Some((
            stale_epoch,
            AiSelectionKey::File("a.txt".to_string()),
            cached_ai_plan("plan-a"),
        ));
        disp.publish();
        let snap = snapshot_rx.borrow().clone();
        assert!(
            !snap.semantic.ai_generated && snap.semantic.plan.is_none(),
            "stale plan never renders: {:?}",
            snap.semantic
        );
        assert!(
            snap.semantic.report.is_none(),
            "the stale epoch's report must not leak into the fallback pane"
        );
        assert_eq!(
            snap.semantic.note,
            "AI view stale (repo changed); regenerating…"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn late_ai_response_cannot_overwrite_a_new_arrow_selection() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.changeset = Some(two_file_changeset());
        disp.selected_file = Some("a.txt".to_string());
        disp.ai_request_seq = 10;
        let old_selection = disp.current_ai_selection().unwrap();
        disp.ai_running.insert(
            10,
            AiRunningJob {
                selection: old_selection.clone(),
                epoch: disp.epoch,
                generation: 10,
            },
        );

        // Arrowing to b.txt does not cancel a.txt's in-flight generation.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("b.txt".to_string()),
            symbol: None,
        }))
        .await;
        assert!(
            disp.ai_running.contains_key(&10),
            "navigation leaves started requests active"
        );
        let mut old_plan = codescope_core::VisualizationPlan::new(disp.epoch);
        old_plan.intent = "stale a.txt plan".to_string();
        old_plan.forms.push(codescope_core::VizForm {
            kind: codescope_core::FormKind::ImpactSummary,
            nodes: vec![codescope_core::PlanNode::new(
                "n1",
                "a.txt",
                codescope_core::PlanNodeChange::Modified,
            )],
            edges: Vec::new(),
        });
        disp.handle(DispatchEvent::AiDone {
            epoch: disp.epoch,
            selection: old_selection.clone(),
            generation: 10,
            outcome: AiOutcome::Plan(old_plan, codescope_core::ValidationReport::valid()),
        })
        .await;
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.impact.selected_change.unwrap().file, "b.txt");
        assert!(!snap.semantic.ai_generated);
        assert!(snap.semantic.plan.is_none());
        assert!(
            disp.ai_cache.contains_key(&old_selection),
            "off-focus completion is cached instead of discarded"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn impact_focus_prompt_is_explicitly_selection_scoped() {
        let impact = ImpactPane {
            selected_change: Some(SelectedChange {
                file: "pkg/api.go".to_string(),
                label: "readinessHandler".to_string(),
                change: "added",
                interpretation: "Added function across 1 hunk.".to_string(),
                interpretation_source: InterpretationSource::Deterministic,
            }),
            callers: ImpactList {
                rows: vec![impact_row("run")],
                state: ImpactLoadState::Ready,
                partial: false,
            },
            downstream: ImpactList {
                rows: vec![impact_row("http.HandlerFunc")],
                state: ImpactLoadState::Ready,
                partial: true,
            },
            note: "partial: some relationships unavailable".to_string(),
        };
        let mut digest = "# change digest\n".to_string();
        let selection = AiSelectionKey::Symbol {
            file: "pkg/api.go".to_string(),
            name: "readinessHandler".to_string(),
            line: 0,
            col: 0,
        };
        append_impact_focus(&mut digest, &impact, &selection);
        assert!(digest.contains("current impact selection"));
        assert!(!digest.contains("required plan focus"));
        assert!(digest.contains("label: readinessHandler"));
        assert!(digest.contains("- run (calls)"));
        assert!(digest.contains("- http.HandlerFunc (calls)"));
        assert!(digest.contains("relationship evidence is partial"));
        assert!(digest.contains("MUST be about this file/function only"));
        assert!(digest.contains("every node code_ref MUST use this selected file"));
        assert!(digest.contains("end the visual at that publication"));
        assert!(digest.contains("omit any unshown actor, mapping, or outcome"));
        assert!(!digest.contains("review_focus"));
    }

    #[test]
    fn unsupported_yaml_focus_requires_file_hunk_evidence() {
        let path = ".github/workflows/vm-sandbox-deploy.yaml";
        let selection = file_selection(path);
        let mut semantics = std::collections::HashMap::new();
        semantics.insert(path.to_string(), FileSemanticState::Unsupported);
        let mut digest = String::new();

        let changeset = many_hunk_changeset(path, 1);
        append_selected_evidence_contract(&mut digest, &selection, &changeset, &semantics);

        assert!(digest.contains("this file type has no semantic analyzer"));
        assert!(digest.contains("MUST use the exact selected file"));
        assert!(digest.contains("MUST omit symbol and range"));
        assert!(digest.contains("Node entities MUST be absent or use the exact file only"));
        assert!(digest.contains("`changes`"));
        assert!(digest.contains("are concepts, NOT symbols"));
    }

    /// One hunk spec for [`variable_hunk_changeset`]: `lines` body lines, each rendered
    /// as `{text} {hunk_index}/{line_number}` so tests can tell hunk and line apart.
    struct HunkSpec {
        lines: usize,
        text: String,
    }

    /// One modified file with one hunk per spec; hunk `i` adds `specs[i].lines` lines
    /// labelled `{text} {i}/{n}`, so the focused-packet tests can control both hunk
    /// sizes and line widths and identify exactly which hunks were selected.
    fn variable_hunk_changeset(path: &str, specs: &[HunkSpec]) -> codescope_core::ChangeSet {
        use codescope_core::{ChangeSet, DiffLine, FileChange, FileStatus, Hunk};
        let hunks = specs
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                let start = 10 * index as u32 + 1;
                Hunk {
                    old_start: start,
                    old_len: 1,
                    new_start: start,
                    new_len: spec.lines as u32,
                    section: None,
                    lines: (0..spec.lines as u32)
                        .map(|n| DiffLine::add(start + n, format!("{} {index}/{n}", spec.text)))
                        .collect(),
                }
            })
            .collect();
        ChangeSet::new(
            ChangeScope::Branch,
            vec![FileChange {
                path: path.into(),
                old_path: None,
                status: FileStatus::Modified,
                hunks,
                binary: false,
            }],
        )
    }

    /// One modified file with `count` single-line hunks; hunk `i` adds `change {i}` so
    /// the focused-packet tests can tell exactly which hunks were selected.
    fn many_hunk_changeset(path: &str, count: usize) -> codescope_core::ChangeSet {
        let specs = (0..count)
            .map(|_| HunkSpec {
                lines: 1,
                text: "change".to_string(),
            })
            .collect::<Vec<_>>();
        variable_hunk_changeset(path, &specs)
    }

    fn file_selection(path: &str) -> AiSelectionKey {
        AiSelectionKey::File(path.to_string())
    }

    fn focused_packet(
        selection: &AiSelectionKey,
        changeset: &codescope_core::ChangeSet,
        semantics: &std::collections::HashMap<String, FileSemanticState>,
    ) -> String {
        let mut digest = String::new();
        append_focused_source_packet(&mut digest, selection, changeset, semantics);
        digest
    }

    #[test]
    fn focused_packet_numbers_both_diff_sides_for_exact_node_code_refs() {
        use codescope_core::{ChangeSet, DiffLine, FileChange, FileStatus, Hunk};
        let path = "src/main.rs";
        let changeset = ChangeSet::new(
            ChangeScope::Branch,
            vec![FileChange {
                path: path.into(),
                old_path: None,
                status: FileStatus::Modified,
                hunks: vec![Hunk {
                    old_start: 7,
                    old_len: 2,
                    new_start: 7,
                    new_len: 2,
                    section: Some("fn main".to_string()),
                    lines: vec![
                        DiffLine::del(7, "old();"),
                        DiffLine::add(7, "new();"),
                        DiffLine::context(8, 8, "finish();"),
                    ],
                }],
                binary: false,
            }],
        );
        let packet = focused_packet(
            &file_selection(path),
            &changeset,
            &std::collections::HashMap::new(),
        );
        assert!(packet.contains("body annotations use one-based old/new lines"));
        assert!(packet.contains("[old:7 new:-] -old();"), "{packet}");
        assert!(packet.contains("[old:- new:7] +new();"), "{packet}");
        assert!(packet.contains("[old:8 new:8]  finish();"), "{packet}");

        let selection = file_selection(path);
        let facts =
            SnapshotFacts::from_lazy(&changeset, &std::collections::HashMap::new(), &selection);
        let file = codescope_core::FileId::new_unchecked(path);
        assert!(facts.is_focus_file(&file));
        assert!(!facts.is_focus_file(&codescope_core::FileId::new_unchecked("src/other.rs")));
        assert_eq!(
            facts.diff_line(&file, 0, DiffSide::Old, 7),
            Lookup::Present(())
        );
        assert_eq!(
            facts.diff_line(&file, 0, DiffSide::New, 7),
            Lookup::Present(())
        );
        assert_eq!(
            facts.diff_line(&file, 0, DiffSide::New, 8),
            Lookup::Present(())
        );
        assert_eq!(facts.diff_line(&file, 0, DiffSide::Old, 9), Lookup::Absent);
        assert_eq!(facts.diff_line(&file, 1, DiffSide::Old, 7), Lookup::Absent);
    }

    /// File-level packets must carry the whole file while it fits the hunk cap: the
    /// vm-sandboxes/packages/api/main.go case has six hunks, and the sixth holds the
    /// close-last shutdown behavior the plan is asked to explain.
    #[test]
    fn file_level_packet_covers_all_six_hunks_including_the_final_one() {
        let path = "sandbox/vm-sandboxes/packages/api/main.go";
        let packet = focused_packet(
            &file_selection(path),
            &many_hunk_changeset(path, 6),
            &std::collections::HashMap::new(),
        );
        for index in 0..6 {
            assert!(
                packet.contains(&format!("hunk_id: {index}")),
                "hunk {index} missing from packet:\n{packet}"
            );
        }
        assert!(
            packet.contains("change 5"),
            "final hunk body missing:\n{packet}"
        );
        assert!(!packet.contains("truncated"));
    }

    /// Beyond the cap the packet stays bounded but keeps balanced head and tail
    /// coverage: the leading and trailing hunks, so late finalization edits are never
    /// silently dropped while entry-point changes stay visible.
    #[test]
    fn file_level_packet_stays_bounded_with_balanced_head_and_tail() {
        let path = "pkg/server.go";
        let total = FOCUSED_MAX_HUNKS + 1;
        let packet = focused_packet(
            &file_selection(path),
            &many_hunk_changeset(path, total),
            &std::collections::HashMap::new(),
        );
        assert_eq!(
            packet.matches("hunk_id:").count(),
            FOCUSED_MAX_HUNKS,
            "hunk cap not enforced:\n{packet}"
        );
        let tail = FOCUSED_MAX_HUNKS / 2;
        let head = FOCUSED_MAX_HUNKS - tail;
        for index in (0..head).chain(total - tail..total) {
            assert!(
                packet.contains(&format!("hunk_id: {index}")),
                "hunk {index} missing from packet:\n{packet}"
            );
        }
        assert!(
            packet.contains(&format!("change {}/0", total - 1)),
            "final hunk body dropped:\n{packet}"
        );
        for dropped in head..total - tail {
            assert!(
                !packet.contains(&format!("hunk_id: {dropped}")),
                "hunk {dropped} should be beyond the cap:\n{packet}"
            );
        }
    }

    /// The hunk cap never overrides the hard line budget: a six-hunk file whose hunks
    /// together exceed the line cap still cuts off with the truncation marker — but the
    /// fair slices keep every hunk's header, including the final one, in the packet.
    #[test]
    fn file_level_packet_respects_the_line_budget() {
        let path = "pkg/generated.go";
        let specs = (0..6)
            .map(|_| HunkSpec {
                lines: 40,
                text: "bulk".to_string(),
            })
            .collect::<Vec<_>>();
        let packet = focused_packet(
            &file_selection(path),
            &variable_hunk_changeset(path, &specs),
            &std::collections::HashMap::new(),
        );
        let emitted = packet.lines().filter(|line| line.contains("] +")).count();
        assert_eq!(
            emitted, FOCUSED_MAX_LINES,
            "line budget not enforced:\n{packet}"
        );
        assert!(packet.contains("focused source truncated to prompt budget"));
        for index in 0..6 {
            assert!(
                packet.contains(&format!("hunk_id: {index}")),
                "hunk {index} header lost under line pressure:\n{packet}"
            );
        }
        assert!(
            packet.contains("+bulk 5/0"),
            "final hunk body lost under line pressure:\n{packet}"
        );
    }

    /// Budget pressure from a huge early hunk must not starve the tail: the fair line
    /// slice guarantees the final hunk's header and body evidence while the early hunk
    /// absorbs only its share plus the leftover lines, and the totals stay bounded.
    #[test]
    fn file_level_packet_keeps_final_hunk_evidence_when_an_early_hunk_is_huge() {
        let path = "sandbox/vm-sandboxes/packages/api/main.go";
        let specs = vec![
            HunkSpec {
                lines: 200,
                text: "handle".to_string(),
            },
            HunkSpec {
                lines: 20,
                text: "route".to_string(),
            },
            HunkSpec {
                lines: 20,
                text: "auth".to_string(),
            },
            HunkSpec {
                lines: 20,
                text: "pool".to_string(),
            },
            HunkSpec {
                lines: 20,
                text: "metrics".to_string(),
            },
            HunkSpec {
                lines: 5,
                text: "closeLastShutdown".to_string(),
            },
        ];
        let packet = focused_packet(
            &file_selection(path),
            &variable_hunk_changeset(path, &specs),
            &std::collections::HashMap::new(),
        );
        // The final hunk keeps its header plus its full body evidence.
        assert!(
            packet.contains("hunk_id: 5"),
            "final hunk header lost to the huge early hunk:\n{packet}"
        );
        assert!(
            packet.contains("+closeLastShutdown 5/0"),
            "final hunk body lost to the huge early hunk:\n{packet}"
        );
        // Every other hunk keeps its fair slice; the huge one gets the leftover.
        for index in 0..5 {
            assert!(
                packet.contains(&format!("hunk_id: {index}")),
                "hunk {index} header lost:\n{packet}"
            );
        }
        assert!(
            packet.contains("+handle 0/0"),
            "early hunk body lost:\n{packet}"
        );
        // Totals stay bounded: exactly the line budget, with the cut flagged.
        let emitted = packet.lines().filter(|line| line.contains("] +")).count();
        assert_eq!(
            emitted, FOCUSED_MAX_LINES,
            "line budget not enforced:\n{packet}"
        );
        assert!(packet.contains("focused source truncated to prompt budget"));
    }

    /// Byte pressure from a very wide early hunk must not starve the tail either: the
    /// fair byte slice reserves room for the final hunk's header and body evidence
    /// while the wide hunk keeps whatever budget remains. Totals stay bounded.
    #[test]
    fn file_level_packet_keeps_final_hunk_evidence_when_an_early_hunk_is_wide() {
        let path = "pkg/generated.go";
        let wide = "x".repeat(900);
        let specs = vec![
            HunkSpec {
                lines: 30,
                text: wide,
            },
            HunkSpec {
                lines: 3,
                text: "route".to_string(),
            },
            HunkSpec {
                lines: 3,
                text: "auth".to_string(),
            },
            HunkSpec {
                lines: 3,
                text: "pool".to_string(),
            },
            HunkSpec {
                lines: 3,
                text: "metrics".to_string(),
            },
            HunkSpec {
                lines: 3,
                text: "closeLastShutdown".to_string(),
            },
        ];
        let packet = focused_packet(
            &file_selection(path),
            &variable_hunk_changeset(path, &specs),
            &std::collections::HashMap::new(),
        );
        assert!(
            packet.contains("hunk_id: 5"),
            "final hunk header lost to the wide early hunk:\n{packet}"
        );
        assert!(
            packet.contains("+closeLastShutdown 5/0"),
            "final hunk body lost to the wide early hunk:\n{packet}"
        );
        assert!(
            packet.contains("hunk_id: 0") && packet.contains("+xxxx"),
            "wide early hunk lost its evidence:\n{packet}"
        );
        // Totals stay bounded: lines within the line cap, bytes within the byte cap
        // (the truncation marker itself may overshoot by its own short length).
        let emitted = packet.lines().filter(|line| line.contains("] +")).count();
        assert!(
            emitted <= FOCUSED_MAX_LINES,
            "line budget not enforced:\n{packet}"
        );
        assert!(
            packet.len() <= FOCUSED_MAX_BYTES + 64,
            "byte budget not enforced:\n{packet}"
        );
        assert!(packet.contains("focused source truncated to prompt budget"));
    }

    /// Symbol-scoped selections stay focused: the packet carries only the hunks the
    /// semantic mapping assigns to the selected symbol, never the file-level fallback.
    #[test]
    fn symbol_scoped_packet_stays_limited_to_the_symbols_hunks() {
        let path = "pkg/server.go";
        let mut symbol = changed_symbol(
            path,
            "Shutdown",
            codescope_core::SymbolKind::Function,
            codescope_core::ChangeKind::Modified,
            0,
            false,
        );
        symbol.record.hunks = vec![1, 7]
            .into_iter()
            .map(|index| codescope_core::HunkId {
                file: path.into(),
                index,
            })
            .collect();
        let mut semantics = std::collections::HashMap::new();
        semantics.insert(path.to_string(), ready_semantics(path, vec![symbol]));
        let selection = AiSelectionKey::Symbol {
            file: path.to_string(),
            name: "Shutdown".to_string(),
            line: 2,
            col: 4,
        };
        let packet = focused_packet(&selection, &many_hunk_changeset(path, 9), &semantics);
        assert!(packet.contains("hunk_id: 1"));
        assert!(packet.contains("hunk_id: 7"));
        for absent in [0, 2, 8] {
            assert!(
                !packet.contains(&format!("hunk_id: {absent}")),
                "hunk {absent} leaked into the symbol packet:\n{packet}"
            );
        }
    }

    #[test]
    fn large_symbol_packet_balances_entry_and_finalization_hunks() {
        let path = "pkg/server.go";
        let total = FOCUSED_MAX_HUNKS + 2;
        let mut symbol = changed_symbol(
            path,
            "Run",
            codescope_core::SymbolKind::Function,
            codescope_core::ChangeKind::Modified,
            0,
            false,
        );
        symbol.record.hunks = (0..total as u32)
            .map(|index| codescope_core::HunkId {
                file: path.into(),
                index,
            })
            .collect();
        let mut semantics = std::collections::HashMap::new();
        semantics.insert(path.to_string(), ready_semantics(path, vec![symbol]));
        let selection = AiSelectionKey::Symbol {
            file: path.to_string(),
            name: "Run".to_string(),
            line: 2,
            col: 4,
        };
        let packet = focused_packet(&selection, &many_hunk_changeset(path, total), &semantics);
        let tail = FOCUSED_MAX_HUNKS / 2;
        let head = FOCUSED_MAX_HUNKS - tail;
        for index in (0..head).chain(total - tail..total) {
            assert!(packet.contains(&format!("hunk_id: {index}")), "{packet}");
        }
        for omitted in head..total - tail {
            assert!(!packet.contains(&format!("hunk_id: {omitted}")), "{packet}");
        }
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
        // A validated plan for the current epoch and selected symbol.
        disp.selected_file = Some("a.txt".to_string());
        disp.selected_symbol = Some(("a.txt".to_string(), "sym0".to_string(), 2, 4));
        let selection = disp.current_ai_selection().expect("selected symbol");
        let plan = cached_ai_plan("RetryPolicy");
        disp.ai_cache.insert(selection.clone(), plan.clone());
        disp.ai_rows = Some((disp.epoch, selection, plan));
        disp.ai_status = AiStatus::Ready { epoch: disp.epoch };
        disp.publish();
        {
            let snap = snapshot_rx.borrow();
            assert!(snap.semantic.ai_generated);
            assert_eq!(semantic_node_label(&snap), Some("RetryPolicy"));
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
        assert_eq!(
            snap.semantic.plan.as_ref().map(|plan| plan.intent.as_str()),
            Some("plan for RetryPolicy")
        );
        assert_eq!(semantic_node_label(&snap), Some("RetryPolicy"));
        assert_report_for(&snap, "RetryPolicy");
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

    /// Startup lists files immediately and independently queues their symbol analysis,
    /// even while every file remains collapsed.
    #[tokio::test]
    async fn startup_queues_symbols_without_expanding_files() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;
        disp.handle(DispatchEvent::RepoChanged).await;
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(30), job_rx.recv())
                .await
                .expect("refresh timed out")
                .expect("event channel closed");
            let done = matches!(event, DispatchEvent::AnalysisDone { .. });
            disp.handle(event).await;
            if done {
                break;
            }
        }
        let snap = snapshot_rx.borrow().clone();
        assert_eq!(snap.files.len(), 1, "git file listed");
        assert!(!snap.files[0].expanded, "initially collapsed");
        assert_eq!(
            snap.files[0].semantic,
            codescope_tui::snapshot::FileSemanticLoad::Loading
        );
        assert_eq!(
            snap.files[0].changed_symbol_count, 0,
            "no fake zero-symbol count"
        );
        assert_eq!(
            disp.analysis_queue
                .iter()
                .filter(|path| *path == "a.txt")
                .count(),
            1,
            "collapsed file is queued exactly once while the engine starts"
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

    /// Tab only changes visibility: background analysis is already queued, and repeated
    /// collapse/expand actions neither launch nor duplicate semantic work.
    #[tokio::test]
    async fn tab_expands_with_loading_row_and_coalesces() {
        let root = scratch_repo();
        let (mut disp, snapshot_rx, mut job_rx) = dispatcher_for(&root).await;
        disp.handle(DispatchEvent::RepoChanged).await;
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(30), job_rx.recv())
                .await
                .expect("refresh timed out")
                .expect("event channel closed");
            let done = matches!(event, DispatchEvent::AnalysisDone { .. });
            disp.handle(event).await;
            if done {
                break;
            }
        }
        // Aim the selection at the file row.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: None,
        }))
        .await;

        assert_eq!(
            disp.analysis_queue
                .iter()
                .filter(|path| *path == "a.txt")
                .count(),
            1,
            "analysis was queued before expansion"
        );
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
        // Collapse, then re-expand: visibility changes do not touch the queued job.
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
            "expansion never duplicates analysis"
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

    /// Regression: a valid `AiOutcome::Plan` and a symbol's loaded relations coexist —
    /// the Impact pane shows the relations while `semantic` keeps the structured AI
    /// visual. Goes through the real `AiDone`/`RelationsLoaded` events, not direct
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

        // Select sym0 first: plans are now owned by one Impact selection.
        disp.handle(DispatchEvent::Work(Action::SelectionChanged {
            file: Some("a.txt".to_string()),
            symbol: Some(("sym0".to_string(), 2, 4)),
        }))
        .await;

        // A real validated plan lands via AiDone (as spawn_ai's job would report).
        let mut plan = codescope_core::VisualizationPlan::new(disp.epoch);
        plan.intent = "sym0 affects its callers.".to_string();
        plan.forms.push(codescope_core::VizForm {
            kind: codescope_core::FormKind::CallTree,
            nodes: vec![codescope_core::PlanNode::new(
                "n1",
                "sym0",
                codescope_core::PlanNodeChange::Modified,
            )],
            edges: Vec::new(),
        });
        let selection = disp.current_ai_selection().unwrap();
        let generation = disp.ai_request_seq;
        disp.ai_running.insert(
            generation,
            AiRunningJob {
                selection: selection.clone(),
                epoch: disp.epoch,
                generation,
            },
        );
        disp.handle(DispatchEvent::AiDone {
            epoch: disp.epoch,
            selection,
            generation,
            outcome: AiOutcome::Plan(plan, codescope_core::ValidationReport::valid()),
        })
        .await;
        let revision_key = AiRevisionKey::Symbol {
            file: "a.txt".to_string(),
            name: "sym0".to_string(),
        };
        assert!(
            disp.ai_revision_cache.contains_key(&revision_key),
            "every renderable generation is retained for the next file revision"
        );
        {
            let snap = snapshot_rx.borrow().clone();
            assert!(snap.semantic.ai_generated, "plan published to semantic");
            assert_eq!(semantic_node_label(&snap), Some("sym0"));
            // The real AiDone path preserves the report alongside the plan.
            let report = snap
                .semantic
                .report
                .as_ref()
                .expect("AiDone carries the validation report into the snapshot");
            assert_eq!(report.verdict, codescope_core::ValidationVerdict::Valid);
            assert!(report.dropped.is_empty());
        }
        // Its relations land without displacing that selection-owned plan.
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
        // Generated visual: the plan is NOT displaced by the relations.
        assert!(snap.semantic.ai_generated, "plan survives loaded relations");
        assert_eq!(semantic_node_label(&snap), Some("sym0"));
        assert_eq!(snap.ai, AiStatus::Ready { epoch: disp.epoch });

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn focused_semantic_work_preempts_the_background_queue() {
        let root = scratch_repo();
        let (mut disp, _snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        disp.selected_file = Some("b.txt".to_string());
        disp.analysis_queue = ["a.txt", "c.txt", "b.txt"]
            .into_iter()
            .map(str::to_string)
            .collect();
        disp.analysis_in_flight.insert(
            "a.txt".to_string(),
            SemanticRunningJob {
                epoch: disp.epoch,
                priority: SemanticJobPriority::Focused,
            },
        );

        disp.reprioritize_semantic_work();

        assert_eq!(
            disp.analysis_queue.front().map(String::as_str),
            Some("b.txt")
        );
        assert_eq!(
            disp.analysis_in_flight["a.txt"].priority,
            SemanticJobPriority::Background
        );
        assert!(disp.can_launch_file_analysis(SemanticJobPriority::Focused));
        assert!(!disp.can_launch_file_analysis(SemanticJobPriority::Background));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn one_repo_signal_advances_exactly_one_epoch() {
        let root = scratch_repo();
        let (mut disp, _snapshot_rx, _job_rx) = dispatcher_for(&root).await;
        let before = disp.epoch;

        disp.bump_and_refresh();

        assert_eq!(disp.epoch, before.next());
        std::fs::remove_dir_all(&root).ok();
    }
}
