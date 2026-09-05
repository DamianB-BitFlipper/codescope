//! Headless render tests (ratatui `TestBackend`): no real terminal is touched.

use codescope_core::{
    AiStatus, ChangeScope, FileId, FormKind, LsStatus, PlanEdge, PlanEdgeKind, PlanEvidence,
    PlanNode, PlanNodeChange, VisualizationPlan, VizForm,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use codescope_tui::app::App;
use codescope_tui::render::render;
use codescope_tui::snapshot::{
    DiffPane, DiffRow, FileRow, RepoBar, ScopeCounts, SemanticPane, SymbolRow, UiSnapshot,
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
            changed_symbol_count: 1,
            added_lines: 1,
            removed_lines: 1,
            expanded: true,
            semantic: codescope_tui::snapshot::FileSemanticLoad::Ready,
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
            focused_symbol: None,
            selection_focus_row: None,
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
            syntax: std::sync::Arc::default(),
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
    assert!(text.contains("branch  LSP ✓"), "scope + lsp in the top bar");
    assert!(text.contains("service.go"), "file row");
    assert!(text.contains("GetDisplayName"), "symbol row");
    assert!(
        text.contains("+        prefix + name"),
        "tab-indented diff add line"
    );
    assert!(text.contains("AI in progress"), "generated Impact viewport");
    assert!(
        !text.contains("SELECTED CHANGE"),
        "no relationship sidebar before rows arrive"
    );
}

#[test]
fn impact_pane_defaults_to_generated_content_at_normal_size() {
    // Impact is permanent, but its relationship sidebar is data-driven rather than default
    // chrome while the selection is unresolved.
    let backend = TestBackend::new(100, 30);
    let mut t = Terminal::new(backend).unwrap();
    let app = App::new();
    let snap = sample();
    t.draw(|f| render(f, &app, &snap)).unwrap();
    let text = buffer_text(&t);
    assert!(text.contains("AI in progress"));
    assert!(!text.contains("SELECTED CHANGE"));
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
        !text.contains("+        prefix + name"),
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
fn help_modal_renders_its_controls() {
    let backend = TestBackend::new(160, 40);
    let mut t = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.show_help = true;
    t.draw(|f| render(f, &app, &sample())).unwrap();
    assert!(buffer_text(&t).contains("codescope — controls"));
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

#[test]
fn ai_plan_renders_after_loading_to_ready_transition() {
    // Regression: the validated generated breakdown published in `UiSnapshot::semantic`
    // must render beside deterministic Impact after Loading → Ready.
    let backend = TestBackend::new(160, 40);
    let mut t = Terminal::new(backend).unwrap();
    let mut app = App::new();

    // AI generation is automatic; the old manual a/A controls are intentionally inert.
    assert_eq!(map_key(key(KeyCode::Char('a')), &app), Action::GenerateAi);
    assert_eq!(
        map_key(key(KeyCode::Char('A')), &app),
        Action::ToggleAiGenerationMode
    );

    let mut loading = sample();
    loading.ai = AiStatus::Loading {
        since_epoch: codescope_core::Epoch(3),
    };
    loading.ai_activity = codescope_tui::snapshot::AiActivity {
        active: true,
        waiting_for_model: true,
        calls: vec![codescope_tui::snapshot::AiToolCallActivity {
            id: "call-1".to_string(),
            name: "git_diff_file".to_string(),
            detail: "service.go · hunk 0".to_string(),
            error: None,
            state: codescope_tui::snapshot::AiToolCallActivityState::Succeeded,
        }],
    };
    app.update(loading.clone());
    t.draw(|f| render(f, &app, &loading)).unwrap();
    assert!(
        buffer_text(&t).contains("✓ git_diff_file · service.go · hunk 0"),
        "the generated half shows tool progress while loading"
    );

    let mut ready = sample();
    ready.ai = AiStatus::Ready {
        epoch: codescope_core::Epoch(3),
    };
    let mut plan = VisualizationPlan::new(codescope_core::Epoch(3));
    plan.intent = "RetryPolicy limits the attempts consumed by handleRequest.".to_string();
    plan.forms.push(VizForm {
        kind: FormKind::RelationshipFlow,
        nodes: vec![
            PlanNode::new("policy", "RetryPolicy", PlanNodeChange::Modified)
                .with_detail("introduces a bounded retry budget"),
            PlanNode::new("handler", "handleRequest", PlanNodeChange::Modified)
                .with_detail("consumes one retry attempt"),
        ],
        edges: vec![PlanEdge {
            from: "policy".into(),
            to: "handler".into(),
            kind: PlanEdgeKind::Calls,
            label: Some("grants each attempt".into()),
        }],
    });
    plan.evidence.push(PlanEvidence {
        file: FileId::new("src/retry.rs").unwrap(),
        hunk: Some(0),
        symbol: Some("RetryPolicy".into()),
        range: None,
        reason: "defines the retry budget".into(),
    });
    ready.semantic = SemanticPane {
        plan: Some(plan),
        report: None,
        note: String::new(),
        ai_generated: true,
    };
    app.update(ready.clone());
    app.dividers.set(codescope_tui::DividerId::WorkReview, 16);
    t.draw(|f| render(f, &app, &ready)).unwrap();
    let text = buffer_text(&t);
    assert!(!text.contains("Impact"), "combined title removed: {text}");
    assert!(!text.contains("AI Plan"), "retired tab name: {text}");
    assert!(
        text.contains("RetryPolicy limits the attempts consumed by handleRequest."),
        "plan description: {text}"
    );
    assert_eq!(
        text.matches("RetryPolicy limits the attempts consumed by handleRequest.")
            .count(),
        1,
        "description must not be repeated"
    );
    assert!(text.contains("RetryPolicy"), "plan root row: {text}");
    assert!(text.contains("handleRequest"), "plan child row: {text}");
    assert!(
        text.contains("introduces a bounded retry") && text.contains("budget"),
        "reviewer-facing explanation: {text}"
    );
    assert!(
        text.contains("▷"),
        "inferred relationship arrow is dashed: {text}"
    );
    assert!(
        !text.contains("▶"),
        "solid arrows stay reserved for verified relationships: {text}"
    );
    assert!(
        !text.contains("inferred from cited diff"),
        "retired provenance legend: {text}"
    );
    assert!(
        !text.contains("retry.rs") && !text.contains("defines the retry budget"),
        "grounding evidence stays out of the diagram UI: {text}"
    );
    assert!(!text.contains("diff modified"), "old change badge: {text}");
    assert!(
        !text.contains("LSP warning"),
        "old diagnostic badge: {text}"
    );
    assert!(
        !text.contains(" !"),
        "opaque diagnostic marker removed: {text}"
    );
    assert!(
        !text.contains("SELECTED CHANGE"),
        "an empty relationship sidebar stays absent: {text}"
    );
}

/// The dispatcher publishes the validation report with every AI plan; a sanitized plan
/// (the sequence extra-edge sanitizer dropped the back-edge) renders one WARN line
/// before the diagram. Snapshot-level contract: reasons never reach the small pane.
#[test]
fn sanitized_sequence_plan_warns_in_the_generated_pane() {
    let backend = TestBackend::new(160, 40);
    let mut t = Terminal::new(backend).unwrap();
    let mut app = App::new();
    let mut ready = sample();
    let mut plan = VisualizationPlan::new(codescope_core::Epoch(3));
    plan.intent = "The server stops accepting work before closing listeners.".to_string();
    plan.forms.push(VizForm {
        kind: FormKind::Sequence,
        nodes: vec![
            PlanNode::new("n1", "markUnready", PlanNodeChange::Modified)
                .with_detail("flips readiness to false first"),
            PlanNode::new("n2", "drain", PlanNodeChange::Added)
                .with_detail("waits out in-flight requests"),
            PlanNode::new("n3", "closeListeners", PlanNodeChange::Modified)
                .with_detail("closes listeners last"),
        ],
        // The sanitizer keeps the consecutive chain; n3 -> n1 is dropped and recorded.
        edges: vec![
            PlanEdge {
                from: "n1".into(),
                to: "n2".into(),
                kind: PlanEdgeKind::Writes,
                label: Some("unready precedes drain".into()),
            },
            PlanEdge {
                from: "n2".into(),
                to: "n3".into(),
                kind: PlanEdgeKind::Writes,
                label: Some("drain completes before close".into()),
            },
            PlanEdge {
                from: "n3".into(),
                to: "n1".into(),
                kind: PlanEdgeKind::Writes,
                label: Some("closes the loop".into()),
            },
        ],
    });
    plan.evidence.push(PlanEvidence {
        file: FileId::new("internal/service/service.go").unwrap(),
        hunk: Some(0),
        symbol: None,
        range: None,
        reason: "the drain ordering hunk".into(),
    });
    ready.semantic = SemanticPane {
        plan: Some(plan),
        report: Some(codescope_core::ValidationReport::with_drops(vec![
            codescope_core::DroppedItem {
                subject: "edge n3 -> n1 in form 0".to_string(),
                reason: "nonconsecutive or duplicate sequence edge".to_string(),
            },
        ])),
        note: String::new(),
        ai_generated: true,
    };
    app.update(ready.clone());
    t.draw(|f| render(f, &app, &ready)).unwrap();
    let text = buffer_text(&t);
    assert!(
        text.contains("⚠ sanitized AI plan · 1 item removed"),
        "one concise WARN line before the plan (singular): {text}"
    );
    assert!(
        !text.contains("nonconsecutive or duplicate sequence edge"),
        "drop reasons stay out of the pane: {text}"
    );
    let warning = text.find("sanitized AI plan").expect("warning rendered");
    let description = text
        .find("The server stops accepting work before closing listeners.")
        .expect("plan description rendered");
    assert!(
        warning < description,
        "the warning precedes the plan: {text}"
    );
}

use codescope_tui::action::{Action, map_key};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn picker_snapshot() -> UiSnapshot {
    let mut s = sample();
    s.ai_model = "openai/gpt-5-mini".to_string();
    s.ai_provider = "openai".to_string();
    s.ai_reasoning_effort = "medium".to_string();
    s.available_reasoning_efforts = [
        "default", "none", "minimal", "low", "medium", "high", "xhigh", "max",
    ]
    .map(str::to_string)
    .to_vec();
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
    assert_eq!(
        map_key(key(KeyCode::Left), &app),
        Action::ReasoningEffortPrevious
    );
    assert_eq!(
        map_key(key(KeyCode::Right), &app),
        Action::ReasoningEffortNext
    );
    assert_eq!(
        map_key(key(KeyCode::Char('j')), &app),
        Action::PickerInput('j')
    );
    assert_eq!(
        map_key(key(KeyCode::Enter), &app),
        Action::AiSettingsSelected {
            model: String::new(),
            reasoning_effort: String::new(),
        }
    );
}

#[test]
fn picker_renders_models_and_current() {
    let backend = TestBackend::new(120, 30);
    let mut t = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.update(picker_snapshot());
    app.apply(Action::ModelPicker);
    t.draw(|f| render(f, &app, &app.snapshot)).unwrap();
    let text = buffer_text(&t);
    assert!(text.contains("AI settings"), "picker title");
    assert!(text.contains("reasoning effort"), "reasoning control");
    assert!(text.contains("medium"), "current reasoning effort");
    assert!(text.contains("openai/gpt-5"), "a listed model");
    assert!(text.contains("claude-fable-5"), "another listed model");
    assert!(text.contains("●"), "current-model marker");
}

#[test]
fn picker_discovery_failure_keeps_the_current_model_available() {
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.show_model_picker = true;
    let mut snap = picker_snapshot();
    snap.available_models = vec![snap.ai_model.clone()];
    snap.model_list_error = Some("provider returned http 404".to_string());
    terminal.draw(|frame| render(frame, &app, &snap)).unwrap();
    let text = buffer_text(&terminal);
    assert!(text.contains("discovery failed"), "{text}");
    assert!(text.contains("current model remains selectable"), "{text}");
    assert!(!text.contains("AI is not configured"), "{text}");
}

#[test]
fn picker_offers_unmatched_query_as_an_exact_model_id() {
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.show_model_picker = true;
    app.model_query = "vendor/new-model".to_string();
    terminal
        .draw(|frame| render(frame, &app, &picker_snapshot()))
        .unwrap();
    let text = buffer_text(&terminal);
    assert!(text.contains("Enter to use"), "{text}");
    assert!(text.contains("vendor/new-model"), "{text}");
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
fn top_bar_shows_provider_selected_model_and_status_in_order() {
    let backend = TestBackend::new(160, 40);
    let mut t = Terminal::new(backend).unwrap();
    let app = App::new();
    t.draw(|f| render(f, &app, &picker_snapshot())).unwrap();
    let text = buffer_text(&t);
    assert!(
        text.contains("openai openai/gpt-5-mini reasoning:medium ✓"),
        "provider, selected model, reasoning effort, then status: {text:?}"
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
