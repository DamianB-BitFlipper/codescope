//! The immutable UI payload the dispatcher publishes and the TUI renders.
//!
//! `UiSnapshot` is owned here (not in core) because it is a *rendering* model: it flattens
//! git/analysis/AI state into display rows so the renderer never reaches into `git`, `lsp`,
//! or `ai` crates. The binary assembles it; `codescope-tui` only consumes it.

use codescope_core::{AiStatus, ChangeScope, Epoch, LsStatus};

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
    /// Center pane: the focused diff for the current selection.
    pub diff: DiffPane,
    /// Right pane: the semantic view for the current selection.
    pub semantic: SemanticPane,
    /// Language-server status for the top bar.
    pub ls: LsStatus,
    /// AI status for the top bar.
    pub ai: AiStatus,
    /// The AI model currently selected (empty when AI is off).
    pub ai_model: String,
    /// Models the provider advertises (for the picker modal; empty until fetched).
    pub available_models: Vec<String>,
    /// The base ref the `Branch` scope compares against (empty until known). Shown in the
    /// top bar; defaults to the nearest ancestor branch, overridable via the base picker.
    pub base_ref: String,
    /// Base candidates for the picker modal (empty until fetched).
    pub available_bases: Vec<String>,
    /// Transient status/help message for the bottom bar.
    pub message: String,
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
            diff: DiffPane::default(),
            semantic: SemanticPane::default(),
            ls: LsStatus::Starting,
            ai: AiStatus::Disabled,
            ai_model: String::new(),
            available_models: Vec::new(),
            base_ref: String::new(),
            available_bases: Vec::new(),
            message: String::new(),
            epoch: Epoch::ZERO,
            refreshing: false,
        }
    }
}

impl UiSnapshot {
    /// A boot-time placeholder shown before the first analysis completes.
    #[must_use]
    pub fn placeholder() -> Self {
        UiSnapshot {
            message: "scanning repository…".to_string(),
            refreshing: true,
            ..UiSnapshot::default()
        }
    }

    /// `true` when there is nothing to show yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
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

/// One row in the left "changed files + symbols" pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    /// Repo-relative path (display string).
    pub path: String,
    /// Short status badge: `M`, `A`, `D`, `R`, `?`, `U`.
    pub status: &'static str,
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
}

/// The center diff pane: a focused unified diff for the current selection.
#[derive(Debug, Clone, Default)]
pub struct DiffPane {
    /// Title (file path or symbol name).
    pub title: String,
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
    /// Title of the current view (e.g. "callers of GetDisplayName").
    pub title: String,
    /// Tree rows (already indented via `depth`).
    pub rows: Vec<SemRow>,
    /// A one-line note when the data is partial/approximate/AI-interpretive.
    pub note: String,
    /// `true` when this view came from the AI plan (vs the deterministic fallback).
    pub ai_generated: bool,
}

/// One row in the semantic tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemRow {
    /// Indentation depth (0 = root).
    pub depth: u16,
    /// Display label (real symbol/file name).
    pub label: String,
    /// Relationship tag, e.g. `calls`, `implements`, `changed`.
    pub relation: &'static str,
    /// `true` for nodes that are themselves part of the change.
    pub changed: bool,
    /// `true` when a diagnostic badge should be shown.
    pub has_diagnostic: bool,
}
