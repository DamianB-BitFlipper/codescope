//! Headless render tests (ratatui `TestBackend`): no real terminal is touched.

use codescope_core::{AiStatus, ChangeScope, LsStatus};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use codescope_tui::app::App;
use codescope_tui::render::render;
use codescope_tui::snapshot::{DiffPane, DiffRow, FileRow, RepoBar, ScopeCounts, SemRow, SemanticPane, SymbolRow, UiSnapshot};

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
        scope_counts: ScopeCounts { branch: 4, staged: 2, unstaged: 2 },
        files: vec![FileRow {
            path: "internal/service/service.go".to_string(),
            status: "M",
            expanded: true,
            symbols: vec![SymbolRow {
                name: "GetDisplayName".to_string(),
                change: "modified",
                confidence: "",
                has_diagnostic: false,
            }],
        }],
        diff: DiffPane {
            title: "internal/service/service.go".to_string(),
            rows: vec![
                DiffRow::HunkHeader("@@ -10,3 +10,4 @@ func GetDisplayName".to_string()),
                DiffRow::Context { old_ln: 10, new_ln: 10, text: "func (s *UserService) GetDisplayName(".to_string() },
                DiffRow::Add { new_ln: 13, text: "\t\tprefix + name".to_string() },
            ],
            current_hunk: 1,
            total_hunks: 1,
        },
        semantic: SemanticPane {
            title: "callers of GetDisplayName".to_string(),
            rows: vec![
                SemRow { depth: 0, label: "GetDisplayName".to_string(), relation: "changed", changed: true, has_diagnostic: false },
                SemRow { depth: 1, label: "Handler.HandleGetUser".to_string(), relation: "calls", changed: false, has_diagnostic: false },
            ],
            note: String::new(),
            ai_generated: false,
        },
        ls: LsStatus::Ready,
        ai: AiStatus::Ready { epoch: codescope_core::Epoch(3) },
        message: String::new(),
        epoch: codescope_core::Epoch(3),
        refreshing: false,
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
        assert!(!text.contains("+prefix + name"), "diff pane not simultaneously visible");
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
        t.draw(|f| render(f, &app, &UiSnapshot::placeholder())).unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("no changes") || text.contains("scanning repository"));
}
