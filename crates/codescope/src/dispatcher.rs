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
    DiffPane, DiffRow, FileRow, RepoBar, ScopeCounts, SemRow, SemanticPane, SymbolRow, UiSnapshot,
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
}

/// Picker entry that returns base selection to inference.
const AUTO_BASE: &str = "(auto / inferred)";

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
    snapshot_tx: watch::Sender<UiSnapshot>,
    /// Where completed jobs report back.
    job_tx: mpsc::Sender<DispatchEvent>,
    /// Status message surfaced in the bottom bar.
    message: String,
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
            base_override: None,
            available_bases: Vec::new(),
            snapshot_tx,
            job_tx,
            message: String::new(),
            repo_ctx: None,
            changeset: None,
        }
    }

    fn publish(&self) {
        let _ = self.snapshot_tx.send(self.build_snapshot());
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
                    self.message = "git-only (no supported language detected)".to_string();
                } else {
                    self.message = format!("git-only (language server failed: {reason})");
                }
                self.publish();
            }
            DispatchEvent::ModelsLoaded(models) => {
                self.available_models = models;
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
                    self.message = "AI not configured (set PRIME_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY)".to_string();
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
            Action::BasePicker => self.spawn_list_bases(),
            Action::BaseSelected(name) => self.set_base(name),
            _ => {}
        }
    }

    /// Fetch the provider's model list for the picker (spawned; non-blocking).
    fn spawn_list_models(&mut self) {
        let Some(ai) = &self.ai else {
            self.message =
                "AI not configured (set PRIME_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY)"
                    .to_string();
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
                self.message = format!("AI model: {name}");
            }
            None => {
                self.message =
                    "AI not configured (set PRIME_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY)"
                        .to_string();
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

    /// Apply a base selection from the picker: everything downstream (repo context,
    /// branch changeset, analysis) is recomputed against the chosen ref.
    fn set_base(&mut self, name: String) {
        if name.is_empty() {
            return;
        }
        if name == AUTO_BASE {
            self.base_override = None;
            self.message = "base: auto (inferred)".to_string();
            self.spawn_refresh();
            return;
        }
        self.message = format!("base: {name}");
        self.base_override = Some(name);
        self.spawn_refresh();
    }

    fn set_scope(&mut self, scope: ChangeScope) {
        if self.scope != scope {
            self.scope = scope;
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
                self.message = "base branch gone; reverted to inferred base".to_string();
                self.spawn_refresh();
                return;
            }
        }
        match result {
            Ok(snap) => {
                self.repo_ctx = Some(snap.repo_ctx.clone());
                self.changeset = Some(snap.changeset.clone());
                self.ls_status = LsStatus::Ready;
                self.message.clear();
                self.analysis = Some(*snap);
            }
            Err(e) => {
                self.message = format!("analysis failed: {e}");
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
                self.message = format!("AI: {reason}");
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
            ls: self.ls_status,
            ai: self.ai_status.clone(),
            ai_model: self.ai.as_ref().map(|a| a.model()).unwrap_or_default(),
            available_models: self.available_models.clone(),
            base_ref,
            available_bases: self.available_bases.clone(),
            message: self.message.clone(),
            epoch: self.epoch,
            refreshing: false,
        }
    }

    fn panes(&self) -> (DiffPane, SemanticPane) {
        let diff = self.changeset.as_ref().map(first_diff).unwrap_or_default();
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
        let change = match c.record.change_kind {
            codescope_core::ChangeKind::Added => "added",
            codescope_core::ChangeKind::Modified => "modified",
            codescope_core::ChangeKind::Deleted => "removed",
        };
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
            });
    }
    a.changeset
        .files
        .iter()
        .map(|f| FileRow {
            path: f.path.to_string(),
            status: status_badge(&f.status),
            symbols: by_file.remove(&f.path.to_string()).unwrap_or_default(),
            expanded: true,
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

fn first_diff(a: &codescope_core::ChangeSet) -> DiffPane {
    let Some(file) = a.files.first() else {
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

/// Run the dispatcher loop until the TUI closes the action channel.
pub async fn run(
    mut disp: Dispatcher,
    mut events: mpsc::Receiver<DispatchEvent>,
    mut actions: mpsc::Receiver<Action>,
) {
    let _ = disp.handle(DispatchEvent::RepoChanged).await;
    loop {
        tokio::select! {
            e = events.recv() => match e { Some(e) => disp.handle(e).await, None => break },
            a = actions.recv() => match a { Some(a) => disp.handle(DispatchEvent::Work(a)).await, None => break },
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
}
