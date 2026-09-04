//! Key handling: the modeless keymap as a pure, fully testable function.
//!
//! codescope is read-only, so there is a single "normal" mode. Vim keys and arrows both
//! work; `?` opens the help modal (the discovery path). [`map_key`] is pure so the entire
//! keymap is unit-testable without a terminal (research 04 §4, §6).

use codescope_core::ChangeScope;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{App, Pane};

/// Stable identity of one AI-plan node inside the current plan.
///
/// Node ids are local to a form, so the form index is part of every mouse/keyboard target.
/// The app clears these targets whenever the generated plan changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlanNodeTarget {
    /// Zero-based form index inside [`codescope_core::VisualizationPlan::forms`].
    pub form: usize,
    /// Plan-local [`codescope_core::PlanNode::id`].
    pub id: String,
}

/// Stable identity of one displayed relationship inside the current AI plan.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlanRelationshipTarget {
    /// Zero-based form index inside the plan.
    pub form: usize,
    /// Zero-based edge index inside the form. This distinguishes parallel edges with the
    /// same endpoints, which can have distinct labels and interaction state.
    pub edge: usize,
    /// Source plan-local node id.
    pub from: String,
    /// Destination plan-local node id.
    pub to: String,
}

/// Display-cell position inside the fully laid-out diff text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffTextPoint {
    /// Physical wrapped/raw display row.
    pub row: usize,
    /// Display-cell column, including the dual line-number gutter.
    pub column: usize,
}

/// One retained linear selection in the rendered diff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffTextSelection {
    /// Gesture anchor.
    pub start: DiffTextPoint,
    /// Current/released endpoint.
    pub end: DiffTextPoint,
}

/// A user intent. The dispatcher turns these into work; [`crate::app::App::apply`] turns
/// them into view state.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Quit (terminal restored by the caller).
    Quit,
    /// Toggle the help modal.
    ToggleHelp,
    /// Open/close the full status-message detail overlay. Mouse clicks produce this;
    /// `Esc` closes the overlay once open.
    ToggleStatusDetail,
    /// Open a particular frozen diagnostic, such as the retained AI failure behind the
    /// generated-pane failure banner. Unlike `ToggleStatusDetail`, this does not depend
    /// on whichever transient message currently occupies the footer.
    OpenStatusDetail(crate::snapshot::StatusMessage),
    /// Focus a pane directly (`1`/`2`/`3`; Tab no longer cycles panes).
    Focus(Pane),
    /// Tab on the files pane: set the expansion of the file the selection is on RIGHT
    /// NOW. The path is resolved by the app at keypress time and carried with the
    /// command, so a coalesced/out-of-order SelectionChanged can never make the
    /// dispatcher toggle a different file than the one the user pressed Tab on
    /// (review 18 M4). Idempotent; this changes visibility only because analysis is
    /// scheduled independently.
    SetFileExpanded {
        /// Repo-relative path of the file row.
        path: String,
        /// The desired expansion state.
        expanded: bool,
    },
    /// Expand or collapse a synthesized changed-directory row. This is local view state;
    /// it never asks the dispatcher to scan the filesystem or generate summaries.
    SetDirectoryExpanded {
        /// Repo-relative directory without a trailing slash.
        path: String,
        /// Desired expansion state.
        expanded: bool,
    },
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
    /// The files-pane selection moved (j/k/arrows, or a snapshot clamped it): the diff
    /// pane retargets to the selected file and, for a symbol row, the dispatcher lazily
    /// expands its callers/callees. Sent by the run loop only when the resolved selection
    /// target actually changed; never produced by [`map_key`].
    SelectionChanged {
        /// Selected changed file (repo-relative); `None` when the file list is empty.
        file: Option<String>,
        /// Selected symbol (name, identifier line, identifier col) when the selection sits
        /// on a symbol row with a position; `None` on file rows and unmapped symbols.
        symbol: Option<(String, u32, u32)>,
    },
    /// The files-pane selection moved to a directory summary scope.
    DirectorySelectionChanged {
        /// Repo-relative directory without a trailing slash.
        directory: String,
    },
    /// A local control client asked the running UI to focus a changed-tree entity.
    /// The TUI resolves this stable identity into its current projected row, then its
    /// normal selection tracker emits the corresponding backend selection action.
    AgentFocus(crate::snapshot::AiSummaryKey),
    /// Apply one atomic command to the same diagram draft used by the internal AI tools.
    AgentDiagram {
        /// Identity returned through the published snapshot after the dispatcher applies it.
        request_id: u64,
        /// Shared renderer-native mutation.
        command: codescope_core::DiagramCommand,
    },
    /// Record a controller edit that could not be decoded into the shared diagram schema.
    /// Parsing happens beside the live socket so malformed CLI calls still join the visible
    /// activity chain instead of disappearing inside a short-lived client process.
    AgentDiagramRejected {
        /// Identity returned through the published snapshot after the dispatcher rejects it.
        request_id: u64,
        /// Bounded operation/subject label derived from the untrusted JSON when possible.
        detail: String,
        /// Scrubbed parse failure returned to the controller and displayed under the call row.
        error: String,
    },
    /// The user selected a changed symbol; the dispatcher lazily expands its callers/callees.
    SelectSymbol {
        /// Repo-relative file of the symbol.
        file: String,
        /// Symbol name.
        name: String,
        /// Line (0-based) of the symbol's identifier.
        line: u32,
        /// Column (0-based, utf-8) of the identifier.
        col: u32,
    },
    /// Expand / collapse the selected tree node.
    ToggleExpand,
    /// Toggle the explicit review mark on the selected directory or file.
    ToggleReviewed,
    /// Mouse: toggle one review marker without changing the files-pane selection.
    ToggleReviewedTarget(crate::review::ReviewTarget),
    /// Collapse the selected node.
    Collapse,
    /// Expand the selected node.
    Expand,
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
    /// Refresh repository state from Git and the working tree.
    RefreshGit,
    /// Generate or regenerate AI output for the currently selected directory, file, or symbol.
    GenerateAi,
    /// Toggle between manual-trigger and automatic selection-following AI generation.
    ToggleAiGenerationMode,
    /// Open/close the AI model picker modal.
    ModelPicker,
    /// Apply the model picker's staged model and reasoning budget together.
    AiSettingsSelected {
        /// Provider model id, or empty while the run loop resolves the highlighted row.
        model: String,
        /// Reasoning budget, or empty while the run loop resolves the staged control.
        reasoning_effort: String,
    },
    /// Move to the previous reasoning budget while the model picker is open.
    ReasoningEffortPrevious,
    /// Move to the next reasoning budget while the model picker is open.
    ReasoningEffortNext,
    /// Open/close the comparison-base picker modal.
    BasePicker,
    /// The user picked a base ref in the picker (the dispatcher applies it).
    BaseSelected(String),
    /// A typed character appended to the open picker's filter query.
    PickerInput(char),
    /// Backspace in the open picker's filter query.
    PickerBackspace,
    /// Jump to the next / previous diff hunk.
    NextHunk,
    /// Jump to the previous diff hunk.
    PrevHunk,
    /// Toggle zoom of the focused pane into the whole main area (`z`).
    ToggleZoom,
    /// Toggle smart wrap in the diff pane (`W`); raw mode clips + h-scrolls.
    ToggleWrap,
    /// Reset the diff pane's horizontal scroll to zero (`0`).
    ResetHScroll,
    /// Mouse motion: set the generated-plan node currently under the pointer. `None`
    /// clears transient hover and its linked diff highlighting.
    HoverPlanNode(Option<PlanNodeTarget>),
    /// Mouse click: expand or collapse one generated-plan box in place.
    TogglePlanNode(PlanNodeTarget),
    /// Mouse drag: move one generated-plan box to this content-local top-left position.
    /// The position is retained for the current plan; it never mutates the AI plan.
    MovePlanNode {
        /// Box being moved.
        target: PlanNodeTarget,
        /// Requested content-local column.
        x: u16,
        /// Requested content-local row.
        y: u16,
    },
    /// Mouse click: show or hide clipped relationship text in an in-place canvas overlay.
    TogglePlanRelationship(PlanRelationshipTarget),
    /// Mouse wheel: set the absolute page offset of the visible relationship overlay.
    ScrollPlanRelationshipOverlay {
        /// Clamped overlay line offset from the complete relationship text.
        offset: usize,
    },
    /// Mouse drag preview: update the visible diff text selection.
    SetDiffSelection(DiffTextSelection),
    /// Mouse click: clear the retained diff highlight without copying anything.
    ClearDiffSelection,
    /// Mouse release: retain the selection and copy this exact visible text.
    CommitDiffSelection {
        /// Final selection range.
        selection: DiffTextSelection,
        /// Text extracted from the same retained diff frame.
        text: String,
    },
    /// Publish or clear the retained diff excerpt for local agent clients. Produced by the
    /// run loop after applying the corresponding local gesture.
    SetAgentDiffSelection(Option<crate::snapshot::SelectedDiffContext>),
    /// Mouse: select the file/symbol row at this logical index (and focus Files).
    /// The selection tracker emits the same SelectionChanged a keyboard move would.
    SelectFileRow {
        /// The logical (selectable) row index.
        logical_index: usize,
    },
    /// Mouse wheel: set the independently scrollable region under the pointer. This is
    /// absolute because the retained frame geometry owns the displayed/clamped origin.
    ScrollRegion {
        /// Stable region identity.
        region: crate::scroll::ScrollRegionId,
        /// New absolute row offset, already clamped to the rendered content.
        offset: usize,
    },
    /// Mouse drag: set any registered divider's leading/trailing extent (clamped).
    ResizeDivider {
        /// Stable structural divider identity.
        divider: crate::divider::DividerId,
        /// Absolute requested extent in terminal cells.
        extent: u16,
    },
    /// Persist the stable global view preferences. Produced by the run loop after an
    /// explicit preference change; never produced directly by a key mapping.
    PersistUiPreferences(crate::app::UiPreferences),
    /// The key did not map to an action.
    None,
}

/// Map a key event to an [`Action`] in the current app context.
///
/// Returns [`Action::None`] for release/repeat events (Windows emits them) and unmapped
/// keys. When the help modal is open, any key other than `?`/`Esc` is swallowed.
#[must_use]
pub fn map_key(key: KeyEvent, app: &App) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    if key.kind == KeyEventKind::Repeat {
        if app.status_detail.is_some() || app.show_help {
            return Action::None;
        }
        // Repeats are navigation-only: holding an arrow (or j/k outside a picker) should
        // move continuously, while commands and toggles must still fire once per press.
        return match key.code {
            KeyCode::Up => Action::Up,
            KeyCode::Down => Action::Down,
            KeyCode::Char('k') if !app.show_model_picker && !app.show_base_picker => Action::Up,
            KeyCode::Char('j') if !app.show_model_picker && !app.show_base_picker => Action::Down,
            _ => Action::None,
        };
    }
    if app.status_detail.is_some() {
        return match key.code {
            KeyCode::Esc => Action::ToggleStatusDetail,
            _ => Action::None,
        };
    }
    if app.show_help {
        return match key.code {
            KeyCode::Char('?') | KeyCode::Esc => Action::ToggleHelp,
            _ => Action::None,
        };
    }
    if app.show_model_picker {
        return match key.code {
            KeyCode::Left => Action::ReasoningEffortPrevious,
            KeyCode::Right => Action::ReasoningEffortNext,
            _ => picker_key(
                key,
                Action::ModelPicker,
                Action::AiSettingsSelected {
                    model: String::new(),
                    reasoning_effort: String::new(),
                },
            ),
        };
    }
    if app.show_base_picker {
        return picker_key(key, Action::BasePicker, Action::BaseSelected(String::new()));
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
        // Esc exits a pinned zoom; otherwise it is deliberately inert (modals handle
        // their own Esc above).
        KeyCode::Esc if app.zoomed => Action::ToggleZoom,
        KeyCode::Esc => Action::None,
        // Tab controls file expansion, not focus cycling. Symbol analysis runs
        // asynchronously regardless of expansion.
        KeyCode::Tab if app.focused == Pane::Files => {
            app.tree_toggle_action().unwrap_or(Action::None)
        }
        KeyCode::Tab | KeyCode::BackTab => Action::None,
        KeyCode::Char('1') => Action::Focus(Pane::Files),
        KeyCode::Char('2') => Action::Focus(Pane::Diff),
        KeyCode::Char('3') => Action::Focus(Pane::Impact),
        KeyCode::Char('s') => Action::ScopeStaged,
        KeyCode::Char('u') => Action::ScopeUnstaged,
        KeyCode::Char('B') => Action::ScopeBranch,
        KeyCode::Char('w') => Action::ScopeWorking,
        KeyCode::Char('S') => Action::ScopeCycle,
        KeyCode::Char('g') => Action::RefreshGit,
        KeyCode::Char('a') => Action::GenerateAi,
        KeyCode::Char('A') => Action::ToggleAiGenerationMode,
        KeyCode::Char('m') => Action::ModelPicker,
        KeyCode::Char('b') => Action::BasePicker,
        KeyCode::Char('j') | KeyCode::Down => Action::Down,
        KeyCode::Char('k') | KeyCode::Up => Action::Up,
        KeyCode::Enter => Action::Activate,
        KeyCode::Char(' ') => Action::ToggleExpand,
        KeyCode::Char('h') | KeyCode::Left => Action::Collapse,
        KeyCode::Char('l') | KeyCode::Right => Action::Expand,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::Char('n') => Action::NextHunk,
        KeyCode::Char('N') => Action::PrevHunk,
        KeyCode::Char('z') => Action::ToggleZoom,
        KeyCode::Char('v') if app.focused == Pane::Files => Action::ToggleReviewed,
        KeyCode::Char('v') => Action::None,
        KeyCode::Char('W') => Action::ToggleWrap,
        KeyCode::Char('0') => Action::ResetHScroll,
        KeyCode::Home => Action::Top,
        KeyCode::Char('G') | KeyCode::End => Action::Bottom,
        _ => Action::None,
    }
}

/// Keys while a picker modal is open: the modal swallows everything, but most keys now
/// feed the type-to-filter query. `j`/`k`/arrows still navigate (navigation wins over
/// input — the existing tradeoff), Esc closes, Enter selects, Backspace edits the query,
/// and any other plain character is appended to the query (`close`/`select` are the
/// picker's toggle and selection actions; the selection name is filled in later).
fn picker_key(key: KeyEvent, close: Action, select: Action) -> Action {
    // A searchable picker is a text field: arrows navigate, every plain character is filter
    // input (j/k included — otherwise refs containing them can't be searched), Backspace
    // deletes, Enter accepts, Esc cancels.
    match key.code {
        KeyCode::Esc => close,
        KeyCode::Up => Action::Up,
        KeyCode::Down => Action::Down,
        KeyCode::Enter => select,
        KeyCode::Backspace => Action::PickerBackspace,
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            Action::PickerInput(c)
        }
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
    fn held_navigation_repeats_but_commands_do_not() {
        let mut down = key(KeyCode::Down);
        down.kind = KeyEventKind::Repeat;
        assert_eq!(map_key(down, &app()), Action::Down);

        let mut j = key(KeyCode::Char('j'));
        j.kind = KeyEventKind::Repeat;
        assert_eq!(map_key(j, &app()), Action::Down);

        let mut toggle = key(KeyCode::Char('A'));
        toggle.kind = KeyEventKind::Repeat;
        assert_eq!(map_key(toggle, &app()), Action::None);

        let mut picker = app();
        picker.show_base_picker = true;
        assert_eq!(map_key(down, &picker), Action::Down);
        assert_eq!(map_key(j, &picker), Action::None);
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
        assert_eq!(map_key(key(KeyCode::Home), &app()), Action::Top);
        assert_eq!(map_key(key(KeyCode::Char('G')), &app()), Action::Bottom);
    }

    #[test]
    fn scope_keys() {
        assert_eq!(
            map_key(key(KeyCode::Char('s')), &app()),
            Action::ScopeStaged
        );
        assert_eq!(
            map_key(key(KeyCode::Char('u')), &app()),
            Action::ScopeUnstaged
        );
        assert_eq!(
            map_key(key(KeyCode::Char('B')), &app()),
            Action::ScopeBranch
        );
        assert_eq!(
            map_key(key(KeyCode::Char('w')), &app()),
            Action::ScopeWorking
        );
        assert_eq!(map_key(key(KeyCode::Char('S')), &app()), Action::ScopeCycle);
    }

    #[test]
    fn manual_refresh_key() {
        assert_eq!(map_key(key(KeyCode::Char('g')), &app()), Action::RefreshGit);
        assert_eq!(map_key(key(KeyCode::Char('R')), &app()), Action::None);
    }

    #[test]
    fn focus_keys() {
        // Tab is lazy file expansion on the files pane, inert elsewhere; Shift-Tab is
        // inert everywhere. 1/2/3 focus panes directly.
        let mut files_focused = app();
        files_focused.focused = Pane::Files;
        files_focused.update(crate::snapshot::UiSnapshot {
            files: vec![crate::snapshot::FileRow {
                path: "a.go".to_string(),
                status: "M",
                changed_symbol_count: 0,
                added_lines: 0,
                removed_lines: 0,
                symbols: Vec::new(),
                expanded: false,
                semantic: crate::snapshot::FileSemanticLoad::Unloaded,
            }],
            ..Default::default()
        });
        assert_eq!(
            map_key(key(KeyCode::Tab), &files_focused),
            Action::SetFileExpanded {
                path: "a.go".to_string(),
                expanded: true,
            }
        );
        let mut diff_focused = app();
        diff_focused.focused = Pane::Diff;
        assert_eq!(map_key(key(KeyCode::Tab), &diff_focused), Action::None);
        assert_eq!(map_key(key(KeyCode::BackTab), &files_focused), Action::None);
        assert_eq!(
            map_key(key(KeyCode::Char('2')), &app()),
            Action::Focus(Pane::Diff)
        );
    }

    #[test]
    fn tree_keys() {
        assert_eq!(
            map_key(key(KeyCode::Char(' ')), &app()),
            Action::ToggleExpand
        );
        assert_eq!(map_key(key(KeyCode::Char('h')), &app()), Action::Collapse);
        assert_eq!(map_key(key(KeyCode::Char('l')), &app()), Action::Expand);
    }

    #[test]
    fn resize_keys_are_unbound() {
        assert_eq!(map_key(key(KeyCode::Char('[')), &app()), Action::None);
        assert_eq!(map_key(key(KeyCode::Char(']')), &app()), Action::None);
        assert_eq!(map_key(key(KeyCode::Char('/')), &app()), Action::None);
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
    fn status_detail_swallows_keys_and_esc_closes_it() {
        let mut a = app();
        a.snapshot.status = crate::snapshot::StatusMessage {
            text: "provider returned HTTP 400".to_string(),
            detail: None,
            level: crate::snapshot::StatusLevel::Warning,
        };
        a.apply(Action::ToggleStatusDetail);
        assert_eq!(map_key(key(KeyCode::Char('q')), &a), Action::None);
        assert_eq!(map_key(key(KeyCode::Char('?')), &a), Action::None);
        assert_eq!(map_key(key(KeyCode::Esc), &a), Action::ToggleStatusDetail);
    }

    #[test]
    fn unmapped_is_none() {
        assert_eq!(map_key(key(KeyCode::Char('x')), &app()), Action::None);
    }

    #[test]
    fn zoom_wrap_reset_keys() {
        assert_eq!(map_key(key(KeyCode::Char('z')), &app()), Action::ToggleZoom);
        assert_eq!(map_key(key(KeyCode::Char('W')), &app()), Action::ToggleWrap);
        assert_eq!(
            map_key(key(KeyCode::Char('0')), &app()),
            Action::ResetHScroll
        );
    }

    #[test]
    fn ai_generation_keys_use_lowercase_for_trigger_and_uppercase_for_mode() {
        assert_eq!(map_key(key(KeyCode::Char('a')), &app()), Action::GenerateAi);
        assert_eq!(
            map_key(key(KeyCode::Char('A')), &app()),
            Action::ToggleAiGenerationMode
        );
    }

    #[test]
    fn v_toggles_review_only_in_the_files_pane() {
        assert_eq!(
            map_key(key(KeyCode::Char('v')), &app()),
            Action::ToggleReviewed
        );
        let mut a = app();
        a.focused = Pane::Diff;
        assert_eq!(map_key(key(KeyCode::Char('v')), &a), Action::None);
    }

    #[test]
    fn esc_exits_zoom_but_is_otherwise_inert() {
        assert_eq!(map_key(key(KeyCode::Esc), &app()), Action::None);
        let mut a = app();
        a.zoomed = true;
        assert_eq!(map_key(key(KeyCode::Esc), &a), Action::ToggleZoom);
    }

    #[test]
    fn b_opens_base_picker() {
        assert_eq!(map_key(key(KeyCode::Char('b')), &app()), Action::BasePicker);
    }

    #[test]
    fn base_picker_modal_swallows_keys() {
        let mut a = app();
        a.show_base_picker = true;
        // Non-character keys stay swallowed; characters now feed the filter query.
        assert_eq!(map_key(key(KeyCode::Tab), &a), Action::None);
        assert_eq!(map_key(key(KeyCode::Esc), &a), Action::BasePicker);
        // Plain chars (incl. j/k) are filter input; arrows navigate.
        assert_eq!(
            map_key(key(KeyCode::Char('j')), &a),
            Action::PickerInput('j')
        );
        assert_eq!(
            map_key(key(KeyCode::Char('k')), &a),
            Action::PickerInput('k')
        );
        assert_eq!(map_key(key(KeyCode::Down), &a), Action::Down);
        assert_eq!(map_key(key(KeyCode::Up), &a), Action::Up);
        assert_eq!(
            map_key(key(KeyCode::Enter), &a),
            Action::BaseSelected(String::new())
        );
    }

    #[test]
    fn open_picker_maps_chars_and_backspace_to_query_input() {
        for (open, close, select) in [
            (
                Action::BasePicker,
                Action::BasePicker,
                Action::BaseSelected(String::new()),
            ),
            (
                Action::ModelPicker,
                Action::ModelPicker,
                Action::AiSettingsSelected {
                    model: String::new(),
                    reasoning_effort: String::new(),
                },
            ),
        ] {
            let mut a = app();
            a.apply(open);
            assert_eq!(
                map_key(key(KeyCode::Char('q')), &a),
                Action::PickerInput('q')
            );
            assert_eq!(
                map_key(key(KeyCode::Char('m')), &a),
                Action::PickerInput('m')
            );
            assert_eq!(
                map_key(key(KeyCode::Char('1')), &a),
                Action::PickerInput('1')
            );
            assert_eq!(
                map_key(key(KeyCode::Backspace), &a),
                Action::PickerBackspace
            );
            assert_eq!(map_key(key(KeyCode::Esc), &a), close);
            assert_eq!(map_key(key(KeyCode::Enter), &a), select);
            // Plain chars (incl. j/k) are filter input; modified chars are swallowed.
            assert_eq!(
                map_key(key(KeyCode::Char('j')), &a),
                Action::PickerInput('j')
            );
            assert_eq!(
                map_key(key(KeyCode::Char('k')), &a),
                Action::PickerInput('k')
            );
            let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
            assert_eq!(map_key(ctrl_x, &a), Action::None);
        }
    }

    #[test]
    fn scope_cycle_order() {
        assert_eq!(next_scope(ChangeScope::Branch), ChangeScope::Staged);
        assert_eq!(next_scope(ChangeScope::Staged), ChangeScope::Unstaged);
        assert_eq!(next_scope(ChangeScope::Unstaged), ChangeScope::Working);
        assert_eq!(next_scope(ChangeScope::Working), ChangeScope::Branch);
    }
}
