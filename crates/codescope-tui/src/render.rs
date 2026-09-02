//! The renderer: draws a [`UiSnapshot`] + [`App`] state into a ratatui frame.
//!
//! Pure with respect to I/O — `render` only touches the frame buffer, so it is fully
//! testable with ratatui's `TestBackend`. Layout is recomputed from the frame area every
//! pass (resize needs no stored state). The pane arrangement is the reference
//! master-detail layout of docs/review/15 §1: one normal tier (top, files+diff,
//! full-width Impact, bottom) and a focus-only fallback below 80x20 or when
//! zoomed. All colors come from the §2 palette below — never a bare `Color::Green`.

use codescope_core::{AiStatus, ChangeScope, DiffSide, LsStatus, ValidationVerdict};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{filter_candidates, App, Pane};
use crate::diagram::{fallback_lines, DiagramRole};
use crate::divider::DividerId;
use crate::elide;
use crate::intraline;
use crate::layout::{
    choose_tier, files_width, impact_left_width, impact_section_heights, Tier, MIN_DIFF_WIDTH,
};
use crate::snapshot::{DiffRow, ImpactList, ImpactLoadState, StatusLevel, UiSnapshot};

// -- palette (docs/review/15 §2) ----------------------------------------------
//
// One palette for the whole interface. `Modifier::BOLD` only for the product name,
// selected/basename labels, column headings, and intraline changed spans. `REVERSED`
// is never used: it would destroy the deliberate red/green diff palette.

/// Top context and combined bottom bar background.
pub(crate) const SURFACE: Color = Color::Rgb(24, 27, 32);
/// Hunk-header band background.
pub(crate) const SURFACE_ALT: Color = Color::Rgb(31, 35, 41);
/// Normal label / source text.
pub(crate) const TEXT: Color = Color::Rgb(210, 214, 220);
/// Context lines, paths, separators, gutters.
pub(crate) const MUTED: Color = Color::Rgb(122, 128, 139);
/// Unfocused borders and inner dividers.
pub(crate) const BORDER: Color = Color::Rgb(67, 73, 83);
/// Focused border, product/repo, active symbol.
pub(crate) const ACCENT: Color = Color::Rgb(91, 166, 255);
/// Active list row background.
pub(crate) const SELECTED_BG: Color = Color::Rgb(46, 54, 66);
/// Owning file's background while one of its child symbols is active.
pub(crate) const OWNER_BG: Color = Color::Rgb(35, 41, 50);
/// `A` / `+` / added-line accent.
pub(crate) const ADD_FG: Color = Color::Rgb(100, 190, 120);
/// Restrained added-line body background.
pub(crate) const ADD_BG: Color = Color::Rgb(27, 49, 35);
/// Changed word in an added line.
pub(crate) const ADD_HI: Color = Color::Rgb(151, 232, 166);
/// Changed-word emphasis background (added).
pub(crate) const ADD_HI_BG: Color = Color::Rgb(46, 88, 58);
/// `D` / `-` / removed-line accent.
pub(crate) const DEL_FG: Color = Color::Rgb(225, 113, 122);
/// Restrained removed-line body background.
pub(crate) const DEL_BG: Color = Color::Rgb(58, 30, 35);
/// Changed word in a removed line.
pub(crate) const DEL_HI: Color = Color::Rgb(255, 166, 172);
/// Changed-word emphasis background (removed).
pub(crate) const DEL_HI_BG: Color = Color::Rgb(101, 45, 53);
/// Modified status, warnings, stale/loading.
pub(crate) const WARN: Color = Color::Rgb(218, 174, 86);
/// Hunk-header band text.
pub(crate) const HUNK_FG: Color = Color::Rgb(132, 190, 229);
/// Background for source rows linked to the generated node under the pointer. A dark
/// amber overlay plus underline remains distinct from both add/delete colors and does not
/// rely on hue alone.
pub(crate) const CODE_LINK_BG: Color = Color::Rgb(64, 55, 34);
/// Failures / diagnostics.
pub(crate) const ERROR: Color = Color::Rgb(238, 95, 101);

/// One render pass: the arrangement is a pure function of frame area + zoom
/// ([`choose_tier`]). Never panics at any size; never does I/O.
pub fn render(frame: &mut Frame, app: &App, snap: &UiSnapshot) {
    let area = frame.area();
    let tier = choose_tier(area, app.zoomed);
    if tier == Tier::TooSmall {
        render_too_small(frame, area);
        return;
    }

    match tier {
        Tier::TooSmall => unreachable!("handled above"),
        Tier::Normal => {
            // Dense chrome: repository context on top; commands, usage, and path below.
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(7),
                Constraint::Length(crate::layout::impact_height(
                    app.dividers.get(DividerId::WorkReview),
                    area.height,
                )),
                Constraint::Length(1),
            ])
            .split(area);
            render_top_bar(frame, rows[0], snap);
            let fw = files_width(app.dividers.get(DividerId::FilesDiff), rows[1].width);
            let work =
                Layout::horizontal([Constraint::Length(fw), Constraint::Min(MIN_DIFF_WIDTH)])
                    .split(rows[1]);
            render_files(frame, work[0], app, snap);
            render_diff(frame, work[1], app, snap);
            render_impact(frame, rows[2], app, snap);
            render_bottom_bar(frame, rows[3], app, snap);
        }
        Tier::FocusOnly => {
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area);
            render_top_bar(frame, rows[0], snap);
            render_focused(frame, rows[1], app, snap);
            render_bottom_bar(frame, rows[2], app, snap);
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
    if let Some(status) = &app.status_detail {
        render_status_detail(frame, area, status);
    }
}

/// The focus-only body: the focused pane gets the whole area.
fn render_focused(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    match app.focused {
        Pane::Files => render_files(frame, area, app, snap),
        Pane::Diff => render_diff(frame, area, app, snap),
        Pane::Impact => render_impact(frame, area, app, snap),
    }
}

fn render_too_small(frame: &mut Frame, area: Rect) {
    let msg = format!("terminal too small ({}x{})", area.width, area.height);
    let p = Paragraph::new(msg).style(Style::new().fg(MUTED));
    frame.render_widget(p, area);
}

/// A bordered pane block with a left and (optional) right title: the title texts are
/// pre-elided by the caller so they never overlap — ratatui does not resolve that.
/// Focused panes get an `ACCENT` border, unfocused `BORDER` (docs/review/15 §2).
fn pane_block(left: Line<'static>, right: Option<Line<'static>>, focused: bool) -> Block<'static> {
    let border_style = if focused {
        Style::new().fg(ACCENT)
    } else {
        Style::new().fg(BORDER)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(left.left_aligned());
    match right {
        Some(r) => block.title(r.right_aligned()),
        None => block,
    }
}

/// ` · ZOOM` title suffix while a pane is pinned-zoomed: at any size the user must be
/// able to tell a deliberate zoom apart from the automatic focus-only fallback.
fn zoom_tag(app: &App, pane: Pane) -> &'static str {
    if app.zoomed && app.focused == pane {
        " · ZOOM"
    } else {
        ""
    }
}

// -- top bar (docs/review/15 §3.1) ---------------------------------------------

/// The top repository/service bar: `codescope  {repo}  {base} ← {branch}  {N} files`
/// on the left, and
/// `{scope}  LSP {status}  {provider} {model} reasoning:{effort} {status}` on the right.
/// The comparison direction and current change-set size replace the retired summary row.
fn render_top_bar(frame: &mut Frame, area: Rect, snap: &UiSnapshot) {
    let r = &snap.repo;
    // The comparison base: `base_ref` is authoritative (dispatcher-owned; reflects a
    // picker override); fall back to the repo-bar base for snapshots that never set it.
    let base = if snap.base_ref.is_empty() {
        r.base.as_deref().unwrap_or("none")
    } else {
        snap.base_ref.as_str()
    };

    // Service/model state owns a bounded right-hand region. Model ids can be arbitrarily
    // long, so cap only that field; provider and status remain visible.
    let (ls_g, ls_style) = ls_status_glyph(snap.ls);
    let (ai_g, ai_style) = ai_status_glyph(&snap.ai);
    let provider = if snap.ai_provider.is_empty() {
        "AI"
    } else {
        snap.ai_provider.as_str()
    };
    let model_budget = ((area.width as usize) / 5).clamp(8, 28);
    let model = truncate_cells(&snap.ai_model, model_budget);
    let mut right: Vec<Span> = vec![
        Span::styled(scope_label(snap.scope), Style::new().fg(MUTED)),
        Span::raw("  "),
        Span::styled("LSP ", Style::new().fg(MUTED)),
        Span::styled(ls_g, ls_style),
        Span::raw("  "),
        Span::styled(provider.to_string(), Style::new().fg(MUTED)),
    ];
    if !model.is_empty() {
        right.push(Span::raw(" "));
        right.push(Span::styled(model, Style::new().fg(TEXT)));
    }
    // `default` is itself useful state: it tells the user that Codescope/provider
    // compatibility logic, rather than an explicit budget, controls this model. Only
    // suppress it when no AI configuration is present at all; snapshot defaults should
    // not make an entirely disabled installation look configured.
    if (!snap.ai_provider.is_empty() || !snap.ai_model.is_empty())
        && !snap.ai_reasoning_effort.is_empty()
    {
        right.push(Span::raw(" "));
        right.push(Span::styled("reasoning:", Style::new().fg(MUTED)));
        right.push(Span::styled(
            snap.ai_reasoning_effort.clone(),
            Style::new().fg(TEXT),
        ));
    }
    right.push(Span::raw(" "));
    right.push(Span::styled(ai_g, ai_style));
    if snap.refreshing {
        right.push(Span::styled("  ⟳", Style::new().fg(WARN)));
    }
    right.push(Span::raw(" "));
    let right_w: usize = right.iter().map(Span::width).sum();

    // Keep the count and comparison direction at every usable width. Product/repository
    // context is progressively dropped before either of those facts.
    let product = Span::styled(
        " codescope ",
        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
    );
    let repo = Span::styled(format!(" {} ", r.repo_name), Style::new().fg(ACCENT));

    let reserved = right_w.min(area.width as usize) as u16;
    let chunks = Layout::horizontal([Constraint::Min(1), Constraint::Length(reserved)]).split(area);
    let budget = chunks[0].width as usize;
    let file_word = if snap.files.len() == 1 {
        "file"
    } else {
        "files"
    };
    let count = format!(" {} {file_word} ", snap.files.len());
    let count_w = count.width();
    let full_comparison = format!(" {base} ← {} ", r.branch);
    let min_comparison = 8usize;
    let prefix = if product.width() + repo.width() + count_w + min_comparison <= budget {
        vec![product, repo]
    } else if product.width() + count_w + min_comparison <= budget {
        vec![product]
    } else {
        Vec::new()
    };
    let prefix_w: usize = prefix.iter().map(Span::width).sum();
    let comparison_budget = budget.saturating_sub(prefix_w + count_w);
    let mut left = prefix;
    left.push(Span::styled(
        truncate_cells(&full_comparison, comparison_budget),
        Style::new().fg(TEXT),
    ));
    if count_w <= budget.saturating_sub(prefix_w) {
        left.push(Span::styled(count, Style::new().fg(MUTED)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(left)).style(Style::new().bg(SURFACE)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(right)).style(Style::new().bg(SURFACE)),
        chunks[1],
    );
}

/// LSP glyph + style: ready `✓`/green, starting/indexing `…`/WARN, degraded `~`/WARN,
/// failed `×`/ERROR (docs/review/15 §3.1).
fn ls_status_glyph(ls: LsStatus) -> (&'static str, Style) {
    match ls {
        LsStatus::Ready => ("✓", Style::new().fg(ADD_FG)),
        LsStatus::Starting | LsStatus::Indexing => ("…", Style::new().fg(WARN)),
        LsStatus::Degraded => ("~", Style::new().fg(WARN)),
        LsStatus::Failed => ("×", Style::new().fg(ERROR)),
    }
}

/// AI glyph + style: ready `✓`/green, loading `…`/WARN, stale `~`/WARN, disabled
/// `×`/MUTED, failed `×`/ERROR. The provider is shown whenever configured, even when
/// AI is toggled off.
fn ai_status_glyph(ai: &AiStatus) -> (&'static str, Style) {
    match ai {
        AiStatus::Ready { .. } => ("✓", Style::new().fg(ADD_FG)),
        AiStatus::Loading { .. } => ("…", Style::new().fg(WARN)),
        AiStatus::WaitingForSymbols { .. }
        | AiStatus::WaitingForRelations { .. }
        | AiStatus::Queued { .. } => ("·", Style::new().fg(WARN)),
        AiStatus::Stale { .. } => ("~", Style::new().fg(WARN)),
        AiStatus::Disabled => ("×", Style::new().fg(MUTED)),
        AiStatus::Idle => ("·", Style::new().fg(MUTED)),
        AiStatus::Failed { .. } => ("×", Style::new().fg(ERROR)),
    }
}

/// The full scope word for the top bar's right group.
fn scope_label(scope: ChangeScope) -> &'static str {
    match scope {
        ChangeScope::Branch => "branch",
        ChangeScope::Staged => "staged",
        ChangeScope::Unstaged => "unstaged",
        ChangeScope::Working => "working",
    }
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

/// The basename of a repo-relative path (the last component).
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// -- files pane (docs/review/15 §3.3) -------------------------------------------

/// The changed-files pane: outer title `Changed files` (left) + the active file count
/// (right). Rows are `{status} {disclosure} {display_path}{pad}+A -D`; line counts are
/// right-aligned and disappear as one unit when the pane cannot preserve a useful path.
/// Directory components are MUTED, the
/// basename TEXT (bold on the active file). The active row gets SELECTED_BG; the file
/// owning an active symbol child gets OWNER_BG.
fn render_files(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let focused = app.focused == Pane::Files;
    let inner_w = (area.width as usize).saturating_sub(2);

    let loc_width = snap
        .files
        .iter()
        .filter(|f| f.added_lines > 0 || f.removed_lines > 0)
        .map(|f| format!("+{} -{}", f.added_lines, f.removed_lines).width())
        .max()
        .unwrap_or(0);
    // Four prefix cells, one separating gap, and at least ten useful path cells.
    let show_loc = loc_width > 0 && inner_w >= 4 + 1 + 10 + loc_width;
    let path_budget = if show_loc {
        inner_w.saturating_sub(5 + loc_width)
    } else {
        inner_w.saturating_sub(4)
    };

    // Display-only shortening; the snapshot keeps full paths as identity.
    let paths: Vec<&str> = snap.files.iter().map(|f| f.path.as_str()).collect();
    let display = elide::elide_paths(&paths, path_budget);

    // Which file (if any) owns the active symbol row (a symbol-row selection keeps its
    // file visible via OWNER_BG)?
    let owner_idx = app
        .selected_file_symbol()
        .filter(|(_, sym)| sym.is_some())
        .and_then(|_| app.selected_file_index());

    let block = pane_block(
        Line::from(Span::styled(
            format!(" Changed files{} ", zoom_tag(app, Pane::Files)),
            Style::new().fg(TEXT),
        )),
        Some(Line::from(Span::styled(
            format!("{} ", snap.files.len()),
            Style::new().fg(MUTED),
        ))),
        focused,
    );

    // Build the flattened rows, tracking which flat row is the active one.
    let mut items: Vec<ListItem> = Vec::new();
    let mut flat = 0usize;
    for (fi, f) in snap.files.iter().enumerate() {
        let active = flat == app.file_sel;
        flat += 1;
        let bg = if active {
            SELECTED_BG
        } else if owner_idx == Some(fi) {
            OWNER_BG
        } else {
            Color::Reset
        };
        items.push(ListItem::new(file_row_line(
            f,
            &display[fi],
            if show_loc { loc_width } else { 0 },
            path_budget,
            inner_w,
            active,
            bg,
        )));
        if f.expanded {
            // Symbol rows, or the per-state placeholder: analysis in flight / not owned /
            // failed / analyzed-but-no-symbols. Never a misleading empty block.
            match f.semantic {
                // Note rows are NOT selectable: they occupy a physical list row but must
                // not advance `flat` (the app's logical selectable index) — otherwise the
                // active-row highlight and ListState scroll target desync (review 18 M6).
                crate::snapshot::FileSemanticLoad::Loading => {
                    items.push(ListItem::new(semantic_note_line(
                        "… analyzing symbols",
                        inner_w,
                    )));
                }
                crate::snapshot::FileSemanticLoad::Unsupported => {
                    items.push(ListItem::new(semantic_note_line(
                        "semantic analysis unavailable",
                        inner_w,
                    )));
                }
                crate::snapshot::FileSemanticLoad::Failed => {
                    items.push(ListItem::new(semantic_note_line(
                        "analysis failed — retries after file change",
                        inner_w,
                    )));
                }
                crate::snapshot::FileSemanticLoad::Ready if f.symbols.is_empty() => {
                    items.push(ListItem::new(semantic_note_line(
                        "no changed symbols mapped",
                        inner_w,
                    )));
                }
                // An expanded Unloaded row (the brief frame before background scheduling)
                // shows the pending marker, not a blank body.
                crate::snapshot::FileSemanticLoad::Unloaded => {
                    items.push(ListItem::new(semantic_note_line(
                        "… analyzing symbols",
                        inner_w,
                    )));
                }
                _ => {
                    for s in &f.symbols {
                        let active = flat == app.file_sel;
                        flat += 1;
                        items.push(ListItem::new(symbol_row_line(s, active, inner_w)));
                    }
                }
            }
        }
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  no changes in this scope",
            Style::new().fg(MUTED),
        ))));
    }

    // Viewport: the shared projection computes the first visible PHYSICAL row from the
    // selection, so mouse hit-testing maps screen rows to the same slice the user sees
    // (review 23: no hidden ListState offset).
    let capacity = area.height.saturating_sub(2) as usize; // inside the border
    let first_visible = app.files_first_visible(capacity);
    let visible: Vec<ListItem> = items
        .into_iter()
        .skip(first_visible)
        .take(capacity.max(1))
        .collect();
    let list = List::new(visible).block(block);
    frame.render_widget(list, area);
}

/// One file row: status + disclosure + path (dirs MUTED, basename TEXT, bold when
/// active) padded to `path_budget`, then optional right-aligned green/red line counts.
fn file_row_line(
    f: &crate::snapshot::FileRow,
    display: &str,
    loc_width: usize,
    path_budget: usize,
    inner_w: usize,
    active: bool,
    bg: Color,
) -> Line<'static> {
    let marker = if f.expanded { "▾" } else { "▸" };
    let base_style = Style::new().fg(TEXT).bg(bg);
    let basename_style = if active {
        base_style.add_modifier(Modifier::BOLD)
    } else {
        base_style
    };
    let (dirs, base) = split_dir_basename(display);
    let loc = if loc_width > 0 && (f.added_lines > 0 || f.removed_lines > 0) {
        Some((
            format!("+{}", f.added_lines),
            format!("-{}", f.removed_lines),
        ))
    } else {
        None
    };
    let pad = path_budget.saturating_sub(display.width());
    let content_w = 4 + display.width() + pad + usize::from(loc_width > 0) + loc_width;
    let trailing = inner_w.saturating_sub(content_w);
    let mut spans = vec![
        Span::styled(f.status.to_string(), status_style(f.status).bg(bg)),
        Span::styled(" ", base_style),
        Span::styled(marker, Style::new().fg(MUTED).bg(bg)),
        Span::styled(" ", base_style),
        Span::styled(dirs, Style::new().fg(MUTED).bg(bg)),
        Span::styled(base, basename_style),
        Span::styled(" ".repeat(pad), base_style),
    ];
    if loc_width > 0 {
        spans.push(Span::styled(" ", base_style));
        match loc {
            Some((added, removed)) => {
                let loc_pad = loc_width.saturating_sub(added.width() + 1 + removed.width());
                spans.push(Span::styled(" ".repeat(loc_pad), base_style));
                spans.push(Span::styled(added, Style::new().fg(ADD_FG).bg(bg)));
                spans.push(Span::styled(" ", base_style));
                spans.push(Span::styled(removed, Style::new().fg(DEL_FG).bg(bg)));
            }
            None => spans.push(Span::styled(" ".repeat(loc_width), base_style)),
        }
    }
    spans.push(Span::styled(" ".repeat(trailing), base_style));
    Line::from(spans)
}

/// Number of decimal cells needed to render an integer.
fn digits(mut n: usize) -> usize {
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

/// An indented muted note row under an expanded file (loading / unsupported / failed /
/// analyzed-but-empty states; never selectable).
fn semantic_note_line(text: &str, inner_w: usize) -> Line<'static> {
    const INDENT: usize = 6;
    let budget = inner_w.saturating_sub(INDENT);
    // Grapheme-safe truncate to the cell budget (… and — are multibyte; byte len would
    // under-pad), then pad from the DISPLAY width (review 18 m7).
    let text = truncate_cells(text, budget);
    let shown = UnicodeWidthStr::width(text.as_str());
    let mut line = Line::from(vec![
        Span::raw(" ".repeat(INDENT)),
        Span::styled(text, Style::new().fg(MUTED)),
    ]);
    if INDENT + shown < inner_w {
        line.push_span(Span::raw(" ".repeat(inner_w - INDENT - shown)));
    }
    line
}

/// Split a display path into its directory prefix (with trailing `/`) and basename.
fn split_dir_basename(display: &str) -> (String, String) {
    match display.rfind('/') {
        Some(i) => (display[..=i].to_string(), display[i + 1..].to_string()),
        None => (String::new(), display.to_string()),
    }
}

/// One expanded symbol row: 4-cell indent, change glyph, name; confidence/diagnostic
/// markers dim/red at the right when they fit.
fn symbol_row_line(s: &crate::snapshot::SymbolRow, active: bool, inner_w: usize) -> Line<'static> {
    let bg = if active { SELECTED_BG } else { Color::Reset };
    let glyph = change_glyph(s.change);
    let base = Style::new().fg(TEXT).bg(bg);
    let name_style = if active {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    };
    let mut spans = vec![
        Span::styled("    ", base),
        Span::styled(
            glyph,
            status_style(match s.change {
                "added" => "+",
                "removed" => "-",
                _ => "~",
            })
            .bg(bg),
        ),
        Span::styled(" ", base),
        Span::styled(s.name.clone(), name_style),
    ];
    // Right-side markers: confidence + diagnostic, only when the row has room.
    let mut right = String::new();
    if !s.confidence.is_empty() {
        right.push_str(s.confidence);
    }
    if s.has_diagnostic {
        right.push('!');
    }
    let used = 4 + 1 + 1 + s.name.width();
    let mut content_w = used;
    if !right.is_empty() && used + right.width() < inner_w {
        let pad = inner_w.saturating_sub(used + right.width());
        spans.push(Span::styled(" ".repeat(pad), base));
        if !s.confidence.is_empty() {
            spans.push(Span::styled(s.confidence, Style::new().fg(MUTED).bg(bg)));
        }
        if s.has_diagnostic {
            spans.push(Span::styled("!", Style::new().fg(ERROR).bg(bg)));
        }
        content_w = used + pad + right.width();
    }
    spans.push(Span::styled(
        " ".repeat(inner_w.saturating_sub(content_w)),
        base,
    ));
    Line::from(spans)
}

/// The change glyph for a symbol row: `+` added, `~` modified, `-` removed.
fn change_glyph(change: &str) -> &'static str {
    match change {
        "added" => "+",
        "removed" => "-",
        _ => "~",
    }
}

/// Status colors (docs/review/15 §3.3): `A`/`?` ADD_FG, `M` WARN, `D` DEL_FG,
/// `R` ACCENT, `U` ERROR.
fn status_style(status: &str) -> Style {
    let color = match status {
        "A" | "?" => ADD_FG,
        "D" => DEL_FG,
        "R" => ACCENT,
        "U" => ERROR,
        _ => WARN,
    };
    Style::new().fg(color)
}

// -- diff pane (docs/review/15 §3.4) ----------------------------------------------

/// The number gutter: `ln_width` is 4..=6 (the widest line number, at least 4 cells),
/// and every source row starts with `{old:>ln} │ {new:>ln} {sign}`. The fixed gutter
/// width is `2 * ln_width + 5` cells (old, `" │ "`, new, one space, the sign).
fn ln_width(rows: &[DiffRow]) -> usize {
    let max_ln = rows
        .iter()
        .map(|r| match r {
            DiffRow::Add { new_ln, .. } => *new_ln,
            DiffRow::Del { old_ln, .. } => *old_ln,
            DiffRow::Context { old_ln, new_ln, .. } => (*old_ln).max(*new_ln),
            DiffRow::HunkHeader(_) => 0,
        })
        .fold(0u32, |a, b| a.max(b));
    digits(max_ln as usize).clamp(4, 6)
}

/// The fixed gutter width for a given `ln_width`: `2 * ln_width + 5`.
fn gutter_width(ln_w: usize) -> usize {
    2 * ln_w + 5
}

/// Diff body styles. Added/removed lines get a restrained body background and a bright,
/// bold intraline for the changed words; context is plain MUTED. `REVERSED` is never
/// used (docs/review/15 §2). Gutter numbers stay MUTED on every row.
const ADD_BODY: Style = Style::new().fg(TEXT).bg(ADD_BG);
const ADD_HI_STYLE: Style = Style::new()
    .fg(ADD_HI)
    .bg(ADD_HI_BG)
    .add_modifier(Modifier::BOLD);
const DEL_BODY: Style = Style::new().fg(TEXT).bg(DEL_BG);
const DEL_HI_STYLE: Style = Style::new()
    .fg(DEL_HI)
    .bg(DEL_HI_BG)
    .add_modifier(Modifier::BOLD);
const CTX_BODY: Style = Style::new().fg(MUTED);
const GUTTER: Style = Style::new().fg(MUTED);
const HUNK: Style = Style::new()
    .fg(HUNK_FG)
    .bg(SURFACE_ALT)
    .add_modifier(Modifier::BOLD);

fn render_diff(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let focused = app.focused == Pane::Diff;
    let d = &snap.diff;
    let inner_w = (area.width as usize).saturating_sub(2); // block borders
    let ln_w = ln_width(&d.rows);
    let gutter_w = gutter_width(ln_w);
    let body_w = inner_w.saturating_sub(gutter_w).max(1);

    // Intraline highlight map: changed-word byte spans per row (empty when unpaired or
    // when the pair shares no equal word — the sibling's intraline module owns both).
    let spans = intraline::row_spans(&d.rows);

    // Pre-wrap (smart mode) or body-slice (raw mode) into display lines. The gutter is
    // fixed: ratatui's own x-scroll is never used (it would carry the numbers offscreen).
    let linked_rows = linked_diff_rows(d, app.active_code_node());
    let mut built = if app.diff_wrap {
        build_wrapped(&d.rows, &spans, ln_w, body_w)
    } else {
        build_raw(
            &d.rows,
            &spans,
            ln_w,
            app.diff_hscroll as usize,
            body_w,
            inner_w,
        )
    };
    apply_linked_diff_style(&mut built, &linked_rows);
    // `diff_scroll` is a logical-row anchor (resize-stable); map it to the first visual
    // line of that row. Raw mode is 1:1, so the map is the identity there.
    let scroll_y = built
        .first_visual
        .get(app.diff_scroll as usize)
        .copied()
        .unwrap_or(0);

    // -- title: basename left, state right; preserve hunk/wrap, elide symbol, then path.
    let mut right_parts: Vec<String> = Vec::new();
    // The snapshot publishes `focused_symbol`; fall back to the local derivation for
    // snapshots that predate it (it stays correct on file rows: `None`).
    let focused_symbol = snap
        .diff
        .focused_symbol
        .as_deref()
        .or_else(|| app.selected_symbol_name());
    if let Some(sym) = focused_symbol {
        right_parts.push(sym.to_string());
    }
    let linked_count = linked_rows.iter().filter(|linked| **linked).count();
    if linked_count > 0 {
        right_parts.push(format!("↔ {linked_count} code lines"));
    }
    if d.total_hunks > 0 {
        right_parts.push(format!("hunk {}/{}", app.current_hunk, d.total_hunks));
    }
    right_parts.push(if app.diff_wrap {
        "wrap on".to_string()
    } else {
        "wrap off".to_string()
    });
    let show_x = !app.diff_wrap && built.effective_x > 0;
    if show_x {
        right_parts.push(format!("x+{:02}", built.effective_x));
    }
    let zoom = zoom_tag(app, Pane::Diff);
    let right_text = format!(" {}{} ", right_parts.join(" · "), zoom);
    let right_w = right_text.width();
    // Elide the symbol (second) before the basename (last): rebuild without the symbol.
    let right_text = if right_w + 6 > inner_w && focused_symbol.is_some() {
        let mut parts: Vec<String> = Vec::new();
        if d.total_hunks > 0 {
            parts.push(format!("hunk {}/{}", app.current_hunk, d.total_hunks));
        }
        parts.push(if app.diff_wrap {
            "wrap on".into()
        } else {
            "wrap off".into()
        });
        if show_x {
            parts.push(format!("x+{:02}", built.effective_x));
        }
        format!(" {}{} ", parts.join(" · "), zoom)
    } else {
        right_text
    };
    let right_w = right_text.width();
    let base_budget = inner_w.saturating_sub(right_w + 1).max(1);
    let base = if d.title.is_empty() {
        String::new()
    } else {
        truncate_cells(basename(&d.title), base_budget)
    };
    let left_text = format!(" {base} ");

    let block = pane_block(
        Line::from(Span::styled(
            left_text,
            Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Some(Line::from(Span::styled(right_text, Style::new().fg(MUTED)))),
        focused,
    );

    let paragraph = Paragraph::new(built.lines)
        .block(block)
        .scroll((scroll_y, 0));
    frame.render_widget(paragraph, area);
}

fn linked_diff_rows(
    diff: &crate::snapshot::DiffPane,
    node: Option<&codescope_core::PlanNode>,
) -> Vec<bool> {
    let mut linked = vec![false; diff.rows.len()];
    let Some(node) = node else {
        return linked;
    };
    let refs = node
        .code_refs
        .iter()
        .filter(|code_ref| code_ref.file.to_string() == diff.title)
        .collect::<Vec<_>>();
    if refs.is_empty() {
        return linked;
    }
    let mut hunk = None;
    for (index, row) in diff.rows.iter().enumerate() {
        if matches!(row, DiffRow::HunkHeader(_)) {
            hunk = Some(hunk.map_or(0, |current: u32| current.saturating_add(1)));
            continue;
        }
        linked[index] = refs.iter().any(|code_ref| {
            if hunk != Some(code_ref.hunk) {
                return false;
            }
            let line = match (code_ref.side, row) {
                (DiffSide::Old, DiffRow::Del { old_ln, .. })
                | (DiffSide::Old, DiffRow::Context { old_ln, .. }) => Some(*old_ln),
                (DiffSide::New, DiffRow::Add { new_ln, .. })
                | (DiffSide::New, DiffRow::Context { new_ln, .. }) => Some(*new_ln),
                _ => None,
            };
            line.is_some_and(|line| line >= code_ref.start_line && line <= code_ref.end_line)
        });
    }
    linked
}

fn apply_linked_diff_style(built: &mut BuiltDiff, linked: &[bool]) {
    for (logical, is_linked) in linked.iter().copied().enumerate() {
        if !is_linked {
            continue;
        }
        let Some(&start) = built.first_visual.get(logical) else {
            continue;
        };
        let end = built
            .first_visual
            .get(logical + 1)
            .copied()
            .unwrap_or_else(|| u16::try_from(built.lines.len()).unwrap_or(u16::MAX));
        for line in built
            .lines
            .iter_mut()
            .take(usize::from(end))
            .skip(usize::from(start))
        {
            for span in &mut line.spans {
                let mut style = span
                    .style
                    .add_modifier(Modifier::UNDERLINED | Modifier::BOLD);
                // Keep the existing green/red backgrounds on added/deleted bodies and
                // intraline spans. The gutter and context text receive the linked-code
                // background, while underline/bold is the non-colour cue across the row.
                if style.bg.is_none() {
                    style = style.bg(CODE_LINK_BG);
                }
                span.style = style;
            }
        }
    }
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

/// Wrap mode: every `DiffRow` becomes one or more display lines. Numbered rows keep the
/// dual old/new gutter + sign on the first line and a blank dual gutter + `↪` on
/// continuations; hunk headers never wrap past the `@@ ... @@` prefix unless that alone
/// fits, and every continuation keeps the band background.
fn build_wrapped(
    rows: &[DiffRow],
    spans: &[intraline::ByteSpans],
    ln_w: usize,
    body_w: usize,
) -> BuiltDiff {
    let gutter_w = gutter_width(ln_w);
    let mut lines = Vec::new();
    let mut first_visual = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        first_visual.push(lines.len() as u16);
        match row {
            DiffRow::HunkHeader(h) => {
                // The band spans the full inner width on every segment (§3.4): pad right.
                let band_w = gutter_w + body_w;
                for seg in wrap_body(h, band_w) {
                    let pad = band_w.saturating_sub(measured_cells(&seg));
                    lines.push(Line::from(Span::styled(
                        format!("{seg}{}", " ".repeat(pad)),
                        HUNK,
                    )));
                }
            }
            DiffRow::Add { new_ln, text } => {
                push_numbered(
                    &mut lines,
                    None,
                    Some(*new_ln),
                    '+',
                    text,
                    ADD_BODY,
                    ADD_HI_STYLE,
                    &spans[i],
                    ln_w,
                    body_w,
                );
            }
            DiffRow::Del { old_ln, text } => {
                push_numbered(
                    &mut lines,
                    Some(*old_ln),
                    None,
                    '-',
                    text,
                    DEL_BODY,
                    DEL_HI_STYLE,
                    &spans[i],
                    ln_w,
                    body_w,
                );
            }
            DiffRow::Context {
                old_ln,
                new_ln,
                text,
            } => {
                push_numbered(
                    &mut lines,
                    Some(*old_ln),
                    Some(*new_ln),
                    ' ',
                    text,
                    CTX_BODY,
                    CTX_BODY,
                    &[],
                    ln_w,
                    body_w,
                );
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
/// the (clamped) horizontal offset while the dual gutter stays fixed. Hunk headers do
/// NOT horizontal-scroll: the section text is truncated with an ellipsis.
fn build_raw(
    rows: &[DiffRow],
    spans: &[intraline::ByteSpans],
    ln_w: usize,
    x: usize,
    body_w: usize,
    inner_w: usize,
) -> BuiltDiff {
    let body_w = body_w.max(1);
    // Clamp x to the longest body minus the body viewport.
    let max_body = rows
        .iter()
        .filter(|r| !matches!(r, DiffRow::HunkHeader(_)))
        .map(|r| measured_cells(row_text(r)))
        .max()
        .unwrap_or(0);
    let effective_x = x.min(max_body.saturating_sub(body_w));
    let mut lines = Vec::with_capacity(rows.len());
    let mut first_visual = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        first_visual.push(lines.len() as u16);
        match row {
            DiffRow::HunkHeader(h) => {
                let text = truncate_cells(h, inner_w);
                let pad = inner_w.saturating_sub(text.width());
                lines.push(Line::from(Span::styled(
                    format!("{text}{}", " ".repeat(pad)),
                    HUNK,
                )))
            }
            DiffRow::Add { new_ln, text } => {
                lines.push(raw_numbered(
                    None,
                    Some(*new_ln),
                    '+',
                    text,
                    ADD_BODY,
                    ADD_HI_STYLE,
                    &spans[i],
                    ln_w,
                    effective_x,
                    body_w,
                ));
            }
            DiffRow::Del { old_ln, text } => {
                lines.push(raw_numbered(
                    Some(*old_ln),
                    None,
                    '-',
                    text,
                    DEL_BODY,
                    DEL_HI_STYLE,
                    &spans[i],
                    ln_w,
                    effective_x,
                    body_w,
                ));
            }
            DiffRow::Context {
                old_ln,
                new_ln,
                text,
            } => {
                lines.push(raw_numbered(
                    Some(*old_ln),
                    Some(*new_ln),
                    ' ',
                    text,
                    CTX_BODY,
                    CTX_BODY,
                    &[],
                    ln_w,
                    effective_x,
                    body_w,
                ));
            }
        }
    }
    BuiltDiff {
        lines,
        first_visual,
        effective_x,
    }
}

/// The `{old:>ln} │ {new:>ln} ` part of the gutter (numbers or exactly `ln_w` blanks),
/// plus the sign cell. Returned as styled spans; the source body follows.
fn gutter_spans(
    old: Option<u32>,
    new: Option<u32>,
    ln_w: usize,
    sign: char,
    sign_style: Style,
) -> Vec<Span<'static>> {
    let old_s = old.map_or_else(|| " ".repeat(ln_w), |n| format!("{n:>ln_w$}"));
    let new_s = new.map_or_else(|| " ".repeat(ln_w), |n| format!("{n:>ln_w$}"));
    vec![
        Span::styled(old_s, GUTTER),
        Span::styled(" │ ", GUTTER),
        Span::styled(new_s, GUTTER),
        Span::styled(" ", GUTTER),
        Span::styled(sign.to_string(), sign_style),
    ]
}

/// One numbered diff row in wrap mode: dual gutter + sign on the first display line,
/// blank dual gutter + `↪` on continuations. `spans` marks the changed-word byte ranges
/// (paired rows only): those graphemes take `hi`, the rest take `base`.
#[allow(clippy::too_many_arguments)]
fn push_numbered(
    lines: &mut Vec<Line<'static>>,
    old: Option<u32>,
    new: Option<u32>,
    sign: char,
    text: &str,
    base: Style,
    hi: Style,
    spans: &[(usize, usize)],
    ln_w: usize,
    body_w: usize,
) {
    let gs: Vec<&str> = unicode_segmentation::UnicodeSegmentation::graphemes(text, true).collect();
    let flags = changed_flags(&gs, spans);
    for (i, (s, e)) in wrap_ranges(&gs, body_w).iter().enumerate() {
        let mut body = styled_graphemes(&gs[*s..*e], &flags[*s..*e], base, hi);
        let mut line = if i == 0 {
            gutter_spans(old, new, ln_w, sign, base)
        } else {
            gutter_spans(None, None, ln_w, '↪', GUTTER)
        };
        line.append(&mut body);
        lines.push(Line::from(line));
    }
}

/// One numbered diff row in raw mode: fixed dual gutter, body sliced to the visible
/// window. `spans` marks the changed-word byte ranges; visible graphemes inside them
/// take `hi`, the rest take `base`.
#[allow(clippy::too_many_arguments)]
fn raw_numbered(
    old: Option<u32>,
    new: Option<u32>,
    sign: char,
    text: &str,
    base: Style,
    hi: Style,
    spans: &[(usize, usize)],
    ln_w: usize,
    x: usize,
    body_w: usize,
) -> Line<'static> {
    let gs: Vec<&str> = unicode_segmentation::UnicodeSegmentation::graphemes(text, true).collect();
    let flags = changed_flags(&gs, spans);
    let mut line = gutter_spans(old, new, ln_w, sign, base);
    line.append(&mut slice_styled(&gs, &flags, x, body_w, base, hi));
    Line::from(line)
}

/// Per-grapheme "changed" flags for a row body: a grapheme is changed when its byte
/// range overlaps any span. Graphemes are never split, so a span boundary inside one
/// (e.g. between a letter and its combining mark) highlights the whole grapheme.
fn changed_flags(gs: &[&str], spans: &[(usize, usize)]) -> Vec<bool> {
    let mut flags = Vec::with_capacity(gs.len());
    let mut byte = 0usize;
    let mut si = 0usize;
    for g in gs {
        let end = byte + g.len();
        while si < spans.len() && spans[si].1 <= byte {
            si += 1;
        }
        flags.push(si < spans.len() && spans[si].0 < end);
        byte = end;
    }
    flags
}

/// Group graphemes into styled spans, switching between `base` and `hi` at changed-flag
/// boundaries. Empty input yields no spans (callers emit the gutter and sign).
fn styled_graphemes(gs: &[&str], flags: &[bool], base: Style, hi: Style) -> Vec<Span<'static>> {
    debug_assert_eq!(gs.len(), flags.len());
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for (g, &changed) in gs.iter().zip(flags) {
        let style = if changed { hi } else { base };
        let width = grapheme_cells(g, col);
        // Ratatui stores cells in an intermediate buffer and drops control characters;
        // a literal tab therefore never reaches a terminal for expansion. Materialize it
        // here using the same four-cell stops used by wrapping and horizontal slicing.
        let displayed = if *g == "\t" {
            " ".repeat(width)
        } else {
            (*g).to_string()
        };
        match out.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(&displayed),
            _ => out.push(Span::styled(displayed, style)),
        }
        col += width;
    }
    out
}

/// The display-cell window `[skip, skip + budget)` of `gs` as styled spans — the same
/// edge policy as the plain slicer (a grapheme straddling the left edge is dropped
/// whole, tabs advance to four-cell stops), but switching between `base` and `hi` at
/// changed-flag boundaries.
fn slice_styled(
    gs: &[&str],
    flags: &[bool],
    skip: usize,
    budget: usize,
    base: Style,
    hi: Style,
) -> Vec<Span<'static>> {
    debug_assert_eq!(gs.len(), flags.len());
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    let mut taken = 0usize;
    for (g, &changed) in gs.iter().zip(flags) {
        let w = grapheme_cells(g, col);
        let next = col + w;
        if next <= skip || col < skip {
            col = next;
            continue;
        }
        if taken + w > budget {
            break;
        }
        let style = if changed { hi } else { base };
        let displayed = if *g == "\t" {
            " ".repeat(w)
        } else {
            (*g).to_string()
        };
        match out.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(&displayed),
            _ => out.push(Span::styled(displayed, style)),
        }
        taken += w;
        col = next;
    }
    out
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
/// kept. Tabs are materialized as spaces at four-cell stops because ratatui's cell buffer
/// drops the literal control character before a real terminal can expand it.
fn wrap_body(text: &str, budget: usize) -> Vec<String> {
    let gs: Vec<&str> = unicode_segmentation::UnicodeSegmentation::graphemes(text, true).collect();
    wrap_ranges(&gs, budget)
        .iter()
        .map(|&(s, e)| expand_tabs(&gs[s..e]))
        .collect()
}

/// Render a grapheme slice with tabs expanded at four-cell stops local to the visual line.
fn expand_tabs(gs: &[&str]) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for g in gs {
        let width = grapheme_cells(g, col);
        if *g == "\t" {
            out.push_str(&" ".repeat(width));
        } else {
            out.push_str(g);
        }
        col += width;
    }
    out
}

/// The [`wrap_body`] break policy as grapheme-index ranges into `gs`, so callers can map
/// per-grapheme styles (intraline highlights) onto each wrapped segment.
fn wrap_ranges(gs: &[&str], budget: usize) -> Vec<(usize, usize)> {
    let budget = budget.max(1);
    let mut out: Vec<(usize, usize)> = Vec::new();
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
            out.push((seg_start, end));
            seg_start = end;
            i = end;
            col = 0;
        } else {
            col += w;
            i += 1;
        }
    }
    out.push((seg_start, gs.len()));
    out
}

/// A grapheme worth breaking a wrapped line after: whitespace or ASCII punctuation.
fn is_soft_break(g: &str) -> bool {
    g.chars()
        .any(|c| c.is_whitespace() || c.is_ascii_punctuation())
}

// -- combined Impact pane ------------------------------------------------------------
//
// Deterministic relationships and the generated breakdown describe the same selection,
// so they stay visible together instead of competing for a tabbed bottom pane.

/// The full-width Impact pane. The left column stacks the deterministic selected change,
/// callers, and downstream relationships; a vertical divider separates the generated,
/// selection-scoped breakdown on the right.
fn render_impact(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let focused = app.focused == Pane::Impact;
    // The contents already identify the selected change and generated explanation. A
    // permanent "Impact" title only repeated that context and consumed visual attention.
    // Keep the zoom state discoverable without naming the section.
    let zoom_title = (app.zoomed && focused).then(|| {
        Line::from(Span::styled(
            " ZOOM ",
            Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
        ))
    });
    let block = pane_block(Line::from(""), zoom_title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 6 || inner.height == 0 {
        return;
    }

    let left_width = impact_left_width(
        app.dividers.get(DividerId::RelationshipsGenerated),
        inner.width,
    );
    let columns =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Min(0)]).split(inner);
    render_impact_body(frame, columns[0], app, snap);

    let generated = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::new().fg(BORDER))
        .border_type(BorderType::Plain)
        .padding(Padding::left(1));
    let generated_inner = generated.inner(columns[1]);
    frame.render_widget(generated, columns[1]);
    if generated_inner.width > 0 && generated_inner.height > 0 {
        render_generated_impact(frame, generated_inner, app, snap);
    }
}

/// The deterministic half of Impact: selected change above callers above downstream.
fn render_impact_body(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let impact = &snap.impact;
    let [selected_height, callers_height, downstream_height] = impact_section_heights(
        app.dividers.get(DividerId::SelectedCallers),
        app.dividers.get(DividerId::CallersDownstream),
        area.height,
    );
    let rows = Layout::vertical([
        Constraint::Length(selected_height),
        Constraint::Length(callers_height),
        Constraint::Length(downstream_height),
    ])
    .split(area);

    render_selected_change(frame, rows[0], impact, true);
    render_impact_list(
        frame,
        rows[1],
        "CALLERS",
        &impact.callers,
        app.callers_scroll,
        true,
    );
    render_impact_list(
        frame,
        rows[2],
        "DOWNSTREAM",
        &impact.downstream,
        app.downstream_scroll,
        false,
    );
}

/// The generated explanation is laid out from validated structure at the current width.
/// If AI is unavailable, the same visual grammar renders a deterministic relationship
/// fallback from the selected change, callers, and downstream facts.
fn render_generated_impact(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let lines = generated_impact_content(app, snap, area.width);
    let height = usize::from(area.height);
    let max_scroll = lines.len().saturating_sub(height);
    let start = app.ai_plan_scroll.min(max_scroll);
    let end = (start + height).min(lines.len());
    let rendered = lines[start..end]
        .iter()
        .cloned()
        .map(render_diagram_line)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rendered), area);
}

/// The generated pane's semantic line layout, shared by rendering and retained mouse
/// geometry. Node-bearing spans keep their plan-local targets until the final Ratatui
/// conversion, so hover hitboxes cannot drift from the cells a user sees.
pub(crate) fn generated_impact_content(
    app: &App,
    snap: &UiSnapshot,
    width: u16,
) -> Vec<crate::diagram::DiagramLine> {
    let sem = &snap.semantic;
    // Node highlighting follows the actual selection, not the dispatcher's data-quality
    // note: the selected change label is what node labels and entity symbols must match.
    let selected_label = snap
        .impact
        .selected_change
        .as_ref()
        .map(|selected| selected.label.as_str())
        .unwrap_or("");
    let diagram = if sem.ai_generated {
        sem.plan.as_ref().map(|plan| {
            let active = app
                .hovered_plan_node
                .as_ref()
                .or(app.expanded_plan_node.as_ref());
            crate::diagram::interactive_plan_lines(
                plan,
                width,
                selected_label,
                active,
                app.expanded_plan_node.as_ref(),
            )
        })
    } else {
        None
    }
    .unwrap_or_else(|| fallback_lines(&snap.impact, width));

    let mut lines = Vec::new();
    if !sem.ai_generated {
        let status: Option<(String, DiagramRole)> = match snap.ai {
            AiStatus::WaitingForSymbols { .. } => Some((
                "Waiting for symbol analysis…".to_string(),
                DiagramRole::Warning,
            )),
            AiStatus::WaitingForRelations { .. } => Some((
                "Waiting for symbol relationships…".to_string(),
                DiagramRole::Warning,
            )),
            AiStatus::Queued { position: 1, .. } => Some((
                "Waiting for AI capacity · priority #1".to_string(),
                DiagramRole::Warning,
            )),
            AiStatus::Queued { position, .. } => Some((
                format!("Waiting for AI capacity · priority #{position}"),
                DiagramRole::Warning,
            )),
            AiStatus::Loading { .. } => Some((
                "Generating a deeper explanation…".to_string(),
                DiagramRole::Warning,
            )),
            AiStatus::Failed { .. } => Some((
                "AI failed; showing known relationships".to_string(),
                DiagramRole::Warning,
            )),
            AiStatus::Stale { .. } => Some((
                "Repository changed; showing known relationships".to_string(),
                DiagramRole::Warning,
            )),
            AiStatus::Disabled => Some((
                "Known relationships · AI not configured".to_string(),
                DiagramRole::Muted,
            )),
            AiStatus::Idle => Some((
                "Known relationships · AI generation is automatic".to_string(),
                DiagramRole::Muted,
            )),
            AiStatus::Ready { .. } => None,
        };
        if let Some((text, role)) = status {
            lines.push(crate::diagram::DiagramLine::plain(text, role));
        }
    } else if let Some(report) = &sem.report {
        // A sanitized plan (the validator dropped content) gets one concise WARN line
        // before the visual; the full reasons stay in the debug-ai JSON, not this pane.
        if report.verdict == ValidationVerdict::ValidWithDrops || !report.dropped.is_empty() {
            let removed = report.dropped.len();
            let items = if removed == 1 { "item" } else { "items" };
            lines.push(crate::diagram::DiagramLine::plain(
                truncate_cells(
                    &format!("⚠ sanitized AI plan · {removed} {items} removed"),
                    width.into(),
                ),
                DiagramRole::Warning,
            ));
        }
    }
    lines.extend(diagram);
    lines
}

fn render_diagram_line(line: crate::diagram::DiagramLine) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|span| {
                let style = match span.role {
                    DiagramRole::Title => Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
                    DiagramRole::Text => Style::new().fg(TEXT),
                    DiagramRole::Border | DiagramRole::Muted => Style::new().fg(MUTED),
                    DiagramRole::Selected => Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
                    DiagramRole::Hovered => Style::new()
                        .fg(TEXT)
                        .bg(SELECTED_BG)
                        .add_modifier(Modifier::BOLD),
                    DiagramRole::Arrow | DiagramRole::Evidence => Style::new().fg(HUNK_FG),
                    DiagramRole::Warning => Style::new().fg(WARN),
                };
                Span::styled(span.text, style)
            })
            .collect::<Vec<_>>(),
    )
}

/// The SELECTED CHANGE column: header, then the symbol label (ACCENT+BOLD) + badge, one
/// deterministic interpretation line, and the pane note when space remains. A file-row
/// selection shows the basename and the "select one to inspect impact" guidance.
fn render_selected_change(
    frame: &mut Frame,
    area: Rect,
    impact: &crate::snapshot::ImpactPane,
    divider: bool,
) {
    let block = impact_section_block(divider);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines: Vec<Line> = vec![header_line("SELECTED CHANGE")];
    match &impact.selected_change {
        Some(sel) => {
            let badge = if sel.change.is_empty() {
                String::new()
            } else {
                format!("  {}", sel.change)
            };
            let badge_color = match sel.change {
                "added" => ADD_FG,
                "removed" => DEL_FG,
                _ => WARN,
            };
            lines.push(Line::from(vec![
                Span::styled(
                    basename(&sel.label).to_string(),
                    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(badge, Style::new().fg(badge_color)),
            ]));
            lines.push(Line::from(Span::styled(
                truncate_cells(&sel.interpretation, inner.width as usize),
                Style::new().fg(MUTED),
            )));
            if !impact.note.is_empty() && inner.height >= 4 {
                lines.push(Line::from(Span::styled(
                    truncate_cells(&impact.note, inner.width as usize),
                    Style::new().fg(MUTED),
                )));
            }
        }
        None => lines.push(Line::from(Span::styled(
            "select a changed file or symbol",
            Style::new().fg(MUTED),
        ))),
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A CALLERS / DOWNSTREAM section: header with a live count (`· …` while loading, never
/// a false zero), then rows; when rows remain past the visible space the final visible
/// row is `… +N more` in MUTED.
fn render_impact_list(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    list: &ImpactList,
    requested_offset: usize,
    divider: bool,
) {
    let block = impact_section_block(divider);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let base_header = match list.state {
        ImpactLoadState::Loading => format!("{name} · …"),
        _ => format!("{name} · {}", list.rows.len()),
    };

    // Interior rows available below the header.
    let avail = (inner.height as usize).saturating_sub(1);
    let rows = &list.rows;
    let max_offset = if avail == 0 {
        0
    } else {
        rows.len().saturating_sub(avail)
    };
    let offset = requested_offset.min(max_offset);
    let header = if offset > 0 {
        format!("{base_header} · ↑{offset}")
    } else {
        base_header
    };
    let mut lines: Vec<Line> = vec![header_line(&header)];
    let visible = &rows[offset..];
    let show = visible.len().min(avail);
    for r in &visible[..show] {
        let style = if r.changed {
            Style::new().fg(WARN)
        } else {
            Style::new().fg(TEXT)
        };
        let mut spans = vec![Span::styled(r.label.clone(), style)];
        if !r.relation.is_empty() {
            spans.push(Span::styled(
                format!("  {}", r.relation),
                Style::new().fg(MUTED),
            ));
        }
        if r.has_diagnostic {
            spans.push(Span::styled(" !", Style::new().fg(ERROR)));
        }
        lines.push(Line::from(spans));
    }
    // Truncation marker: replace the last visible row when more remain.
    if visible.len() > avail && avail > 0 {
        let more = visible.len() - (avail - 1);
        lines.truncate(1 + avail - 1);
        lines.push(Line::from(Span::styled(
            format!("… +{more} more"),
            Style::new().fg(MUTED),
        )));
    }
    if rows.is_empty() {
        let msg = match list.state {
            ImpactLoadState::Loading => "loading…",
            ImpactLoadState::Unavailable => "unavailable",
            ImpactLoadState::Idle => "select a symbol",
            ImpactLoadState::Ready => "none",
        };
        lines.push(Line::from(Span::styled(msg, Style::new().fg(MUTED))));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Shared styling for the vertically stacked deterministic Impact sections.
fn impact_section_block(divider: bool) -> Block<'static> {
    let block = Block::default().padding(Padding::horizontal(1));
    if divider {
        block
            .borders(Borders::BOTTOM)
            .border_style(Style::new().fg(BORDER))
            .border_type(BorderType::Plain)
    } else {
        block
    }
}

/// A column header: MUTED + BOLD (docs/review/15 §2).
fn header_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::new().fg(MUTED).add_modifier(Modifier::BOLD),
    ))
}

// -- combined bottom bar ----------------------------------------------------------

/// Commands/status on the left; process token usage and the selected file on the right.
/// The filename is always the final field, so it remains visually right-justified.
fn render_bottom_bar(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let chunks = bottom_bar_chunks(area, app, snap);
    let path = app.selected_file_path().unwrap_or("");
    let right_width = chunks[1].width as usize;

    if snap.status.text.is_empty() {
        frame.render_widget(
            Paragraph::new(help_line(chunks[0].width as usize)).style(Style::new().bg(SURFACE)),
            chunks[0],
        );
    } else {
        let fg = match snap.status.level {
            StatusLevel::Error => ERROR,
            StatusLevel::Warning => WARN,
            StatusLevel::Info => MUTED,
        };
        let text = truncate_cells(&format!(" {}", snap.status.text), chunks[0].width as usize);
        frame.render_widget(
            Paragraph::new(text).style(Style::new().fg(fg).bg(SURFACE)),
            chunks[0],
        );
    }

    let right = bottom_right_text(path, snap, right_width);
    frame.render_widget(
        Paragraph::new(right)
            .alignment(Alignment::Right)
            .style(Style::new().fg(MUTED).bg(SURFACE)),
        chunks[1],
    );
}

/// Split the bottom bar once for both rendering and mouse hit-testing. The left rectangle
/// is the clickable status/help segment; the right rectangle owns token usage and path.
pub(crate) fn bottom_bar_chunks(area: Rect, app: &App, snap: &UiSnapshot) -> [Rect; 2] {
    let width = area.width as usize;
    let path = app.selected_file_path().unwrap_or("");
    let desired_right = bottom_right_text(path, snap, usize::MAX)
        .width()
        .saturating_add(1);
    let min_left = if width >= 96 {
        48
    } else if width >= 64 {
        28
    } else {
        12
    };
    let right_width = desired_right.min(width.saturating_sub(min_left));
    let chunks = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(u16::try_from(right_width).unwrap_or(u16::MAX)),
    ])
    .split(area);
    [chunks[0], chunks[1]]
}

fn help_line(width: usize) -> Line<'static> {
    let groups: &[(&str, &str)] = if width >= 83 {
        &[
            ("R", "refresh"),
            ("Tab", "expand"),
            ("1-3", "pane"),
            ("n/N", "hunk"),
            ("wheel", "scroll"),
            ("drag", "resize"),
            ("?", "help"),
        ]
    } else if width >= 48 {
        &[
            ("R", "refresh"),
            ("Tab", "expand"),
            ("1-3", "pane"),
            ("?", "help"),
        ]
    } else {
        &[("R", ""), ("Tab", ""), ("1-3", ""), ("?", "")]
    };
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for (i, (key, label)) in groups.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(MUTED)));
        }
        spans.push(Span::styled((*key).to_string(), Style::new().fg(TEXT)));
        if !label.is_empty() {
            spans.push(Span::styled(format!(" {label}"), Style::new().fg(MUTED)));
        }
    }
    Line::from(spans)
}

fn bottom_right_text(path: &str, snap: &UiSnapshot, budget: usize) -> String {
    let input = compact_count(snap.ai_tokens.input);
    let output = compact_count(snap.ai_tokens.output);
    let full_usage = format!("tokens in {input} out {output}");
    let compact_usage = format!("in {input} out {output}");
    if path.is_empty() {
        return truncate_cells(&format!("{full_usage} "), budget);
    }

    for usage in [&full_usage, &compact_usage] {
        for shown_path in [path, basename(path)] {
            let candidate = format!("{usage} · {shown_path} ");
            if candidate.width() <= budget {
                return candidate;
            }
        }
    }

    let fixed = compact_usage.width() + " ·  ".width();
    if budget > fixed {
        let shown_path = truncate_cells(basename(path), budget - fixed);
        return format!("{compact_usage} · {shown_path} ");
    }
    truncate_cells(&compact_usage, budget)
}

fn compact_count(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=9_999 => format!("{:.1}k", value as f64 / 1_000.0),
        10_000..=999_999 => format!("{}k", value / 1_000),
        1_000_000..=9_999_999 => format!("{:.1}m", value as f64 / 1_000_000.0),
        _ => format!("{}m", value / 1_000_000),
    }
}

// -- status detail overlay -------------------------------------------------------

/// Full, wrapped rendering of the exact status the user clicked. The dialog is content
/// sized and width-capped; it grows only when the provider response actually needs room.
fn render_status_detail(frame: &mut Frame, area: Rect, status: &crate::snapshot::StatusMessage) {
    let border = match status.level {
        StatusLevel::Error => ERROR,
        StatusLevel::Warning => WARN,
        StatusLevel::Info => ACCENT,
    };
    let detail = status.detail.as_deref().unwrap_or(&status.text);
    let popup = status_detail_rect(area, detail);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(border))
        .title(" status details · click or Esc to close ");
    let paragraph = Paragraph::new(detail.to_string())
        .style(Style::new().fg(TEXT))
        .wrap(Wrap { trim: false })
        .block(block);
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(paragraph, popup);
}

/// A comfortable status-dialog rectangle: at most 120 columns / 80% of the terminal,
/// with height derived from word-wrapped display cells and a two-row border allowance.
fn status_detail_rect(area: Rect, text: &str) -> Rect {
    let available_width = area.width.saturating_sub(4).max(1);
    let preferred_width = area.width.saturating_mul(4) / 5;
    let width = preferred_width.clamp(1, 120).max(40).min(available_width);
    let content_width = width.saturating_sub(2).max(1);
    let wrapped = u16::try_from(wrapped_line_count(text, content_width)).unwrap_or(u16::MAX);
    let available_height = area.height.saturating_sub(4).max(1);
    let height = wrapped.saturating_add(2).max(5).min(available_height);
    centered_rect(area, width, height)
}

/// Estimate Ratatui's word wrapping in terminal display cells, including hard wrapping
/// for individual JSON/token fragments wider than the dialog.
fn wrapped_line_count(text: &str, width: u16) -> usize {
    let width = usize::from(width.max(1));
    text.split('\n')
        .map(|line| {
            let mut rows = 1usize;
            let mut used = 0usize;
            for token in unicode_segmentation::UnicodeSegmentation::split_word_bounds(line) {
                let token_width = token.width();
                if token_width == 0 {
                    continue;
                }
                if used > 0 && used.saturating_add(token_width) > width {
                    rows = rows.saturating_add(1);
                    used = 0;
                }
                if token_width <= width {
                    used = used.saturating_add(token_width);
                    continue;
                }

                let full_rows = token_width / width;
                let remainder = token_width % width;
                if remainder == 0 {
                    rows = rows.saturating_add(full_rows.saturating_sub(1));
                    used = width;
                } else {
                    rows = rows.saturating_add(full_rows);
                    used = remainder;
                }
            }
            rows
        })
        .sum::<usize>()
        .max(1)
}

// -- help modal -------------------------------------------------------------------

fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered(area, 70, 70);
    frame.render_widget(ratatui::widgets::Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            "codescope — controls",
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  q / Ctrl-C      quit"),
        Line::from("  ? / Esc         this help / close"),
        Line::from("  R               refresh repository state"),
        Line::from("  Tab             expand / collapse file (symbols load automatically)"),
        Line::from("  1 / 2 / 3       focus files / diff / impact"),
        Line::from("  j/k · ↑/↓       move selection · scroll"),
        Line::from("  Ctrl-d/u · Pg   half / full page in diff"),
        Line::from("  s / u / B / w   staged / unstaged / branch / working scope"),
        Line::from("  S               cycle scope"),
        Line::from("  b               pick comparison base (default: nearest ancestor)"),
        Line::from("  Enter           jump to symbol / re-center view"),
        Line::from("  Space h l       expand / collapse"),
        Line::from("  mouse hover     impact node highlights its linked diff code"),
        Line::from("  click / Space   expand hovered impact-node details"),
        Line::from("  mouse wheel     scroll the section under the pointer"),
        Line::from("  mouse drag      resize any pane divider"),
        Line::from("  n / N           next / previous diff hunk"),
        Line::from("  z               zoom the focused pane (Tab still switches)"),
        Line::from("  W / 0           diff: toggle wrap / reset horizontal scroll"),
        Line::from("  g / G           top / bottom"),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(WARN))
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

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width.min(area.width),
        height.min(area.height),
    )
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
        format!(" AI settings{provider} ")
    } else {
        format!(" AI settings (model: {}){provider} ", snap.ai_model)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(ACCENT))
        .title(title);
    let models = filter_candidates(&snap.available_models, &app.model_query);
    let supports_reasoning_effort = snap.ai_provider != "anthropic";
    let selected_effort = app.selected_reasoning_effort();
    let mut items = if supports_reasoning_effort {
        vec![
            ListItem::new(Line::from(vec![
                Span::styled("  reasoning effort  ", Style::new().fg(MUTED)),
                Span::styled(
                    format!("← {selected_effort} →"),
                    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ])),
            ListItem::new(Line::from(Span::styled(
                "  default is automatic; explicit support varies by model",
                Style::new().fg(MUTED),
            ))),
        ]
    } else {
        vec![
            ListItem::new(Line::from(Span::styled(
                "  reasoning effort is unavailable through Anthropic's native API",
                Style::new().fg(MUTED),
            ))),
            ListItem::new(Line::from("")),
        ]
    };
    let model_items: Vec<ListItem> = if snap.available_models.is_empty() {
        let message = if snap.ai_model.is_empty() && snap.ai_provider.is_empty() {
            "  AI is not configured; set a provider API key".to_string()
        } else if snap.model_list_loading {
            "  fetching models from the configured provider…".to_string()
        } else {
            "  provider returned no discoverable models; type an exact model id".to_string()
        };
        vec![ListItem::new(Line::from(Span::styled(
            message,
            Style::new().fg(MUTED),
        )))]
    } else if models.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            if app.model_query.trim().is_empty() {
                "  no matches".to_string()
            } else {
                format!(
                    "  Enter to use {:?} as an exact model id",
                    app.model_query.trim()
                )
            },
            Style::new().fg(WARN),
        )))]
    } else {
        models
            .iter()
            .map(|m| {
                let cur = if *m == snap.ai_model { " ●" } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(cur, Style::new().fg(ADD_FG)),
                    Span::raw(format!(" {m}")),
                ]))
            })
            .collect()
    };
    items.extend(model_items);
    if snap.model_list_loading && !snap.available_models.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  fetching additional models…",
            Style::new().fg(MUTED),
        ))));
    }
    if let Some(error) = &snap.model_list_error {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("  discovery failed: {error}"),
            Style::new().fg(WARN),
        ))));
        items.push(ListItem::new(Line::from(Span::styled(
            "  current model remains selectable; type an exact model id to switch",
            Style::new().fg(MUTED),
        ))));
    }
    items.push(ListItem::new(Line::from(Span::styled(
        format!("  filter: {}", app.model_query),
        Style::new().fg(WARN),
    ))));
    items.push(ListItem::new(Line::from(Span::styled(
        if supports_reasoning_effort {
            "  ←/→ reasoning · type to filter · ↑/↓ move · Enter apply · Esc close"
        } else {
            "  type to filter · ↑/↓ move · Enter model · Esc close"
        },
        Style::new().fg(MUTED),
    ))));
    let mut state = ListState::default();
    if !models.is_empty() {
        state.select(Some(2 + app.model_sel.min(models.len().saturating_sub(1))));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(SELECTED_BG));
    frame.render_stateful_widget(list, popup, &mut state);
}

// -- base picker modal --------------------------------------------------------

fn render_base_picker(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let popup = centered(area, 50, 50);
    frame.render_widget(ratatui::widgets::Clear, popup);
    let selected = if snap.base_ref.is_empty() {
        "none"
    } else {
        snap.base_ref.as_str()
    };
    let bounded = if snap.base_candidates_truncated {
        " · bounded list"
    } else {
        ""
    };
    let title = format!(" comparison base (selected: {selected}{bounded}) ");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(ACCENT))
        .title(title);
    let bases = filter_candidates(&snap.available_bases, &app.base_query);
    let mut items: Vec<ListItem> = if snap.available_bases.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  fetching base candidates…",
            Style::new().fg(MUTED),
        )))]
    } else if bases.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no matches",
            Style::new().fg(MUTED),
        )))]
    } else {
        bases
            .iter()
            .map(|b| {
                let cur = if *b == snap.base_ref { " ●" } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(cur, Style::new().fg(ADD_FG)),
                    Span::raw(format!(" {b}")),
                ]))
            })
            .collect()
    };
    if snap.base_candidates_truncated {
        items.push(ListItem::new(Line::from(Span::styled(
            "  more ancestors omitted by the scan bound",
            Style::new().fg(WARN),
        ))));
    }
    items.push(ListItem::new(Line::from(Span::styled(
        format!("  filter: {}", app.base_query),
        Style::new().fg(WARN),
    ))));
    items.push(ListItem::new(Line::from(Span::styled(
        "  type to filter · ↑/↓ move · Enter select · Esc close",
        Style::new().fg(MUTED),
    ))));
    let mut state = ListState::default();
    if !bases.is_empty() {
        state.select(Some(app.base_sel.min(bases.len().saturating_sub(1))));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(SELECTED_BG));
    frame.render_stateful_widget(list, popup, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{DiffPane, FileRow, SymbolRow};
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

    /// An app that has consumed `snap` (so `current_hunk` and scroll state are set).
    fn app_with(snap: &UiSnapshot) -> App {
        let mut app = App::new();
        app.update(snap.clone());
        app
    }

    /// One buffer row as a string.
    fn row_text(t: &Terminal<TestBackend>, y: u16) -> String {
        let buf = t.backend().buffer();
        let w = buf.area.width;
        (0..w)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
            .collect()
    }

    /// (symbol, fg, bg, modifier) of one buffer cell.
    fn cell(t: &Terminal<TestBackend>, x: u16, y: u16) -> (String, Color, Color, Modifier) {
        let c = t.backend().buffer().cell((x, y)).unwrap();
        (c.symbol().to_string(), c.fg, c.bg, c.modifier)
    }

    fn snap_with_base() -> UiSnapshot {
        let mut snap = UiSnapshot::default();
        snap.repo.repo_name = "demo".to_string();
        snap.repo.branch = "feature/x".to_string();
        snap.repo.base = Some("origin/main".to_string());
        snap.base_ref = "release/2.0".to_string();
        snap.scope = ChangeScope::Branch;
        snap.ls = LsStatus::Ready;
        snap.ai = AiStatus::Disabled;
        snap.ai_provider = "prime".to_string();
        snap.ai_model = "z-ai/glm-5.3".to_string();
        snap.available_bases = vec![
            "release/2.0".to_string(),
            "main".to_string(),
            "origin/main".to_string(),
        ];
        snap
    }

    /// A snapshot with one changed file holding a symbol and one hunk of diff.
    fn sample() -> UiSnapshot {
        let mut snap = snap_with_base();
        snap.files = vec![FileRow {
            path: "internal/service/service.go".to_string(),
            status: "M",
            expanded: true,
            changed_symbol_count: 1,
            added_lines: 1,
            removed_lines: 1,
            semantic: crate::snapshot::FileSemanticLoad::Ready,
            symbols: vec![SymbolRow {
                name: "GetDisplayName".to_string(),
                change: "modified",
                confidence: "",
                has_diagnostic: false,
                position: None,
            }],
        }];
        snap.diff = DiffPane {
            title: "internal/service/service.go".to_string(),
            focused_symbol: None,
            rows: vec![
                DiffRow::HunkHeader("@@ -10,3 +10,4 @@ func GetDisplayName".to_string()),
                DiffRow::Context {
                    old_ln: 10,
                    new_ln: 10,
                    text: "func (s *UserService) GetDisplayName(".to_string(),
                },
                DiffRow::Del {
                    old_ln: 11,
                    text: "    return name".to_string(),
                },
                DiffRow::Add {
                    new_ln: 11,
                    text: "    return prefix + name".to_string(),
                },
            ],
            current_hunk: 1,
            total_hunks: 1,
        };
        snap
    }

    // -- §7.1/§7.2: exact normal geometry ------------------------------------------

    #[test]
    fn geometry_140x40_exact() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let app = app_with(&sample());
        t.draw(|f| render(f, &app, &sample())).unwrap();
        // Dense layout: top y=0, work y=1..22, Impact y=23..38, bottom y=39.
        assert!(
            row_text(&t, 0).contains("codescope"),
            "top bar: {}",
            row_text(&t, 0)
        );
        assert!(row_text(&t, 0).contains("1 file"), "count moved to top");
        assert!(
            row_text(&t, 1).contains("Changed files"),
            "files block top: {}",
            row_text(&t, 1)
        );
        // Work row split: files width 42 → its right border is x=41, diff starts x=42.
        assert_eq!(cell(&t, 41, 1).0, "┐", "files right border at x=41");
        assert_eq!(cell(&t, 42, 1).0, "┌", "diff left border at x=42");
        assert!(!row_text(&t, 23).contains("Impact"));
        assert!(row_text(&t, 23).starts_with('┌'), "impact top border");
        assert!(row_text(&t, 38).starts_with('└'), "impact bottom border");
        assert!(
            row_text(&t, 22).starts_with('└'),
            "files/diff bottom at y=22"
        );
        assert!(
            row_text(&t, 39).contains("? help"),
            "help: {}",
            row_text(&t, 39)
        );
        assert_eq!(cell(&t, 0, 39).2, SURFACE, "help bar bg surface");
    }

    #[test]
    fn geometry_80x20_minimum() {
        let mut t = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let app = app_with(&sample());
        t.draw(|f| render(f, &app, &sample())).unwrap();
        // At the minimum size the work area keeps its seven-row floor: work y=1..7,
        // Impact y=8..18, combined bottom y=19.
        assert_eq!(cell(&t, 31, 1).0, "┐", "files right border x=31");
        assert_eq!(cell(&t, 32, 1).0, "┌", "diff left border x=32");
        assert!(
            row_text(&t, 7).starts_with('└'),
            "work bottom y=7: {}",
            row_text(&t, 7)
        );
        assert!(row_text(&t, 8).starts_with('┌'), "impact top y=8");
        assert!(row_text(&t, 18).starts_with('└'), "impact bottom y=18");
    }

    #[test]
    fn geometry_focus_only_below_normal() {
        // 79x40 and 140x19: focus-only; Tab changes the visible pane.
        for (w, h) in [(79u16, 40u16), (140, 19)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut app = App::new();
            let snap = sample();
            t.draw(|f| render(f, &app, &snap)).unwrap();
            assert!(
                buffer_text(&t).contains("Changed files"),
                "{w}x{h} files first"
            );
            assert!(
                !buffer_text(&t).contains("func (s *UserService)"),
                "{w}x{h} no diff yet"
            );
            app.apply(crate::action::Action::Focus(crate::app::Pane::Diff));
            t.draw(|f| render(f, &app, &snap)).unwrap();
            assert!(
                buffer_text(&t).contains("func (s *UserService)"),
                "{w}x{h} diff after focus"
            );
            app.apply(crate::action::Action::Focus(crate::app::Pane::Impact));
            t.draw(|f| render(f, &app, &snap)).unwrap();
            assert!(
                buffer_text(&t).contains("SELECTED CHANGE"),
                "{w}x{h} impact after focus"
            );
        }
    }

    #[test]
    fn geometry_too_small_and_minimum_usable() {
        for (w, h) in [(29u16, 8u16), (30, 7)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| render(f, &app_with(&sample()), &sample()))
                .unwrap();
            assert!(buffer_text(&t).contains("too small"), "{w}x{h}");
        }
        // 30x8: no panic, valid frame.
        let mut t = Terminal::new(TestBackend::new(30, 8)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        assert!(!buffer_text(&t).contains("too small"));
    }

    // -- §7.4: top bar ---------------------------------------------------------------

    #[test]
    fn top_bar_content_and_right_reservation() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        let top = row_text(&t, 0);
        assert!(top.contains("codescope"), "{top}");
        assert!(top.contains("demo"), "repo: {top}");
        assert!(
            top.contains("release/2.0 ← feature/x"),
            "explicit branch/base labels: {top}"
        );
        assert!(
            top.contains("branch  LSP ✓  prime z-ai/glm-5.3 reasoning:default ×"),
            "right group: {top}"
        );
        // The right group is reserved flush against the terminal's right edge.
        assert!(top.ends_with("× "), "right-aligned: {top:?}");
    }

    #[test]
    fn top_bar_puts_provider_and_selected_model_before_ai_status() {
        let mut snap = sample();
        snap.ai = AiStatus::Ready {
            epoch: codescope_core::Epoch(1),
        };
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        let top = row_text(&t, 0);
        assert!(
            top.contains("prime z-ai/glm-5.3 reasoning:default ✓"),
            "provider/model/reasoning/status order: {top}"
        );
        assert!(!top.contains("AI ✓ prime"), "retired order: {top}");
    }

    #[test]
    fn top_bar_long_branch_never_clips_service_state() {
        let mut snap = sample();
        snap.repo.branch = "feature/".to_string() + &"a-very-long-branch-name".repeat(6);
        for w in [140u16, 100, 80] {
            let mut t = Terminal::new(TestBackend::new(w, 40)).unwrap();
            t.draw(|f| render(f, &App::new(), &snap)).unwrap();
            let top = row_text(&t, 0);
            assert!(top.contains("LSP ✓"), "{w}: lsp survives: {top}");
            assert!(
                top.contains("prime") && top.contains('×'),
                "{w}: ai survives: {top}"
            );
        }
    }

    #[test]
    fn top_bar_reads_base_ref_then_repo_base() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let app = App::new();
        t.draw(|f| render(f, &app, &snap_with_base())).unwrap();
        assert!(row_text(&t, 0).contains("release/2.0 ← feature/x"));
        let mut snap = snap_with_base();
        snap.base_ref.clear();
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(row_text(&t, 0).contains("origin/main ← feature/x"));
    }

    #[test]
    fn top_bar_labels_missing_base_explicitly() {
        let mut snap = snap_with_base();
        snap.base_ref.clear();
        snap.repo.base = None;
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert!(row_text(&t, 0).contains("none ← feature/x"));
    }

    #[test]
    fn top_bar_refresh_spinner() {
        let mut snap = sample();
        snap.refreshing = true;
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert!(row_text(&t, 0).contains('⟳'), "spinner");
    }

    // -- retired summary row ----------------------------------------------------------

    #[test]
    fn changed_file_count_lives_in_top_bar_and_summary_row_is_gone() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let snap = sample();
        t.draw(|f| render(f, &app_with(&snap), &snap)).unwrap();
        assert!(row_text(&t, 0).contains("1 file"));
        assert!(row_text(&t, 1).contains("Changed files"));
        let text = buffer_text(&t);
        assert!(!text.contains("1 symbol in service.go"));
        assert!(!text.contains("semantics unavailable"));
    }

    #[test]
    fn top_bar_file_count_handles_empty_and_plural() {
        let mut snap = snap_with_base();
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert!(row_text(&t, 0).contains("0 files"));
        snap.files = vec![sample().files[0].clone(), sample().files[0].clone()];
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert!(row_text(&t, 0).contains("2 files"));
    }

    #[test]
    fn hunk_navigation_remains_in_diff_title() {
        let mut snap = sample();
        snap.diff
            .rows
            .push(DiffRow::HunkHeader("@@ -30,2 +30,2 @@ tail".to_string()));
        snap.diff.rows.push(DiffRow::Context {
            old_ln: 30,
            new_ln: 30,
            text: "}".to_string(),
        });
        snap.diff.total_hunks = 2;
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let mut app = app_with(&snap);
        app.apply(crate::action::Action::NextHunk);
        t.draw(|f| render(f, &app, &snap)).unwrap();
        let diff_title_row = row_text(&t, 1);
        assert!(
            diff_title_row.contains("hunk 2/2"),
            "diff title: {diff_title_row}"
        );
    }

    // -- §3.3 / §7.6: files pane ---------------------------------------------------------

    #[test]
    fn files_pane_titles_count_and_status_colors() {
        let mut snap = sample();
        snap.files.push(FileRow {
            path: "README.md".to_string(),
            status: "A",
            expanded: false,
            changed_symbol_count: 0,
            added_lines: 1,
            removed_lines: 0,
            semantic: crate::snapshot::FileSemanticLoad::Ready,
            symbols: vec![],
        });
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&snap), &snap)).unwrap();
        let files_top = row_text(&t, 1);
        assert!(
            files_top.contains("Changed files"),
            "left title: {files_top}"
        );
        // Right title is the active count, right-aligned against the right border (x=41).
        let files_top: String = (0..42u16).map(|x| cell(&t, x, 1).0).collect();
        assert!(
            files_top.contains("Changed files"),
            "left title: {files_top:?}"
        );
        assert_eq!(
            cell(&t, 39, 1).0,
            "2",
            "count right-aligned before the border: {files_top:?}"
        );
        // Status colors: M is WARN, A is ADD_FG.
        assert_eq!(cell(&t, 1, 2).0, "M");
        assert_eq!(cell(&t, 1, 2).1, WARN);
        let plus_x = (0..41u16)
            .find(|&x| cell(&t, x, 2).0 == "+" && cell(&t, x + 1, 2).0 == "1")
            .expect("added LoC");
        let minus_x = (0..41u16)
            .find(|&x| cell(&t, x, 2).0 == "-" && cell(&t, x + 1, 2).0 == "1")
            .expect("removed LoC");
        assert_eq!(cell(&t, plus_x, 2).1, ADD_FG, "added LoC is green");
        assert_eq!(cell(&t, minus_x, 2).1, DEL_FG, "removed LoC is red");
        // The file row is at y=2 (block top border y=1). Find the `A` row.
        let y_a = (2..28u16)
            .find(|&y| cell(&t, 1, y).0 == "A")
            .expect("added file row");
        assert_eq!(cell(&t, 1, y_a).1, ADD_FG);
        // Selected row background on the first file row (active file is file_sel=0);
        // the SELECTED_BG fills the whole inner row.
        assert_eq!(cell(&t, 10, 2).2, SELECTED_BG, "selected row bg");
        assert_eq!(
            cell(&t, 39, 2).2,
            SELECTED_BG,
            "selected row bg to the row end"
        );
    }

    #[test]
    fn files_pane_dirs_muted_basename_text() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        // The file row (y=2) shows `M ▾ internal/service/service.go`: the directory
        // components and separators are MUTED; the basename is TEXT + BOLD (active).
        let buf = t.backend().buffer();
        let cells: Vec<(String, Color, Modifier)> = (0..42u16)
            .map(|x| {
                let c = buf.cell((x, 2)).unwrap();
                (c.symbol().to_string(), c.fg, c.modifier)
            })
            .collect();
        let joined: String = cells.iter().map(|(s, _, _)| s.as_str()).collect();
        assert!(
            joined.contains("internal/service/service.go"),
            "row: {joined:?}"
        );
        // Basename = the chars after the LAST '/' in the path text. Find the LAST '/'
        // cell (char index == x because every glyph here is one cell wide).
        let slash = cells
            .iter()
            .position(|(s, _, _)| s == "/")
            .map(|first| {
                // there are two '/'; take the last one
                cells
                    .iter()
                    .rposition(|(s, _, _)| s == "/")
                    .unwrap_or(first)
            })
            .unwrap();
        for (x, (sym, fg, _)) in cells.iter().enumerate() {
            if x > 4 && x < slash && !sym.trim().is_empty() {
                assert_eq!(*fg, MUTED, "dir char {sym:?} at x{x} muted");
            }
        }
        // The basename chars sit strictly between the last '/' and the LoC suffix.
        let count_x = cells.iter().position(|(s, _, _)| s == "+").unwrap();
        for (x, (sym, fg, mods)) in cells.iter().enumerate() {
            if x > slash && x < count_x && !sym.trim().is_empty() {
                assert_eq!(*fg, TEXT, "basename char {sym:?} at x{x} text");
                assert!(mods.contains(Modifier::BOLD), "basename char {sym:?} bold");
            }
        }
        let plus = cells.iter().position(|(s, _, _)| s == "+").unwrap();
        let minus = cells.iter().position(|(s, _, _)| s == "-").unwrap();
        assert_eq!(cells[plus].1, ADD_FG, "added LoC is green");
        assert_eq!(cells[minus].1, DEL_FG, "removed LoC is red");
    }

    #[test]
    fn files_pane_symbol_indent_and_owner_bg() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let snap = sample();
        let mut app = app_with(&snap);
        // Select the symbol row (flat index 1): its owning file keeps OWNER_BG.
        app.apply(crate::action::Action::Down);
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert_eq!(
            cell(&t, 5, 3).0,
            "~",
            "change glyph after the 4-cell indent"
        );
        assert_eq!(cell(&t, 5, 3).2, SELECTED_BG, "active symbol row bg");
        assert_eq!(
            cell(&t, 10, 2).2,
            OWNER_BG,
            "owning file row keeps OWNER_BG"
        );
    }

    // -- §3.4 / §7.8/§7.9/§7.10: diff pane --------------------------------------------

    #[test]
    fn hovered_node_links_exact_old_and_new_hunk_rows() {
        let diff = crate::snapshot::DiffPane {
            title: "src/main.rs".to_string(),
            focused_symbol: None,
            rows: vec![
                DiffRow::HunkHeader("@@ -10,2 +10,2 @@".to_string()),
                DiffRow::Del {
                    old_ln: 10,
                    text: "old value".to_string(),
                },
                DiffRow::Add {
                    new_ln: 10,
                    text: "new value".to_string(),
                },
                DiffRow::Context {
                    old_ln: 11,
                    new_ln: 11,
                    text: "context".to_string(),
                },
                DiffRow::HunkHeader("@@ -30,0 +30,1 @@".to_string()),
                DiffRow::Add {
                    new_ln: 30,
                    text: "finish".to_string(),
                },
            ],
            current_hunk: 1,
            total_hunks: 2,
        };
        let node =
            codescope_core::PlanNode::new("n1", "update", codescope_core::PlanNodeChange::Modified)
                .with_code_ref(codescope_core::PlanCodeRef::new(
                    codescope_core::FileId::new("src/main.rs").unwrap(),
                    0,
                    DiffSide::Old,
                    10,
                    11,
                ))
                .with_code_ref(codescope_core::PlanCodeRef::new(
                    codescope_core::FileId::new("src/main.rs").unwrap(),
                    1,
                    DiffSide::New,
                    30,
                    30,
                ));
        let linked = linked_diff_rows(&diff, Some(&node));
        assert_eq!(linked, vec![false, true, false, true, false, true]);

        let spans = intraline::row_spans(&diff.rows);
        let mut built = build_raw(&diff.rows, &spans, 2, 0, 40, 48);
        apply_linked_diff_style(&mut built, &linked);
        for index in [1usize, 3, 5] {
            assert!(built.lines[index]
                .spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::UNDERLINED)));
            assert!(built.lines[index]
                .spans
                .iter()
                .any(|span| span.style.bg == Some(CODE_LINK_BG)));
        }
        assert!(
            built.lines[1]
                .spans
                .iter()
                .any(|span| span.style.bg == Some(DEL_BG)),
            "deleted body retains its semantic background"
        );
        assert!(
            built.lines[5]
                .spans
                .iter()
                .any(|span| span.style.bg == Some(ADD_BG)),
            "added body retains its semantic background"
        );
        assert!(built.lines[2]
            .spans
            .iter()
            .all(|span| span.style.bg != Some(CODE_LINK_BG)));
    }

    #[test]
    fn diff_dual_gutter_blanks_on_the_absent_side() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        // Diff pane x starts at 43 (border at 42); ln_width = 4 (max ln 11 → 2 digits,
        // clamped to 4). Rows: header y=2, context y=3, del y=4, add y=5.
        // Context row y=3: both numbers present. The gutter starts at x=43 (block inner).
        let ctx: String = (43..54u16).map(|x| cell(&t, x, 3).0).collect();
        assert_eq!(ctx, "  10 │   10", "dual gutter: {ctx:?}");
        // Del row y=4: old number present, new side exactly ln_width blanks.
        let del_old: String = (43..47u16).map(|x| cell(&t, x, 4).0).collect();
        assert_eq!(del_old, "  11", "del old number: {del_old:?}");
        let del_new: String = (50..54u16).map(|x| cell(&t, x, 4).0).collect();
        assert_eq!(del_new, "    ", "del new side blank: {del_new:?}");
        assert_eq!(cell(&t, 48, 4).0, "│");
        assert_eq!(cell(&t, 55, 4).0, "-", "sign cell");
        // Add row y=5: old side blank, new number present.
        let add_old: String = (43..47u16).map(|x| cell(&t, x, 5).0).collect();
        assert_eq!(add_old, "    ", "add old side blank");
        let add_new: String = (50..54u16).map(|x| cell(&t, x, 5).0).collect();
        assert_eq!(add_new, "  11", "add new number");
        assert_eq!(cell(&t, 55, 5).0, "+");
    }

    #[test]
    fn diff_expands_source_tabs_in_raw_and_wrapped_modes() {
        for wrapped in [false, true] {
            let mut snap = sample();
            snap.diff.rows = vec![
                DiffRow::HunkHeader("@@ -1,0 +1,2 @@ func handler()".to_string()),
                DiffRow::Add {
                    new_ln: 1,
                    text: "\tif ready {".to_string(),
                },
                DiffRow::Add {
                    new_ln: 2,
                    text: "\t\treturn".to_string(),
                },
            ];
            let mut app = app_with(&snap);
            app.diff_wrap = wrapped;
            let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
            terminal.draw(|frame| render(frame, &app, &snap)).unwrap();

            // Diff inner x=43, 4-cell dual gutter + sign ends at x=55, body starts x=56.
            assert_eq!(cell(&terminal, 55, 3).0, "+");
            assert_eq!(
                (56..60)
                    .map(|x| cell(&terminal, x, 3).0)
                    .collect::<String>(),
                "    ",
                "first tab indentation, wrapped={wrapped}"
            );
            assert_eq!(cell(&terminal, 60, 3).0, "i");
            assert_eq!(
                (56..64)
                    .map(|x| cell(&terminal, x, 4).0)
                    .collect::<String>(),
                "        ",
                "nested tab indentation, wrapped={wrapped}"
            );
            assert_eq!(cell(&terminal, 64, 4).0, "r");
        }
    }

    #[test]
    fn diff_gutter_fixed_during_hscroll() {
        let mut snap = sample();
        // A line long enough to make hscroll non-trivial (longer than the 83-cell body).
        snap.diff.rows.push(DiffRow::Add {
            new_ln: 12,
            text: "let result = compute_something(with_some, arguments, that, make, this, line, quite, long, indeed);".to_string(),
        });
        let mut app = app_with(&snap);
        app.focused = Pane::Diff;
        // Raw mode is the default; scroll right by 8.
        app.apply(crate::action::Action::Expand);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &snap)).unwrap();
        // The gutter columns are unchanged at hscroll x+08.
        let del_old: String = (43..47u16).map(|x| cell(&t, x, 4).0).collect();
        assert_eq!(del_old, "  11", "gutter fixed under hscroll");
        assert_eq!(cell(&t, 48, 4).0, "│");
        assert_eq!(cell(&t, 55, 4).0, "-", "sign fixed under hscroll");
        // The title shows the raw offset.
        assert!(
            row_text(&t, 1).contains("x+08"),
            "title: {}",
            row_text(&t, 1)
        );
    }

    #[test]
    fn hunk_header_band_spans_the_full_inner_width() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        // The header band is row y=2; the diff block spans x 42..139, so the inner is
        // x 43..=138. EVERY interior cell carries the band style (padded to the right).
        for x in [43u16, 60, 100, 138] {
            let (_, _, bg, _) = cell(&t, x, 2);
            assert_eq!(bg, SURFACE_ALT, "x={x} band bg");
        }
        let (_, fg, _, mods) = cell(&t, 43, 2);
        assert_eq!(fg, HUNK_FG);
        assert!(mods.contains(Modifier::BOLD));
    }

    #[test]
    fn intraline_only_changed_words_brighten() {
        // `    return name` → `    return prefix + name` (paired): only the inserted
        // words get the bright style; the equal prefix keeps the restrained body.
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        // Del row y=4: nothing changes on the old side (pure deletion of nothing) —
        // `return` is equal, `name` moves; the del side has no inserted span.
        // Add row y=5: body starts at x=57 (gutter x=43, 14 cells: 2*4+5+1 → 43+14).
        let body_x = 57u16;
        // `    return ` is equal → restrained ADD body (fg TEXT, bg ADD_BG).
        let (_, fg, bg, mods) = cell(&t, body_x + 5, 5); // 'e' in "return"
        assert_eq!((fg, bg), (TEXT, ADD_BG), "equal words restrained");
        assert!(!mods.contains(Modifier::BOLD));
        // `prefix + ` was inserted → bright.
        let (_, fg, bg, mods) = cell(&t, body_x + 12, 5); // inside "prefix"
        assert_eq!((fg, bg), (ADD_HI, ADD_HI_BG), "changed words bright");
        assert!(mods.contains(Modifier::BOLD));
    }

    #[test]
    fn diff_title_basename_and_state() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let app = app_with(&sample());
        t.draw(|f| render(f, &app, &sample())).unwrap();
        let title = row_text(&t, 1);
        // Basename only (not the full path) on the left; hunk + wrap on the right.
        assert!(title.contains("service.go"), "{title}");
        assert!(
            !title.contains("internal/service"),
            "no full path in title: {title}"
        );
        assert!(title.contains("hunk 1/1"), "{title}");
        assert!(title.contains("wrap off"), "{title}");
    }

    #[test]
    fn diff_title_shows_focused_symbol_on_symbol_rows() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let mut snap = sample();
        snap.diff.focused_symbol = Some("GetDisplayName".to_string());
        let app = app_with(&snap);
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(
            row_text(&t, 1).contains("GetDisplayName · hunk 1/1"),
            "{}",
            row_text(&t, 1)
        );
        // File-row selection (no focused_symbol): no symbol in the title.
        snap.diff.focused_symbol = None;
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(
            !row_text(&t, 1).contains("GetDisplayName ·"),
            "no symbol: {}",
            row_text(&t, 1)
        );
    }

    // -- §3.5 / §7.11: impact pane ---------------------------------------------------

    #[test]
    fn impact_three_headers_always_present() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("SELECTED CHANGE"), "{text}");
        assert!(text.contains("CALLERS ·"), "{text}");
        assert!(text.contains("DOWNSTREAM ·"), "{text}");
    }

    /// An impact pane for a selected symbol with callers/downstream.
    fn impact_sample() -> crate::snapshot::ImpactPane {
        use crate::snapshot::{ImpactList, ImpactLoadState, ImpactRow, SelectedChange};
        crate::snapshot::ImpactPane {
            selected_change: Some(SelectedChange {
                file: "internal/service/service.go".to_string(),
                label: "GetDisplayName".to_string(),
                change: "modified",
                interpretation: "Modified implementation across 1 hunk.".to_string(),
                interpretation_source: crate::snapshot::InterpretationSource::Deterministic,
            }),
            callers: ImpactList {
                rows: vec![
                    ImpactRow {
                        label: "Handler.HandleGetUser".to_string(),
                        relation: "calls",
                        changed: false,
                        has_diagnostic: false,
                    },
                    ImpactRow {
                        label: "main".to_string(),
                        relation: "calls",
                        changed: false,
                        has_diagnostic: false,
                    },
                ],
                state: ImpactLoadState::Ready,
                partial: false,
            },
            downstream: ImpactList {
                rows: vec![ImpactRow {
                    label: "formatName".to_string(),
                    relation: "calls",
                    changed: false,
                    has_diagnostic: false,
                }],
                state: ImpactLoadState::Ready,
                partial: false,
            },
            note: String::new(),
        }
    }

    #[test]
    fn impact_shows_selected_change_and_counts() {
        let mut snap = sample();
        snap.impact = impact_sample();
        let mut app = App::new();
        app.dividers.set(DividerId::WorkReview, 16);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &snap)).unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("GetDisplayName"), "symbol label: {text}");
        assert!(text.contains("modified"), "badge: {text}");
        assert!(
            text.contains("Modified implementation across 1 hunk."),
            "deterministic interpretation: {text}"
        );
        assert!(text.contains("CALLERS · 2"), "callers count: {text}");
        assert!(text.contains("DOWNSTREAM · 1"), "downstream count: {text}");
        assert!(text.contains("Handler.HandleGetUser"), "caller row: {text}");
    }

    #[test]
    fn impact_loading_state_shows_ellipsis_not_zero() {
        let mut snap = sample();
        let mut impact = impact_sample();
        impact.callers.state = ImpactLoadState::Loading;
        impact.callers.rows.clear();
        snap.impact = impact;
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("CALLERS · …"), "loading shows …: {text}");
        assert!(!text.contains("CALLERS · 0"), "never a false zero: {text}");
    }

    #[test]
    fn impact_relationship_lists_render_their_independent_scroll_offsets() {
        let mut snap = sample();
        let mut impact = impact_sample();
        impact.callers.rows = (0..10)
            .map(|index| crate::snapshot::ImpactRow {
                label: format!("caller-{index}"),
                relation: "calls",
                changed: false,
                has_diagnostic: false,
            })
            .collect();
        snap.impact = impact;
        let mut app = App::new();
        app.callers_scroll = 3;
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &snap)).unwrap();
        let text = buffer_text(&t);
        let relationship_stack = (0..40)
            .map(|y| row_text(&t, y).chars().take(54).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("CALLERS · 10 · ↑3"), "scroll marker: {text}");
        assert!(
            relationship_stack.contains("caller-3"),
            "scrolled first row: {text}"
        );
        assert!(
            !relationship_stack.contains("caller-0"),
            "earlier row stays above viewport: {text}"
        );
    }

    #[test]
    fn impact_empty_selection_is_graceful() {
        let mut snap = sample();
        snap.impact = crate::snapshot::ImpactPane::default();
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("SELECTED CHANGE"), "header stays: {text}");
        assert!(
            text.contains("select a changed file or symbol"),
            "guidance: {text}"
        );
    }

    // -- generated half of the combined Impact pane --------------------------------------

    /// A snapshot whose semantic pane carries a validated, epoch-matched AI plan.
    /// `report` defaults to a clean `Valid`; tests swap in drops to exercise the
    /// sanitizer-warning line.
    fn ai_plan_snap(rows: usize) -> UiSnapshot {
        ai_plan_snap_with_report(rows, codescope_core::ValidationReport::valid())
    }

    fn ai_plan_snap_with_report(
        rows: usize,
        report: codescope_core::ValidationReport,
    ) -> UiSnapshot {
        use codescope_core::{FormKind, PlanEdge, PlanEdgeKind, PlanNode, PlanNodeChange, VizForm};

        let mut snap = sample();
        snap.epoch = codescope_core::Epoch(3);
        snap.ai = AiStatus::Ready {
            epoch: codescope_core::Epoch(3),
        };
        snap.semantic = crate::snapshot::SemanticPane {
            report: Some(report),
            plan: (rows > 0).then(|| {
                let mut plan = codescope_core::VisualizationPlan::new(codescope_core::Epoch(3));
                plan.intent = "Each request moves through the changed authentication path.".into();
                plan.forms.push(VizForm {
                    kind: FormKind::RelationshipFlow,
                    nodes: (0..rows)
                        .map(|i| {
                            PlanNode::new(
                                format!("n{i}"),
                                format!("PlanStep{i}"),
                                PlanNodeChange::Modified,
                            )
                            .with_detail(format!("explains effect {i}"))
                        })
                        .collect(),
                    edges: (1..rows)
                        .map(|i| PlanEdge {
                            from: format!("n{}", i - 1),
                            to: format!("n{i}"),
                            kind: PlanEdgeKind::Calls,
                            label: Some(format!("passes step {i}")),
                        })
                        .collect(),
                });
                plan
            }),
            note: String::new(),
            ai_generated: true,
        };
        snap
    }

    /// Drive the Loading → Ready edge so the generated half is populated.
    fn app_after_ai_landed(plan: &UiSnapshot) -> App {
        let mut app = App::new();
        let mut loading = sample();
        loading.epoch = codescope_core::Epoch(3);
        loading.ai = AiStatus::Loading {
            since_epoch: codescope_core::Epoch(3),
        };
        app.update(loading);
        app.update(plan.clone());
        app
    }

    #[test]
    fn ai_plan_renders_one_description_after_loading_to_ready() {
        let plan = ai_plan_snap(3);
        let app = app_after_ai_landed(&plan);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        // The outer border has no redundant section or retired tab title.
        assert!(
            !row_text(&t, 23).contains("Impact") && !row_text(&t, 23).contains("AI Plan"),
            "title-free border: {}",
            row_text(&t, 23)
        );
        // The fuller intent is the sole prose description; the redundant title is hidden.
        let header = row_text(&t, 24);
        assert!(
            header.contains("Each request moves through"),
            "description: {header}"
        );
        assert!(
            !buffer_text(&t).contains("Authentication request flow"),
            "redundant title is hidden"
        );
        assert!(
            !header.contains("AI-selected"),
            "opaque provenance: {header}"
        );
        assert_eq!(
            (24..38)
                .map(|y| row_text(&t, y))
                .collect::<String>()
                .matches("Each request moves through")
                .count(),
            1,
            "the description renders once"
        );
        // Rows carry explanation and visual grammar, never a metadata badge inventory.
        let generated: String = (24..38).map(|y| row_text(&t, y)).collect();
        assert!(generated.contains('┌'), "node box: {generated}");
        assert!(
            generated.contains("PlanStep0"),
            "selected node: {generated}"
        );
        assert!(
            generated.contains("explains effect"),
            "node detail: {generated}"
        );
        assert!(
            !generated.contains("diff modified"),
            "old badge: {generated}"
        );
        assert!(!generated.contains("LSP info"), "old badge: {generated}");
        assert!(
            !generated.contains("AI-selected"),
            "old legend: {generated}"
        );
        // The deterministic context stays visible beside generated Impact.
        let body: String = (24..38).map(|y| row_text(&t, y)).collect();
        assert!(body.contains("SELECTED CHANGE"), "impact context: {body}");
    }

    #[test]
    fn combined_impact_has_no_section_title_and_keeps_both_halves() {
        let plan = ai_plan_snap(2);
        let mut app = App::new();
        app.update(plan.clone());
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        assert!(!row_text(&t, 22).contains("Impact"));
        let text = buffer_text(&t);
        assert!(!text.contains("AI Plan"), "retired tab name: {text}");
        assert!(text.contains("SELECTED CHANGE"), "left half: {text}");
        assert!(text.contains("PlanStep0"), "right half: {text}");
    }

    #[test]
    fn generated_impact_shows_progress_while_loading() {
        let mut snap = sample();
        snap.ai = AiStatus::Loading {
            since_epoch: codescope_core::Epoch(3),
        };
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert!(
            buffer_text(&t).contains("Generating a deeper explanation…"),
            "loading state: {}",
            buffer_text(&t)
        );
    }

    #[test]
    fn generated_impact_distinguishes_prerequisites_and_queue_position() {
        let cases = [
            (
                AiStatus::WaitingForSymbols {
                    epoch: codescope_core::Epoch(3),
                },
                "Waiting for symbol analysis…",
            ),
            (
                AiStatus::WaitingForRelations {
                    epoch: codescope_core::Epoch(3),
                },
                "Waiting for symbol relationships…",
            ),
            (
                AiStatus::Queued {
                    epoch: codescope_core::Epoch(3),
                    position: 4,
                },
                "Waiting for AI capacity · priority #4",
            ),
        ];
        for (status, expected) in cases {
            let mut snap = sample();
            snap.ai = status;
            let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
            t.draw(|f| render(f, &App::new(), &snap)).unwrap();
            assert!(
                buffer_text(&t).contains(expected),
                "missing {expected:?}: {}",
                buffer_text(&t)
            );
        }
    }

    #[test]
    fn ai_plan_view_explains_empty_or_unavailable_states() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        // A validated-but-empty plan falls back to deterministic selection guidance.
        let mut app = App::new();
        let empty = ai_plan_snap(0);
        app.update(empty.clone());
        t.draw(|f| render(f, &app, &empty)).unwrap();
        assert!(
            buffer_text(&t).contains("Select a changed file or symbol"),
            "empty generated impact: {}",
            buffer_text(&t)
        );
        // AI off: unavailable. (update() flips the view back; toggle after it.)
        let mut snap = sample(); // ai: Disabled, semantic: default
        let mut app = App::new();
        app.update(snap.clone());
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(
            buffer_text(&t).contains("Known relationships · AI not configured"),
            "disabled: {}",
            buffer_text(&t)
        );
        // A stale publish carries the dispatcher's note.
        snap.ai = AiStatus::Stale {
            epoch: codescope_core::Epoch(2),
        };
        snap.semantic.note = "AI view stale (repo changed); regenerating…".to_string();
        let mut app = App::new();
        app.update(snap.clone());
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(
            buffer_text(&t).contains("Repository changed; showing known relationships"),
            "stale note: {}",
            buffer_text(&t)
        );
    }

    /// A sanitized plan (ValidWithDrops) gets exactly one WARN line before the diagram;
    /// the drop reasons stay out of the small pane (full detail lives in debug-ai JSON).
    #[test]
    fn sanitized_plan_warns_once_before_the_diagram() {
        let report = codescope_core::ValidationReport::with_drops(vec![
            codescope_core::DroppedItem {
                subject: "edge n3 -> n1 in form 0".to_string(),
                reason: "nonconsecutive or duplicate sequence edge".to_string(),
            },
            codescope_core::DroppedItem {
                subject: "node n9 in form 0".to_string(),
                reason: "entity does not resolve".to_string(),
            },
        ]);
        let plan = ai_plan_snap_with_report(3, report);
        let app = app_after_ai_landed(&plan);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let text = buffer_text(&t);
        assert_eq!(
            text.matches("sanitized AI plan").count(),
            1,
            "exactly one warning line: {text}"
        );
        assert!(
            text.contains("sanitized AI plan · 2 items removed"),
            "concise count line (plural): {text}"
        );
        assert!(
            !text.contains("entity does not resolve")
                && !text.contains("nonconsecutive or duplicate sequence edge"),
            "drop reasons stay out of the small pane: {text}"
        );
        // The warning precedes the plan's sole description, and uses WARN styling.
        let warn_row = (24..38u16)
            .find(|y| row_text(&t, *y).contains("sanitized AI plan"))
            .expect("warning visible in the default viewport");
        let description_row = (24..38u16)
            .find(|y| row_text(&t, *y).contains("Each request moves through"))
            .expect("plan description visible");
        assert!(
            warn_row < description_row,
            "warning precedes the plan: {text}"
        );
        let mut styled = false;
        for x in 0..140u16 {
            let (symbol, fg, _, _) = cell(&t, x, warn_row);
            if symbol == "⚠" && fg == WARN {
                styled = true;
            }
        }
        assert!(styled, "the warning uses WARN styling");
    }

    /// A clean report (Valid, zero drops) adds no warning line; neither does a fallback
    /// pane (no AI plan) nor a report-less semantic pane.
    #[test]
    fn clean_report_adds_no_warning_line() {
        // Valid with zero drops: no line anywhere.
        let plan = ai_plan_snap(3);
        let app = app_after_ai_landed(&plan);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        assert!(
            !buffer_text(&t).contains("sanitized AI plan"),
            "a clean report must not warn: {}",
            buffer_text(&t)
        );
        // Fallback pane (deterministic relationships): the slot stays warning-free even
        // if a stale report were left behind.
        let mut snap = sample();
        snap.semantic.report = Some(codescope_core::ValidationReport::with_drops(vec![
            codescope_core::DroppedItem {
                subject: "node n1 in form 0".to_string(),
                reason: "entity does not resolve".to_string(),
            },
        ]));
        let app = App::new();
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(
            !buffer_text(&t).contains("sanitized AI plan"),
            "fallback panes never carry the AI sanitizer warning: {}",
            buffer_text(&t)
        );
    }

    /// The real failure shape: a validated 7-step sequence must show its first steps in
    /// the default Impact pane. The old five-row boxes showed no diagram at all.
    #[test]
    fn default_impact_pane_shows_ladder_steps() {
        let mut plan = ai_plan_snap(7);
        {
            let viz = plan.semantic.plan.as_mut().unwrap();
            viz.intent = "Stop new traffic before waiting for in-flight requests.".into();
            let form = &mut viz.forms[0];
            form.kind = codescope_core::FormKind::Sequence;
            for (index, node) in form.nodes.iter_mut().enumerate() {
                node.label = format!("Step{index}");
                node.detail = Some(format!("explains effect {index}"));
            }
            form.edges[0].label = Some("SIGTERM/SIGINT triggers shutdown, unblocks waiters".into());
            for (index, edge) in form.edges.iter_mut().enumerate().skip(1) {
                edge.label = Some(format!("passes step {index}"));
            }
        }
        let mut app = App::new();
        app.update(plan.clone());
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let pane: String = (24..38).map(|y| format!("{}\n", row_text(&t, y))).collect();
        assert!(
            pane.contains("Stop new traffic before waiting for in-flight requests."),
            "description: {pane}"
        );
        assert!(!pane.contains("API graceful drain"), "title hidden: {pane}");
        assert!(
            !pane.contains("inferred from cited diff"),
            "no legend: {pane}"
        );
        assert!(
            pane.contains(" 1  Step0"),
            "the first step is visible without scrolling: {pane}"
        );
        assert!(
            pane.contains(" 2  Step1"),
            "the second step is visible without scrolling: {pane}"
        );
        assert!(
            pane.contains("SIGTERM/SIGINT triggers shutdown, unblocks waiters"),
            "the causal label keeps the full pane width: {pane}"
        );
        // Terra P1 fix: the raised default height (12) gives the generated half ten
        // inner rows, so after the header block the viewport reaches materially into
        // the ladder — at least through step 3.
        assert!(
            pane.contains(" 3  Step2"),
            "the default viewport reaches at least step 3: {pane}"
        );

        // With the retired summary/status rows gone, 80x20 gives Impact 11 rows while
        // still preserving the seven-row work minimum.
        let mut t80 = Terminal::new(TestBackend::new(80, 20)).unwrap();
        t80.draw(|f| render(f, &App::new(), &plan)).unwrap();
        assert!(
            crate::layout::impact_height(
                crate::divider::DividerSizes::default().get(DividerId::WorkReview),
                20
            ) == 11,
            "80x20 gives Impact the reclaimed chrome rows"
        );
    }

    /// Node highlighting is wired to the actual selected change: a plan node whose label
    /// matches `impact.selected_change.label` renders with the selection style. The old
    /// wiring passed the dispatcher's data-quality note, which could never match.
    #[test]
    fn selected_node_label_is_highlighted() {
        let mut plan = ai_plan_snap(5);
        plan.semantic.plan.as_mut().unwrap().forms[0].nodes[2].label = "WiredTarget".into();
        plan.impact.selected_change = Some(crate::snapshot::SelectedChange {
            file: "internal/service/service.go".to_string(),
            label: "WiredTarget".to_string(),
            change: "modified",
            interpretation: "Signature changed.".to_string(),
            interpretation_source: Default::default(),
        });
        let mut app = App::new();
        app.update(plan.clone());
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        // Five steps cannot fit side by side: the ladder renders, step 3 at the last row
        // of the default pane.
        let mut highlighted = false;
        for y in 24..38u16 {
            let row = row_text(&t, y);
            if !row.contains("WiredTarget") {
                continue;
            }
            for x in 0..140u16 {
                let (symbol, fg, _, modifier) = cell(&t, x, y);
                if symbol == "W" && fg == ACCENT && modifier.contains(Modifier::BOLD) {
                    highlighted = true;
                }
            }
        }
        assert!(
            highlighted,
            "the matching node uses the selection style: {}",
            (24..38).map(|y| row_text(&t, y)).collect::<String>()
        );
    }

    /// At a 40-column focus-only size no box border is ever clipped: every top border
    /// that opens also closes within the pane (the old clamp forced 18-cell boxes into a
    /// 17-cell half).
    #[test]
    fn narrow_focus_renders_without_clipped_borders() {
        let plan = ai_plan_snap(3);
        let mut app = App::new();
        app.update(plan.clone());
        app.apply(crate::action::Action::Focus(Pane::Impact));
        let mut t = Terminal::new(TestBackend::new(40, 24)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let buf = t.backend().buffer();
        for y in 0..24u16 {
            let mut opens = 0usize;
            let mut closes = 0usize;
            let mut bottom_opens = 0usize;
            let mut bottom_closes = 0usize;
            for x in 0..40u16 {
                match buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ") {
                    "┌" => opens += 1,
                    "┐" => closes += 1,
                    "└" => bottom_opens += 1,
                    "┘" => bottom_closes += 1,
                    _ => {}
                }
            }
            assert_eq!(
                opens,
                closes,
                "clipped top border at row {y}: {}",
                row_text(&t, y)
            );
            assert_eq!(
                bottom_opens,
                bottom_closes,
                "clipped bottom border at row {y}: {}",
                row_text(&t, y)
            );
        }
        let text = buffer_text(&t);
        assert!(text.contains("PlanStep0"), "steps survive at 40x24: {text}");
    }

    #[test]
    fn ai_plan_scrolls_over_physical_diagram_lines() {
        let plan = ai_plan_snap(8);
        let app = app_after_ai_landed(&plan);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let initial: String = (24..38).map(|y| row_text(&t, y)).collect();
        assert!(
            initial.contains("Each request moves through the changed authentication path."),
            "description: {initial}"
        );
        assert!(initial.contains("PlanStep0"), "first step: {initial}");
        assert!(
            !initial.contains("inferred from cited diff"),
            "no legend: {initial}"
        );
        // Scrolling moves over physical ladder lines computed for this width: one line
        // per step, one per edge rail.
        let mut app = app_after_ai_landed(&plan);
        app.apply(crate::action::Action::Focus(Pane::Impact));
        for _ in 0..6 {
            app.apply(crate::action::Action::Down);
        }
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let moved: String = (24..38).map(|y| row_text(&t, y)).collect();
        assert!(
            !moved.contains("Each request moves through the changed authentication path."),
            "description scrolled: {moved}"
        );
        assert!(
            !moved.contains("PlanStep0"),
            "the first step scrolled out: {moved}"
        );
        assert!(
            moved.contains("PlanStep4"),
            "later steps enter view: {moved}"
        );
        app.apply(crate::action::Action::Bottom);
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let bottom: String = (24..38).map(|y| row_text(&t, y)).collect();
        assert!(bottom.contains("PlanStep7"), "last step: {bottom}");
        assert!(
            bottom.contains(" 1  PlanStep") || bottom.contains("PlanStep6"),
            "the taller default pane reaches deep into the ladder: {bottom}"
        );
        assert!(
            !bottom.contains("cited diff"),
            "the note stays pinned to the top of the ladder: {bottom}"
        );
    }

    #[test]
    fn zoomed_ai_plan_renders_rows_full_area() {
        let plan = ai_plan_snap(3);
        let mut app = app_after_ai_landed(&plan);
        app.apply(crate::action::Action::Focus(Pane::Impact));
        app.apply(crate::action::Action::ToggleZoom);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let text = buffer_text(&t);
        assert!(!text.contains("Impact"), "section title removed: {text}");
        assert!(!text.contains("AI Plan"), "retired tab absent: {text}");
        assert!(text.contains("ZOOM"), "zoom tag: {text}");
        assert!(text.contains("PlanStep0"), "rows in zoom: {text}");
    }

    #[test]
    fn ai_plan_renders_at_narrow_sizes_without_panic() {
        let plan = ai_plan_snap(3);
        // 90x20: normal tier, bottom pane visible.
        let app = app_after_ai_landed(&plan);
        let mut t = Terminal::new(TestBackend::new(90, 20)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        assert!(buffer_text(&t).contains("PlanStep0"), "90x20 rows");
        // 30x8: focus-only minimum with the bottom pane focused.
        let mut app = app_after_ai_landed(&plan);
        app.apply(crate::action::Action::Focus(Pane::Impact));
        let mut t = Terminal::new(TestBackend::new(30, 8)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        // 79x40: focus-only fallback keeps the same combined layout.
        let mut app = app_after_ai_landed(&plan);
        app.apply(crate::action::Action::Focus(Pane::Impact));
        let mut t = Terminal::new(TestBackend::new(79, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let text = buffer_text(&t);
        assert!(!text.contains("Impact"), "79x40 title removed: {text}");
        assert!(!text.contains("AI Plan"), "79x40 retired tab: {text}");
        assert!(text.contains("PlanStep0"), "79x40 rows: {text}");
    }

    #[test]
    fn generated_impact_wraps_long_explanations_instead_of_truncating_them() {
        // The intent is capped at two lines: a length that wraps into exactly two lines
        // must be shown complete, not clipped or elided.
        let mut plan = ai_plan_snap(1);
        plan.semantic.plan.as_mut().unwrap().intent =
            "The readiness endpoint returns unavailable before shutdown so new traffic stops"
                .to_string();
        let app = app_after_ai_landed(&plan);
        let mut t = Terminal::new(TestBackend::new(100, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let body: String = (24..38).map(|y| format!("{}\n", row_text(&t, y))).collect();
        assert!(
            body.contains("The readiness endpoint"),
            "first line: {body}"
        );
        assert!(body.contains("before shutdown"), "wrapped line: {body}");
        assert!(
            body.contains("new traffic stops"),
            "untruncated ending: {body}"
        );
        assert!(!body.contains('…'), "long content was not elided: {body}");
    }

    #[test]
    fn combined_impact_renders_relationship_stack_and_generated_breakdown() {
        let mut snap = sample();
        snap.impact = impact_sample();
        snap.ai = AiStatus::Ready {
            epoch: codescope_core::Epoch(3),
        };
        snap.semantic = ai_plan_snap(3).semantic;
        let mut app = App::new();
        app.dividers.set(DividerId::WorkReview, 16);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &snap)).unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("SELECTED CHANGE"), "{text}");
        assert!(text.contains("CALLERS ·"), "{text}");
        assert!(text.contains("DOWNSTREAM ·"), "{text}");
        assert!(text.contains("PlanStep0"), "generated half: {text}");
        assert!(!text.contains("AI Plan"), "retired tab: {text}");
    }

    // -- §3.6 / §3.7: status + help bars ------------------------------------------------

    #[test]
    fn bottom_bar_right_justifies_tokens_and_path() {
        let mut snap = sample();
        snap.ai_tokens.input = 1_250;
        snap.ai_tokens.output = 42;
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&snap), &snap)).unwrap();
        let status = row_text(&t, 39);
        assert!(
            status.contains("internal/service/service.go"),
            "full path: {status}"
        );
        assert!(status.contains("tokens in 1.2k out 42"), "usage: {status}");
        assert!(
            status.trim_end().ends_with("internal/service/service.go"),
            "path is final and right-aligned: {status:?}"
        );
        let path_x = status.find("internal/service/service.go").unwrap() as u16;
        assert_eq!(cell(&t, path_x, 39).1, MUTED, "path muted");
    }

    #[test]
    fn status_bar_colors_messages_by_severity() {
        let mut snap = sample();
        snap.status = crate::snapshot::StatusMessage {
            text: "AI timed out after 20s · m change model · retries automatically".to_string(),
            detail: None,
            level: crate::snapshot::StatusLevel::Error,
        };
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert_eq!(cell(&t, 2, 39).1, ERROR, "AI failure is an error");
        assert!(
            row_text(&t, 39).contains("AI timed out after 20s"),
            "actionable text"
        );
        snap.status = crate::snapshot::StatusMessage {
            text: "base: main".to_string(),
            detail: None,
            level: crate::snapshot::StatusLevel::Info,
        };
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert_eq!(cell(&t, 2, 39).1, MUTED, "info is muted");
    }

    #[test]
    fn status_bar_warning_level() {
        let mut snap = sample();
        snap.status = crate::snapshot::StatusMessage {
            text: "git-only (no supported language detected)".to_string(),
            detail: None,
            level: crate::snapshot::StatusLevel::Warning,
        };
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert_eq!(cell(&t, 2, 39).1, WARN, "warning");
    }

    #[test]
    fn clicked_status_detail_wraps_and_shows_the_untruncated_tail() {
        let mut snap = sample();
        let tail = "TAIL_OF_PROVIDER_RESPONSE";
        snap.status = crate::snapshot::StatusMessage {
            text: "AI provider returned HTTP 400…".to_string(),
            detail: Some(format!(
                "AI provider returned HTTP 400: {{\"error\":{{\"message\":\"{} {tail}\"}}}}",
                "unsupported provider parameter ".repeat(8)
            )),
            level: crate::snapshot::StatusLevel::Warning,
        };
        let mut app = App::new();
        app.update(snap.clone());
        app.apply(crate::action::Action::ToggleStatusDetail);

        let mut t = Terminal::new(TestBackend::new(80, 20)).unwrap();
        t.draw(|f| render(f, &app, &snap)).unwrap();
        let text = buffer_text(&t);

        assert!(text.contains("status details"), "overlay title: {text}");
        assert!(
            text.contains("AI provider returned HTTP 400"),
            "message prefix: {text}"
        );
        assert!(text.contains(tail), "untruncated tail: {text}");
        assert!(text.contains("click or Esc to close"), "close hint: {text}");
    }

    #[test]
    fn status_detail_dialog_is_content_sized_and_width_capped() {
        let area = Rect::new(0, 0, 200, 40);
        let short = status_detail_rect(area, "brief provider error");
        assert_eq!(
            short.width, 120,
            "wide terminals keep a readable line length"
        );
        assert_eq!(
            short.height, 5,
            "short errors use the minimum dialog height"
        );
        assert!(short.width < area.width && short.height < area.height);

        let long = status_detail_rect(area, &"provider response detail ".repeat(40));
        assert!(long.height > short.height, "content grows the dialog");
        assert!(long.height < area.height.saturating_sub(4));
        assert_eq!(wrapped_line_count("abcdefghij", 4), 3);
    }

    #[test]
    fn help_bar_compacts_with_width() {
        let snap = sample();
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert!(
            row_text(&t, 39).contains("drag resize"),
            "full: {}",
            row_text(&t, 39)
        );
        // Focus-only at 79 wide: manual refresh remains visible while mouse guidance drops.
        let mut t = Terminal::new(TestBackend::new(79, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        let help = row_text(&t, 39);
        assert!(
            help.contains("R refresh"),
            "manual mode is discoverable: {help}"
        );
        assert!(!help.contains("resize"), "resize dropped: {help}");
    }

    #[test]
    fn zoom_keeps_all_chrome_rows() {
        let mut app = App::new();
        app.apply(crate::action::Action::ToggleZoom);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &sample())).unwrap();
        assert!(row_text(&t, 0).contains("codescope"), "top survives zoom");
        assert!(
            row_text(&t, 0).contains("1 file"),
            "file count survives zoom"
        );
        assert!(row_text(&t, 39).contains("? help"), "help survives zoom");
        assert!(buffer_text(&t).contains("· ZOOM"), "zoom tag visible");
    }

    #[test]
    fn width_sweep_never_panics() {
        let mut w = 20u16;
        while w <= 200 {
            let mut h = 6u16;
            while h <= 40 {
                let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
                t.draw(|f| render(f, &app_with(&sample()), &sample()))
                    .unwrap();
                h += 3;
            }
            w += 7;
        }
    }

    #[test]
    fn help_modal_covers_screen() {
        let mut app = App::new();
        app.show_help = true;
        let mut t = Terminal::new(TestBackend::new(160, 40)).unwrap();
        t.draw(|f| render(f, &app, &sample())).unwrap();
        assert!(buffer_text(&t).contains("codescope — controls"));
    }

    #[test]
    fn empty_state_is_graceful() {
        let mut t = Terminal::new(TestBackend::new(160, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &UiSnapshot::placeholder()))
            .unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("no changes") || text.contains("scanning repository"));
    }

    // -- picker modals (unchanged behavior) ---------------------------------------------

    #[test]
    fn base_picker_renders_candidates_and_current() {
        let mut t = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut app = App::new();
        app.show_base_picker = true;
        t.draw(|f| render(f, &app, &snap_with_base())).unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("comparison base"), "picker title: {text}");
        assert!(
            text.contains("selected: release/2.0"),
            "picker names the selected base explicitly: {text}"
        );
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
            "non-matching entry filtered: {text}"
        );
    }

    #[test]
    fn base_picker_labels_no_selection_explicitly() {
        let mut t = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut app = App::new();
        app.show_base_picker = true;
        let mut snap = snap_with_base();
        snap.base_ref.clear();
        snap.repo.base = None;
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(buffer_text(&t).contains("selected: none"));
    }

    #[test]
    fn base_picker_marks_a_bounded_candidate_scan() {
        let mut t = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut app = App::new();
        app.show_base_picker = true;
        let mut snap = snap_with_base();
        snap.base_candidates_truncated = true;
        t.draw(|frame| render(frame, &app, &snap)).unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("bounded list"), "picker title: {text}");
        assert!(
            text.contains("more ancestors omitted"),
            "picker note: {text}"
        );
    }

    /// The files pane renders each semantic load state distinctly (asynchronous per-file
    /// analysis): Unloaded shows no fake zero, Loading shows the analyzing marker,
    /// Unsupported/Failed explain themselves, Ready-with-zero says so explicitly.
    #[test]
    fn files_pane_semantic_states_render_distinctly() {
        use crate::snapshot::FileSemanticLoad as L;
        let mut snap = sample();
        let mk = |path: &str, expanded: bool, semantic: L, symbols: Vec<SymbolRow>| FileRow {
            path: path.to_string(),
            status: "M",
            changed_symbol_count: symbols.len(),
            added_lines: 0,
            removed_lines: 0,
            symbols,
            expanded,
            semantic,
        };
        let sym = SymbolRow {
            name: "Handle".to_string(),
            change: "modified",
            confidence: "",
            has_diagnostic: false,
            position: Some((10, 4)),
        };
        snap.files = vec![
            mk("a_unloaded.go", false, L::Unloaded, vec![]),
            mk("b_loading.go", true, L::Loading, vec![]),
            mk("c_unsupported.go", true, L::Unsupported, vec![]),
            mk("d_failed.go", true, L::Failed, vec![]),
            mk("e_empty.go", true, L::Ready, vec![]),
            mk("f_ready.go", true, L::Ready, vec![sym]),
        ];
        let app = app_with(&snap);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &snap)).unwrap();
        let text = buffer_text(&t);
        assert!(
            text.contains("… analyzing symbols"),
            "loading row: {text:?}"
        );
        assert!(
            text.contains("semantic analysis unavailable"),
            "unsupported: {text:?}"
        );
        assert!(
            text.contains("analysis failed — retries after"),
            "failed: {text:?}"
        );
        assert!(
            text.contains("no changed symbols mapped"),
            "ready-empty: {text:?}"
        );
        assert!(text.contains("Handle"), "ready symbols: {text:?}");
        // The unloaded row must not claim `0` symbols (unknown): find its pane row and
        // assert the count cell (right-aligned before the border) is blank, not a digit.
        let buf = t.backend().buffer();
        let a_y = (0..40u16)
            .find(|&y| {
                (0..42u16)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
                    .contains("a_unloaded.go")
            })
            .expect("a_unloaded row rendered");
        let count_cell = buf.cell((38, a_y)).unwrap().symbol();
        assert!(
            count_cell.trim().is_empty() || count_cell == "…",
            "no fake zero on unloaded (count cell: {count_cell:?})"
        );
    }

    /// Tab on the files pane flips the selected file's expansion optimistically (the
    /// dispatcher reconciles); on other panes it is inert. `1`/`2`/`3` focus panes.
    #[test]
    fn tab_toggles_the_selected_file_not_pane_focus() {
        let mut app = app_with(&sample());
        assert_eq!(app.focused, Pane::Files);
        let path = app.snapshot.files[0].path.clone();
        let before = app.focused;
        // The targeted command: optimistic apply flips expansion without moving focus.
        app.apply(crate::action::Action::SetFileExpanded {
            path: path.clone(),
            expanded: false,
        });
        assert_eq!(app.focused, before, "Tab never changes focus");
        assert!(!app.snapshot.files[0].expanded, "toggled off");
        app.apply(crate::action::Action::SetFileExpanded {
            path: path.clone(),
            expanded: true,
        });
        assert!(app.snapshot.files[0].expanded, "toggled back on");
        // Idempotent: re-applying the same target state is a no-op.
        app.apply(crate::action::Action::SetFileExpanded {
            path,
            expanded: true,
        });
        assert!(app.snapshot.files[0].expanded);
    }
}
