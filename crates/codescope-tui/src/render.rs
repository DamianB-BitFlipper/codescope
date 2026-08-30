//! The renderer: draws a [`UiSnapshot`] + [`App`] state into a ratatui frame.
//!
//! Pure with respect to I/O — `render` only touches the frame buffer, so it is fully
//! testable with ratatui's `TestBackend`. Layout is recomputed from the frame area every
//! pass (resize needs no stored state; research 04 §2).

use codescope_core::{AiStatus, ChangeScope, LsStatus};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::{App, Pane};
use crate::snapshot::{DiffRow, UiSnapshot};

/// One render pass.
pub fn render(frame: &mut Frame, app: &App, snap: &UiSnapshot) {
    let area = frame.area();
    if area.width < 30 || area.height < 8 {
        render_too_small(frame, area);
        return;
    }

    let outer = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(area);
    render_top_bar(frame, outer[0], snap);
    render_bottom_bar(frame, outer[2], app, snap);

    let main = outer[1];
    if area.width >= 120 && area.height >= 20 {
        // Wide: all three panes.
        let panes = Layout::horizontal([
            Constraint::Length(30),
            Constraint::Min(40),
            Constraint::Length(36),
        ])
        .split(main);
        render_files(frame, panes[0], app, snap);
        render_diff(frame, panes[1], app, snap);
        render_semantic(frame, panes[2], app, snap);
    } else if area.width >= 80 {
        // Medium: files + diff; semantic as an overlay when focused.
        let panes =
            Layout::horizontal([Constraint::Length(26), Constraint::Min(30)]).split(main);
        render_files(frame, panes[0], app, snap);
        render_diff(frame, panes[1], app, snap);
        if app.focused == Pane::Semantic {
            render_semantic_overlay(frame, main, app, snap);
        }
    } else {
        // Narrow: one pane at a time; Tab cycles.
        match app.focused {
            Pane::Files => render_files(frame, main, app, snap),
            Pane::Diff => render_diff(frame, main, app, snap),
            Pane::Semantic => render_semantic(frame, main, app, snap),
        }
    }

    if app.show_help {
        render_help(frame, area);
    }
    if app.show_model_picker {
        render_model_picker(frame, area, app, snap);
    }
}

fn render_too_small(frame: &mut Frame, area: Rect) {
    let msg = format!("terminal too small ({}x{})", area.width, area.height);
    let p = Paragraph::new(msg).style(Style::new().fg(Color::DarkGray));
    frame.render_widget(p, area);
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title)
}

// -- top bar ------------------------------------------------------------------

fn render_top_bar(frame: &mut Frame, area: Rect, snap: &UiSnapshot) {
    let r = &snap.repo;
    let base = r.base.as_deref().unwrap_or("?");
    let mut spans = vec![
        Span::styled(" codescope ", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(&r.repo_name, Style::new().fg(Color::Cyan)),
        Span::raw(format!("  {} ◂ {}", r.branch, base)),
    ];
    if r.ahead > 0 || r.behind > 0 {
        spans.push(Span::styled(
            format!("  +{} -{}", r.ahead, r.behind),
            Style::new().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::raw(format!(
        "  │  scope: {}",
        scope_label(snap.scope)
    )));
    spans.push(Span::raw(format!(
        "  │  lsp: {}",
        ls_label(snap.ls)
    )));
    let ai_text = if snap.ai_model.is_empty() {
        ai_label(&snap.ai)
    } else {
        format!("{} ({})", ai_label(&snap.ai), snap.ai_model)
    };
    spans.push(Span::raw(format!("  │  AI: {ai_text}")));
    if snap.refreshing {
        spans.push(Span::styled("  ⟳", Style::new().fg(Color::Yellow)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn scope_label(scope: ChangeScope) -> &'static str {
    match scope {
        ChangeScope::Branch => "branch",
        ChangeScope::Staged => "staged",
        ChangeScope::Unstaged => "unstaged",
    }
}

fn ls_label(ls: LsStatus) -> &'static str {
    match ls {
        LsStatus::Starting => "starting",
        LsStatus::Indexing => "indexing…",
        LsStatus::Ready => "✓",
        LsStatus::Degraded => "degraded",
        LsStatus::Failed => "✗",
    }
}

fn ai_label(ai: &AiStatus) -> String {
    match ai {
        AiStatus::Disabled => "off".to_string(),
        AiStatus::Idle => "idle".to_string(),
        AiStatus::Loading { .. } => "…".to_string(),
        AiStatus::Ready { .. } => "✓".to_string(),
        AiStatus::Stale { .. } => "stale".to_string(),
        AiStatus::Failed { .. } => "failed".to_string(),
    }
}

// -- bottom bar ---------------------------------------------------------------

fn render_bottom_bar(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let help = " q quit · ? help · Tab pane · s/u/B scope · a AI · n/N hunk ";
    let text = if snap.message.is_empty() {
        help.to_string()
    } else {
        format!(" {}  ·  {}", snap.message, help)
    };
    let style = if app.show_help {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

// -- files pane ---------------------------------------------------------------

fn render_files(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let focused = app.focused == Pane::Files;
    let title = format!(" changed ({}) ", snap.files.len());
    let block = pane_block(title, focused);

    let mut items: Vec<ListItem> = Vec::new();
    for f in &snap.files {
        let marker = if f.expanded { "▾" } else { "▸" };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(f.status, status_style(f.status)),
            Span::raw(" "),
            Span::styled(marker, Style::new().fg(Color::DarkGray)),
            Span::raw(" "),
            Span::raw(&f.path),
        ])));
        if f.expanded {
            for s in &f.symbols {
                let diag = if s.has_diagnostic { " !" } else { "" };
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(&s.name, Style::new().fg(change_color(s.change))),
                    Span::styled(
                        format!(" {}{}", s.change, s.confidence),
                        Style::new().fg(Color::DarkGray),
                    ),
                    Span::styled(diag, Style::new().fg(Color::Red)),
                ])));
            }
        }
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  no changes in this scope",
            Style::new().fg(Color::DarkGray),
        ))));
    }

    let mut state = ListState::default();
    if !snap.files.is_empty() {
        state.select(Some(app.file_sel.min(items.len().saturating_sub(1))));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut state);
}

// -- diff pane ----------------------------------------------------------------

fn render_diff(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let focused = app.focused == Pane::Diff;
    let d = &snap.diff;
    let title = if d.total_hunks > 0 {
        format!(" {} · hunk {}/{} ", d.title, d.current_hunk, d.total_hunks)
    } else {
        format!(" {} ", d.title)
    };
    let block = pane_block(title, focused);

    let lines: Vec<Line> = d
        .rows
        .iter()
        .map(|row| match row {
            DiffRow::HunkHeader(h) => {
                Line::from(Span::styled(h.clone(), Style::new().fg(Color::Cyan)))
            }
            DiffRow::Add { new_ln, text } => Line::from(vec![
                Span::styled(format!("{:>5} ", new_ln), Style::new().fg(Color::DarkGray)),
                Span::styled(format!("+{text}"), Style::new().fg(Color::Green)),
            ]),
            DiffRow::Del { old_ln, text } => Line::from(vec![
                Span::styled(format!("{:>5} ", old_ln), Style::new().fg(Color::DarkGray)),
                Span::styled(format!("-{text}"), Style::new().fg(Color::Red)),
            ]),
            DiffRow::Context { new_ln, text, .. } => Line::from(vec![
                Span::styled(format!("{:>5} ", new_ln), Style::new().fg(Color::DarkGray)),
                Span::styled(format!(" {text}"), Style::new().fg(Color::DarkGray)),
            ]),
        })
        .collect();

    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((app.diff_scroll, app.diff_hscroll));
    frame.render_widget(paragraph, area);
}

// -- semantic pane ------------------------------------------------------------

fn render_semantic(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let focused = app.focused == Pane::Semantic;
    let s = &snap.semantic;
    let title = if s.ai_generated {
        format!(" {} · AI ", s.title)
    } else {
        format!(" {} ", s.title)
    };
    let block = pane_block(title, focused);

    let mut items: Vec<ListItem> = s
        .rows
        .iter()
        .map(|r| {
            let guide = tree_guide(r.depth);
            let tag = if r.relation.is_empty() {
                String::new()
            } else {
                format!("  {}", r.relation)
            };
            let style = if r.changed {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new()
            };
            let diag = if r.has_diagnostic { " !" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(guide, Style::new().fg(Color::DarkGray)),
                Span::styled(&r.label, style),
                Span::styled(diag, Style::new().fg(Color::Red)),
                Span::styled(tag, Style::new().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    if !s.note.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("  ⓘ {}", s.note),
            Style::new().fg(Color::DarkGray),
        ))));
    }
    if s.rows.is_empty() && s.note.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  select a changed symbol",
            Style::new().fg(Color::DarkGray),
        ))));
    }

    let mut state = ListState::default();
    if !s.rows.is_empty() {
        state.select(Some(app.sem_sel.min(s.rows.len().saturating_sub(1))));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_semantic_overlay(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let popup = centered(area, 60, 70);
    frame.render_widget(ratatui::widgets::Clear, popup);
    render_semantic(frame, popup, app, snap);
}

// -- help modal ---------------------------------------------------------------

fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered(area, 70, 70);
    frame.render_widget(ratatui::widgets::Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            "codescope — keyboard controls",
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  q / Ctrl-C      quit"),
        Line::from("  ? / Esc         this help / close"),
        Line::from("  Tab / 1 2 3     focus files / diff / semantic"),
        Line::from("  j/k · ↑/↓       move selection · scroll"),
        Line::from("  Ctrl-d/u · Pg   half / full page in diff"),
        Line::from("  s / u / B       staged / unstaged / branch scope"),
        Line::from("  S               cycle scope"),
        Line::from("  Enter           jump to symbol / re-center view"),
        Line::from("  Space h l       expand / collapse"),
        Line::from("  + / -           semantic depth"),
        Line::from("  n / N           next / previous diff hunk"),
        Line::from("  R               rescan git"),
        Line::from("  a / A           AI toggle / refresh"),
        Line::from("  g / G           top / bottom"),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Yellow))
                .title(" help "),
        ),
        popup,
    );
}

// -- helpers ------------------------------------------------------------------

fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_h) / 2),
        Constraint::Percentage(pct_h),
        Constraint::Percentage((100 - pct_h) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_w) / 2),
        Constraint::Percentage(pct_w),
        Constraint::Percentage((100 - pct_w) / 2),
    ])
    .split(v[1])[1]
}

fn tree_guide(depth: u16) -> String {
    if depth == 0 {
        return String::new();
    }
    let mut s = String::new();
    for _ in 1..depth {
        s.push_str("│ ");
    }
    s.push_str("├─");
    s
}

fn status_style(status: &str) -> Style {
    let color = match status {
        "A" | "?" => Color::Green,
        "D" => Color::Red,
        "R" => Color::Cyan,
        "U" => Color::Magenta,
        _ => Color::Yellow,
    };
    Style::new().fg(color)
}

fn change_color(change: &str) -> Color {
    match change {
        "added" => Color::Green,
        "removed" => Color::Red,
        _ => Color::Yellow,
    }
}

// -- model picker modal -------------------------------------------------------

fn render_model_picker(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let popup = centered(area, 50, 50);
    frame.render_widget(ratatui::widgets::Clear, popup);
    let title = if snap.ai_model.is_empty() {
        " AI model ".to_string()
    } else {
        format!(" AI model (current: {}) ", snap.ai_model)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(title);
    let mut items: Vec<ListItem> = if snap.available_models.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no models loaded (is AI configured?)",
            Style::new().fg(Color::DarkGray),
        )))]
    } else {
        snap.available_models
            .iter()
            .map(|m| {
                let cur = if *m == snap.ai_model { " ●" } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(cur, Style::new().fg(Color::Green)),
                    Span::raw(format!(" {m}")),
                ]))
            })
            .collect()
    };
    items.push(ListItem::new(Line::from(Span::styled(
        "  ↑/↓ move · Enter select · Esc close",
        Style::new().fg(Color::DarkGray),
    ))));
    let mut state = ListState::default();
    if !snap.available_models.is_empty() {
        state.select(Some(
            app.model_sel.min(snap.available_models.len().saturating_sub(1)),
        ));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, popup, &mut state);
}
