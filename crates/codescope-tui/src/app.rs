//! Application state and pure `Action` transitions. No I/O — the run loop feeds it
//! [`UiSnapshot`]s and [`Action`]s; rendering reads it.

use codescope_core::{AiStatus, ChangeScope};

use crate::action::{next_scope, Action};
use crate::layout::DEFAULT_FILES_WIDTH;
use crate::snapshot::{DiffRow, UiSnapshot};

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

/// Which view the bottom pane shows (`v` toggles, docs/review/16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BottomView {
    /// The deterministic three-column impact view (selected change, callers, downstream).
    #[default]
    Impact,
    /// The validated AI plan published in [`UiSnapshot::semantic`].
    AiPlan,
}

/// View-state for the running app.
#[derive(Debug)]
pub struct App {
    /// The latest published snapshot.
    pub snapshot: UiSnapshot,
    /// Which pane has focus.
    pub focused: Pane,
    /// Selected row in the files pane (flattened file+symbol index).
    pub file_sel: usize,
    /// Vertical scroll of the diff pane (a logical-row anchor; the renderer maps it to a
    /// visual line).
    pub diff_scroll: u16,
    /// Horizontal scroll of the diff pane (raw mode: long lines are clipped + scrolled).
    pub diff_hscroll: u16,
    /// 1-based hunk under the diff scroll anchor; 0 when the diff has no hunks. App-owned
    /// view state (docs/review/15 §4): the snapshot's `total_hunks` is immutable data, but
    /// the current hunk follows navigation, so it must survive snapshot publishes.
    pub current_hunk: usize,
    /// Requested files-pane width in the normal layout (`[`/`]` resize in two-cell steps,
    /// clamped to 28..=56; the renderer may narrow it further to protect the diff).
    pub files_width: u16,
    /// Whether the focused pane is zoomed to fill the whole body (`z`).
    pub zoomed: bool,
    /// Which view the bottom pane renders (`v`): the deterministic Impact columns or the
    /// validated AI plan.
    pub bottom_view: BottomView,
    /// Vertical scroll offset of the AI plan rows (only meaningful in
    /// [`BottomView::AiPlan`]).
    pub ai_plan_scroll: usize,
    /// Edge-detection for the one-shot AI auto-switch: the `since_epoch` of the last
    /// `Loading` snapshot consumed by [`App::update`]. Keeping the request's epoch (not
    /// a bare boolean) means only the Ready that answers THAT request for the CURRENT
    /// repo state can fire the switch — a coalesced/replayed Ready for another epoch
    /// never does.
    prev_ai_loading_epoch: Option<codescope_core::Epoch>,
    /// Whether the diff pane smart-wraps long lines (`W`); off = raw clip + h-scroll.
    /// The reference mode is raw (`wrap off`), docs/review/15 §3.4.
    pub diff_wrap: bool,
    /// Whether the help modal is open.
    pub show_help: bool,
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

impl Default for App {
    fn default() -> Self {
        App {
            snapshot: UiSnapshot::default(),
            focused: Pane::default(),
            file_sel: 0,
            diff_scroll: 0,
            diff_hscroll: 0,
            current_hunk: 0,
            files_width: DEFAULT_FILES_WIDTH,
            show_help: false,
            show_model_picker: false,
            model_sel: 0,
            model_query: String::new(),
            show_base_picker: false,
            base_sel: 0,
            base_query: String::new(),
            should_quit: false,
            zoomed: false,
            diff_wrap: false,
            bottom_view: BottomView::default(),
            ai_plan_scroll: 0,
            prev_ai_loading_epoch: None,
        }
    }
}

impl App {
    /// A fresh app (branch scope, raw diff mode, files pane at the default width).
    #[must_use]
    pub fn new() -> Self {
        App::default()
    }

    /// Replace the snapshot, clamping selection into the new bounds.
    pub fn update(&mut self, snapshot: UiSnapshot) {
        // The diff pane follows the files-pane selection: when the dispatcher retargets it
        // to a different file, start at the top of the new diff instead of keeping a scroll
        // offset computed against the old one. `DiffPane::title` is the file path today
        // (MERGE: the dispatcher half renames it to `file_path`; the comparison stays).
        let retargeted = self.snapshot.diff.title != snapshot.diff.title;
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
        self.sync_bottom_view(&snapshot);
        self.snapshot = snapshot;
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

    /// Bottom-pane view transitions driven by the AI status edge (docs/review/16):
    /// auto-switch to the AI plan exactly once when a Loading request lands as a Ready
    /// for the SAME epoch, that epoch is the snapshot's current repo state, and the plan
    /// pane is a non-empty `ai_generated` one; switch back to Impact when the plan is
    /// gone for good (Stale/Failed/Disabled, or a Ready/Loading tagged with another
    /// epoch, without an `ai_generated` pane). A manual `v` switch survives later
    /// publishes: only the Loading → Ready edge flips the view forward.
    fn sync_bottom_view(&mut self, snapshot: &UiSnapshot) {
        let ready_plan_landed = match (self.prev_ai_loading_epoch, &snapshot.ai) {
            (Some(pending), AiStatus::Ready { epoch }) => {
                *epoch == pending
                    && *epoch == snapshot.epoch
                    && snapshot.semantic.ai_generated
                    && !snapshot.semantic.rows.is_empty()
            }
            _ => false,
        };
        if ready_plan_landed {
            self.bottom_view = BottomView::AiPlan;
            self.ai_plan_scroll = 0;
        }
        // The pending request's epoch is consumed by any non-Loading status (Ready,
        // Failed, …): only a Ready observed while it is still the in-flight request
        // can fire the switch, and it can fire exactly once.
        self.prev_ai_loading_epoch = match &snapshot.ai {
            AiStatus::Loading { since_epoch } => Some(*since_epoch),
            _ => None,
        };
        // A status tagged with a superseded epoch (a refresh moved `snapshot.epoch` on
        // without re-requesting AI) is as good as gone when no plan pane is published.
        let stale_status = match &snapshot.ai {
            AiStatus::Ready { epoch } | AiStatus::Loading { since_epoch: epoch } => {
                *epoch != snapshot.epoch
            }
            _ => false,
        };
        let plan_gone = (stale_status
            || matches!(
                snapshot.ai,
                AiStatus::Stale { .. } | AiStatus::Failed { .. } | AiStatus::Disabled
            ))
            && !snapshot.semantic.ai_generated;
        if self.bottom_view == BottomView::AiPlan && plan_gone {
            self.bottom_view = BottomView::Impact;
            self.ai_plan_scroll = 0;
        }
    }

    /// Apply an action to the view state. I/O actions (RefreshGit/Ai*) only toggle flags
    /// here; the dispatcher observes them via the returned snapshot channel separately.
    pub fn apply(&mut self, action: Action) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::ToggleHelp => self.show_help = !self.show_help,
            Action::SetFileExpanded { path, expanded } => {
                self.set_file_expanded(&path, expanded);
            }
            // Space maps to the expansion intent, but the targeted command (with the
            // resolved path) is built in run.rs; here it is a no-op placeholder so the
            // key alone never mutates dispatcher-owned expansion state (review 18 m4).
            Action::ToggleExpand => {}
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
            Action::ToggleBottomView => {
                self.bottom_view = match self.bottom_view {
                    BottomView::Impact => BottomView::AiPlan,
                    BottomView::AiPlan => BottomView::Impact,
                };
                self.ai_plan_scroll = 0;
            }
            Action::ToggleWrap => self.diff_wrap = !self.diff_wrap,
            Action::ResetHScroll => self.diff_hscroll = 0,
            // View-state resize: two-cell steps clamped to the spec range; the renderer
            // may still narrow further to protect MIN_DIFF_WIDTH without touching this.
            Action::ResizeFilesNarrower => {
                self.files_width = self.files_width.saturating_sub(2).clamp(
                    crate::layout::MIN_FILES_WIDTH,
                    crate::layout::MAX_FILES_WIDTH,
                );
            }
            Action::ResizeFilesWider => {
                self.files_width = self.files_width.saturating_add(2).clamp(
                    crate::layout::MIN_FILES_WIDTH,
                    crate::layout::MAX_FILES_WIDTH,
                );
            }
            Action::Collapse => match self.focused {
                // Wrapped mode has no hidden horizontal state: h must not move it.
                Pane::Diff if self.diff_wrap => {}
                Pane::Diff => self.diff_hscroll = self.diff_hscroll.saturating_sub(8),
                // Files-pane expansion is dispatcher-owned: run.rs routes Space/h/l to
                // the targeted SetFileExpanded command; App applies no local tree
                // mutation for them (review 18 m4).
                _ => {}
            },
            Action::Expand => match self.focused {
                Pane::Diff if self.diff_wrap => {}
                Pane::Diff => self.diff_hscroll = self.diff_hscroll.saturating_add(8),
                _ => {}
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
            // RefreshGit / AiToggle / AiRefresh are dispatcher concerns; nothing to do here.
            // SelectionChanged / SelectSymbol are derived from the view state by the run
            // loop and forwarded to the dispatcher; applying them locally would be a no-op.
            Action::ModelSelected(_)
            | Action::BaseSelected(_)
            | Action::SelectSymbol { .. }
            | Action::SelectionChanged { .. }
            | Action::RefreshGit
            | Action::AiToggle
            | Action::AiRefresh
            | Action::None => {}
        }
        self.clamp();
    }

    fn set_scope(&mut self, scope: ChangeScope) {
        self.snapshot.scope = scope;
        self.file_sel = 0;
        self.diff_scroll = 0;
        self.diff_hscroll = 0;
        self.current_hunk = usize::from(self.snapshot.diff.total_hunks > 0);
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
            }
            // The Impact view has no cursor: it is a three-column summary of the current
            // files-pane selection (docs/review/15 §3.5). The AI plan view is a flat row
            // list, so movement scrolls it.
            Pane::Impact if self.bottom_view == BottomView::AiPlan => {
                self.ai_plan_scroll = step(self.ai_plan_scroll, delta, self.ai_plan_rows());
            }
            Pane::Impact => {}
            Pane::Diff => self.scroll_diff(delta),
        }
    }

    /// Page keys: the diff, except when the focused bottom pane shows the AI plan — then
    /// they page the plan (the Impact view itself stays scroll-free).
    fn page(&mut self, delta: i32) {
        if self.focused == Pane::Impact && self.bottom_view == BottomView::AiPlan {
            self.ai_plan_scroll = step(self.ai_plan_scroll, delta, self.ai_plan_rows());
        } else {
            self.scroll_diff(delta);
        }
    }

    /// Rows the AI plan view can scroll through (0 unless the pane holds a plan).
    fn ai_plan_rows(&self) -> usize {
        if self.snapshot.semantic.ai_generated {
            self.snapshot.semantic.rows.len()
        } else {
            0
        }
    }

    fn scroll_ai_plan_to(&mut self, pos: usize) {
        self.ai_plan_scroll = pos.min(self.ai_plan_rows().saturating_sub(1));
    }

    fn scroll_diff(&mut self, delta: i32) {
        let len = self.snapshot.diff.rows.len() as i32;
        let cur = self.diff_scroll as i32;
        self.diff_scroll = (cur + delta).clamp(0, len.saturating_sub(1).max(0)) as u16;
        self.sync_current_hunk();
    }

    fn top(&mut self) {
        match self.focused {
            Pane::Files => self.file_sel = 0,
            Pane::Impact if self.bottom_view == BottomView::AiPlan => self.scroll_ai_plan_to(0),
            Pane::Impact => {}
            Pane::Diff => {
                self.diff_scroll = 0;
                self.sync_current_hunk();
            }
        }
    }

    fn bottom(&mut self) {
        match self.focused {
            Pane::Files => self.file_sel = self.flat_file_rows().saturating_sub(1),
            Pane::Impact if self.bottom_view == BottomView::AiPlan => {
                self.scroll_ai_plan_to(usize::MAX);
            }
            Pane::Impact => {}
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
        let mut idx = self.file_sel;
        for f in &self.snapshot.files {
            if idx == 0 {
                return Some(f.path.as_str());
            }
            idx -= 1;
            if f.expanded {
                if idx < f.symbols.len() {
                    return Some(f.path.as_str());
                }
                idx -= f.symbols.len();
            }
        }
        None
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
        let mut idx = self.file_sel;
        for f in &self.snapshot.files {
            if idx == 0 {
                return Some((f, None));
            }
            idx -= 1;
            if f.expanded {
                if idx < f.symbols.len() {
                    return Some((f, Some(&f.symbols[idx])));
                }
                idx -= f.symbols.len();
            }
        }
        None
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

    fn clamp(&mut self) {
        self.file_sel = self.file_sel.min(self.flat_file_rows().saturating_sub(1));
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
        self.ai_plan_scroll = self
            .ai_plan_scroll
            .min(self.ai_plan_rows().saturating_sub(1));
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
    fn files_width_resizes_in_two_cell_steps_and_clamps() {
        let mut app = App::new();
        assert_eq!(app.files_width, 42, "default from the spec");
        app.apply(Action::ResizeFilesNarrower);
        assert_eq!(app.files_width, 40);
        app.apply(Action::ResizeFilesWider);
        assert_eq!(app.files_width, 42, "two-cell steps cancel");
        for _ in 0..20 {
            app.apply(Action::ResizeFilesNarrower);
        }
        assert_eq!(app.files_width, 28, "clamped at MIN_FILES_WIDTH");
        for _ in 0..40 {
            app.apply(Action::ResizeFilesWider);
        }
        assert_eq!(app.files_width, 56, "clamped at MAX_FILES_WIDTH");
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

    // -- bottom view: Impact | AI Plan --------------------------------------------------

    use codescope_core::Epoch;

    /// The repo-state epoch an AI status describes: Loading/Ready/Stale carry it; the
    /// epoch-less statuses (Disabled/Idle/Failed) ride whatever repo state is current —
    /// the fixtures below all stay on epoch 1 unless a test says otherwise.
    fn status_epoch(ai: &AiStatus) -> Epoch {
        match ai {
            AiStatus::Loading { since_epoch } => *since_epoch,
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
                title: "plan: auth refactor".to_string(),
                rows: (0..rows)
                    .map(|i| crate::snapshot::SemRow {
                        depth: (i % 2) as u16,
                        label: format!("step{i}"),
                        relation: "changed",
                        changed: true,
                        has_diagnostic: false,
                    })
                    .collect(),
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
    fn toggle_bottom_view_flips_and_resets_scroll() {
        let mut app = App::new();
        assert_eq!(app.bottom_view, BottomView::Impact, "default is Impact");
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(1) }, true, 5));
        app.apply(Action::ToggleBottomView);
        assert_eq!(app.bottom_view, BottomView::AiPlan);
        app.apply(Action::Focus(Pane::Impact));
        app.apply(Action::Down);
        app.apply(Action::Down);
        assert_eq!(app.ai_plan_scroll, 2);
        app.apply(Action::ToggleBottomView);
        assert_eq!(app.bottom_view, BottomView::Impact);
        assert_eq!(app.ai_plan_scroll, 0, "toggling resets the scroll");
    }

    #[test]
    fn ai_plan_auto_switch_fires_on_loading_to_ready() {
        let mut app = App::new();
        app.update(ai_plan_snap(
            AiStatus::Loading {
                since_epoch: Epoch(1),
            },
            false,
            0,
        ));
        assert_eq!(app.bottom_view, BottomView::Impact);
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(1) }, true, 3));
        assert_eq!(app.bottom_view, BottomView::AiPlan);
        assert_eq!(app.ai_plan_scroll, 0);
    }

    #[test]
    fn ai_plan_auto_switch_needs_rows_and_a_generated_pane() {
        // Ready but the semantic pane is not the AI plan (deterministic rows): no flip.
        let mut app = App::new();
        app.update(ai_plan_snap(
            AiStatus::Loading {
                since_epoch: Epoch(1),
            },
            false,
            0,
        ));
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(1) }, false, 0));
        assert_eq!(app.bottom_view, BottomView::Impact);
        // Ready with an AI pane that has no rows: nothing to show, no flip.
        let mut app = App::new();
        app.update(ai_plan_snap(
            AiStatus::Loading {
                since_epoch: Epoch(1),
            },
            false,
            0,
        ));
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(1) }, true, 0));
        assert_eq!(app.bottom_view, BottomView::Impact);
        // Ready without a preceding Loading: no edge, no flip.
        let mut app = App::new();
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(1) }, true, 3));
        assert_eq!(app.bottom_view, BottomView::Impact);
    }

    #[test]
    fn ai_plan_auto_switch_fires_exactly_once() {
        let mut app = app_with_ai_plan(3);
        assert_eq!(app.bottom_view, BottomView::AiPlan);
        // Manual switch back to Impact …
        app.apply(Action::ToggleBottomView);
        assert_eq!(app.bottom_view, BottomView::Impact);
        // … survives identical Ready republishes (no new Loading edge).
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(1) }, true, 3));
        assert_eq!(
            app.bottom_view,
            BottomView::Impact,
            "a manual switch back survives later publishes"
        );
        // A fresh request is a new edge and flips forward again.
        app.update(ai_plan_snap(
            AiStatus::Loading {
                since_epoch: Epoch(2),
            },
            false,
            0,
        ));
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(2) }, true, 3));
        assert_eq!(app.bottom_view, BottomView::AiPlan);
    }

    #[test]
    fn ai_plan_auto_switch_rejects_a_mismatched_ready_epoch() {
        // A Ready answering a DIFFERENT request than the observed Loading: no switch.
        let mut app = App::new();
        app.update(ai_plan_snap(
            AiStatus::Loading {
                since_epoch: Epoch(2),
            },
            false,
            0,
        ));
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(1) }, true, 3));
        assert_eq!(
            app.bottom_view,
            BottomView::Impact,
            "a Ready replay for another request never fires"
        );
        // A Ready for the observed request but published against a NEWER repo state
        // (snap.epoch moved on): stale, no switch — the fixture's epochs intentionally
        // disagree (the production publisher never emits this).
        let mut app = App::new();
        app.update(ai_plan_snap(
            AiStatus::Loading {
                since_epoch: Epoch(1),
            },
            false,
            0,
        ));
        app.update(ai_plan_snap_at(
            Epoch(2),
            AiStatus::Ready { epoch: Epoch(1) },
            true,
            3,
        ));
        assert_eq!(
            app.bottom_view,
            BottomView::Impact,
            "a Ready for a superseded repo state never fires"
        );
        // And neither arms a later switch: the pending epoch was consumed by the Ready.
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(2) }, true, 3));
        assert_eq!(app.bottom_view, BottomView::Impact);
    }

    #[test]
    fn ai_plan_view_unstrands_on_a_stale_ready_or_loading() {
        // An auto-opened AI view must not strand when a refresh advances the repo epoch
        // while the AI status still carries the OLD epoch and no plan is published.
        for stale in [
            AiStatus::Ready { epoch: Epoch(1) },
            AiStatus::Loading {
                since_epoch: Epoch(1),
            },
        ] {
            let mut app = app_with_ai_plan(3);
            assert_eq!(app.bottom_view, BottomView::AiPlan);
            app.apply(Action::Focus(Pane::Impact));
            app.apply(Action::Down);
            assert_eq!(app.ai_plan_scroll, 1);
            let label = format!("{stale:?}");
            // snap.epoch moved to 2; the AI status still tags epoch 1.
            app.update(ai_plan_snap_at(Epoch(2), stale, false, 0));
            assert_eq!(app.bottom_view, BottomView::Impact, "unstranded: {label}");
            assert_eq!(app.ai_plan_scroll, 0, "scroll reset: {label}");
        }
        // A stale-tagged status while the epoch-matched AI pane is still published keeps
        // the plan visible (the pane, not the status, owns the rows).
        let mut app = app_with_ai_plan(3);
        app.update(ai_plan_snap_at(
            Epoch(2),
            AiStatus::Ready { epoch: Epoch(1) },
            true,
            3,
        ));
        assert_eq!(app.bottom_view, BottomView::AiPlan);
    }

    #[test]
    fn ai_plan_view_falls_back_to_impact_when_the_plan_is_gone() {
        for gone in [
            AiStatus::Stale { epoch: Epoch(1) },
            AiStatus::Failed {
                reason: "boom".to_string(),
            },
            AiStatus::Disabled,
        ] {
            let mut app = app_with_ai_plan(3);
            assert_eq!(app.bottom_view, BottomView::AiPlan);
            let label = format!("{gone:?}");
            let mut snap = ai_plan_snap(gone, false, 0);
            snap.semantic.note = "AI view stale (repo changed); regenerating…".to_string();
            app.update(snap);
            assert_eq!(app.bottom_view, BottomView::Impact, "plan gone: {label}");
        }
        // A stale status while the (epoch-matched) AI pane is still published keeps the
        // plan visible.
        let mut app = app_with_ai_plan(3);
        app.update(ai_plan_snap(AiStatus::Stale { epoch: Epoch(1) }, true, 3));
        assert_eq!(app.bottom_view, BottomView::AiPlan);
        // Idle/Loading keep the current view (the pane shows its note/unavailable line).
        let mut app = app_with_ai_plan(3);
        app.update(ai_plan_snap(AiStatus::Idle, false, 0));
        assert_eq!(app.bottom_view, BottomView::AiPlan);
    }

    #[test]
    fn ai_plan_scroll_navigates_and_clamps() {
        let mut app = app_with_ai_plan(5);
        app.apply(Action::Focus(Pane::Impact));
        for _ in 0..10 {
            app.apply(Action::Down);
        }
        assert_eq!(app.ai_plan_scroll, 4, "clamped at the last row");
        for _ in 0..10 {
            app.apply(Action::Up);
        }
        assert_eq!(app.ai_plan_scroll, 0);
        app.apply(Action::Bottom);
        assert_eq!(app.ai_plan_scroll, 4);
        app.apply(Action::Top);
        assert_eq!(app.ai_plan_scroll, 0);
        app.apply(Action::PageDown);
        assert_eq!(app.ai_plan_scroll, 4);
        app.apply(Action::HalfPageUp);
        assert_eq!(app.ai_plan_scroll, 0);
        // A smaller republished plan clamps the scroll via update().
        app.apply(Action::Bottom);
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(1) }, true, 2));
        assert_eq!(app.ai_plan_scroll, 1);
        // Empty plan: everything collapses to 0.
        app.update(ai_plan_snap(AiStatus::Ready { epoch: Epoch(1) }, true, 0));
        assert_eq!(app.ai_plan_scroll, 0);
        app.apply(Action::Down);
        assert_eq!(app.ai_plan_scroll, 0);
    }

    #[test]
    fn impact_view_keeps_the_bottom_pane_scroll_free() {
        let mut app = app_with_ai_plan(5);
        app.apply(Action::ToggleBottomView); // back to Impact
        app.apply(Action::Focus(Pane::Impact));
        app.apply(Action::Down);
        app.apply(Action::PageDown);
        app.apply(Action::Bottom);
        assert_eq!(app.ai_plan_scroll, 0, "Impact view has no cursor");
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
        app.bottom_view = BottomView::AiPlan; // the view toggle must not hijack diff paging
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
