//! Key handling: the modeless keymap as a pure, fully testable function.
//!
//! codescope is read-only, so there is a single "normal" mode. Vim keys and arrows both
//! work; `?` opens the help modal (the discovery path). [`map_key`] is pure so the entire
//! keymap is unit-testable without a terminal (research 04 §4, §6).

use codescope_core::ChangeScope;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{App, Pane};

/// A user intent. The dispatcher turns these into work; [`crate::app::App::apply`] turns
/// them into view state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Quit (terminal restored by the caller).
    Quit,
    /// Toggle the help modal.
    ToggleHelp,
    /// Focus the next / previous pane.
    FocusNext,
    /// Focus the previous pane.
    FocusPrev,
    /// Focus a pane directly.
    Focus(Pane),
    /// Move the selection/scroll down / up by one.
    Down,
    /// Move the selection/scroll up by one.
    Up,
    /// Half-page down / up.
    HalfPageDown,
    /// Half-page up.
    HalfPageUp,
    /// Page down / up.
    PageDown,
    /// Page up.
    PageUp,
    /// Jump to top / bottom.
    Top,
    /// Jump to bottom.
    Bottom,
    /// Activate the selection (jump diff+semantic to it / re-center impact view).
    Activate,
    /// Expand / collapse the selected tree node.
    ToggleExpand,
    /// Collapse the selected node.
    Collapse,
    /// Expand the selected node.
    Expand,
    /// Increase / decrease the default semantic expansion depth.
    ExpandMore,
    /// Decrease the default semantic expansion depth.
    ExpandLess,
    /// Show the staged scope.
    ScopeStaged,
    /// Show the unstaged scope.
    ScopeUnstaged,
    /// Show the branch-vs-base scope.
    ScopeBranch,
    /// Show the working scope (all uncommitted changes: staged + unstaged).
    ScopeWorking,
    /// Cycle scope (branch → staged → unstaged → working).
    ScopeCycle,
    /// Re-scan git data.
    RefreshGit,
    /// Toggle AI on/off (inert when AI is not configured).
    AiToggle,
    /// Force an AI refresh for the current view.
    AiRefresh,
    /// Open/close the AI model picker modal.
    ModelPicker,
    /// The user picked a model in the picker (the dispatcher applies it).
    ModelSelected(String),
    /// Open/close the comparison-base picker modal.
    BasePicker,
    /// The user picked a base ref in the picker (the dispatcher applies it).
    BaseSelected(String),
    /// Jump to the next / previous diff hunk.
    NextHunk,
    /// Jump to the previous diff hunk.
    PrevHunk,
    /// The key did not map to an action.
    None,
}

/// Map a key event to an [`Action`] in the current app context.
///
/// Returns [`Action::None`] for release/repeat events (Windows emits them) and unmapped
/// keys. When the help modal is open, any key other than `?`/`Esc` is swallowed.
#[must_use]
pub fn map_key(key: KeyEvent, app: &App) -> Action {
    if key.kind != KeyEventKind::Press {
        return Action::None;
    }
    if app.show_help {
        return match key.code {
            KeyCode::Char('?') | KeyCode::Esc => Action::ToggleHelp,
            _ => Action::None,
        };
    }
    if app.show_model_picker {
        return match key.code {
            KeyCode::Esc => Action::ModelPicker,
            KeyCode::Char('j') | KeyCode::Down => Action::Down,
            KeyCode::Char('k') | KeyCode::Up => Action::Up,
            KeyCode::Enter => Action::ModelSelected(String::new()), // app fills the name
            _ => Action::None,
        };
    }
    if app.show_base_picker {
        return match key.code {
            KeyCode::Esc => Action::BasePicker,
            KeyCode::Char('j') | KeyCode::Down => Action::Down,
            KeyCode::Char('k') | KeyCode::Up => Action::Up,
            KeyCode::Enter => Action::BaseSelected(String::new()), // app fills the name
            _ => Action::None,
        };
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Action::Quit,
            KeyCode::Char('d') => Action::HalfPageDown,
            KeyCode::Char('u') => Action::HalfPageUp,
            _ => Action::None,
        };
    }
    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('?') => Action::ToggleHelp,
        KeyCode::Esc => Action::None,
        KeyCode::Tab => Action::FocusNext,
        KeyCode::BackTab => Action::FocusPrev,
        KeyCode::Char('1') => Action::Focus(Pane::Files),
        KeyCode::Char('2') => Action::Focus(Pane::Diff),
        KeyCode::Char('3') => Action::Focus(Pane::Semantic),
        KeyCode::Char('s') => Action::ScopeStaged,
        KeyCode::Char('u') => Action::ScopeUnstaged,
        KeyCode::Char('B') => Action::ScopeBranch,
        KeyCode::Char('w') => Action::ScopeWorking,
        KeyCode::Char('S') => Action::ScopeCycle,
        KeyCode::Char('R') => Action::RefreshGit,
        KeyCode::Char('a') => Action::AiToggle,
        KeyCode::Char('A') => Action::AiRefresh,
        KeyCode::Char('m') => Action::ModelPicker,
        KeyCode::Char('b') => Action::BasePicker,
        KeyCode::Char('j') | KeyCode::Down => Action::Down,
        KeyCode::Char('k') | KeyCode::Up => Action::Up,
        KeyCode::Enter => Action::Activate,
        KeyCode::Char(' ') => Action::ToggleExpand,
        KeyCode::Char('h') | KeyCode::Left => Action::Collapse,
        KeyCode::Char('l') | KeyCode::Right => Action::Expand,
        KeyCode::Char('+') => Action::ExpandMore,
        KeyCode::Char('-') => Action::ExpandLess,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::Char('n') => Action::NextHunk,
        KeyCode::Char('N') => Action::PrevHunk,
        KeyCode::Char('g') | KeyCode::Home => Action::Top,
        KeyCode::Char('G') | KeyCode::End => Action::Bottom,
        _ => Action::None,
    }
}

/// Cycle order for [`Action::ScopeCycle`].
#[must_use]
pub fn next_scope(scope: ChangeScope) -> ChangeScope {
    match scope {
        ChangeScope::Branch => ChangeScope::Staged,
        ChangeScope::Staged => ChangeScope::Unstaged,
        ChangeScope::Unstaged => ChangeScope::Working,
        ChangeScope::Working => ChangeScope::Branch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app() -> App {
        App::default()
    }

    #[test]
    fn release_events_are_ignored() {
        let mut k = key(KeyCode::Char('j'));
        k.kind = KeyEventKind::Release;
        assert_eq!(map_key(k, &app()), Action::None);
    }

    #[test]
    fn quit_keys() {
        assert_eq!(map_key(key(KeyCode::Char('q')), &app()), Action::Quit);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(map_key(ctrl_c, &app()), Action::Quit);
    }

    #[test]
    fn navigation_both_styles() {
        assert_eq!(map_key(key(KeyCode::Char('j')), &app()), Action::Down);
        assert_eq!(map_key(key(KeyCode::Down), &app()), Action::Down);
        assert_eq!(map_key(key(KeyCode::Char('k')), &app()), Action::Up);
        assert_eq!(map_key(key(KeyCode::Up), &app()), Action::Up);
        assert_eq!(map_key(key(KeyCode::Char('g')), &app()), Action::Top);
        assert_eq!(map_key(key(KeyCode::Char('G')), &app()), Action::Bottom);
    }

    #[test]
    fn scope_keys() {
        assert_eq!(map_key(key(KeyCode::Char('s')), &app()), Action::ScopeStaged);
        assert_eq!(map_key(key(KeyCode::Char('u')), &app()), Action::ScopeUnstaged);
        assert_eq!(map_key(key(KeyCode::Char('B')), &app()), Action::ScopeBranch);
        assert_eq!(map_key(key(KeyCode::Char('w')), &app()), Action::ScopeWorking);
        assert_eq!(map_key(key(KeyCode::Char('S')), &app()), Action::ScopeCycle);
    }

    #[test]
    fn focus_keys() {
        assert_eq!(map_key(key(KeyCode::Tab), &app()), Action::FocusNext);
        assert_eq!(map_key(key(KeyCode::BackTab), &app()), Action::FocusPrev);
        assert_eq!(map_key(key(KeyCode::Char('2')), &app()), Action::Focus(Pane::Diff));
    }

    #[test]
    fn tree_keys() {
        assert_eq!(map_key(key(KeyCode::Char(' ')), &app()), Action::ToggleExpand);
        assert_eq!(map_key(key(KeyCode::Char('h')), &app()), Action::Collapse);
        assert_eq!(map_key(key(KeyCode::Char('l')), &app()), Action::Expand);
        assert_eq!(map_key(key(KeyCode::Char('+')), &app()), Action::ExpandMore);
        assert_eq!(map_key(key(KeyCode::Char('-')), &app()), Action::ExpandLess);
    }

    #[test]
    fn hunk_keys() {
        assert_eq!(map_key(key(KeyCode::Char('n')), &app()), Action::NextHunk);
        assert_eq!(map_key(key(KeyCode::Char('N')), &app()), Action::PrevHunk);
    }

    #[test]
    fn help_modal_swallows_keys() {
        let mut a = app();
        a.show_help = true;
        assert_eq!(map_key(key(KeyCode::Char('j')), &a), Action::None);
        assert_eq!(map_key(key(KeyCode::Char('q')), &a), Action::None);
        assert_eq!(map_key(key(KeyCode::Char('?')), &a), Action::ToggleHelp);
        assert_eq!(map_key(key(KeyCode::Esc), &a), Action::ToggleHelp);
    }

    #[test]
    fn unmapped_is_none() {
        assert_eq!(map_key(key(KeyCode::Char('z')), &app()), Action::None);
    }

    #[test]
    fn b_opens_base_picker() {
        assert_eq!(map_key(key(KeyCode::Char('b')), &app()), Action::BasePicker);
    }

    #[test]
    fn base_picker_modal_swallows_keys() {
        let mut a = app();
        a.show_base_picker = true;
        assert_eq!(map_key(key(KeyCode::Char('q')), &a), Action::None);
        assert_eq!(map_key(key(KeyCode::Esc), &a), Action::BasePicker);
        assert_eq!(map_key(key(KeyCode::Char('j')), &a), Action::Down);
        assert_eq!(map_key(key(KeyCode::Char('k')), &a), Action::Up);
        assert_eq!(
            map_key(key(KeyCode::Enter), &a),
            Action::BaseSelected(String::new())
        );
    }

    #[test]
    fn scope_cycle_order() {
        assert_eq!(next_scope(ChangeScope::Branch), ChangeScope::Staged);
        assert_eq!(next_scope(ChangeScope::Staged), ChangeScope::Unstaged);
        assert_eq!(next_scope(ChangeScope::Unstaged), ChangeScope::Working);
        assert_eq!(next_scope(ChangeScope::Working), ChangeScope::Branch);
    }
}
