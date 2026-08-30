//! Application state and pure `Action` transitions. No I/O — the run loop feeds it
//! [`UiSnapshot`]s and [`Action`]s; rendering reads it.

use codescope_core::ChangeScope;

use crate::action::{next_scope, Action};
use crate::snapshot::UiSnapshot;

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
#[derive(Debug, Default)]
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
    /// Whether the help modal is open.
    pub show_help: bool,
    /// Whether the AI model picker modal is open.
    pub show_model_picker: bool,
    /// Selected row in the model picker.
    pub model_sel: usize,
    /// Whether the comparison-base picker modal is open.
    pub show_base_picker: bool,
    /// Selected row in the base picker.
    pub base_sel: usize,
    /// Set when the user asked to quit.
    pub should_quit: bool,
}

impl App {
    /// A fresh app (branch scope, depth 2).
    #[must_use]
    pub fn new() -> Self {
        App {
            sem_depth: 2,
            ..App::default()
        }
    }

    /// Replace the snapshot, clamping selection into the new bounds.
    pub fn update(&mut self, snapshot: UiSnapshot) {
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
            Action::Collapse => match self.focused {
                Pane::Diff => self.diff_hscroll = self.diff_hscroll.saturating_sub(8),
                _ => self.collapse_sel(),
            },
            Action::Expand => match self.focused {
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
            }
            Action::BasePicker => {
                self.show_base_picker = !self.show_base_picker;
                self.base_sel = 0;
            }
            // ModelSelected/BaseSelected are applied by the dispatcher (it owns the
            // AiService / base override).
            // RefreshGit / AiToggle / AiRefresh are dispatcher concerns; nothing to do here.
            Action::ModelSelected(_)
            | Action::BaseSelected(_)
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
    }

    fn move_sel(&mut self, delta: i32) {
        if self.show_model_picker {
            let len = self.snapshot.available_models.len();
            self.model_sel = step(self.model_sel, delta, len);
            return;
        }
        if self.show_base_picker {
            let len = self.snapshot.available_bases.len();
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
        self.snapshot.diff.current_hunk = (cur + delta).clamp(1, total) as usize;
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
        self.sem_sel = self.sem_sel.min(self.snapshot.semantic.rows.len().saturating_sub(1));
        let max_scroll = self.snapshot.diff.rows.len().saturating_sub(1) as u16;
        self.diff_scroll = self.diff_scroll.min(max_scroll);
    }
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
}
