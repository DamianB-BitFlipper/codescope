//! Application state and pure `Action` transitions. No I/O — the run loop feeds it
//! [`UiSnapshot`]s and [`Action`]s; rendering reads it.

use codescope_core::ChangeScope;

use crate::action::{next_scope, Action};
use crate::snapshot::{DiffRow, UiSnapshot};

/// The three focusable panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    /// Left: changed files + symbols.
    #[default]
    Files,
    /// Center: focused diff.
    Diff,
    /// Right: semantic (callers/callees/impact) view.
    Semantic,
}

impl Pane {
    fn next(self) -> Self {
        match self {
            Pane::Files => Pane::Diff,
            Pane::Diff => Pane::Semantic,
            Pane::Semantic => Pane::Files,
        }
    }

    fn prev(self) -> Self {
        match self {
            Pane::Files => Pane::Semantic,
            Pane::Diff => Pane::Files,
            Pane::Semantic => Pane::Diff,
        }
    }
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
    /// Selected row in the semantic pane.
    pub sem_sel: usize,
    /// Vertical scroll of the diff pane.
    pub diff_scroll: u16,
    /// Horizontal scroll of the diff pane (long lines are clipped, not wrapped).
    pub diff_hscroll: u16,
    /// Default semantic expansion depth (`+`/`-`).
    pub sem_depth: u16,
    /// Whether the focused pane is zoomed to fill the whole main area (`z`).
    pub zoomed: bool,
    /// Whether the diff pane smart-wraps long lines (`W`); off = raw clip + h-scroll.
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
            sem_sel: 0,
            diff_scroll: 0,
            diff_hscroll: 0,
            sem_depth: 0,
            show_help: false,
            show_model_picker: false,
            model_sel: 0,
            model_query: String::new(),
            show_base_picker: false,
            base_sel: 0,
            base_query: String::new(),
            should_quit: false,
            zoomed: false,
            diff_wrap: true,
        }
    }
}

impl App {
    /// A fresh app (branch scope, depth 2, diff wrap on).
    #[must_use]
    pub fn new() -> Self {
        App {
            sem_depth: 2,
            ..App::default()
        }
    }

    /// Replace the snapshot, clamping selection into the new bounds.
    pub fn update(&mut self, snapshot: UiSnapshot) {
        // The diff pane follows the files-pane selection: when the dispatcher retargets it
        // to a different file, start at the top of the new diff instead of keeping a scroll
        // offset computed against the old one.
        if self.snapshot.diff.title != snapshot.diff.title {
            self.diff_scroll = 0;
            self.diff_hscroll = 0;
        }
        self.snapshot = snapshot;
        self.clamp();
    }

    /// Apply an action to the view state. I/O actions (RefreshGit/Ai*) only toggle flags
    /// here; the dispatcher observes them via the returned snapshot channel separately.
    pub fn apply(&mut self, action: Action) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::ToggleHelp => self.show_help = !self.show_help,
            Action::FocusNext => self.focused = self.focused.next(),
            Action::FocusPrev => self.focused = self.focused.prev(),
            Action::Focus(p) => self.focused = p,
            Action::Down => self.move_sel(1),
            Action::Up => self.move_sel(-1),
            Action::HalfPageDown => self.scroll_diff(10),
            Action::HalfPageUp => self.scroll_diff(-10),
            Action::PageDown => self.scroll_diff(20),
            Action::PageUp => self.scroll_diff(-20),
            Action::Top => self.top(),
            Action::Bottom => self.bottom(),
            Action::ToggleExpand => self.toggle_expand(),
            Action::Activate => self.activate(),
            Action::ToggleZoom => self.zoomed = !self.zoomed,
            Action::ToggleWrap => self.diff_wrap = !self.diff_wrap,
            Action::ResetHScroll => self.diff_hscroll = 0,
            Action::Collapse => match self.focused {
                // Wrapped mode has no hidden horizontal state: h must not move it.
                Pane::Diff if self.diff_wrap => {}
                Pane::Diff => self.diff_hscroll = self.diff_hscroll.saturating_sub(8),
                _ => self.collapse_sel(),
            },
            Action::Expand => match self.focused {
                Pane::Diff if self.diff_wrap => {}
                Pane::Diff => self.diff_hscroll = self.diff_hscroll.saturating_add(8),
                _ => self.expand_sel(),
            },
            Action::ExpandMore => self.sem_depth = self.sem_depth.saturating_add(1).min(8),
            Action::ExpandLess => self.sem_depth = self.sem_depth.saturating_sub(1),
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
        self.sem_sel = 0;
        self.diff_scroll = 0;
        self.diff_hscroll = 0;
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
            Pane::Semantic => {
                let len = self.snapshot.semantic.rows.len();
                self.sem_sel = step(self.sem_sel, delta, len);
            }
            Pane::Diff => self.scroll_diff(delta),
        }
    }

    fn scroll_diff(&mut self, delta: i32) {
        let len = self.snapshot.diff.rows.len() as i32;
        let cur = self.diff_scroll as i32;
        self.diff_scroll = (cur + delta).clamp(0, len.saturating_sub(1).max(0)) as u16;
    }

    fn top(&mut self) {
        match self.focused {
            Pane::Files => self.file_sel = 0,
            Pane::Semantic => self.sem_sel = 0,
            Pane::Diff => self.diff_scroll = 0,
        }
    }

    fn bottom(&mut self) {
        match self.focused {
            Pane::Files => self.file_sel = self.flat_file_rows().saturating_sub(1),
            Pane::Semantic => self.sem_sel = self.snapshot.semantic.rows.len().saturating_sub(1),
            Pane::Diff => {
                self.diff_scroll = self.snapshot.diff.rows.len().saturating_sub(1) as u16;
            }
        }
    }

    fn toggle_expand(&mut self) {
        self.set_expanded_toggle();
    }

    fn collapse_sel(&mut self) {
        self.set_expanded(false);
    }

    fn expand_sel(&mut self) {
        self.set_expanded(true);
    }

    fn set_expanded_toggle(&mut self) {
        if let Some(row) = self.current_file_row() {
            row.expanded = !row.expanded;
        }
    }

    fn set_expanded(&mut self, value: bool) {
        if let Some(row) = self.current_file_row() {
            row.expanded = value;
        }
    }

    /// The file row the flattened selection sits on (symbol rows map to their file).
    fn current_file_row(&mut self) -> Option<&mut crate::snapshot::FileRow> {
        let mut idx = self.file_sel;
        for row in &mut self.snapshot.files {
            if idx == 0 {
                return Some(row);
            }
            idx -= 1;
            if row.expanded {
                if idx < row.symbols.len() {
                    return Some(row); // a symbol row: expand acts on its file
                }
                idx -= row.symbols.len();
            }
        }
        None
    }

    /// `Enter`: in the files pane, jump diff+semantic to the selection (handled by the
    /// dispatcher via a forwarded action would be ideal; locally we at least scroll the
    /// diff to the selected file's first hunk). In other panes, no-op for now.
    fn activate(&mut self) {
        if self.focused == Pane::Files {
            self.set_expanded(true);
        }
    }

    fn jump_hunk(&mut self, delta: i32) {
        let total = self.snapshot.diff.total_hunks as i32;
        if total == 0 {
            return;
        }
        let cur = self.snapshot.diff.current_hunk as i32;
        let next = (cur + delta).clamp(1, total) as usize;
        self.snapshot.diff.current_hunk = next;
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
        self.sem_sel = self
            .sem_sel
            .min(self.snapshot.semantic.rows.len().saturating_sub(1));
        let max_scroll = self.snapshot.diff.rows.len().saturating_sub(1) as u16;
        self.diff_scroll = self.diff_scroll.min(max_scroll);
        self.model_sel = self
            .model_sel
            .min(self.filtered_models().len().saturating_sub(1));
        self.base_sel = self
            .base_sel
            .min(self.filtered_bases().len().saturating_sub(1));
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
        app.apply(Action::ToggleExpand);
        assert!(!app.snapshot.files[0].expanded);
        assert_eq!(app.flat_file_rows(), 2); // a.go + b.go
    }

    #[test]
    fn focus_cycles() {
        let mut app = App::new();
        assert_eq!(app.focused, Pane::Files);
        app.apply(Action::FocusNext);
        assert_eq!(app.focused, Pane::Diff);
        app.apply(Action::FocusNext);
        assert_eq!(app.focused, Pane::Semantic);
        app.apply(Action::FocusPrev);
        assert_eq!(app.focused, Pane::Diff);
        app.apply(Action::Focus(Pane::Semantic));
        assert_eq!(app.focused, Pane::Semantic);
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
    fn depth_bounds() {
        let mut app = App::new();
        for _ in 0..20 {
            app.apply(Action::ExpandMore);
        }
        assert_eq!(app.sem_depth, 8);
        for _ in 0..20 {
            app.apply(Action::ExpandLess);
        }
        assert_eq!(app.sem_depth, 0);
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
        app.snapshot.diff.current_hunk = 1;
        app.apply(Action::NextHunk);
        assert_eq!(app.snapshot.diff.current_hunk, 2);
        app.apply(Action::NextHunk);
        assert_eq!(app.snapshot.diff.current_hunk, 3);
        app.apply(Action::NextHunk);
        assert_eq!(app.snapshot.diff.current_hunk, 3);
        app.apply(Action::PrevHunk);
        assert_eq!(app.snapshot.diff.current_hunk, 2);
    }

    #[test]
    fn hunk_jump_anchors_scroll_to_the_header_row() {
        use crate::snapshot::DiffRow;
        let mut app = App::new();
        app.snapshot.diff.rows = vec![
            DiffRow::HunkHeader("@@ -1,2 +1,2 @@".to_string()),
            DiffRow::Context { old_ln: 1, new_ln: 1, text: "a".to_string() },
            DiffRow::HunkHeader("@@ -40,2 +40,2 @@".to_string()),
            DiffRow::Add { new_ln: 41, text: "b".to_string() },
        ];
        app.snapshot.diff.total_hunks = 2;
        app.snapshot.diff.current_hunk = 1;
        app.apply(Action::NextHunk);
        assert_eq!(app.snapshot.diff.current_hunk, 2);
        assert_eq!(app.diff_scroll, 2, "scroll anchors on the second hunk header");
        app.apply(Action::PrevHunk);
        assert_eq!(app.diff_scroll, 0);
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
    fn wrap_defaults_on_and_toggles() {
        let mut app = App::new();
        assert!(app.diff_wrap, "wrap is the default");
        app.apply(Action::ToggleWrap);
        assert!(!app.diff_wrap);
        app.apply(Action::ToggleWrap);
        assert!(app.diff_wrap);
    }

    #[test]
    fn hscroll_moves_only_in_raw_mode() {
        let mut app = App::new();
        app.focused = Pane::Diff;
        // Wrap mode (default): h/l must not move hidden horizontal state.
        app.apply(Action::Expand);
        assert_eq!(app.diff_hscroll, 0);
        // Raw mode: l steps by 8, h steps back, 0 resets.
        app.apply(Action::ToggleWrap);
        app.apply(Action::Expand);
        app.apply(Action::Expand);
        assert_eq!(app.diff_hscroll, 16);
        app.apply(Action::Collapse);
        assert_eq!(app.diff_hscroll, 8);
        app.apply(Action::ResetHScroll);
        assert_eq!(app.diff_hscroll, 0);
    }

    #[test]
    fn scope_switch_resets_hscroll() {
        let mut app = App::new();
        app.focused = Pane::Diff;
        app.apply(Action::ToggleWrap);
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
}
