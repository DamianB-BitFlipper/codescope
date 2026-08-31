//! The renderer: draws a [`UiSnapshot`] + [`App`] state into a ratatui frame.
//!
//! Pure with respect to I/O — `render` only touches the frame buffer, so it is fully
//! testable with ratatui's `TestBackend`. Layout is recomputed from the frame area every
//! pass (resize needs no stored state; research 04 §2).

use codescope_core::{AiStatus, ChangeScope, LsStatus};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{filter_candidates, App, Pane};
use crate::elide;
use crate::layout::{choose_tier, Tier};
use crate::snapshot::{DiffRow, UiSnapshot};

/// One render pass.
///
/// The pane arrangement is a pure function of the frame area + zoom state
/// ([`choose_tier`]; docs/review/13 §"Exact layout tiers"): zoom wins, then spacious
/// columns at width ≥ 150, a full-width vertical stack when tall enough, a files+detail
/// pair at width ≥ 80, else the focused pane alone.
pub fn render(frame: &mut Frame, app: &App, snap: &UiSnapshot) {
    let area = frame.area();
    let tier = choose_tier(area, app.zoomed);
    if tier == Tier::TooSmall {
        render_too_small(frame, area);
        return;
    }

    // Outer chrome: the footer is the first vertical luxury to go (heights 8–11). This
    // split must match the `main` height choose_tier assumes.
    let (top, main, bottom) = if area.height >= 12 {
        let outer = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
        (outer[0], outer[1], Some(outer[2]))
    } else {
        let outer =
            Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).split(area);
        (outer[0], outer[1], None)
    };
    render_top_bar(frame, top, snap);
    if let Some(bottom) = bottom {
        render_bottom_bar(frame, bottom, app, snap);
    }

    match tier {
        Tier::TooSmall => unreachable!("handled above"),
        Tier::FocusOnly => {
            // Explicit zoom, or too narrow to split: the focused pane gets all of `main`.
            match app.focused {
                Pane::Files => render_files(frame, main, app, snap),
                Pane::Diff => render_diff(frame, main, app, snap),
                Pane::Semantic => render_semantic(frame, main, app, snap),
            }
        }
        Tier::Medium => {
            // Files + one detail slot: the diff normally, relations (in the same slot,
            // not a popup) when Semantic is focused.
            let panes =
                Layout::horizontal([Constraint::Length(32), Constraint::Min(48)]).split(main);
            render_files(frame, panes[0], app, snap);
            if app.focused == Pane::Semantic {
                frame.render_widget(Clear, panes[1]);
                render_semantic(frame, panes[1], app, snap);
            } else {
                render_diff(frame, panes[1], app, snap);
            }
        }
        Tier::TallStack => {
            // All three concerns at full width; the diff absorbs every surplus row.
            let panes = Layout::vertical([
                Constraint::Length(10),
                Constraint::Min(14),
                Constraint::Length(10),
            ])
            .split(main);
            render_files(frame, panes[0], app, snap);
            render_diff(frame, panes[1], app, snap);
            render_semantic(frame, panes[2], app, snap);
        }
        Tier::Spacious => {
            let panes = Layout::horizontal([
                Constraint::Length(38),
                Constraint::Min(72),
                Constraint::Length(40),
            ])
            .split(main);
            render_files(frame, panes[0], app, snap);
            render_diff(frame, panes[1], app, snap);
            render_semantic(frame, panes[2], app, snap);
        }
    }

    if app.show_help {
        render_help(frame, area);
    }
    if app.show_model_picker {
        render_model_picker(frame, area, app, snap);
    }
    if app.show_base_picker {
        render_base_picker(frame, area, app, snap);
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

/// `· ZOOM ` title suffix while a pane is pinned-zoomed: at a narrow size the user must be
/// able to tell a deliberate zoom apart from the automatic focus-only tier.
fn zoom_tag(app: &App, pane: Pane) -> &'static str {
    if app.zoomed && app.focused == pane {
        "· ZOOM "
    } else {
        ""
    }
}

// -- top bar ------------------------------------------------------------------

/// The top state bar, built to a measured budget (docs/review/13 §"Compact chrome"): the
/// right status group is always reserved first, so a long branch or repo name can never
/// clip health or refresh state. Narrower widths drop product, divergence, and model, then
/// the branch/base, and finally the healthy-service glyphs.
fn render_top_bar(frame: &mut Frame, area: Rect, snap: &UiSnapshot) {
    let r = &snap.repo;
    // The comparison base: `base_ref` is authoritative (dispatcher-owned; reflects a picker
    // override); fall back to the repo-bar base for snapshots that never set it.
    let base = if snap.base_ref.is_empty() {
        r.base.as_deref().unwrap_or("?")
    } else {
        snap.base_ref.as_str()
    };
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let cyan = Style::new().fg(Color::Cyan);
    let (left, right): (Vec<Span>, Vec<Span>) = if area.width >= 150 {
        let mut left = vec![
            Span::styled(" codescope ", bold),
            Span::styled(format!(" {}", r.repo_name), cyan),
            Span::raw(format!("  {} ◂ {}", r.branch, base)),
        ];
        if r.ahead > 0 || r.behind > 0 {
            left.push(Span::styled(
                format!("  +{} -{}", r.ahead, r.behind),
                Style::new().fg(Color::DarkGray),
            ));
        }
        let provider = if snap.ai_provider.is_empty() {
            String::new()
        } else {
            format!("@{}", snap.ai_provider)
        };
        let status_line = |ai: String| {
            format!(
                "scope: {}  │  lsp: {}  │  AI: {}",
                scope_label(snap.scope),
                ls_label(snap.ls),
                ai
            )
        };
        let mut status = status_line(if snap.ai_model.is_empty() {
            format!("{}{}", ai_label(&snap.ai), provider)
        } else {
            format!("{} ({}){}", ai_label(&snap.ai), snap.ai_model, provider)
        });
        // The model name is the first luxury: drop it when the full row would overflow.
        let left_w: usize = left.iter().map(Span::width).sum();
        if !snap.ai_model.is_empty() && left_w + status.width() + 1 > area.width as usize {
            status = status_line(format!("{}{}", ai_label(&snap.ai), provider));
        }
        let mut right = vec![Span::raw(status)];
        if snap.refreshing {
            right.push(Span::styled("  ⟳", Style::new().fg(Color::Yellow)));
        }
        right.push(Span::raw(" "));
        (left, right)
    } else if area.width >= 80 {
        // Left: elided `repo  branch ◂ base`; right: compact scope + service glyphs.
        (
            vec![
                Span::styled(format!(" {}", r.repo_name), cyan),
                Span::raw(format!("  {} ◂ {}", r.branch, base)),
            ],
            compact_status(snap),
        )
    } else if area.width >= 50 {
        (
            vec![Span::styled(format!(" {}", r.repo_name), cyan)],
            compact_status(snap),
        )
    } else {
        // 30..49: repo + compact scope on the left; only failures/refresh on the right.
        (
            vec![
                Span::styled(format!(" {}", r.repo_name), cyan),
                Span::raw(format!(" {}", scope_glyph(snap.scope))),
            ],
            failure_status(snap),
        )
    };

    let right_w: usize = right.iter().map(Span::width).sum();
    let chunks = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(right_w.min(area.width as usize) as u16),
    ])
    .split(area);
    let budget = chunks[0].width as usize;
    let left_w: usize = left.iter().map(Span::width).sum();
    if left_w <= budget {
        frame.render_widget(Paragraph::new(Line::from(left)), chunks[0]);
    } else {
        // Styled spans can't be partially rendered; fall back to one truncated string.
        let plain: String = left.iter().map(|s| s.content.as_ref()).collect();
        frame.render_widget(Paragraph::new(truncate_cells(&plain, budget)), chunks[0]);
    }
    frame.render_widget(Paragraph::new(Line::from(right)), chunks[1]);
}

/// Compact right-side status group: scope letter + LSP/AI glyphs + refresh spinner.
fn compact_status(snap: &UiSnapshot) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(format!(
        "{} {} {}",
        scope_glyph(snap.scope),
        ls_glyph(snap.ls),
        ai_glyph(&snap.ai)
    ))];
    if snap.refreshing {
        spans.push(Span::styled(" ⟳", Style::new().fg(Color::Yellow)));
    }
    spans.push(Span::raw(" "));
    spans
}

/// The 30–49-column status group: failure glyphs and the refresh spinner only (healthy
/// services are omitted).
fn failure_status(snap: &UiSnapshot) -> Vec<Span<'static>> {
    let mut text = String::new();
    if matches!(snap.ls, LsStatus::Degraded | LsStatus::Failed) {
        text.push_str(ls_glyph(snap.ls));
    }
    if matches!(snap.ai, AiStatus::Stale { .. } | AiStatus::Failed { .. }) {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(ai_glyph(&snap.ai));
    }
    let mut spans: Vec<Span<'static>> = if text.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(text, Style::new().fg(Color::Yellow))]
    };
    if snap.refreshing {
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled("⟳", Style::new().fg(Color::Yellow)));
    }
    if !spans.is_empty() {
        spans.push(Span::raw(" "));
    }
    spans
}

/// Truncate to at most `budget` display cells, grapheme-safe, appending `…` when cut.
fn truncate_cells(s: &str, budget: usize) -> String {
    if s.width() <= budget {
        return s.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let keep = budget - 1; // room for '…'
    let mut out = String::new();
    let mut used = 0;
    for g in unicode_segmentation::UnicodeSegmentation::graphemes(s, true) {
        let w = UnicodeWidthStr::width(g);
        if used + w > keep {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out.push('…');
    out
}

fn scope_label(scope: ChangeScope) -> &'static str {
    match scope {
        ChangeScope::Branch => "branch",
        ChangeScope::Staged => "staged",
        ChangeScope::Unstaged => "unstaged",
        ChangeScope::Working => "working",
    }
}

/// One-letter compact scope for narrow top bars (legend lives in the help modal).
fn scope_glyph(scope: ChangeScope) -> &'static str {
    match scope {
        ChangeScope::Branch => "B",
        ChangeScope::Staged => "S",
        ChangeScope::Unstaged => "U",
        ChangeScope::Working => "W",
    }
}

/// Compact LSP status glyph for narrow top bars (`L✓` ok, `L~` degraded, `L!` failed).
fn ls_glyph(ls: LsStatus) -> &'static str {
    match ls {
        LsStatus::Starting | LsStatus::Indexing => "L…",
        LsStatus::Ready => "L✓",
        LsStatus::Degraded => "L~",
        LsStatus::Failed => "L!",
    }
}

/// Compact AI status glyph for narrow top bars (mirrors the `L` glyphs).
fn ai_glyph(ai: &AiStatus) -> &'static str {
    match ai {
        AiStatus::Disabled => "A-",
        AiStatus::Idle => "A·",
        AiStatus::Loading { .. } => "A…",
        AiStatus::Ready { .. } => "A✓",
        AiStatus::Stale { .. } => "A~",
        AiStatus::Failed { .. } => "A!",
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

/// The contextual footer. Hints shrink with width; a pending message (or, failing that,
/// the selected file's full unelided path) comes first, and hints join only when the whole
/// row fits — never concatenate-and-clip (docs/review/13 §"Compact chrome").
fn render_bottom_bar(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let hints = if area.width >= 100 {
        "q quit · ? help · Tab pane · z zoom · W wrap · s/u/B/w scope · b base · a AI · n/N hunk"
    } else if area.width >= 60 {
        "Tab pane · z zoom · ? keys · q quit"
    } else {
        "Tab · z · ? · q"
    };
    let primary = if !snap.message.is_empty() {
        snap.message.as_str()
    } else {
        app.selected_file_path().unwrap_or("")
    };
    let width = area.width as usize;
    let text = if primary.is_empty() {
        format!(" {hints} ")
    } else if primary.width() + hints.width() + 7 <= width {
        format!(" {primary}  ·  {hints} ")
    } else {
        format!(" {primary} ")
    };
    let style = if app.show_help {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(truncate_cells(&text, width)).style(style),
        area,
    );
}

// -- files pane ---------------------------------------------------------------

fn render_files(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let focused = app.focused == Pane::Files;
    // Display-only shortening (docs/review/13 §"Paths"): a worthwhile shared directory
    // root goes into the title once; rows are relative to it and middle-elided into the
    // row budget. The snapshot keeps full paths as the identity for selection/actions.
    let paths: Vec<&str> = snap.files.iter().map(|f| f.path.as_str()).collect();
    let budget = (area.width as usize).saturating_sub(6); // 2 border cells + "M ▾ "
    let display = elide::elide_paths(&paths, budget);
    let mut parts = vec![format!("changed ({})", snap.files.len())];
    if let Some(root) = elide::shared_root(&paths) {
        let root = root.trim_end_matches('/');
        let fixed = parts[0].width() + zoom_tag(app, Pane::Files).width() + 2 * 2 + 4;
        let avail = (area.width as usize).saturating_sub(fixed + 2); // block borders
        if avail >= 4 {
            let root = elide::elide_paths(&[root], avail).remove(0);
            parts.push(format!("@ {root}/"));
        }
    }
    let title = format!(" {} {}", parts.join(" · "), zoom_tag(app, Pane::Files));
    let block = pane_block(title, focused);

    let mut items: Vec<ListItem> = Vec::new();
    for (f, disp) in snap.files.iter().zip(display.iter()) {
        let marker = if f.expanded { "▾" } else { "▸" };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(f.status, status_style(f.status)),
            Span::raw(" "),
            Span::styled(marker, Style::new().fg(Color::DarkGray)),
            Span::raw(" "),
            Span::raw(disp.clone()),
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

/// Width of the fixed left gutter on numbered diff rows: a 5-wide line number + space +
/// the `+`/`-`/space sign. Continuation lines hang a `↪` in the same seventh column, so
/// code always starts at the same x and no continuation reads as a new diff line.
const DIFF_GUTTER: usize = 7;

fn render_diff(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let focused = app.focused == Pane::Diff;
    let d = &snap.diff;
    let inner_w = (area.width as usize).saturating_sub(2); // block borders
    let body_w = inner_w.saturating_sub(DIFF_GUTTER).max(1);

    // Pre-wrap (smart mode) or body-slice (raw mode) into display lines. Ratatui's own
    // x-scroll is never used: it would carry the line number and sign offscreen.
    let built = if app.diff_wrap {
        build_wrapped(&d.rows, body_w)
    } else {
        build_raw(&d.rows, app.diff_hscroll as usize, body_w)
    };
    // `diff_scroll` is a logical-row anchor (resize-stable); map it to the first visual
    // line of that row. Raw mode is 1:1, so the map is the identity there.
    let scroll_y = built
        .first_visual
        .get(app.diff_scroll as usize)
        .copied()
        .unwrap_or(0);

    // Title: the path is elided into whatever the reserved state markers leave
    // (`hunk N/M`, the wrap/x+NN mode, ZOOM) — state must not vanish behind a long path.
    let mut right_parts: Vec<String> = Vec::new();
    if d.total_hunks > 0 {
        right_parts.push(format!("hunk {}/{}", d.current_hunk, d.total_hunks));
    }
    right_parts.push(if app.diff_wrap {
        "wrap".to_string()
    } else {
        format!("x+{:02}", built.effective_x)
    });
    let zoom = zoom_tag(app, Pane::Diff);
    let reserved = right_parts.join(" · ").width() + zoom.width() + 4;
    let path = if d.title.is_empty() {
        String::new()
    } else {
        let path_budget = inner_w.saturating_sub(reserved + 1).max(1);
        elide::elide_paths(&[d.title.as_str()], path_budget).remove(0)
    };
    let mut title_parts: Vec<String> = Vec::new();
    if !path.is_empty() {
        title_parts.push(path);
    }
    title_parts.extend(right_parts);
    let title = format!(" {} {}", title_parts.join(" · "), zoom);
    let block = pane_block(title, focused);

    let paragraph = Paragraph::new(built.lines)
        .block(block)
        .scroll((scroll_y, 0));
    frame.render_widget(paragraph, area);
}

/// The diff rows rendered into display lines, plus the logical→visual anchor map.
struct BuiltDiff {
    /// Display lines (pre-wrapped or body-sliced).
    lines: Vec<Line<'static>>,
    /// `first_visual[logical_row]` = index of that row's first display line.
    first_visual: Vec<u16>,
    /// The horizontal offset actually shown after clamping (raw mode; 0 when wrapped).
    effective_x: usize,
}

/// Smart-wrap mode: every `DiffRow` becomes one or more display lines (docs/review/13
/// §"Diff"). Numbered rows keep the six-cell gutter + sign on the first line and a
/// seven-cell hanging `↪` on continuations; hunk headers wrap with a `↪ ` marker.
fn build_wrapped(rows: &[DiffRow], body_w: usize) -> BuiltDiff {
    let mut lines = Vec::new();
    let mut first_visual = Vec::with_capacity(rows.len());
    for row in rows {
        first_visual.push(lines.len() as u16);
        match row {
            DiffRow::HunkHeader(h) => {
                let style = Style::new().fg(Color::Cyan);
                // Headers have no number gutter; the wrap marker keeps a continuation
                // from reading as a new header.
                for (i, seg) in wrap_body(h, body_w + DIFF_GUTTER - 2).iter().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(Span::styled(seg.clone(), style)));
                    } else {
                        lines.push(Line::from(Span::styled(format!("↪ {seg}"), style)));
                    }
                }
            }
            DiffRow::Add { new_ln, text } => {
                push_numbered(&mut lines, *new_ln, '+', text, Color::Green, body_w);
            }
            DiffRow::Del { old_ln, text } => {
                push_numbered(&mut lines, *old_ln, '-', text, Color::Red, body_w);
            }
            DiffRow::Context { new_ln, text, .. } => {
                push_numbered(&mut lines, *new_ln, ' ', text, Color::DarkGray, body_w);
            }
        }
    }
    BuiltDiff {
        lines,
        first_visual,
        effective_x: 0,
    }
}

/// Raw mode: one display line per logical row; the source body is display-cell-sliced at
/// the (clamped) horizontal offset while the gutter stays fixed.
fn build_raw(rows: &[DiffRow], x: usize, body_w: usize) -> BuiltDiff {
    let body_w = body_w.max(1);
    // Clamp x to the longest body minus the body viewport.
    let max_body = rows
        .iter()
        .map(|r| measured_cells(row_text(r)))
        .max()
        .unwrap_or(0);
    let effective_x = x.min(max_body.saturating_sub(body_w));
    let mut lines = Vec::with_capacity(rows.len());
    let mut first_visual = Vec::with_capacity(rows.len());
    for row in rows {
        first_visual.push(lines.len() as u16);
        match row {
            DiffRow::HunkHeader(h) => lines.push(Line::from(Span::styled(
                slice_cells(h, effective_x, body_w + DIFF_GUTTER),
                Style::new().fg(Color::Cyan),
            ))),
            DiffRow::Add { new_ln, text } => {
                lines.push(raw_numbered(*new_ln, '+', text, Color::Green, effective_x, body_w));
            }
            DiffRow::Del { old_ln, text } => {
                lines.push(raw_numbered(*old_ln, '-', text, Color::Red, effective_x, body_w));
            }
            DiffRow::Context { new_ln, text, .. } => {
                lines.push(raw_numbered(*new_ln, ' ', text, Color::DarkGray, effective_x, body_w));
            }
        }
    }
    BuiltDiff {
        lines,
        first_visual,
        effective_x,
    }
}

/// One numbered diff row in wrap mode: gutter + sign on the first display line, hanging
/// `↪` continuations after it.
fn push_numbered(
    lines: &mut Vec<Line<'static>>,
    ln: u32,
    sign: char,
    text: &str,
    color: Color,
    body_w: usize,
) {
    let gutter = Style::new().fg(Color::DarkGray);
    let body = Style::new().fg(color);
    for (i, seg) in wrap_body(text, body_w).iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(format!("{ln:>5} "), gutter),
                Span::styled(format!("{sign}{seg}"), body),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("      ↪", gutter),
                Span::styled(seg.clone(), body),
            ]));
        }
    }
}

/// One numbered diff row in raw mode: fixed gutter, body sliced to the visible window.
fn raw_numbered(
    ln: u32,
    sign: char,
    text: &str,
    color: Color,
    x: usize,
    body_w: usize,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{ln:>5} "), Style::new().fg(Color::DarkGray)),
        Span::styled(
            format!("{sign}{}", slice_cells(text, x, body_w)),
            Style::new().fg(color),
        ),
    ])
}

/// The source text of a diff row (headers count as full-width bodies).
fn row_text(row: &DiffRow) -> &str {
    match row {
        DiffRow::HunkHeader(h) => h,
        DiffRow::Add { text, .. } | DiffRow::Del { text, .. } | DiffRow::Context { text, .. } => {
            text
        }
    }
}

/// Display cells of one grapheme at body column `col`: tabs advance to the next
/// four-cell stop; control characters measure zero (ratatui skips them on render).
fn grapheme_cells(g: &str, col: usize) -> usize {
    if g == "	" {
        4 - (col % 4)
    } else {
        UnicodeWidthStr::width(g)
    }
}

/// Display-cell width with tabs measured as four-cell stops (matches `grapheme_cells`).
fn measured_cells(s: &str) -> usize {
    let mut col = 0;
    for g in unicode_segmentation::UnicodeSegmentation::graphemes(s, true) {
        col += grapheme_cells(g, col);
    }
    col
}

/// Wrap `text` into segments of at most `budget` display cells. Prefers a whitespace or
/// punctuation break in the segment's second half (the break character is preserved, not
/// trimmed); an overlong token is hard-broken by display width. Original graphemes are
/// kept: tabs are measured as four-cell stops but passed through for display, because
/// ratatui (and real terminals) render the tab character themselves.
fn wrap_body(text: &str, budget: usize) -> Vec<String> {
    let budget = budget.max(1);
    let gs: Vec<&str> =
        unicode_segmentation::UnicodeSegmentation::graphemes(text, true).collect();
    let mut out: Vec<String> = Vec::new();
    let mut seg_start = 0usize;
    let mut col = 0usize;
    let mut i = 0usize;
    while i < gs.len() {
        let w = grapheme_cells(gs[i], col);
        if col + w > budget && i > seg_start {
            let lo = seg_start + (i - seg_start) / 2;
            let end = match (lo..i).rev().find(|&j| is_soft_break(gs[j])) {
                Some(j) => j + 1,
                None => i, // hard-break an overlong token at the edge
            };
            out.push(gs[seg_start..end].concat());
            seg_start = end;
            i = end;
            col = 0;
        } else {
            col += w;
            i += 1;
        }
    }
    out.push(gs[seg_start..].concat());
    out
}

/// A grapheme worth breaking a wrapped line after: whitespace or ASCII punctuation.
fn is_soft_break(g: &str) -> bool {
    g.chars()
        .any(|c| c.is_whitespace() || c.is_ascii_punctuation())
}

/// Take the display-cell window `[skip, skip + budget)` of `s`, grapheme-safe (a grapheme
/// straddling the left edge is dropped whole, never split).
fn slice_cells(s: &str, skip: usize, budget: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    let mut taken = 0usize;
    for g in unicode_segmentation::UnicodeSegmentation::graphemes(s, true) {
        let w = grapheme_cells(g, col);
        let next = col + w;
        if next <= skip {
            col = next;
            continue;
        }
        if col < skip {
            col = next;
            continue;
        }
        if taken + w > budget {
            break;
        }
        out.push_str(g);
        taken += w;
        col = next;
    }
    out
}

// -- semantic pane ------------------------------------------------------------

fn render_semantic(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let focused = app.focused == Pane::Semantic;
    let s = &snap.semantic;
    let title = if s.ai_generated {
        format!(" {} · AI {}", s.title, zoom_tag(app, Pane::Semantic))
    } else {
        format!(" {} {}", s.title, zoom_tag(app, Pane::Semantic))
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
        Line::from("  s / u / B / w   staged / unstaged / branch / working scope"),
        Line::from("  S               cycle scope"),
        Line::from("  b               pick comparison base (default: nearest ancestor)"),
        Line::from("  Enter           jump to symbol / re-center view"),
        Line::from("  Space h l       expand / collapse"),
        Line::from("  + / -           semantic depth"),
        Line::from("  n / N           next / previous diff hunk"),
        Line::from("  z               zoom the focused pane (Tab still switches)"),
        Line::from("  W / 0           diff: toggle wrap / reset horizontal scroll"),
        Line::from("  R               rescan git"),
        Line::from("  a / A           AI toggle / refresh"),
        Line::from("  g / G           top / bottom"),
        Line::from(""),
        Line::from("  Narrow bars use compact glyphs: B/S/U/W scope,"),
        Line::from("  L✓/L~/L! LSP ok/degraded/failed, A✓/A~/A! AI ok/stale/failed."),
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
    let provider = if snap.ai_provider.is_empty() {
        String::new()
    } else {
        format!(" · via {}", snap.ai_provider)
    };
    let title = if snap.ai_model.is_empty() {
        format!(" AI model{provider} ")
    } else {
        format!(" AI model (current: {}){provider} ", snap.ai_model)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(title);
    let models = filter_candidates(&snap.available_models, &app.model_query);
    let mut items: Vec<ListItem> = if snap.available_models.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no models loaded (is AI configured?)",
            Style::new().fg(Color::DarkGray),
        )))]
    } else if models.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no matches",
            Style::new().fg(Color::DarkGray),
        )))]
    } else {
        models
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
        format!("  filter: {}", app.model_query),
        Style::new().fg(Color::Yellow),
    ))));
    items.push(ListItem::new(Line::from(Span::styled(
        "  type to filter · ↑/↓ move · Enter select · Esc close",
        Style::new().fg(Color::DarkGray),
    ))));
    let mut state = ListState::default();
    if !models.is_empty() {
        state.select(Some(app.model_sel.min(models.len().saturating_sub(1))));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, popup, &mut state);
}

// -- base picker modal --------------------------------------------------------

fn render_base_picker(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let popup = centered(area, 50, 50);
    frame.render_widget(ratatui::widgets::Clear, popup);
    let title = if snap.base_ref.is_empty() {
        " comparison base ".to_string()
    } else {
        format!(" comparison base (current: {}) ", snap.base_ref)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(title);
    let bases = filter_candidates(&snap.available_bases, &app.base_query);
    let mut items: Vec<ListItem> = if snap.available_bases.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  fetching base candidates…",
            Style::new().fg(Color::DarkGray),
        )))]
    } else if bases.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no matches",
            Style::new().fg(Color::DarkGray),
        )))]
    } else {
        bases
            .iter()
            .map(|b| {
                let cur = if *b == snap.base_ref { " ●" } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(cur, Style::new().fg(Color::Green)),
                    Span::raw(format!(" {b}")),
                ]))
            })
            .collect()
    };
    items.push(ListItem::new(Line::from(Span::styled(
        format!("  filter: {}", app.base_query),
        Style::new().fg(Color::Yellow),
    ))));
    items.push(ListItem::new(Line::from(Span::styled(
        "  type to filter · ↑/↓ move · Enter select · Esc close",
        Style::new().fg(Color::DarkGray),
    ))));
    let mut state = ListState::default();
    if !bases.is_empty() {
        state.select(Some(app.base_sel.min(bases.len().saturating_sub(1))));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, popup, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn buffer_text(t: &Terminal<TestBackend>) -> String {
        t.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn snap_with_base() -> UiSnapshot {
        let mut snap = UiSnapshot::default();
        snap.repo.repo_name = "demo".to_string();
        snap.repo.branch = "feature/x".to_string();
        snap.repo.base = Some("origin/main".to_string());
        snap.base_ref = "release/2.0".to_string();
        snap.available_bases = vec![
            "release/2.0".to_string(),
            "main".to_string(),
            "origin/main".to_string(),
        ];
        snap
    }

    #[test]
    fn top_bar_reads_base_ref() {
        let mut t = Terminal::new(TestBackend::new(160, 40)).unwrap();
        let app = App::new();
        t.draw(|f| render(f, &app, &snap_with_base())).unwrap();
        let text = buffer_text(&t);
        assert!(
            text.contains("feature/x ◂ release/2.0"),
            "top bar shows the base from base_ref: {text}"
        );
    }

    #[test]
    fn top_bar_falls_back_to_repo_bar_base() {
        let mut t = Terminal::new(TestBackend::new(160, 40)).unwrap();
        let app = App::new();
        let mut snap = snap_with_base();
        snap.base_ref.clear();
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(buffer_text(&t).contains("feature/x ◂ origin/main"));
    }

    #[test]
    fn base_picker_renders_candidates_and_current() {
        let mut t = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut app = App::new();
        app.show_base_picker = true;
        t.draw(|f| render(f, &app, &snap_with_base())).unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("comparison base"), "picker title: {text}");
        assert!(text.contains("release/2.0"), "current base listed");
        assert!(text.contains("origin/main"), "a candidate listed");
        assert!(text.contains("●"), "current-base marker");
    }

    #[test]
    fn base_picker_filter_shows_query_and_only_matches() {
        let mut t = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut app = App::new();
        app.show_base_picker = true;
        app.base_query = "main".to_string();
        let mut snap = snap_with_base();
        snap.base_ref = "main".to_string();
        snap.available_bases = vec![
            "main".to_string(),
            "origin/main".to_string(),
            "develop".to_string(),
        ];
        t.draw(|f| render(f, &app, &snap)).unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("filter: main"), "query footer: {text}");
        assert!(text.contains("origin/main"), "matching entry listed");
        assert!(
            !text.contains("develop"),
            "non-matching entry filtered out: {text}"
        );
    }
}
