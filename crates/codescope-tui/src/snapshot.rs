//! The immutable UI payload the dispatcher publishes and the TUI renders.
//!
//! `UiSnapshot` is owned here (not in core) because it is a *rendering* model: it flattens
//! git/analysis/AI state into display rows so the renderer never reaches into `git`, `lsp`,
//! or `ai` crates. The binary assembles it; `codescope-tui` only consumes it.

use std::collections::HashMap;

use codescope_core::{AiStatus, ChangeScope, DiagramDraft, Epoch, LsStatus};

/// Everything the interface needs to draw one frame.
#[derive(Debug, Clone)]
pub struct UiSnapshot {
    /// Repository + branch context for the top bar.
    pub repo: RepoBar,
    /// Which change scope is being shown.
    pub scope: ChangeScope,
    /// Per-scope change counts for the scope switcher.
    pub scope_counts: ScopeCounts,
    /// Left pane: changed files and the symbols inside them.
    pub files: Vec<FileRow>,
    /// Generation state for every selectable directory, file, and symbol summary.
    pub ai_summaries: HashMap<AiSummaryKey, AiSummaryState>,
    /// Center pane: the focused diff for the current selection.
    pub diff: DiffPane,
    /// Right pane: the semantic view for the current selection.
    ///
    /// Legacy flattened view — superseded by [`UiSnapshot::impact`] (spec §4); both are
    /// published while the renderer migrates.
    pub semantic: SemanticPane,
    /// Editable renderer-native diagram for the current selection, when an internal or
    /// external agent is still constructing/revising it.
    pub diagram_draft: Option<DiagramDraft>,
    /// Right pane: the impact view (selected change, callers, downstream).
    pub impact: ImpactPane,
    /// Language-server status for the top bar.
    pub ls: LsStatus,
    /// AI status for the top bar.
    pub ai: AiStatus,
    /// The AI model currently selected (empty when AI is off).
    pub ai_model: String,
    /// Selected Chat Completions reasoning budget (`default` uses automatic behavior).
    pub ai_reasoning_effort: String,
    /// Reasoning-budget values accepted by the backend, in picker order.
    pub available_reasoning_efforts: Vec<String>,
    /// Which AI provider/credential is active ("prime"/"openai"/"anthropic"/"custom"; empty
    /// when AI is off).
    pub ai_provider: String,
    /// Provider-reported tokens consumed by this running process.
    pub ai_tokens: AiTokenUsage,
    /// Models the provider advertises (for the picker modal; empty until fetched).
    pub available_models: Vec<String>,
    /// `true` while the user-triggered provider model-discovery request is in flight.
    pub model_list_loading: bool,
    /// Safe, user-visible reason model discovery failed. The configured/current model
    /// remains usable and a model id may still be entered manually.
    pub model_list_error: Option<String>,
    /// The base ref the `Branch` scope compares against (empty until known). Shown in the
    /// top bar; defaults to the nearest ancestor branch, overridable via the base picker.
    pub base_ref: String,
    /// Base candidates for the picker modal (empty until fetched).
    pub available_bases: Vec<String>,
    /// `true` when the base picker list was bounded before every ancestor was visited.
    pub base_candidates_truncated: bool,
    /// Transient status/help message for the bottom bar.
    ///
    /// Legacy plain-text mirror of [`UiSnapshot::status`] (`status.text`); kept while the
    /// renderer migrates to the typed status message.
    pub message: String,
    /// Typed status message for the bottom bar: text plus severity.
    pub status: StatusMessage,
    /// The repo-state epoch this snapshot describes.
    pub epoch: Epoch,
    /// `true` while a refresh is in flight (spinner).
    pub refreshing: bool,
}

impl Default for UiSnapshot {
    fn default() -> Self {
        UiSnapshot {
            repo: RepoBar::default(),
            scope: ChangeScope::Branch,
            scope_counts: ScopeCounts::default(),
            files: Vec::new(),
            ai_summaries: HashMap::new(),
            diff: DiffPane::default(),
            semantic: SemanticPane::default(),
            diagram_draft: None,
            impact: ImpactPane::default(),
            ls: LsStatus::Starting,
            ai: AiStatus::Disabled,
            ai_model: String::new(),
            ai_reasoning_effort: "default".to_string(),
            available_reasoning_efforts: Vec::new(),
            ai_provider: String::new(),
            ai_tokens: AiTokenUsage::default(),
            available_models: Vec::new(),
            model_list_loading: false,
            model_list_error: None,
            base_ref: String::new(),
            available_bases: Vec::new(),
            base_candidates_truncated: false,
            message: String::new(),
            status: StatusMessage::default(),
            epoch: Epoch::ZERO,
            refreshing: false,
        }
    }
}

/// Stable identity of a selectable row in the changed-files tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AiSummaryKey {
    /// Every changed file below this repo-relative directory.
    Directory(String),
    /// One changed file.
    File(String),
    /// One changed symbol. Unmapped symbols retain `None` for selection stability but
    /// fall back to their file when requesting AI.
    Symbol {
        /// Owning repo-relative file.
        file: String,
        /// Display/semantic symbol name.
        name: String,
        /// Identifier position when the language server resolved it.
        position: Option<(u32, u32)>,
    },
}

/// Whether a summary exists for one changed-tree row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AiSummaryState {
    /// No request has completed for this row.
    #[default]
    NotGenerated,
    /// A debounce or provider request is active.
    Generating,
    /// A validated summary is cached and ready to display.
    Ready,
    /// The last request for this row failed.
    Failed,
}

impl UiSnapshot {
    /// Generation state of `key`; rows absent from the map have not been generated.
    #[must_use]
    pub fn ai_summary_state(&self, key: &AiSummaryKey) -> AiSummaryState {
        self.ai_summaries.get(key).copied().unwrap_or_default()
    }
}

/// Process-lifetime provider usage displayed in the bottom bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AiTokenUsage {
    /// Input/prompt tokens.
    pub input: u64,
    /// Output/completion tokens.
    pub output: u64,
}

impl UiSnapshot {
    /// A boot-time placeholder shown before the first analysis completes.
    #[must_use]
    pub fn placeholder() -> Self {
        UiSnapshot {
            message: "scanning repository…".to_string(),
            status: StatusMessage {
                text: "scanning repository…".to_string(),
                detail: None,
                level: StatusLevel::Info,
            },
            refreshing: true,
            ..UiSnapshot::default()
        }
    }

    /// `true` when there is nothing to show yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Full click-open diagnostic for the deterministic fallback banner shown after an
    /// AI request fails. The failure reason lives in `AiStatus`, independently of the
    /// transient footer, so later progress messages cannot erase the diagnostic before
    /// the user opens it.
    #[must_use]
    pub fn ai_failure_status(&self) -> Option<StatusMessage> {
        let codescope_core::AiStatus::Failed { reason } = &self.ai else {
            return None;
        };
        Some(StatusMessage {
            text: "AI failed; showing known relationships".to_string(),
            detail: Some(reason.clone()),
            level: StatusLevel::Warning,
        })
    }
}

/// Top-bar repository context.
#[derive(Debug, Clone, Default)]
pub struct RepoBar {
    /// Repository directory name (last path component).
    pub repo_name: String,
    /// Current branch, or "(detached)" / "(no commits)".
    pub branch: String,
    /// Comparison base ref (e.g. `main`), when known.
    pub base: Option<String>,
    /// Commits ahead of the base.
    pub ahead: u32,
    /// Commits behind the base.
    pub behind: u32,
}

/// Changed-file counts per scope (for the scope switcher in the top bar).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScopeCounts {
    /// Files changed on the branch vs its base.
    pub branch: usize,
    /// Files with staged changes.
    pub staged: usize,
    /// Files with unstaged changes (incl. untracked).
    pub unstaged: usize,
}

/// Per-file asynchronous semantic-analysis state. `Unloaded` is the brief interval before
/// the dispatcher queues the file; the symbol count is unknown and must not render as `0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileSemanticLoad {
    /// No analysis request has been queued yet.
    #[default]
    Unloaded,
    /// A per-file analysis job is in flight.
    Loading,
    /// Analysis completed; `symbols`/`changed_symbol_count` are authoritative (possibly
    /// zero — that is a real answer, not "unknown").
    Ready,
    /// The language service does not own this file (binary, gitlink, unowned language).
    Unsupported,
    /// The analysis job failed (retryable).
    Failed,
}

/// One row in the left "changed files + symbols" pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    /// Semantic load state for this file's symbols.
    pub semantic: FileSemanticLoad,
    /// Repo-relative path (display string).
    pub path: String,
    /// Short status badge: `M`, `A`, `D`, `R`, `?`, `U`.
    pub status: &'static str,
    /// Number of changed symbols inside the file (right-aligned in the files pane).
    pub changed_symbol_count: usize,
    /// Added source lines in this file's parsed hunks.
    pub added_lines: usize,
    /// Removed source lines in this file's parsed hunks.
    pub removed_lines: usize,
    /// Changed symbols inside the file (indented under it).
    pub symbols: Vec<SymbolRow>,
    /// Whether the row's symbol list is expanded in the UI.
    pub expanded: bool,
}

/// One changed-symbol row nested under a [`FileRow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    /// Display name (e.g. `(*MemoryRepo).Get`).
    pub name: String,
    /// `added` / `modified` / `removed`.
    pub change: &'static str,
    /// Mapping confidence marker: `` exact, `~` approximate, `?` unmapped.
    pub confidence: &'static str,
    /// `true` when a diagnostic touches this symbol.
    pub has_diagnostic: bool,
    /// Position of the symbol identifier (for lazy relationship expansion on select).
    /// `None` for rows that can't be expanded (unmapped).
    pub position: Option<(u32, u32)>,
}

/// The center diff pane: a focused unified diff for the current selection.
#[derive(Debug, Clone, Default)]
pub struct DiffPane {
    /// Title (file path or symbol name).
    pub title: String,
    /// The selected symbol's label when the diff shows its file (`None` on file rows and
    /// on the first-file fallback). The full path stays in `title`; shortening a symbol
    /// label is render-only.
    pub focused_symbol: Option<String>,
    /// Render-ready diff rows.
    pub rows: Vec<DiffRow>,
    /// 1-based index of the hunk the cursor is on (`n`/`N` navigation).
    pub current_hunk: usize,
    /// Total hunks in the file (for the `hunk 2/5` indicator).
    pub total_hunks: usize,
}

/// One rendered diff line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRow {
    /// `@@ ... @@ section` header.
    HunkHeader(String),
    /// `+` added line (old-side line number absent).
    Add {
        /// New-side line number.
        new_ln: u32,
        /// Line text (no prefix).
        text: String,
    },
    /// `-` removed line.
    Del {
        /// Old-side line number.
        old_ln: u32,
        /// Line text (no prefix).
        text: String,
    },
    /// Context line.
    Context {
        /// Old-side line number.
        old_ln: u32,
        /// New-side line number.
        new_ln: u32,
        /// Line text.
        text: String,
    },
}

/// The right semantic pane: how the selection relates to the rest of the system.
#[derive(Debug, Clone, Default)]
pub struct SemanticPane {
    /// Validated, structured plan. Layout is deliberately deferred until render time so
    /// diagrams (ladders, trees, adjacency) can respond to the pane's current width.
    pub plan: Option<codescope_core::VisualizationPlan>,
    /// The validation report that produced `plan` (verdict, dropped items, notes).
    /// `Some` only for published AI panes; fallback/stale panes carry `None` so a prior
    /// selection's report can never leak (Terra: sanitized content must stay labeled).
    pub report: Option<codescope_core::ValidationReport>,
    /// A one-line note when the data is partial/approximate/AI-interpretive.
    pub note: String,
    /// `true` when this view came from the AI plan (vs the deterministic fallback).
    pub ai_generated: bool,
}

/// The right impact pane: the selected change, its callers, and its downstream one-hop
/// relations (redesign spec §4). Replaces the flattened [`SemanticPane`] — both are
/// published while the renderer migrates.
///
/// The default is an empty but renderable frame: no selection, idle empty lists.
#[derive(Debug, Clone, Default)]
pub struct ImpactPane {
    /// The deterministically-described selected change (`None` before any selection).
    pub selected_change: Option<SelectedChange>,
    /// Who calls the selected symbol (lazy LSP call hierarchy + incoming graph `Calls`).
    pub callers: ImpactList,
    /// What the selected symbol calls / relates to. One hop only — the analysis graph is
    /// intentionally shallow, so rows must never be presented as transitive impact.
    pub downstream: ImpactList,
    /// One-line caveat when any data behind the pane is partial or approximate.
    pub note: String,
}

/// One impact column (callers or downstream): rows plus a load state that distinguishes
/// "zero rows" from "rows have not returned yet".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImpactList {
    /// The relation rows (may be non-empty while `state` is still `Loading`: one-hop
    /// impact-graph rows are available synchronously, lazy LSP rows land later).
    pub rows: Vec<ImpactRow>,
    /// Fetch state of the lazy evidence behind this list.
    pub state: ImpactLoadState,
    /// `true` when the evidence is incomplete (timeout, truncation, unsupported server
    /// feature); the pane notes this instead of implying the list is exhaustive.
    pub partial: bool,
}

/// Fetch state of an [`ImpactList`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImpactLoadState {
    /// Nothing requested (no symbol selected). The list is empty by definition.
    #[default]
    Idle,
    /// A fetch is in flight; rows so far (impact graph) may already be shown.
    Loading,
    /// The lazy fetch returned; `rows` is the full answer (possibly empty).
    Ready,
    /// No language service, so the lazy fetch can never return (git-only mode). Any
    /// rows present come from the synchronous impact graph alone.
    Unavailable,
}

/// The change the selection sits on, described deterministically from the analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedChange {
    /// Repo-relative file (full path; the basename is render-only).
    pub file: String,
    /// Symbol label (e.g. `(*MemoryRepo).Get`), or the file path for a file-row
    /// selection.
    pub label: String,
    /// `added` / `modified` / `removed`.
    pub change: &'static str,
    /// Exactly one interpretation sentence (see the dispatcher's deterministic builder).
    pub interpretation: String,
    /// Who produced `interpretation`.
    pub interpretation_source: InterpretationSource,
}

/// Provenance of a [`SelectedChange`] interpretation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InterpretationSource {
    /// Built from `ChangedSymbolInfo` (kind, change kind, hunk count, signature touch).
    #[default]
    Deterministic,
    /// A validated, epoch-matched AI result explicitly tied to the selected entity.
    Ai,
}

/// One row in an [`ImpactList`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactRow {
    /// Display label (real symbol or file name).
    pub label: String,
    /// Relationship suffix, e.g. `calls`, `implements`, `references`.
    pub relation: &'static str,
    /// `true` when this entity is itself part of the change.
    pub changed: bool,
    /// `true` when a diagnostic badge should be shown.
    pub has_diagnostic: bool,
}

/// A typed status-bar message: concise text, optional full detail, and severity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusMessage {
    /// Concise message for the one-line footer (empty: fall back to the selected path).
    pub text: String,
    /// Full diagnostic shown by the click-open details dialog. When absent, the dialog
    /// uses [`Self::text`]. This must retain information omitted from the footer summary.
    pub detail: Option<String>,
    /// Severity driving the bar's styling.
    pub level: StatusLevel,
}

/// Severity of a [`StatusMessage`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StatusLevel {
    /// Neutral feedback (confirmations, progress).
    #[default]
    Info,
    /// Degraded functionality (git-only mode, AI failure, recovered picker override).
    Warning,
    /// A hard failure (analysis could not run).
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default snapshot is an empty but renderable frame (spec §4): no selection,
    /// idle impact lists, empty typed status.
    #[test]
    fn defaults_are_an_empty_renderable_frame() {
        let snap = UiSnapshot::default();
        assert!(snap.impact.selected_change.is_none());
        assert_eq!(snap.impact.callers, ImpactList::default());
        assert_eq!(snap.impact.downstream, ImpactList::default());
        assert_eq!(snap.impact.callers.state, ImpactLoadState::Idle);
        assert_eq!(snap.impact.downstream.state, ImpactLoadState::Idle);
        assert!(snap.impact.note.is_empty());
        assert_eq!(snap.status, StatusMessage::default());
        assert_eq!(snap.status.level, StatusLevel::Info);
        assert!(snap.status.detail.is_none());
        assert!(snap.diff.focused_symbol.is_none());
        assert!(snap.files.is_empty());
    }

    /// The boot placeholder reports progress through the typed status too, keeping the
    /// legacy `message` field as its text mirror.
    #[test]
    fn placeholder_reports_scanning_via_typed_status() {
        let snap = UiSnapshot::placeholder();
        assert_eq!(snap.status.text, "scanning repository…");
        assert_eq!(snap.status.level, StatusLevel::Info);
        assert_eq!(snap.message, snap.status.text);
        assert!(snap.refreshing);
    }

    #[test]
    fn ai_failure_status_retains_the_complete_reason_independently_of_the_footer() {
        let reason = "provider HTTP 422\nsecond line that the footer cannot show";
        let snap = UiSnapshot {
            ai: codescope_core::AiStatus::Failed {
                reason: reason.to_string(),
            },
            status: StatusMessage {
                text: "automatic retry queued".to_string(),
                detail: None,
                level: StatusLevel::Info,
            },
            ..UiSnapshot::default()
        };

        let failure = snap.ai_failure_status().expect("retained AI failure");
        assert_eq!(failure.text, "AI failed; showing known relationships");
        assert_eq!(failure.detail.as_deref(), Some(reason));
        assert_eq!(failure.level, StatusLevel::Warning);
    }

    /// Defaults must draw: an empty frame is what a user sees before the first analysis
    /// lands, so rendering it must never panic.
    #[test]
    fn default_snapshots_render_an_empty_frame() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let app = crate::app::App::new();
        terminal
            .draw(|f| crate::render::render(f, &app, &UiSnapshot::default()))
            .expect("the default snapshot renders");
        terminal
            .draw(|f| crate::render::render(f, &app, &UiSnapshot::placeholder()))
            .expect("the placeholder snapshot renders");
    }
}
