//! Application state and pure `Action` transitions. No I/O — the run loop feeds it
//! [`UiSnapshot`]s and [`Action`]s; rendering reads it.

use codescope_core::ChangeScope;

use crate::action::{next_scope, Action, PlanNodeTarget};
use crate::divider::DividerSizes;
use crate::scroll::ScrollRegionId;
use crate::snapshot::{DiffRow, StatusMessage, UiSnapshot};

/// The three focusable panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    /// Left: changed files + symbols.
    #[default]
    Files,
    /// Center: focused diff.
    Diff,
    /// Bottom (full width): the deterministic Impact view (selected change, callers,
    /// downstream). Renamed from `Semantic` in the reference redesign (docs/review/15).
    Impact,
}

/// Stable, repository-independent view preferences that may be restored between runs.
///
/// Selection, focus, scroll positions, zoom, open modals, and repository scope are
/// intentionally absent: those are session or repository state, not global preferences.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UiPreferences {
    /// Whether long diff rows wrap instead of clipping and horizontally scrolling.
    pub diff_wrap: bool,
    /// Requested extent of every structural divider.
    pub dividers: DividerSizes,
}

/// View-state for the running app.
#[derive(Debug, Default)]
pub struct App {
    /// The latest published snapshot.
    pub snapshot: UiSnapshot,
    /// Which pane has focus.
    pub focused: Pane,
    /// Selected row in the files pane (flattened file+symbol index).
    pub file_sel: usize,
    /// Independent physical-row viewport used after wheel-scrolling Files.
    pub files_scroll: usize,
    /// `false` keeps keyboard selection visible; `true` lets the wheel inspect rows without
    /// changing selection or retargeting the diff/Impact panes.
    pub files_scroll_detached: bool,
    /// Vertical scroll of the diff pane (a logical-row anchor; the renderer maps it to a
    /// visual line).
    pub diff_scroll: u16,
    /// Horizontal scroll of the diff pane (raw mode: long lines are clipped + scrolled).
    pub diff_hscroll: u16,
    /// 1-based hunk under the diff scroll anchor; 0 when the diff has no hunks. App-owned
    /// view state (docs/review/15 §4): the snapshot's `total_hunks` is immutable data, but
    /// the current hunk follows navigation, so it must survive snapshot publishes.
    pub current_hunk: usize,
    /// Requested extents of every draggable structural divider.
    pub dividers: DividerSizes,
    /// Whether the focused pane is zoomed to fill the whole body (`z`).
    pub zoomed: bool,
    /// Vertical scroll offset of the generated Impact rows.
    pub ai_plan_scroll: usize,
    /// Generated-plan node currently under the mouse. This is transient view state and
    /// drives both node emphasis and linked diff-row highlighting.
    pub hovered_plan_node: Option<PlanNodeTarget>,
    /// Generated-plan node whose deeper detail/code-reference inspector is open.
    pub expanded_plan_node: Option<PlanNodeTarget>,
    /// Independent offset for the deterministic incoming-callers list.
    pub callers_scroll: usize,
    /// Independent offset for the deterministic downstream-relationships list.
    pub downstream_scroll: usize,
    /// Whether the diff pane smart-wraps long lines (`W`); off = raw clip + h-scroll.
    /// The reference mode is raw (`wrap off`), docs/review/15 §3.4.
    pub diff_wrap: bool,
    /// Whether the help modal is open.
    pub show_help: bool,
    /// Frozen copy of the status message whose full detail overlay is open. Freezing it
    /// keeps an automatic retry from replacing the error while the user reads it.
    pub status_detail: Option<StatusMessage>,
    /// Whether the AI model picker modal is open.
    pub show_model_picker: bool,
    /// Selected row in the model picker (into the filtered list).
    pub model_sel: usize,
    /// Type-to-filter query of the model picker.
    pub model_query: String,
    /// Whether the comparison-base picker modal is open.
    pub show_base_picker: bool,
    /// Selected row in the base picker (into the filtered list).
    pub base_sel: usize,
    /// Type-to-filter query of the base picker.
    pub base_query: String,
    /// Set when the user asked to quit.
    pub should_quit: bool,
}

impl App {
    /// A fresh app (branch scope, raw diff mode, files pane at the default width).
    #[must_use]
    pub fn new() -> Self {
        App::default()
    }

    /// Build a fresh app with global, repository-independent view preferences restored.
    #[must_use]
    pub fn with_preferences(preferences: UiPreferences) -> Self {
        App {
            diff_wrap: preferences.diff_wrap,
            dividers: preferences.dividers,
            ..App::default()
        }
    }

    /// Capture only the stable view preferences suitable for global persistence.
    #[must_use]
    pub fn preferences(&self) -> UiPreferences {
        UiPreferences {
            diff_wrap: self.diff_wrap,
            dividers: self.dividers,
        }
    }

    /// Replace the snapshot, clamping selection into the new bounds.
    pub fn update(&mut self, snapshot: UiSnapshot) {
        // The diff pane follows the files-pane selection: when the dispatcher retargets it
        // to a different file, start at the top of the new diff instead of keeping a scroll
        // offset computed against the old one. `DiffPane::title` is the file path today
        // (MERGE: the dispatcher half renames it to `file_path`; the comparison stays).
        let retargeted = self.snapshot.diff.title != snapshot.diff.title;
        let generated_retargeted = self.snapshot.semantic.note != snapshot.semantic.note
            || self.snapshot.semantic.plan != snapshot.semantic.plan;
        let impact_retargeted =
            self.snapshot.impact.selected_change != snapshot.impact.selected_change;
        if retargeted {
            self.diff_scroll = 0;
            self.diff_hscroll = 0;
            self.current_hunk = usize::from(snapshot.diff.total_hunks > 0);
        } else if self.current_hunk == 0 && snapshot.diff.total_hunks > 0 {
            // First diff for this path (hunks just arrived): start at hunk 1.
            self.current_hunk = 1;
        }
        // Selection identity survives the swap (review 18 M5): an expanded file filling
        // in its symbol rows shifts flat indices; without re-resolving by (file, symbol)
        // the cursor would slide onto whatever row now holds the old ordinal.
        let keep = self
            .selected_file_symbol()
            .map(|(f, sym)| (f.path.clone(), sym.map(|s| (s.name.clone(), s.position))));
        self.snapshot = snapshot;
        if generated_retargeted {
            self.ai_plan_scroll = 0;
            self.hovered_plan_node = None;
            self.expanded_plan_node = None;
        }
        if impact_retargeted {
            self.callers_scroll = 0;
            self.downstream_scroll = 0;
        }
        self.clamp();
        if let Some((file, sym)) = keep {
            self.restore_selection(&file, sym.as_ref());
        }
    }

    /// Re-resolve a previously selected (file, symbol) against the current snapshot:
    /// the symbol row when it still exists, else the owning file row, else the nearest
    /// row that survived (clamp).
    fn restore_selection(&mut self, file: &str, sym: Option<&(String, Option<(u32, u32)>)>) {
        let mut flat = 0usize;
        let mut file_row_idx: Option<usize> = None;
        let mut sym_row_idx: Option<usize> = None;
        for f in &self.snapshot.files {
            if f.path == file {
                file_row_idx = Some(flat);
                if let Some((name, pos)) = sym {
                    if f.expanded {
                        for (si, s) in f.symbols.iter().enumerate() {
                            let same = s.name == *name
                                && (pos.is_none() || s.position == *pos || s.position.is_none());
                            if same {
                                sym_row_idx = Some(flat + 1 + si);
                                break;
                            }
                        }
                    }
                }
                break;
            }
            flat += 1 + if f.expanded { f.symbols.len() } else { 0 };
        }
        self.file_sel = sym_row_idx
            .or(file_row_idx)
            .unwrap_or_else(|| self.file_sel.min(self.flat_file_rows().saturating_sub(1)));
    }

    /// Apply an action to the view state. I/O actions (RefreshGit/Ai*) only toggle flags
    /// here; the dispatcher observes them via the returned snapshot channel separately.
    pub fn apply(&mut self, action: Action) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::ToggleHelp => self.show_help = !self.show_help,
            Action::ToggleStatusDetail => {
                self.status_detail =
                    if self.status_detail.is_some() || self.snapshot.status.text.is_empty() {
                        None
                    } else {
                        Some(self.snapshot.status.clone())
                    };
            }
            Action::SetFileExpanded { path, expanded } => {
                self.set_file_expanded(&path, expanded);
            }
            // Files expansion is dispatcher-owned and resolved in run.rs. In Impact,
            // Space toggles the node currently under the pointer; no hover means no-op.
            Action::ToggleExpand => {
                if self.focused == Pane::Impact {
                    if let Some(target) = self.hovered_plan_node.clone() {
                        self.toggle_plan_node(target);
                    }
                }
            }
            Action::Focus(p) => self.focused = p,
            Action::Down => self.move_sel(1),
            Action::Up => self.move_sel(-1),
            Action::HalfPageDown => self.page(10),
            Action::HalfPageUp => self.page(-10),
            Action::PageDown => self.page(20),
            Action::PageUp => self.page(-20),
            Action::Top => self.top(),
            Action::Bottom => self.bottom(),
            Action::Activate => self.activate(),
            Action::ToggleZoom => self.zoomed = !self.zoomed,
            Action::ToggleWrap => self.diff_wrap = !self.diff_wrap,
            Action::ResetHScroll => self.diff_hscroll = 0,
            Action::HoverPlanNode(target) => {
                self.hovered_plan_node = target.filter(|target| self.plan_node(target).is_some());
            }
            Action::TogglePlanNode(target) => {
                self.focused = Pane::Impact;
                self.hovered_plan_node = Some(target.clone());
                self.toggle_plan_node(target);
            }
            // Mouse: select a file/symbol row by logical index and focus Files. The
            // selection tracker emits the same SelectionChanged a keyboard move would.
            Action::SelectFileRow { logical_index } => {
                self.focused = Pane::Files;
                self.file_sel = logical_index.min(self.flat_file_rows().saturating_sub(1));
                self.files_scroll_detached = false;
            }
            Action::ScrollRegion { region, offset } => self.set_scroll_region(region, offset),
            // Generic mouse resize: identity owns its floor; live layout owns viewport
            // constraints and can yield without overwriting the stable request.
            Action::ResizeDivider { divider, extent } => self.dividers.set(divider, extent),
            Action::Collapse => match self.focused {
                // Wrapped mode has no hidden horizontal state: h must not move it.
                Pane::Diff if self.diff_wrap => {}
                Pane::Diff => self.diff_hscroll = self.diff_hscroll.saturating_sub(8),
                Pane::Impact => self.expanded_plan_node = None,
                // Files-pane expansion is dispatcher-owned: run.rs routes Space/h/l to
                // the targeted SetFileExpanded command; App applies no local tree
                // mutation for them (review 18 m4).
                Pane::Files => {}
            },
            Action::Expand => match self.focused {
                Pane::Diff if self.diff_wrap => {}
                Pane::Diff => self.diff_hscroll = self.diff_hscroll.saturating_add(8),
                Pane::Impact => {
                    if let Some(target) = self.hovered_plan_node.clone() {
                        if self.plan_node(&target).is_some() {
                            self.expanded_plan_node = Some(target);
                        }
                    }
                }
                Pane::Files => {}
            },
            Action::ScopeStaged => self.set_scope(ChangeScope::Staged),
            Action::ScopeUnstaged => self.set_scope(ChangeScope::Unstaged),
            Action::ScopeBranch => self.set_scope(ChangeScope::Branch),
            Action::ScopeWorking => self.set_scope(ChangeScope::Working),
            Action::ScopeCycle => self.set_scope(next_scope(self.snapshot.scope)),
            Action::NextHunk => self.jump_hunk(1),
            Action::PrevHunk => self.jump_hunk(-1),
            Action::ModelPicker => {
                self.show_model_picker = !self.show_model_picker;
                self.model_sel = 0;
                self.model_query.clear();
            }
            Action::BasePicker => {
                self.show_base_picker = !self.show_base_picker;
                self.base_sel = 0;
                self.base_query.clear();
            }
            Action::PickerInput(c) => {
                if self.show_model_picker {
                    self.model_query.push(c);
                } else if self.show_base_picker {
                    self.base_query.push(c);
                }
            }
            Action::PickerBackspace => {
                if self.show_model_picker {
                    self.model_query.pop();
                } else if self.show_base_picker {
                    self.base_query.pop();
                }
            }
            // ModelSelected/BaseSelected are applied by the dispatcher (it owns the
            // AiService / base override).
            // RefreshGit is a dispatcher concern; nothing to do here.
            // SelectionChanged / SelectSymbol are derived from the view state by the run
            // loop and forwarded to the dispatcher; applying them locally would be a no-op.
            Action::ModelSelected(_)
            | Action::BaseSelected(_)
            | Action::PersistUiPreferences(_)
            | Action::SelectSymbol { .. }
            | Action::SelectionChanged { .. }
            | Action::RefreshGit
            | Action::None => {}
        }
        self.clamp();
    }

    fn set_scope(&mut self, scope: ChangeScope) {
        self.snapshot.scope = scope;
        self.file_sel = 0;
        self.files_scroll = 0;
        self.files_scroll_detached = false;
        self.diff_scroll = 0;
        self.diff_hscroll = 0;
        self.callers_scroll = 0;
        self.downstream_scroll = 0;
        self.ai_plan_scroll = 0;
        self.hovered_plan_node = None;
        self.expanded_plan_node = None;
        self.current_hunk = usize::from(self.snapshot.diff.total_hunks > 0);
    }

    /// Resolve one plan-local UI target against the currently displayed generated plan.
    #[must_use]
    pub fn plan_node(&self, target: &PlanNodeTarget) -> Option<&codescope_core::PlanNode> {
        self.snapshot
            .semantic
            .plan
            .as_ref()?
            .forms
            .get(target.form)?
            .nodes
            .iter()
            .find(|node| node.id == target.id)
    }

    /// The currently hovered generated-plan node, when its target still resolves.
    #[must_use]
    pub fn hovered_node(&self) -> Option<&codescope_core::PlanNode> {
        self.hovered_plan_node
            .as_ref()
            .and_then(|target| self.plan_node(target))
    }

    /// Node whose code links are active. Transient hover wins; an expanded node remains
    /// pinned while the pointer moves into the diff so the reviewer can inspect its rows.
    #[must_use]
    pub fn active_code_node(&self) -> Option<&codescope_core::PlanNode> {
        self.hovered_node().or_else(|| {
            self.expanded_plan_node
                .as_ref()
                .and_then(|target| self.plan_node(target))
        })
    }

    fn toggle_plan_node(&mut self, target: PlanNodeTarget) {
        if self.plan_node(&target).is_none() {
            return;
        }
        if self.expanded_plan_node.as_ref() == Some(&target) {
            self.expanded_plan_node = None;
        } else {
            self.expanded_plan_node = Some(target);
            // Expanded details render in a fixed strip above the form, so pinning always
            // reveals them even when the generated pane had been scrolled.
            self.ai_plan_scroll = 0;
        }
    }

    /// Model candidates matching the picker's filter query (the visible list).
    #[must_use]
    pub fn filtered_models(&self) -> Vec<&str> {
        filter_candidates(&self.snapshot.available_models, &self.model_query)
    }

    /// Base candidates matching the picker's filter query (the visible list).
    #[must_use]
    pub fn filtered_bases(&self) -> Vec<&str> {
        filter_candidates(&self.snapshot.available_bases, &self.base_query)
    }

    fn move_sel(&mut self, delta: i32) {
        if self.show_model_picker {
            let len = self.filtered_models().len();
            self.model_sel = step(self.model_sel, delta, len);
            return;
        }
        if self.show_base_picker {
            let len = self.filtered_bases().len();
            self.base_sel = step(self.base_sel, delta, len);
            return;
        }
        match self.focused {
            Pane::Files => {
                let len = self.flat_file_rows();
                self.file_sel = step(self.file_sel, delta, len);
                self.files_scroll_detached = false;
            }
            // The left relationship stack is fixed; movement in the combined Impact
            // pane scrolls the generated breakdown on the right.
            Pane::Impact => {
                self.scroll_ai_plan(delta);
            }
            Pane::Diff => self.scroll_diff(delta),
        }
    }

    /// Page keys scroll the generated breakdown when Impact is focused, otherwise diff.
    fn page(&mut self, delta: i32) {
        if self.focused == Pane::Impact {
            self.scroll_ai_plan(delta);
        } else {
            self.scroll_diff(delta);
        }
    }

    fn scroll_ai_plan(&mut self, delta: i32) {
        const MAX_GENERATED_SCROLL: usize = 10_000;
        if self.snapshot.semantic.plan.is_none() && self.snapshot.impact.selected_change.is_none() {
            self.ai_plan_scroll = 0;
            return;
        }
        if delta < 0 {
            self.ai_plan_scroll = self
                .ai_plan_scroll
                .saturating_sub(delta.unsigned_abs() as usize);
        } else {
            self.ai_plan_scroll = self
                .ai_plan_scroll
                .saturating_add(delta as usize)
                .min(MAX_GENERATED_SCROLL);
        }
    }

    fn scroll_ai_plan_to(&mut self, pos: usize) {
        self.ai_plan_scroll = if self.snapshot.semantic.plan.is_some()
            || self.snapshot.impact.selected_change.is_some()
        {
            pos.min(10_000)
        } else {
            0
        };
    }

    fn scroll_diff(&mut self, delta: i32) {
        let len = self.snapshot.diff.rows.len() as i32;
        let cur = self.diff_scroll as i32;
        self.diff_scroll = (cur + delta).clamp(0, len.saturating_sub(1).max(0)) as u16;
        self.sync_current_hunk();
    }

    fn set_scroll_region(&mut self, region: ScrollRegionId, offset: usize) {
        match region {
            ScrollRegionId::Files => {
                self.files_scroll = offset;
                self.files_scroll_detached = true;
            }
            ScrollRegionId::Diff => {
                self.diff_scroll = u16::try_from(offset).unwrap_or(u16::MAX);
                self.sync_current_hunk();
            }
            ScrollRegionId::Callers => self.callers_scroll = offset,
            ScrollRegionId::Downstream => self.downstream_scroll = offset,
            ScrollRegionId::GeneratedImpact => {
                self.ai_plan_scroll = offset;
                self.hovered_plan_node = None;
            }
        }
    }

    fn top(&mut self) {
        match self.focused {
            Pane::Files => {
                self.file_sel = 0;
                self.files_scroll_detached = false;
            }
            Pane::Impact => self.scroll_ai_plan_to(0),
            Pane::Diff => {
                self.diff_scroll = 0;
                self.sync_current_hunk();
            }
        }
    }

    fn bottom(&mut self) {
        match self.focused {
            Pane::Files => {
                self.file_sel = self.flat_file_rows().saturating_sub(1);
                self.files_scroll_detached = false;
            }
            Pane::Impact => self.scroll_ai_plan_to(usize::MAX),
            Pane::Diff => {
                self.diff_scroll = self.snapshot.diff.rows.len().saturating_sub(1) as u16;
                self.sync_current_hunk();
            }
        }
    }

    /// Recompute `current_hunk` from the scroll anchor: the number of hunk-header rows at
    /// or before the anchor (the 1-based index of the hunk the user is looking at), or 0
    /// when the diff has no hunks (docs/review/15 §4 "Hunk state ownership").
    fn sync_current_hunk(&mut self) {
        let total = self.snapshot.diff.total_hunks;
        if total == 0 {
            self.current_hunk = 0;
            return;
        }
        // `..=scroll` inclusive: an anchor sitting exactly on a header row is that hunk.
        let upto = (self.diff_scroll as usize + 1).min(self.snapshot.diff.rows.len());
        let seen = self.snapshot.diff.rows[..upto]
            .iter()
            .filter(|r| matches!(r, DiffRow::HunkHeader(_)))
            .count();
        self.current_hunk = seen.clamp(1, total);
    }

    /// Optimistically apply the targeted expansion command so the frame the user sees
    /// matches their keypress; the dispatcher remains the source of truth and its next
    /// snapshot reconciles `expanded` (and fills `semantic`/symbols). Idempotent.
    fn set_file_expanded(&mut self, path: &str, expanded: bool) {
        if let Some(row) = self.snapshot.files.iter_mut().find(|f| f.path == path) {
            row.expanded = expanded;
        }
        self.files_scroll_detached = false;
        self.clamp();
    }

    /// `Enter`: in the files pane, jump diff+semantic to the selection (handled by the
    /// dispatcher via a forwarded action would be ideal; locally we at least scroll the
    /// diff to the selected file's first hunk). In other panes, no-op for now.
    fn activate(&mut self) {
        // Expansion is dispatcher-owned (review 18 m4): Enter forwards through the
        // targeted SetFileExpanded path in run.rs; App performs no local expansion.
    }

    fn jump_hunk(&mut self, delta: i32) {
        let total = self.snapshot.diff.total_hunks as i32;
        if total == 0 {
            return;
        }
        let cur = self.current_hunk as i32;
        let next = (cur + delta).clamp(1, total) as usize;
        self.current_hunk = next;
        // Anchor the scroll to the hunk's header row so the jump is visible. The renderer
        // maps this logical row through first_visual_line when wrap mode is on.
        let mut seen = 0usize;
        for (i, row) in self.snapshot.diff.rows.iter().enumerate() {
            if matches!(row, DiffRow::HunkHeader(_)) {
                seen += 1;
                if seen == next {
                    self.diff_scroll = i as u16;
                    break;
                }
            }
        }
    }

    /// The full repo-relative path of the file under the files-pane selection (symbol rows
    /// map to their file). The footer shows this unelided path when no message is pending.
    #[must_use]
    /// The `(path, desired expanded)` pair a Tab press right now would command: the file
    /// under the selection (symbol rows map to their file) and the inverse of its current
    /// expansion. Resolved against the app's snapshot — the same flattened rows the user
    /// sees (review 18 M4).
    pub fn file_toggle_target(&self) -> Option<(String, bool)> {
        let mut idx = self.file_sel;
        for f in &self.snapshot.files {
            if idx == 0 {
                return Some((f.path.clone(), !f.expanded));
            }
            idx -= 1;
            if f.expanded {
                if idx < f.symbols.len() {
                    return Some((f.path.clone(), false)); // on a symbol row: collapse
                }
                idx -= f.symbols.len();
            }
        }
        None
    }

    /// The selected file's repo-relative path (symbol rows map to their owning file).
    pub fn selected_file_path(&self) -> Option<&str> {
        // The shared projection decides what is selectable (review 24 M4).
        crate::file_rows::resolve_logical(&self.snapshot.files, self.file_sel)
            .map(|(f, _)| f.path.as_str())
    }

    /// The symbol name when the files-pane selection sits on a symbol row (the diff
    /// title's `focused_symbol`). MERGE: drop this local derivation once the snapshot
    /// publishes `DiffPane::focused_symbol` (docs/review/15 §4).
    #[must_use]
    pub fn selected_symbol_name(&self) -> Option<&str> {
        let mut idx = self.file_sel;
        for f in &self.snapshot.files {
            if idx == 0 {
                return None;
            }
            idx -= 1;
            if f.expanded {
                if idx < f.symbols.len() {
                    return Some(f.symbols[idx].name.as_str());
                }
                idx -= f.symbols.len();
            }
        }
        None
    }

    /// The index into `snapshot.files` of the file under the flattened files-pane
    /// selection (symbol rows map to their file's index).
    #[must_use]
    pub fn selected_file_index(&self) -> Option<usize> {
        let mut idx = self.file_sel;
        for (i, f) in self.snapshot.files.iter().enumerate() {
            if idx == 0 {
                return Some(i);
            }
            idx -= 1;
            if f.expanded {
                if idx < f.symbols.len() {
                    return Some(i);
                }
                idx -= f.symbols.len();
            }
        }
        None
    }

    /// The file row and the symbol row (when the selection is on a symbol) under the
    /// flattened files-pane selection.
    #[must_use]
    pub fn selected_file_symbol(
        &self,
    ) -> Option<(
        &crate::snapshot::FileRow,
        Option<&crate::snapshot::SymbolRow>,
    )> {
        // The shared projection decides what is selectable (review 24 M4).
        crate::file_rows::resolve_logical(&self.snapshot.files, self.file_sel)
    }

    /// Flattened file+symbol row count (expanded symbols included).
    #[must_use]
    pub fn flat_file_rows(&self) -> usize {
        self.snapshot
            .files
            .iter()
            .map(|f| 1 + if f.expanded { f.symbols.len() } else { 0 })
            .sum()
    }

    /// Physical first row for the files viewport. Keyboard navigation follows selection;
    /// a mouse-wheel inspection uses its independent offset until selection moves again.
    #[must_use]
    pub fn files_first_visible(&self, capacity: usize) -> usize {
        if self.files_scroll_detached {
            self.files_scroll.min(
                crate::file_rows::project(&self.snapshot.files)
                    .len()
                    .saturating_sub(capacity),
            )
        } else {
            crate::file_rows::first_visible(&self.snapshot.files, self.file_sel, capacity)
        }
    }

    fn clamp(&mut self) {
        self.file_sel = self.file_sel.min(self.flat_file_rows().saturating_sub(1));
        self.files_scroll = self.files_scroll.min(
            crate::file_rows::project(&self.snapshot.files)
                .len()
                .saturating_sub(1),
        );
        let max_scroll = self.snapshot.diff.rows.len().saturating_sub(1) as u16;
        self.diff_scroll = self.diff_scroll.min(max_scroll);
        // Keep the hunk cursor inside the snapshot's (immutable) total.
        let total = self.snapshot.diff.total_hunks;
        self.current_hunk = if total == 0 {
            0
        } else {
            self.current_hunk.clamp(1, total)
        };
        self.model_sel = self
            .model_sel
            .min(self.filtered_models().len().saturating_sub(1));
        self.base_sel = self
            .base_sel
            .min(self.filtered_bases().len().saturating_sub(1));
        if self.snapshot.semantic.plan.is_none() && self.snapshot.impact.selected_change.is_none() {
            self.ai_plan_scroll = 0;
        } else {
            self.ai_plan_scroll = self.ai_plan_scroll.min(10_000);
        }
        self.callers_scroll = self
            .callers_scroll
            .min(self.snapshot.impact.callers.rows.len().saturating_sub(1));
        self.downstream_scroll = self
            .downstream_scroll
            .min(self.snapshot.impact.downstream.rows.len().saturating_sub(1));
    }
}

/// Case-insensitive substring filter over picker candidates: the visible list while a
/// query is typed. An empty query returns everything.
#[must_use]
pub fn filter_candidates<'a>(items: &'a [String], query: &str) -> Vec<&'a str> {
    let query = query.to_lowercase();
    items
        .iter()
        .map(String::as_str)
        .filter(|item| item.to_lowercase().contains(&query))
        .collect()
}

fn step(cur: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (cur as i32 + delta).clamp(0, len as i32 - 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::divider::DividerId;
    use crate::snapshot::{FileRow, SymbolRow};

    fn row(name: &str, expanded: bool, symbols: usize) -> FileRow {
        FileRow {
            path: name.to_string(),
            status: "M",
            semantic: if symbols > 0 {
                crate::snapshot::FileSemanticLoad::Ready
            } else {
                crate::snapshot::FileSemanticLoad::Unloaded
            },
            changed_symbol_count: symbols,
            added_lines: 0,
            removed_lines: 0,
            symbols: (0..symbols)
                .map(|i| SymbolRow {
                    name: format!("sym{i}"),
                    change: "modified",
                    confidence: "",
                    has_diagnostic: false,
                    position: None,
                })
                .collect(),
            expanded,
        }
    }

    fn app_with_files() -> App {
        let mut app = App::new();
        app.update(UiSnapshot {
            files: vec![row("a.go", true, 2), row("b.go", false, 3)],
            ..UiSnapshot::default()
        });
        app
    }

    #[test]
    fn move_selection_clamps() {
        let mut app = app_with_files();
        // a.go + 2 symbols + b.go = 4 rows
        assert_eq!(app.flat_file_rows(), 4);
        for _ in 0..10 {
            app.apply(Action::Down);
        }
        assert_eq!(app.file_sel, 3);
        for _ in 0..10 {
            app.apply(Action::Up);
        }
        assert_eq!(app.file_sel, 0);
    }

    #[test]
    fn hover_scroll_offsets_are_independent_of_focus_and_selection() {
        let mut app = app_with_files();
        app.focused = Pane::Diff;
        app.file_sel = 2;
        app.apply(Action::ScrollRegion {
            region: ScrollRegionId::Files,
            offset: 1,
        });
        assert_eq!(app.focused, Pane::Diff);
        assert_eq!(app.file_sel, 2);
        assert_eq!(app.files_scroll, 1);
        assert!(app.files_scroll_detached);

        let relationships = (0..10)
            .map(|index| crate::snapshot::ImpactRow {
                label: format!("rel-{index}"),
                relation: "calls",
                changed: false,
                has_diagnostic: false,
            })
            .collect::<Vec<_>>();
        app.snapshot.impact.callers.rows = relationships.clone();
        app.snapshot.impact.downstream.rows = relationships;
        app.snapshot.impact.selected_change = Some(crate::snapshot::SelectedChange {
            file: "a.go".to_string(),
            label: "selected".to_string(),
            change: "modified",
            interpretation: "changed".to_string(),
            interpretation_source: crate::snapshot::InterpretationSource::Deterministic,
        });
        app.apply(Action::ScrollRegion {
            region: ScrollRegionId::Callers,
            offset: 4,
        });
        app.apply(Action::ScrollRegion {
            region: ScrollRegionId::Downstream,
            offset: 5,
        });
        app.apply(Action::ScrollRegion {
            region: ScrollRegionId::GeneratedImpact,
            offset: 6,
        });
        assert_eq!(app.callers_scroll, 4);
        assert_eq!(app.downstream_scroll, 5);
        assert_eq!(app.ai_plan_scroll, 6);

        app.focused = Pane::Files;
        app.apply(Action::Down);
        assert!(
            !app.files_scroll_detached,
            "keyboard navigation resumes selection-follow"
        );
    }

    #[test]
    fn collapse_hides_symbols() {
        let mut app = app_with_files();
        app.file_sel = 0;
        // Expansion is dispatcher-owned: the app applies the targeted command (what
        // run.rs sends after resolving the selected file at keypress time).
        let path = app.snapshot.files[0].path.clone();
        app.apply(Action::SetFileExpanded {
            path,
            expanded: false,
        });
        assert!(!app.snapshot.files[0].expanded);
        assert_eq!(app.flat_file_rows(), 2); // a.go + b.go
    }

    #[test]
    fn focus_keys_direct() {
        // Tab no longer cycles panes; 1/2/3 focus directly.
        let mut app = App::new();
        assert_eq!(app.focused, Pane::Files);
        app.apply(Action::Focus(Pane::Diff));
        assert_eq!(app.focused, Pane::Diff);
        app.apply(Action::Focus(Pane::Impact));
        assert_eq!(app.focused, Pane::Impact);
        app.apply(Action::Focus(Pane::Files));
        assert_eq!(app.focused, Pane::Files);
    }

    #[test]
    fn scope_switch_resets_selection() {
        let mut app = app_with_files();
        app.file_sel = 2;
        app.apply(Action::ScopeUnstaged);
        assert_eq!(app.snapshot.scope, ChangeScope::Unstaged);
        assert_eq!(app.file_sel, 0);
        app.apply(Action::ScopeWorking);
        assert_eq!(app.snapshot.scope, ChangeScope::Working);
    }

    #[test]
    fn scope_cycle_visits_all_scopes() {
        let mut app = App::new();
        assert_eq!(app.snapshot.scope, ChangeScope::Branch);
        let mut seen = vec![app.snapshot.scope];
        for _ in 0..4 {
            app.apply(Action::ScopeCycle);
            seen.push(app.snapshot.scope);
        }
        assert_eq!(
            seen,
            vec![
                ChangeScope::Branch,
                ChangeScope::Staged,
                ChangeScope::Unstaged,
                ChangeScope::Working,
                ChangeScope::Branch,
            ]
        );
    }

    #[test]
    fn quit_and_help() {
        let mut app = App::new();
        app.apply(Action::ToggleHelp);
        assert!(app.show_help);
        app.apply(Action::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn status_detail_freezes_the_clicked_message_until_closed() {
        let mut app = App::new();
        app.snapshot.status = StatusMessage {
            text: "original provider error".to_string(),
            level: crate::snapshot::StatusLevel::Warning,
        };
        app.apply(Action::ToggleStatusDetail);
        assert_eq!(
            app.status_detail
                .as_ref()
                .map(|status| status.text.as_str()),
            Some("original provider error")
        );

        let mut updated = app.snapshot.clone();
        updated.status.text = "automatic retry started".to_string();
        app.update(updated);
        assert_eq!(
            app.status_detail
                .as_ref()
                .map(|status| status.text.as_str()),
            Some("original provider error"),
            "background statuses cannot replace the message being inspected"
        );

        app.apply(Action::ToggleStatusDetail);
        assert!(app.status_detail.is_none());
    }

    #[test]
    fn stable_preferences_restore_without_session_state() {
        let mut dividers = DividerSizes::default();
        dividers.set(DividerId::FilesDiff, 7);
        dividers.set(DividerId::WorkReview, 2);
        dividers.set(DividerId::RelationshipsGenerated, 3);
        let app = App::with_preferences(UiPreferences {
            diff_wrap: true,
            dividers,
        });
        assert!(app.diff_wrap);
        assert_eq!(
            app.dividers.get(DividerId::FilesDiff),
            crate::layout::MIN_FILES_WIDTH
        );
        assert_eq!(
            app.dividers.get(DividerId::WorkReview),
            crate::layout::MIN_IMPACT_HEIGHT
        );
        assert_eq!(
            app.dividers.get(DividerId::RelationshipsGenerated),
            crate::layout::MIN_IMPACT_LEFT_WIDTH
        );
        assert_eq!(app.focused, Pane::Files, "focus is session state");
        assert_eq!(app.file_sel, 0);
        assert!(!app.zoomed);
        assert_eq!(
            app.preferences(),
            UiPreferences {
                diff_wrap: true,
                dividers
            }
        );
    }

    #[test]
    fn base_picker_toggle_and_navigation_clamps() {
        let mut app = App::new();
        let snap = UiSnapshot {
            available_bases: vec![
                "main".to_string(),
                "origin/main".to_string(),
                "develop".to_string(),
            ],
            ..UiSnapshot::default()
        };
        app.update(snap);
        app.apply(Action::BasePicker);
        assert!(app.show_base_picker);
        for _ in 0..10 {
            app.apply(Action::Down);
        }
        assert_eq!(app.base_sel, 2); // clamped at last candidate
        for _ in 0..10 {
            app.apply(Action::Up);
        }
        assert_eq!(app.base_sel, 0);
        app.apply(Action::BasePicker);
        assert!(!app.show_base_picker);
    }

    #[test]
    fn picker_input_filters_and_clamps_selection() {
        let mut app = App::new();
        app.update(UiSnapshot {
            available_bases: vec![
                "main".to_string(),
                "origin/main".to_string(),
                "develop".to_string(),
            ],
            ..UiSnapshot::default()
        });
        app.apply(Action::BasePicker);
        assert_eq!(app.filtered_bases(), vec!["main", "origin/main", "develop"]);

        // Select the last entry, then filter down to the two "main" refs: the selection
        // clamps into the filtered list.
        for _ in 0..2 {
            app.apply(Action::Down);
        }
        assert_eq!(app.base_sel, 2);
        for c in "MAIN".chars() {
            app.apply(Action::PickerInput(c));
        }
        assert_eq!(app.base_query, "MAIN"); // case-insensitive matching below
        assert_eq!(app.filtered_bases(), vec!["main", "origin/main"]);
        assert_eq!(app.base_sel, 1, "selection clamps to the filtered list");

        // Backspace pops the query; empty query shows everything again.
        for _ in 0..5 {
            app.apply(Action::PickerBackspace);
        }
        assert!(app.base_query.is_empty());
        assert_eq!(app.filtered_bases().len(), 3);

        // Closing (Esc) clears the query for the next open.
        app.apply(Action::PickerInput('x'));
        app.apply(Action::BasePicker);
        assert!(!app.show_base_picker);
        assert!(app.base_query.is_empty());
    }

    #[test]
    fn picker_navigation_stays_within_filtered_list() {
        let mut app = App::new();
        app.update(UiSnapshot {
            available_models: vec![
                "openai/gpt-5".to_string(),
                "anthropic/claude-fable-5".to_string(),
            ],
            ..UiSnapshot::default()
        });
        app.apply(Action::ModelPicker);
        app.apply(Action::PickerInput('g')); // only "openai/gpt-5" matches
        assert_eq!(app.filtered_models(), vec!["openai/gpt-5"]);
        for _ in 0..5 {
            app.apply(Action::Down);
        }
        assert_eq!(app.model_sel, 0);
        // Input in a closed picker is inert.
        app.apply(Action::ModelPicker);
        app.apply(Action::PickerInput('z'));
        assert!(app.model_query.is_empty());
    }

    #[test]
    fn hunk_jump_clamps() {
        let mut app = App::new();
        app.snapshot.diff.total_hunks = 3;
        app.current_hunk = 1;
        app.apply(Action::NextHunk);
        assert_eq!(app.current_hunk, 2);
        app.apply(Action::NextHunk);
        assert_eq!(app.current_hunk, 3);
        app.apply(Action::NextHunk);
        assert_eq!(app.current_hunk, 3);
        app.apply(Action::PrevHunk);
        assert_eq!(app.current_hunk, 2);
    }

    #[test]
    fn hunk_jump_anchors_scroll_to_the_header_row() {
        use crate::snapshot::DiffRow;
        let mut app = App::new();
        app.snapshot.diff.rows = vec![
            DiffRow::HunkHeader("@@ -1,2 +1,2 @@".to_string()),
            DiffRow::Context {
                old_ln: 1,
                new_ln: 1,
                text: "a".to_string(),
            },
            DiffRow::HunkHeader("@@ -40,2 +40,2 @@".to_string()),
            DiffRow::Add {
                new_ln: 41,
                text: "b".to_string(),
            },
        ];
        app.snapshot.diff.total_hunks = 2;
        app.current_hunk = 1;
        app.apply(Action::NextHunk);
        assert_eq!(app.current_hunk, 2);
        assert_eq!(
            app.diff_scroll, 2,
            "scroll anchors on the second hunk header"
        );
        app.apply(Action::PrevHunk);
        assert_eq!(app.diff_scroll, 0);
        assert_eq!(app.current_hunk, 1);
    }

    #[test]
    fn scrolling_recomputes_the_current_hunk() {
        use crate::snapshot::DiffRow;
        let mut app = App::new();
        app.focused = Pane::Diff;
        app.snapshot.diff.rows = vec![
            DiffRow::HunkHeader("@@ -1,2 +1,2 @@".to_string()),
            DiffRow::Context {
                old_ln: 1,
                new_ln: 1,
                text: "a".to_string(),
            },
            DiffRow::HunkHeader("@@ -40,2 +40,2 @@".to_string()),
            DiffRow::Add {
                new_ln: 41,
                text: "b".to_string(),
            },
        ];
        app.snapshot.diff.total_hunks = 2;
        app.current_hunk = 1;
        // Ordinary vertical scrolling (not n/N) recomputes the hunk under the anchor.
        app.apply(Action::Bottom);
        assert_eq!(app.diff_scroll, 3);
        assert_eq!(app.current_hunk, 2, "scrolled past the second header");
        app.apply(Action::Up);
        app.apply(Action::Up);
        assert_eq!(app.diff_scroll, 1);
        assert_eq!(app.current_hunk, 1);
        app.apply(Action::Top);
        assert_eq!(app.current_hunk, 1);
    }

    #[test]
    fn current_hunk_resets_when_the_diff_retargets() {
        let mut app = App::new();
        app.snapshot.diff.title = "a.go".to_string();
        app.snapshot.diff.total_hunks = 5;
        app.current_hunk = 4;
        // Same path: the hunk cursor survives a refresh publish.
        app.update(UiSnapshot {
            diff: crate::snapshot::DiffPane {
                title: "a.go".to_string(),
                total_hunks: 5,
                ..crate::snapshot::DiffPane::default()
            },
            ..UiSnapshot::default()
        });
        assert_eq!(app.current_hunk, 4);
        // New file: back to the top of the diff.
        app.update(UiSnapshot {
            diff: crate::snapshot::DiffPane {
                title: "b.go".to_string(),
                total_hunks: 2,
                ..crate::snapshot::DiffPane::default()
            },
            ..UiSnapshot::default()
        });
        assert_eq!(app.current_hunk, 1);
        // No hunks: 0.
        app.update(UiSnapshot {
            diff: crate::snapshot::DiffPane {
                title: "c.go".to_string(),
                ..crate::snapshot::DiffPane::default()
            },
            ..UiSnapshot::default()
        });
        assert_eq!(app.current_hunk, 0);
    }

    #[test]
    fn zoom_toggles() {
        let mut app = App::new();
        assert!(!app.zoomed);
        app.apply(Action::ToggleZoom);
        assert!(app.zoomed);
        app.apply(Action::ToggleZoom);
        assert!(!app.zoomed);
    }

    #[test]
    fn wrap_defaults_off_and_toggles() {
        // The reference mode is raw (`wrap off`, docs/review/15 §3.4); W toggles.
        let mut app = App::new();
        assert!(!app.diff_wrap, "raw is the default");
        app.apply(Action::ToggleWrap);
        assert!(app.diff_wrap);
        app.apply(Action::ToggleWrap);
        assert!(!app.diff_wrap);
    }

    #[test]
    fn hscroll_moves_only_in_raw_mode() {
        let mut app = App::new();
        app.focused = Pane::Diff;
        // Raw mode (default): l steps by 8, h steps back, 0 resets.
        app.apply(Action::Expand);
        app.apply(Action::Expand);
        assert_eq!(app.diff_hscroll, 16);
        app.apply(Action::Collapse);
        assert_eq!(app.diff_hscroll, 8);
        app.apply(Action::ResetHScroll);
        assert_eq!(app.diff_hscroll, 0);
        // Wrap mode: h/l must not move hidden horizontal state.
        app.apply(Action::Expand);
        app.apply(Action::ToggleWrap);
        let before = app.diff_hscroll;
        app.apply(Action::Expand);
        assert_eq!(app.diff_hscroll, before);
    }

    #[test]
    fn scope_switch_resets_hscroll() {
        let mut app = App::new();
        app.focused = Pane::Diff;
        app.apply(Action::Expand);
        assert_eq!(app.diff_hscroll, 8);
        app.apply(Action::ScopeStaged);
        assert_eq!(app.diff_hscroll, 0);
    }

    #[test]
    fn selected_file_path_maps_symbol_rows_to_their_file() {
        let app = app_with_files();
        let mut app2 = app_with_files();
        app2.file_sel = 2; // sym1 of a.go
        assert_eq!(app.selected_file_path(), Some("a.go"));
        assert_eq!(app2.selected_file_path(), Some("a.go"));
        let mut app3 = app_with_files();
        app3.file_sel = 3; // b.go
        assert_eq!(app3.selected_file_path(), Some("b.go"));
    }

    #[test]
    fn selected_symbol_name_only_on_symbol_rows() {
        let mut app = app_with_files();
        assert_eq!(app.selected_symbol_name(), None, "file row: no symbol");
        app.file_sel = 2; // sym1 of a.go
        assert_eq!(app.selected_symbol_name(), Some("sym1"));
        app.file_sel = 3; // b.go file row
        assert_eq!(app.selected_symbol_name(), None);
    }

    // -- generated rows inside the combined Impact view --------------------------------

    use codescope_core::{AiStatus, Epoch, FormKind, PlanNode, PlanNodeChange, VizForm};

    /// The repo-state epoch an AI status describes: Loading/Ready/Stale carry it; the
    /// epoch-less statuses (Disabled/Idle/Failed) ride whatever repo state is current —
    /// the fixtures below all stay on epoch 1 unless a test says otherwise.
    fn status_epoch(ai: &AiStatus) -> Epoch {
        match ai {
            AiStatus::Loading { since_epoch } => *since_epoch,
            AiStatus::WaitingForSymbols { epoch }
            | AiStatus::WaitingForRelations { epoch }
            | AiStatus::Queued { epoch, .. } => *epoch,
            AiStatus::Ready { epoch } | AiStatus::Stale { epoch } => *epoch,
            AiStatus::Disabled | AiStatus::Idle | AiStatus::Failed { .. } => Epoch(1),
        }
    }

    /// A snapshot whose `epoch` matches the AI status's epoch (a consistent fixture:
    /// the production publisher never mixes them).
    fn ai_plan_snap(ai: AiStatus, ai_generated: bool, rows: usize) -> UiSnapshot {
        let epoch = status_epoch(&ai);
        ai_plan_snap_at(epoch, ai, ai_generated, rows)
    }

    /// [`ai_plan_snap`] with an explicit snapshot epoch — the negative fixture for
    /// epoch-mismatch tests.
    fn ai_plan_snap_at(
        snap_epoch: Epoch,
        ai: AiStatus,
        ai_generated: bool,
        rows: usize,
    ) -> UiSnapshot {
        UiSnapshot {
            ai,
            epoch: snap_epoch,
            semantic: crate::snapshot::SemanticPane {
                report: None,
                plan: (rows > 0).then(|| {
                    let mut plan = codescope_core::VisualizationPlan::new(
                        snap_epoch,
                        "How does authentication flow?",
                    );
                    plan.title = "Authentication flow".into();
                    plan.intent = "Requests pass through each authentication step.".into();
                    plan.forms.push(VizForm {
                        kind: FormKind::CallTree,
                        title: "runtime".into(),
                        summary: String::new(),
                        nodes: (0..rows)
                            .map(|i| {
                                PlanNode::new(
                                    format!("n{i}"),
                                    format!("step{i}"),
                                    PlanNodeChange::Modified,
                                )
                                .with_detail(format!("explains step {i}"))
                            })
                            .collect(),
                        edges: Vec::new(),
                    });
                    plan
                }),
                note: String::new(),
                ai_generated,
            },
            ..UiSnapshot::default()
        }
    }

    /// Drive the app to the AI plan view via the Loading → Ready edge.
    fn app_with_ai_plan(rows: usize) -> App {
        let mut app = App::new();
        app.update(ai_plan_snap(
            AiStatus::Loading {
                since_epoch: Epoch(1),
            },
            false,
            0,
        ));
        app.update(ai_plan_snap(
            AiStatus::Ready { epoch: Epoch(1) },
            true,
            rows,
        ));
        app
    }

    #[test]
    fn ai_plan_scroll_navigates_and_clamps() {
        let mut app = app_with_ai_plan(5);
        app.apply(Action::Focus(Pane::Impact));
        for _ in 0..10 {
            app.apply(Action::Down);
        }
        assert_eq!(
            app.ai_plan_scroll, 10,
            "physical height is clamped by renderer"
        );
        for _ in 0..10 {
            app.apply(Action::Up);
        }
        assert_eq!(app.ai_plan_scroll, 0);
        app.apply(Action::Bottom);
        assert_eq!(app.ai_plan_scroll, 10_000);
        app.apply(Action::Top);
        assert_eq!(app.ai_plan_scroll, 0);
        app.apply(Action::PageDown);
        assert_eq!(app.ai_plan_scroll, 20);
        app.apply(Action::HalfPageUp);
        assert_eq!(app.ai_plan_scroll, 10);
        // A different plan resets the width-dependent physical scroll.
        app.apply(Action::Bottom);
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(1) }, true, 2));
        assert_eq!(app.ai_plan_scroll, 0);
        // Empty plan: everything collapses to 0.
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(1) }, true, 0));
        assert_eq!(app.ai_plan_scroll, 0);
        app.apply(Action::Down);
        assert_eq!(app.ai_plan_scroll, 0);
    }

    #[test]
    fn combined_impact_scrolls_the_generated_half() {
        let mut app = app_with_ai_plan(5);
        app.apply(Action::Focus(Pane::Impact));
        app.apply(Action::Down);
        assert_eq!(app.ai_plan_scroll, 1);
        app.apply(Action::Bottom);
        assert_eq!(app.ai_plan_scroll, 10_000);
    }

    #[test]
    fn page_keys_still_scroll_the_diff_when_it_is_focused() {
        use crate::snapshot::DiffRow;
        let mut app = App::new();
        app.snapshot.diff.rows = (0..30)
            .map(|i| DiffRow::Context {
                old_ln: i as u32 + 1,
                new_ln: i as u32 + 1,
                text: format!("line {i}"),
            })
            .collect();
        app.focused = Pane::Diff;
        app.apply(Action::PageDown);
        assert_eq!(app.diff_scroll, 20);
        assert_eq!(app.ai_plan_scroll, 0);
    }

    /// Review 18 M5: a snapshot that inserts symbol rows (a file's lazy analysis landing)
    /// must not slide the selection onto a different entity — the selected (file, symbol)
    /// re-resolves by identity.
    #[test]
    fn selection_follows_the_entity_when_symbols_arrive() {
        // Two collapsed files; select the second (b.go).
        let mut app = App::new();
        app.update(UiSnapshot {
            files: vec![row("a.go", false, 0), row("b.go", false, 0)],
            ..UiSnapshot::default()
        });
        app.file_sel = 1;
        assert_eq!(app.selected_file_path(), Some("b.go"));
        // a.go's analysis lands: its symbols insert rows, shifting b.go's ordinal.
        let mut snap = app.snapshot.clone();
        snap.files[0].expanded = true;
        snap.files[0].semantic = crate::snapshot::FileSemanticLoad::Ready;
        snap.files[0].symbols = vec![
            SymbolRow {
                name: "NewSym".to_string(),
                change: "added",
                confidence: "",
                has_diagnostic: false,
                position: Some((1, 0)),
            },
            SymbolRow {
                name: "Other".to_string(),
                change: "modified",
                confidence: "",
                has_diagnostic: false,
                position: Some((3, 0)),
            },
        ];
        app.update(snap);
        // b.go is still selected (its flat index moved from 1 to 3).
        assert_eq!(app.selected_file_path(), Some("b.go"));
    }
}
