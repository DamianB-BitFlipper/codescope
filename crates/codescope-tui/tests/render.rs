//! Headless render tests (ratatui `TestBackend`): no real terminal is touched.

use codescope_core::{AiStatus, ChangeScope, LsStatus};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use codescope_tui::app::App;
use codescope_tui::render::render;
use codescope_tui::snapshot::{
    DiffPane, DiffRow, FileRow, RepoBar, ScopeCounts, SemRow, SemanticPane, SymbolRow, UiSnapshot,
};

fn sample() -> UiSnapshot {
    UiSnapshot {
        repo: RepoBar {
            repo_name: "codescopefx".to_string(),
            branch: "feature/api-changes".to_string(),
            base: Some("main".to_string()),
            ahead: 2,
            behind: 0,
        },
        scope: ChangeScope::Branch,
        scope_counts: ScopeCounts {
            branch: 4,
            staged: 2,
            unstaged: 2,
        },
        files: vec![FileRow {
            path: "internal/service/service.go".to_string(),
            status: "M",
            expanded: true,
            symbols: vec![SymbolRow {
                name: "GetDisplayName".to_string(),
                change: "modified",
                confidence: "",
                has_diagnostic: false,
                position: None,
            }],
        }],
        diff: DiffPane {
            title: "internal/service/service.go".to_string(),
            rows: vec![
                DiffRow::HunkHeader("@@ -10,3 +10,4 @@ func GetDisplayName".to_string()),
                DiffRow::Context {
                    old_ln: 10,
                    new_ln: 10,
                    text: "func (s *UserService) GetDisplayName(".to_string(),
                },
                DiffRow::Add {
                    new_ln: 13,
                    text: "\t\tprefix + name".to_string(),
                },
            ],
            current_hunk: 1,
            total_hunks: 1,
        },
        semantic: SemanticPane {
            title: "callers of GetDisplayName".to_string(),
            rows: vec![
                SemRow {
                    depth: 0,
                    label: "GetDisplayName".to_string(),
                    relation: "changed",
                    changed: true,
                    has_diagnostic: false,
                },
                SemRow {
                    depth: 1,
                    label: "Handler.HandleGetUser".to_string(),
                    relation: "calls",
                    changed: false,
                    has_diagnostic: false,
                },
            ],
            note: String::new(),
            ai_generated: false,
        },
        ls: LsStatus::Ready,
        ai: AiStatus::Ready {
            epoch: codescope_core::Epoch(3),
        },
        message: String::new(),
        epoch: codescope_core::Epoch(3),
        refreshing: false,
        ..UiSnapshot::default()
    }
}

fn buffer_text(t: &Terminal<TestBackend>) -> String {
    t.backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}

#[test]
fn wide_layout_shows_all_panes() {
    let backend = TestBackend::new(160, 40);
    let mut t = Terminal::new(backend).unwrap();
    let app = App::new();
    let snap = sample();
    t.draw(|f| render(f, &app, &snap)).unwrap();
    let text = buffer_text(&t);
    assert!(text.contains("codescopefx"), "repo name");
    assert!(text.contains("feature/api-changes"), "branch");
    assert!(text.contains("scope: branch"), "scope");
    assert!(text.contains("service.go"), "file row");
    assert!(text.contains("GetDisplayName"), "symbol row");
    assert!(text.contains("+prefix + name"), "diff add line");
    assert!(text.contains("Handler.HandleGetUser"), "semantic row");
    assert!(text.contains("AI: ✓"), "ai status");
}

#[test]
fn medium_layout_hides_semantic_until_focused() {
    let backend = TestBackend::new(100, 30);
    let mut t = Terminal::new(backend).unwrap();
    let mut app = App::new();
    let snap = sample();
    t.draw(|f| render(f, &app, &snap)).unwrap();
    // semantic pane is an overlay: not shown when files focused
    assert!(!buffer_text(&t).contains("callers of"));
    app.focused = codescope_tui::Pane::Semantic;
    t.draw(|f| render(f, &app, &snap)).unwrap();
    assert!(buffer_text(&t).contains("callers of GetDisplayName"));
}

#[test]
fn narrow_layout_shows_one_pane() {
    let backend = TestBackend::new(60, 20);
    let mut t = Terminal::new(backend).unwrap();
    let app = App::new();
    t.draw(|f| render(f, &app, &sample())).unwrap();
    let text = buffer_text(&t);
    assert!(text.contains("service.go"), "files pane present");
    assert!(
        !text.contains("+prefix + name"),
        "diff pane not simultaneously visible"
    );
}

#[test]
fn too_small_renders_message() {
    let backend = TestBackend::new(24, 6);
    let mut t = Terminal::new(backend).unwrap();
    let app = App::new();
    t.draw(|f| render(f, &app, &sample())).unwrap();
    assert!(buffer_text(&t).contains("too small"));
}

#[test]
fn width_sweep_never_panics() {
    let mut w = 20u16;
    while w <= 200 {
        let mut h = 6u16;
        while h <= 40 {
            let backend = TestBackend::new(w, h);
            let mut t = Terminal::new(backend).unwrap();
            let app = App::new();
            t.draw(|f| render(f, &app, &sample())).unwrap();
            h += 5;
        }
        w += 7;
    }
}

#[test]
fn help_modal_covers_screen() {
    let backend = TestBackend::new(160, 40);
    let mut t = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.show_help = true;
    t.draw(|f| render(f, &app, &sample())).unwrap();
    assert!(buffer_text(&t).contains("keyboard controls"));
}

#[test]
fn empty_state_is_graceful() {
    let backend = TestBackend::new(160, 40);
    let mut t = Terminal::new(backend).unwrap();
    let app = App::new();
    t.draw(|f| render(f, &app, &UiSnapshot::placeholder()))
        .unwrap();
    let text = buffer_text(&t);
    assert!(text.contains("no changes") || text.contains("scanning repository"));
}

use codescope_tui::action::{map_key, Action};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn picker_snapshot() -> UiSnapshot {
    let mut s = sample();
    s.ai_model = "openai/gpt-5-mini".to_string();
    s.available_models = vec![
        "openai/gpt-5-mini".to_string(),
        "openai/gpt-5".to_string(),
        "anthropic/claude-fable-5".to_string(),
    ];
    s
}

#[test]
fn m_opens_model_picker() {
    let app = App::new();
    assert_eq!(map_key(key(KeyCode::Char('m')), &app), Action::ModelPicker);
}

#[test]
fn picker_modal_swallows_keys() {
    let mut app = App::new();
    app.show_model_picker = true;
    // Characters feed the filter query; non-character keys stay swallowed.
    assert_eq!(map_key(key(KeyCode::Tab), &app), Action::None);
    assert_eq!(
        map_key(key(KeyCode::Char('q')), &app),
        Action::PickerInput('q')
    );
    assert_eq!(
        map_key(key(KeyCode::Backspace), &app),
        Action::PickerBackspace
    );
    assert_eq!(map_key(key(KeyCode::Esc), &app), Action::ModelPicker);
    assert_eq!(map_key(key(KeyCode::Char('j')), &app), Action::Down);
    assert_eq!(
        map_key(key(KeyCode::Enter), &app),
        Action::ModelSelected(String::new())
    );
}

#[test]
fn picker_renders_models_and_current() {
    let backend = TestBackend::new(120, 30);
    let mut t = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.show_model_picker = true;
    t.draw(|f| render(f, &app, &picker_snapshot())).unwrap();
    let text = buffer_text(&t);
    assert!(text.contains("AI model"), "picker title");
    assert!(text.contains("openai/gpt-5"), "a listed model");
    assert!(text.contains("claude-fable-5"), "another listed model");
    assert!(text.contains("●"), "current-model marker");
}

#[test]
fn picker_filter_shows_query_and_only_matching_models() {
    let backend = TestBackend::new(120, 30);
    let mut t = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.show_model_picker = true;
    app.model_query = "gpt-5".to_string();
    t.draw(|f| render(f, &app, &picker_snapshot())).unwrap();
    let text = buffer_text(&t);
    assert!(text.contains("filter: gpt-5"), "query footer: {text}");
    assert!(text.contains("openai/gpt-5"), "matching model listed");
    assert!(
        !text.contains("claude-fable-5"),
        "non-matching model filtered out: {text}"
    );
}

#[test]
fn top_bar_shows_current_model() {
    let backend = TestBackend::new(160, 40);
    let mut t = Terminal::new(backend).unwrap();
    let app = App::new();
    t.draw(|f| render(f, &app, &picker_snapshot())).unwrap();
    assert!(
        buffer_text(&t).contains("gpt-5-mini"),
        "current model in top bar"
    );
}

#[test]
fn picker_navigation_clamps() {
    let mut app = App::new();
    app.update(picker_snapshot());
    app.show_model_picker = true;
    for _ in 0..10 {
        app.apply(Action::Down);
    }
    assert_eq!(app.model_sel, 2); // clamped at last model
    for _ in 0..10 {
        app.apply(Action::Up);
    }
    assert_eq!(app.model_sel, 0);
}
