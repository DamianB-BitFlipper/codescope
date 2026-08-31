//! The renderer: draws a [`UiSnapshot`] + [`App`] state into a ratatui frame.
//!
//! Pure with respect to I/O — `render` only touches the frame buffer, so it is fully
//! testable with ratatui's `TestBackend`. Layout is recomputed from the frame area every
//! pass (resize needs no stored state). The pane arrangement is the reference
//! master-detail layout of docs/review/15 §1: one normal tier (top, summary, files+diff,
//! full-width Impact, status, help) and a focus-only fallback below 80x20 or when
//! zoomed. All colors come from the §2 palette below — never a bare `Color::Green`.

use codescope_core::{AiStatus, ChangeScope, LsStatus};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Padding, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{filter_candidates, App, BottomView, Pane};
use crate::elide;
use crate::intraline;
use crate::layout::{choose_tier, files_width, Tier, MIN_DIFF_WIDTH};
use crate::snapshot::{DiffRow, ImpactList, ImpactLoadState, StatusLevel, UiSnapshot};

// -- palette (docs/review/15 §2) ----------------------------------------------
//
// One palette for the whole interface. `Modifier::BOLD` only for the product name,
// selected/basename labels, column headings, and intraline changed spans. `REVERSED`
// is never used: it would destroy the deliberate red/green diff palette.

/// Top, status, and help bar background.
pub(crate) const SURFACE: Color = Color::Rgb(24, 27, 32);
/// Summary bar and hunk-header band background.
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
            // docs/review/15 §1.1: top, summary, work (surplus), Impact, status, help.
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(7),
                Constraint::Length(crate::layout::impact_height(app.impact_height, area.height)),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
            render_top_bar(frame, rows[0], snap);
            render_summary_bar(frame, rows[1], app, snap);
            let fw = files_width(app.files_width, rows[2].width);
            let work =
                Layout::horizontal([Constraint::Length(fw), Constraint::Min(MIN_DIFF_WIDTH)])
                    .split(rows[2]);
            render_files(frame, work[0], app, snap);
            render_diff(frame, work[1], app, snap);
            render_impact(frame, rows[3], app, snap);
            render_status_bar(frame, rows[4], app, snap);
            render_help_bar(frame, rows[5]);
        }
        Tier::FocusOnly => {
            // docs/review/15 §1.2: keep the chrome, render only the focused pane. The
            // help row is the first luxury dropped (heights 8..=11).
            if area.height >= 12 {
                let rows = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(3),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(area);
                render_top_bar(frame, rows[0], snap);
                render_summary_bar(frame, rows[1], app, snap);
                render_focused(frame, rows[2], app, snap);
                render_status_bar(frame, rows[3], app, snap);
                render_help_bar(frame, rows[4]);
            } else {
                let rows = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(3),
                    Constraint::Length(1),
                ])
                .split(area);
                render_top_bar(frame, rows[0], snap);
                render_summary_bar(frame, rows[1], app, snap);
                render_focused(frame, rows[2], app, snap);
                render_status_bar(frame, rows[3], app, snap);
            }
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

/// The top repository/service bar: `codescope  {repo}  {branch} ◂ {base}` on the left,
/// `{scope}  LSP {glyph}  AI {glyph}[ {provider}]` reserved on the right (plus `  ⟳`
/// while refreshing). The right group is measured and reserved FIRST; the left group is
/// then elided into what remains, dropping base, branch, repo, and product in that
/// order. Service failures and the refresh state are never clipped.
fn render_top_bar(frame: &mut Frame, area: Rect, snap: &UiSnapshot) {
    let r = &snap.repo;
    // The comparison base: `base_ref` is authoritative (dispatcher-owned; reflects a
    // picker override); fall back to the repo-bar base for snapshots that never set it.
    let base = if snap.base_ref.is_empty() {
        r.base.as_deref().unwrap_or("?")
    } else {
        snap.base_ref.as_str()
    };

    // -- right group: scope + service glyphs (+ provider) + spinner -----------------
    let (ls_g, ls_style) = ls_status_glyph(snap.ls);
    let (ai_g, ai_style) = ai_status_glyph(&snap.ai);
    let provider = if snap.ai_provider.is_empty() {
        String::new()
    } else {
        format!(" {}", snap.ai_provider)
    };
    let mut right: Vec<Span> = vec![
        Span::styled(scope_label(snap.scope), Style::new().fg(MUTED)),
        Span::raw("  "),
        Span::styled("LSP ", Style::new().fg(MUTED)),
        Span::styled(ls_g, ls_style),
        Span::raw("  "),
        Span::styled("AI ", Style::new().fg(MUTED)),
        Span::styled(ai_g, ai_style),
        Span::styled(provider, Style::new().fg(MUTED)),
    ];
    if snap.refreshing {
        right.push(Span::styled("  ⟳", Style::new().fg(WARN)));
    }
    right.push(Span::raw(" "));
    let right_w: usize = right.iter().map(Span::width).sum();

    // -- left group: drop base, then branch, then repo, then product -----------------
    let product = Span::styled(
        " codescope ",
        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
    );
    let repo = Span::styled(format!(" {} ", r.repo_name), Style::new().fg(ACCENT));
    let branch = Span::styled(format!(" {} ", r.branch), Style::new().fg(TEXT));
    let base_span = Span::styled(format!("◂ {} ", base), Style::new().fg(MUTED));

    let reserved = right_w.min(area.width as usize) as u16;
    let chunks = Layout::horizontal([Constraint::Min(1), Constraint::Length(reserved)]).split(area);
    let budget = chunks[0].width as usize;

    let mut left: Vec<Span> = Vec::new();
    for candidate in [
        vec![
            product.clone(),
            repo.clone(),
            branch.clone(),
            base_span.clone(),
        ],
        vec![product.clone(), repo.clone(), branch.clone()],
        vec![product.clone(), repo.clone()],
        vec![product.clone()],
    ] {
        let w: usize = candidate.iter().map(Span::width).sum();
        if w <= budget {
            left = candidate;
            break;
        }
    }
    if left.is_empty() {
        // Even the product alone is too wide: truncate the plain text grapheme-safely.
        frame.render_widget(
            Paragraph::new(truncate_cells(" codescope", budget)).style(Style::new().bg(SURFACE)),
            chunks[0],
        );
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(left)).style(Style::new().bg(SURFACE)),
            chunks[0],
        );
    }
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

// -- summary bar (docs/review/15 §3.2) ------------------------------------------

/// The one-line summary: `{N} changed files · {M} symbols in {basename} · hunk {c} / {t}`.
/// The symbol count belongs to the file shown in the diff (matched by path, never by a
/// transient list index); the hunk phrase uses the same App-owned `current_hunk` as the
/// diff title. At constrained widths the file phrase is elided first; the count and
/// hunk phrases survive.
fn render_summary_bar(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let text = summary_text(app, snap, area.width as usize);
    frame.render_widget(
        Paragraph::new(text).style(Style::new().fg(TEXT).bg(SURFACE_ALT)),
        area,
    );
}

/// The summary text, budgeted (pure; tested directly). Always one leading space.
fn summary_text(app: &App, snap: &UiSnapshot, width: usize) -> String {
    let n = snap.files.len();
    let files = if n == 1 {
        "1 changed file".to_string()
    } else {
        format!("{n} changed files")
    };
    if n == 0 {
        return truncate_cells(&format!(" {files} · no selection"), width.saturating_sub(1));
    }

    // The file shown in the diff: match the snapshot's diff path against the file rows.
    // (`DiffPane::title` is the full path today; MERGE: the dispatcher half renames it
    // to `file_path` — same comparison.)
    let diff_path = snap.diff.title.as_str();
    let diff_file = snap.files.iter().find(|f| f.path == diff_path);

    let file_phrase = diff_file.map(|f| {
        // The symbol count is real only once the file's lazy analysis landed; an
        // unanalyzed file must not claim `0 symbols`.
        let sym = match f.semantic {
            crate::snapshot::FileSemanticLoad::Ready => {
                let count = f.changed_symbol_count;
                if count == 1 {
                    "1 symbol".to_string()
                } else {
                    format!("{count} symbols")
                }
            }
            crate::snapshot::FileSemanticLoad::Loading => "analyzing…".to_string(),
            crate::snapshot::FileSemanticLoad::Unsupported => "semantics unavailable".to_string(),
            crate::snapshot::FileSemanticLoad::Failed => "analysis failed".to_string(),
            crate::snapshot::FileSemanticLoad::Unloaded => "not analyzed".to_string(),
        };
        format!("{sym} in {}", basename(&f.path))
    });

    let hunk_phrase = if snap.diff.total_hunks > 0 {
        Some(format!(
            "hunk {} / {}",
            app.current_hunk, snap.diff.total_hunks
        ))
    } else {
        None
    };

    // Budget: keep `N changed files` and the hunk phrase; elide the middle file phrase.
    let mut parts = vec![files.clone()];
    if let Some(fp) = file_phrase {
        parts.push(fp);
    }
    if let Some(hp) = hunk_phrase {
        parts.push(hp);
    }
    let full = format!(" {}", parts.join(" · "));
    if full.width() <= width {
        return full;
    }
    // Drop the middle phrase first (never the count or the hunk).
    let short = if parts.len() > 2 {
        format!(" {} · {}", parts[0], parts.last().unwrap())
    } else {
        format!(" {}", parts.join(" · "))
    };
    if short.width() <= width {
        return short;
    }
    // Still too wide (a long basename in the middle): truncate the middle, keep the ends.
    if parts.len() > 2 {
        let fixed = parts[0].width() + parts[2].width() + 2 * " · ".width() + 1 + 1; // space + …
        let mid_budget = width.saturating_sub(fixed);
        let mid = truncate_cells(&parts[1], mid_budget.max(1));
        return format!(" {} · {mid} · {}", parts[0], parts[2]);
    }
    truncate_cells(&short, width.saturating_sub(1))
}

/// The basename of a repo-relative path (the last component).
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// -- files pane (docs/review/15 §3.3) -------------------------------------------

/// The changed-files pane: outer title `Changed files` (left) + the active file count
/// (right). Rows are `{status} {disclosure} {display_path}{pad}{count}` with the count
/// right-aligned to the widest visible count; directory components are MUTED, the
/// basename TEXT (bold on the active file). The active row gets SELECTED_BG; the file
/// owning an active symbol child gets OWNER_BG.
fn render_files(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let focused = app.focused == Pane::Files;
    let inner_w = (area.width as usize).saturating_sub(2);

    let counts: Vec<usize> = snap.files.iter().map(|f| f.changed_symbol_count).collect();
    let count_width = counts.iter().map(|c| digits(*c)).max().unwrap_or(1).max(1);
    // status + space + disclosure + space + at-least-one gap = 5 fixed cells.
    let path_budget = inner_w.saturating_sub(5 + count_width);

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
        // A zero count on a file that HAS no symbols yet would still draw a "0"; show
        // the count only when there is anything to count.
        let count = counts[fi];
        items.push(ListItem::new(file_row_line(
            f,
            &display[fi],
            count,
            count_width,
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
                        "analysis failed — Tab to retry",
                        inner_w,
                    )));
                }
                crate::snapshot::FileSemanticLoad::Ready if f.symbols.is_empty() => {
                    items.push(ListItem::new(semantic_note_line(
                        "no changed symbols mapped",
                        inner_w,
                    )));
                }
                // An expanded Unloaded row (optimistic frame before the dispatcher's
                // Loading publish) shows the pending marker, not a blank body.
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
    let first_visible = crate::file_rows::first_visible(&snap.files, app.file_sel, capacity);
    let visible: Vec<ListItem> = items
        .into_iter()
        .skip(first_visible)
        .take(capacity.max(1))
        .collect();
    let list = List::new(visible).block(block);
    frame.render_widget(list, area);
}

/// The decimal digit count of `n` (for right-aligning counts).
fn digits(mut n: usize) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// One file row: status + disclosure + path (dirs MUTED, basename TEXT, bold when
/// active) padded to `path_budget`, then the right-aligned count.
#[allow(clippy::too_many_arguments)]
fn file_row_line(
    f: &crate::snapshot::FileRow,
    display: &str,
    count: usize,
    count_width: usize,
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
    let pad = path_budget.saturating_sub(display.width()) + 1;
    let content_w = 4 + display.width() + pad + count_width;
    let trailing = inner_w.saturating_sub(content_w);
    Line::from(vec![
        Span::styled(f.status.to_string(), status_style(f.status).bg(bg)),
        Span::styled(" ", base_style),
        Span::styled(marker, Style::new().fg(MUTED).bg(bg)),
        Span::styled(" ", base_style),
        Span::styled(dirs, Style::new().fg(MUTED).bg(bg)),
        Span::styled(base, basename_style),
        Span::styled(" ".repeat(pad), base_style),
        // The count is only meaningful once analysis landed: Unloaded shows `…`
        // (unknown, not zero), Loading shows the pending marker.
        Span::styled(
            match f.semantic {
                crate::snapshot::FileSemanticLoad::Ready => {
                    format!("{count:>count_width$}")
                }
                crate::snapshot::FileSemanticLoad::Loading => {
                    format!("{:>count_width$}", "…")
                }
                _ => " ".repeat(count_width),
            },
            Style::new().fg(MUTED).bg(bg),
        ),
        Span::styled(" ".repeat(trailing), base_style),
    ])
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
    let built = if app.diff_wrap {
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
    for (g, &changed) in gs.iter().zip(flags) {
        let style = if changed { hi } else { base };
        match out.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(g),
            _ => out.push(Span::styled(g.to_string(), style)),
        }
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
        match out.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(g),
            _ => out.push(Span::styled(g.to_string(), style)),
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
/// kept: tabs are measured as four-cell stops but passed through for display, because
/// ratatui (and real terminals) render the tab character themselves.
fn wrap_body(text: &str, budget: usize) -> Vec<String> {
    let gs: Vec<&str> = unicode_segmentation::UnicodeSegmentation::graphemes(text, true).collect();
    wrap_ranges(&gs, budget)
        .iter()
        .map(|&(s, e)| gs[s..e].concat())
        .collect()
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

// -- bottom pane: Impact | AI Plan (docs/review/15 §3.5, docs/review/16) -------------
//
// The bottom pane is tabbed (`v`): the deterministic Impact view reads `snap.impact`
// (spec §4), the AI plan view reads the validated plan the dispatcher publishes in
// `snap.semantic` (only ever rendered when `ai_generated`).

/// The full-width bottom pane: one bordered block whose title is the `Impact | AI Plan`
/// tab strip (the active tab is ACCENT+BOLD, the inactive MUTED; `AI Plan …` while a
/// request is in flight). The body is the active tab's view.
fn render_impact(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let focused = app.focused == Pane::Impact;
    let block = pane_block(bottom_tab_title(app, snap), None, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 6 || inner.height == 0 {
        return;
    }
    match app.bottom_view {
        BottomView::Impact => render_impact_body(frame, inner, snap),
        BottomView::AiPlan => render_ai_plan(frame, inner, app, snap),
    }
}

/// The ` Impact | AI Plan ` tab strip: the active tab is ACCENT+BOLD, the inactive
/// MUTED, separated by a MUTED ` | `. While `snap.ai` is Loading the AI tab gains a
/// WARN `…` suffix — the label itself keeps its active/inactive style, so the active
/// tab never loses its cue. The zoom tag trails, as before.
fn bottom_tab_title(app: &App, snap: &UiSnapshot) -> Line<'static> {
    let loading = matches!(snap.ai, AiStatus::Loading { .. });
    let active = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
    let inactive = Style::new().fg(MUTED);
    let (impact_style, ai_style) = match app.bottom_view {
        BottomView::Impact => (active, inactive),
        BottomView::AiPlan => (inactive, active),
    };
    let mut spans = vec![
        Span::styled(" Impact", impact_style),
        Span::styled(" | ", Style::new().fg(MUTED)),
        Span::styled("AI Plan", ai_style),
    ];
    if loading {
        spans.push(Span::styled(" …", Style::new().fg(WARN)));
    }
    spans.push(Span::styled(
        format!("{} ", zoom_tag(app, Pane::Impact)),
        Style::new().fg(TEXT),
    ));
    Line::from(spans)
}

/// The Impact body: three inner columns (40/30/30) with right-border dividers on the
/// first two. Headers are the first interior row of each column: `SELECTED CHANGE`,
/// `CALLERS · {N|…}`, `DOWNSTREAM · {N|…}`.
fn render_impact_body(frame: &mut Frame, area: Rect, snap: &UiSnapshot) {
    let impact = &snap.impact;
    let cols = Layout::horizontal([
        Constraint::Percentage(40),
        Constraint::Percentage(30),
        Constraint::Percentage(30),
    ])
    .split(area);

    render_selected_change(frame, cols[0], impact);
    render_impact_list(frame, cols[1], "CALLERS", &impact.callers, true);
    render_impact_list(frame, cols[2], "DOWNSTREAM", &impact.downstream, false);
}

/// The AI plan body: the semantic pane's title row (MUTED+BOLD header + a MUTED ` AI`
/// badge, with the pane note appended when present), then one indented row per
/// [`crate::snapshot::SemRow`], scrolled by `app.ai_plan_scroll`. A validated plan with
/// no rows — or a stale/unavailable one — renders a single MUTED explanation instead,
/// chosen by AI state so a deterministic pane's note never poses as an AI message.
fn render_ai_plan(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let sem = &snap.semantic;
    // No renderable plan: a validated plan that came back empty, or no current plan at
    // all. Order by AI state; the pane note is only trusted for Failed/stale plans (a
    // non-AI pane's note never speaks for the AI tab).
    if !sem.ai_generated || sem.rows.is_empty() {
        let msg = if sem.ai_generated {
            // (rows.is_empty(), per the guard above): the plan validated but had nothing
            // renderable.
            "AI returned no renderable rows".to_string()
        } else if matches!(snap.ai, AiStatus::Loading { .. }) {
            "generating AI plan…".to_string()
        } else if matches!(snap.ai, AiStatus::Disabled | AiStatus::Idle) {
            "AI plan unavailable".to_string()
        } else if matches!(snap.ai, AiStatus::Failed { .. }) {
            if sem.note.is_empty() {
                "AI request failed — see status bar".to_string()
            } else {
                sem.note.clone()
            }
        } else if !sem.note.is_empty() {
            // Stale plan (Stale status, or a Ready/Loading tagged with an old epoch).
            sem.note.clone()
        } else {
            "AI plan unavailable".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, Style::new().fg(MUTED)))),
            area,
        );
        return;
    }

    // Width budget: the ` AI` badge always survives; the title takes what remains, and
    // the note what is left after that.
    let width = usize::from(area.width);
    let title = truncate_cells(&sem.title, width.saturating_sub(3));
    let mut header = header_line(&title);
    header.push_span(Span::styled(" AI", Style::new().fg(MUTED)));
    if !sem.note.is_empty() {
        let note_budget = width.saturating_sub(header.width() + 2);
        if note_budget > 0 {
            header.push_span(Span::styled(
                format!("  {}", truncate_cells(&sem.note, note_budget)),
                Style::new().fg(MUTED),
            ));
        }
    }
    let mut lines: Vec<Line> = vec![header];

    // Rows under the header, windowed by the scroll offset; the last visible row is a
    // `… +N more` marker when the window does not reach the end.
    let avail = (area.height as usize).saturating_sub(1);
    let total = sem.rows.len();
    let start = app.ai_plan_scroll.min(total);
    let end = if total - start > avail && avail > 0 {
        start + avail - 1
    } else {
        (start + avail).min(total)
    };
    for r in &sem.rows[start..end] {
        let label_style = if r.changed {
            Style::new().fg(WARN)
        } else {
            Style::new().fg(TEXT)
        };
        // Budget the row: indentation is capped, the relation suffix and the
        // diagnostic ` !` are reserved, and the label is truncated into what remains —
        // a model-controlled label can never push the suffixes off the row.
        let indent = (usize::from(r.depth) * 2).min(8);
        let suffix = match (r.relation.is_empty(), r.has_diagnostic) {
            (false, false) => UnicodeWidthStr::width(r.relation) + 1,
            (false, true) => UnicodeWidthStr::width(r.relation) + 1 + 2,
            (true, true) => 2,
            (true, false) => 0,
        };
        let label = truncate_cells(&r.label, width.saturating_sub(indent + suffix));
        let mut spans = vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(label, label_style),
        ];
        if !r.relation.is_empty() {
            spans.push(Span::styled(
                format!(" {}", r.relation),
                Style::new().fg(MUTED),
            ));
        }
        if r.has_diagnostic {
            spans.push(Span::styled(" !", Style::new().fg(ERROR)));
        }
        lines.push(Line::from(spans));
    }
    if end < total {
        lines.push(Line::from(Span::styled(
            format!("… +{} more", total - end),
            Style::new().fg(MUTED),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The SELECTED CHANGE column: header, then the symbol label (ACCENT+BOLD) + badge, one
/// deterministic interpretation line, and the pane note when space remains. A file-row
/// selection shows the basename and the "select one to inspect impact" guidance.
fn render_selected_change(frame: &mut Frame, area: Rect, impact: &crate::snapshot::ImpactPane) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::new().fg(BORDER))
        .border_type(BorderType::Plain)
        .padding(Padding::horizontal(1));
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

/// A CALLERS / DOWNSTREAM column: header with a live count (`· …` while loading, never
/// a false zero), then rows; when rows remain past the visible space the final visible
/// row is `… +N more` in MUTED.
fn render_impact_list(frame: &mut Frame, area: Rect, name: &str, list: &ImpactList, divider: bool) {
    let block = if divider {
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::new().fg(BORDER))
            .padding(Padding::horizontal(1))
    } else {
        Block::default().padding(Padding::horizontal(1))
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header = match list.state {
        ImpactLoadState::Loading => format!("{name} · …"),
        _ => format!("{name} · {}", list.rows.len()),
    };
    let mut lines: Vec<Line> = vec![header_line(&header)];

    // Interior rows available below the header.
    let avail = (inner.height as usize).saturating_sub(1);
    let rows = &list.rows;
    let show = rows.len().min(avail);
    for r in &rows[..show] {
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
    if rows.len() > avail && avail > 0 {
        let more = rows.len() - (avail - 1);
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

/// A column header: MUTED + BOLD (docs/review/15 §2).
fn header_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::new().fg(MUTED).add_modifier(Modifier::BOLD),
    ))
}

// -- status + help bars (docs/review/15 §3.6, §3.7) --------------------------------

/// The status message row (always reserved): the typed `snap.status` (spec §3.6), or
/// the selected file's full repo-relative path in MUTED when there is no message.
fn render_status_bar(frame: &mut Frame, area: Rect, app: &App, snap: &UiSnapshot) {
    let (text, style) = if snap.status.text.is_empty() {
        (
            format!(" {}", app.selected_file_path().unwrap_or("")),
            Style::new().fg(MUTED),
        )
    } else {
        let fg = match snap.status.level {
            StatusLevel::Error => ERROR,
            StatusLevel::Warning => WARN,
            StatusLevel::Info => MUTED,
        };
        (format!(" {}", snap.status.text), Style::new().fg(fg))
    };
    frame.render_widget(
        Paragraph::new(truncate_cells(&text, area.width as usize)).style(style.bg(SURFACE)),
        area,
    );
}

/// The compact help row: styled spans (keys TEXT, separators/explanations MUTED). The
/// full layout at width >= 96; resize and the impact/AI toggle are dropped first at
/// 64..=95; the minimal set at 30..=63. The help modal holds the rest.
fn render_help_bar(frame: &mut Frame, area: Rect) {
    let groups: &[(&str, &str)] = if area.width >= 96 {
        &[
            ("Tab", "analyze"),
            ("1-3", "pane"),
            ("z", "zoom"),
            ("W", "wrap"),
            ("n/N", "hunk"),
            ("[/]", "resize"),
            ("v", "impact/AI"),
            ("?", "help"),
        ]
    } else if area.width >= 64 {
        &[
            ("Tab", "analyze"),
            ("1-3", "pane"),
            ("z", "zoom"),
            ("W", "wrap"),
            ("n/N", "hunk"),
            ("?", "help"),
        ]
    } else {
        &[
            ("Tab", ""),
            ("1-3", ""),
            ("z", ""),
            ("W", ""),
            ("n/N", ""),
            ("?", ""),
        ]
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
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(SURFACE)),
        area,
    );
}

// -- help modal -------------------------------------------------------------------

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
        Line::from("  Tab             expand file + analyze symbols / collapse"),
        Line::from("  1 / 2 / 3       focus files / diff / impact"),
        Line::from("  j/k · ↑/↓       move selection · scroll"),
        Line::from("  Ctrl-d/u · Pg   half / full page in diff"),
        Line::from("  s / u / B / w   staged / unstaged / branch / working scope"),
        Line::from("  S               cycle scope"),
        Line::from("  b               pick comparison base (default: nearest ancestor)"),
        Line::from("  Enter           jump to symbol / re-center view"),
        Line::from("  Space h l       expand / collapse"),
        Line::from("  [ / ]           resize the files pane"),
        Line::from("  n / N           next / previous diff hunk"),
        Line::from("  z               zoom the focused pane (Tab still switches)"),
        Line::from("  v               toggle impact / AI plan"),
        Line::from("  W / 0           diff: toggle wrap / reset horizontal scroll"),
        Line::from("  R               rescan git"),
        Line::from("  a / A           AI toggle / refresh"),
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
        .border_style(Style::new().fg(ACCENT))
        .title(title);
    let models = filter_candidates(&snap.available_models, &app.model_query);
    let mut items: Vec<ListItem> = if snap.available_models.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no models loaded (is AI configured?)",
            Style::new().fg(MUTED),
        )))]
    } else if models.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no matches",
            Style::new().fg(MUTED),
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
    items.push(ListItem::new(Line::from(Span::styled(
        format!("  filter: {}", app.model_query),
        Style::new().fg(WARN),
    ))));
    items.push(ListItem::new(Line::from(Span::styled(
        "  type to filter · ↑/↓ move · Enter select · Esc close",
        Style::new().fg(MUTED),
    ))));
    let mut state = ListState::default();
    if !models.is_empty() {
        state.select(Some(app.model_sel.min(models.len().saturating_sub(1))));
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
    let title = if snap.base_ref.is_empty() {
        " comparison base ".to_string()
    } else {
        format!(" comparison base (current: {}) ", snap.base_ref)
    };
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
        // §1.1: top y=0, summary y=1, work y=2..28 (h 27), Impact y=29..37 (h 9),
        // status y=38, help y=39.
        assert!(
            row_text(&t, 0).contains("codescope"),
            "top bar: {}",
            row_text(&t, 0)
        );
        assert!(
            row_text(&t, 1).contains("1 changed file"),
            "summary: {}",
            row_text(&t, 1)
        );
        assert!(
            row_text(&t, 2).contains("Changed files"),
            "files block top: {}",
            row_text(&t, 2)
        );
        // Work row split: files width 42 → its right border is x=41, diff starts x=42.
        assert_eq!(cell(&t, 41, 2).0, "┐", "files right border at x=41");
        assert_eq!(cell(&t, 42, 2).0, "┌", "diff left border at x=42");
        // Impact block: full width, y=29..37.
        assert!(
            row_text(&t, 29).contains("Impact"),
            "impact top: {}",
            row_text(&t, 29)
        );
        assert!(row_text(&t, 29).starts_with('┌'), "impact top border");
        assert!(row_text(&t, 37).starts_with('└'), "impact bottom border");
        assert!(
            row_text(&t, 28).starts_with('└'),
            "files/diff bottom at y=28"
        );
        // Status y=38, help y=39.
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
        // §7.2: files width 32, diff width 48, work height 7 (y=2..8), impact y=9..17.
        assert_eq!(cell(&t, 31, 2).0, "┐", "files right border x=31");
        assert_eq!(cell(&t, 32, 2).0, "┌", "diff left border x=32");
        assert!(
            row_text(&t, 8).starts_with('└'),
            "work bottom y=8: {}",
            row_text(&t, 8)
        );
        assert!(row_text(&t, 9).starts_with('┌'), "impact top y=9");
        assert!(row_text(&t, 17).starts_with('└'), "impact bottom y=17");
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
            top.contains("feature/x ◂ release/2.0"),
            "branch ◂ base: {top}"
        );
        assert!(
            top.contains("branch  LSP ✓  AI × prime"),
            "right group: {top}"
        );
        // The right group is reserved flush against the terminal's right edge.
        assert!(top.ends_with("prime "), "right-aligned: {top:?}");
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
            assert!(top.contains("AI × prime"), "{w}: ai survives: {top}");
        }
    }

    #[test]
    fn top_bar_reads_base_ref_then_repo_base() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let app = App::new();
        t.draw(|f| render(f, &app, &snap_with_base())).unwrap();
        assert!(row_text(&t, 0).contains("feature/x ◂ release/2.0"));
        let mut snap = snap_with_base();
        snap.base_ref.clear();
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(row_text(&t, 0).contains("feature/x ◂ origin/main"));
    }

    #[test]
    fn top_bar_refresh_spinner() {
        let mut snap = sample();
        snap.refreshing = true;
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert!(row_text(&t, 0).contains('⟳'), "spinner");
    }

    // -- §3.2 / §7.5: summary bar ------------------------------------------------------

    #[test]
    fn summary_counts_symbols_of_the_diff_file() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let mut app = app_with(&sample());
        let snap = sample();
        t.draw(|f| render(f, &app, &snap)).unwrap();
        let s = row_text(&t, 1);
        assert!(s.contains("1 changed file"), "{s}");
        assert!(s.contains("1 symbol in service.go"), "{s}");
        assert!(s.contains("hunk 1 / 1"), "{s}");
        // Selection on the nested symbol keeps the same file's count.
        app.apply(crate::action::Action::Down);
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(row_text(&t, 1).contains("1 symbol in service.go"));
    }

    #[test]
    fn summary_empty_and_singular() {
        let mut snap = snap_with_base();
        snap.files.clear();
        snap.diff = DiffPane::default();
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert!(row_text(&t, 1).contains("0 changed files · no selection"));

        // A selected file with no mapped symbols.
        let mut snap = sample();
        snap.files[0].symbols.clear();
        snap.files[0].changed_symbol_count = 0;
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert!(row_text(&t, 1).contains("0 symbols in service.go"));
    }

    #[test]
    fn summary_and_title_hunk_values_match() {
        // current_hunk is App-owned: n/N moves it and summary + diff title agree.
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
        assert!(
            row_text(&t, 1).contains("hunk 2 / 2"),
            "summary: {}",
            row_text(&t, 1)
        );
        let diff_title_row = row_text(&t, 2);
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
            semantic: crate::snapshot::FileSemanticLoad::Ready,
            symbols: vec![],
        });
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&snap), &snap)).unwrap();
        let files_top = row_text(&t, 2);
        assert!(
            files_top.contains("Changed files"),
            "left title: {files_top}"
        );
        // Right title is the active count, right-aligned against the right border (x=41).
        let files_top: String = (0..42u16).map(|x| cell(&t, x, 2).0).collect();
        assert!(
            files_top.contains("Changed files"),
            "left title: {files_top:?}"
        );
        assert_eq!(
            cell(&t, 39, 2).0,
            "2",
            "count right-aligned before the border: {files_top:?}"
        );
        // Status colors: M is WARN, A is ADD_FG.
        assert_eq!(cell(&t, 1, 3).0, "M");
        assert_eq!(cell(&t, 1, 3).1, WARN);
        // The file row is at y=3 (block top border y=2). Find the `A` row.
        let y_a = (3..28u16)
            .find(|&y| cell(&t, 1, y).0 == "A")
            .expect("added file row");
        assert_eq!(cell(&t, 1, y_a).1, ADD_FG);
        // Selected row background on the first file row (active file is file_sel=0);
        // the SELECTED_BG fills the whole inner row.
        assert_eq!(cell(&t, 10, 3).2, SELECTED_BG, "selected row bg");
        assert_eq!(
            cell(&t, 39, 3).2,
            SELECTED_BG,
            "selected row bg to the row end"
        );
    }

    #[test]
    fn files_pane_dirs_muted_basename_text() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        // The file row (y=3) shows `M ▾ internal/service/service.go`: the directory
        // components and separators are MUTED; the basename is TEXT + BOLD (active).
        let buf = t.backend().buffer();
        let cells: Vec<(String, Color, Modifier)> = (0..42u16)
            .map(|x| {
                let c = buf.cell((x, 3)).unwrap();
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
        // The basename chars sit strictly between the last '/' and the count padding.
        let count_x = cells.iter().position(|(s, _, _)| s == "1").unwrap();
        for (x, (sym, fg, mods)) in cells.iter().enumerate() {
            if x > slash && x < count_x && !sym.trim().is_empty() {
                assert_eq!(*fg, TEXT, "basename char {sym:?} at x{x} text");
                assert!(mods.contains(Modifier::BOLD), "basename char {sym:?} bold");
            }
        }
        // The right-aligned count sits at the row's end (before the border at x=41).
        assert_eq!(cells[40].0, "1", "count right-aligned");
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
            cell(&t, 5, 4).0,
            "~",
            "change glyph after the 4-cell indent"
        );
        assert_eq!(cell(&t, 5, 4).2, SELECTED_BG, "active symbol row bg");
        assert_eq!(
            cell(&t, 10, 3).2,
            OWNER_BG,
            "owning file row keeps OWNER_BG"
        );
    }

    // -- §3.4 / §7.8/§7.9/§7.10: diff pane --------------------------------------------

    #[test]
    fn diff_dual_gutter_blanks_on_the_absent_side() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        // Diff pane x starts at 43 (border at 42); ln_width = 4 (max ln 11 → 2 digits,
        // clamped to 4). Rows: header y=3, context y=4, del y=5, add y=6.
        // Context row y=4: both numbers present. The gutter starts at x=43 (block inner).
        let ctx: String = (43..54u16).map(|x| cell(&t, x, 4).0).collect();
        assert_eq!(ctx, "  10 │   10", "dual gutter: {ctx:?}");
        // Del row y=5: old number present, new side exactly ln_width blanks.
        let del_old: String = (43..47u16).map(|x| cell(&t, x, 5).0).collect();
        assert_eq!(del_old, "  11", "del old number: {del_old:?}");
        let del_new: String = (50..54u16).map(|x| cell(&t, x, 5).0).collect();
        assert_eq!(del_new, "    ", "del new side blank: {del_new:?}");
        assert_eq!(cell(&t, 48, 5).0, "│");
        assert_eq!(cell(&t, 55, 5).0, "-", "sign cell");
        // Add row y=6: old side blank, new number present.
        let add_old: String = (43..47u16).map(|x| cell(&t, x, 6).0).collect();
        assert_eq!(add_old, "    ", "add old side blank");
        let add_new: String = (50..54u16).map(|x| cell(&t, x, 6).0).collect();
        assert_eq!(add_new, "  11", "add new number");
        assert_eq!(cell(&t, 55, 6).0, "+");
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
        let del_old: String = (43..47u16).map(|x| cell(&t, x, 5).0).collect();
        assert_eq!(del_old, "  11", "gutter fixed under hscroll");
        assert_eq!(cell(&t, 48, 5).0, "│");
        assert_eq!(cell(&t, 55, 5).0, "-", "sign fixed under hscroll");
        // The title shows the raw offset.
        assert!(
            row_text(&t, 2).contains("x+08"),
            "title: {}",
            row_text(&t, 2)
        );
    }

    #[test]
    fn hunk_header_band_spans_the_full_inner_width() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        // The header band is row y=3; the diff block spans x 42..139, so the inner is
        // x 43..=138. EVERY interior cell carries the band style (padded to the right).
        for x in [43u16, 60, 100, 138] {
            let (_, _, bg, _) = cell(&t, x, 3);
            assert_eq!(bg, SURFACE_ALT, "x={x} band bg");
        }
        let (_, fg, _, mods) = cell(&t, 43, 3);
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
        // Del row y=5: nothing changes on the old side (pure deletion of nothing) —
        // `return` is equal, `name` moves; the del side has no inserted span.
        // Add row y=6: body starts at x=57 (gutter x=43, 14 cells: 2*4+5+1 → 43+14).
        let body_x = 57u16;
        // `    return ` is equal → restrained ADD body (fg TEXT, bg ADD_BG).
        let (_, fg, bg, mods) = cell(&t, body_x + 5, 6); // 'e' in "return"
        assert_eq!((fg, bg), (TEXT, ADD_BG), "equal words restrained");
        assert!(!mods.contains(Modifier::BOLD));
        // `prefix + ` was inserted → bright.
        let (_, fg, bg, mods) = cell(&t, body_x + 12, 6); // inside "prefix"
        assert_eq!((fg, bg), (ADD_HI, ADD_HI_BG), "changed words bright");
        assert!(mods.contains(Modifier::BOLD));
    }

    #[test]
    fn diff_title_basename_and_state() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let app = app_with(&sample());
        t.draw(|f| render(f, &app, &sample())).unwrap();
        let title = row_text(&t, 2);
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
            row_text(&t, 2).contains("GetDisplayName · hunk 1/1"),
            "{}",
            row_text(&t, 2)
        );
        // File-row selection (no focused_symbol): no symbol in the title.
        snap.diff.focused_symbol = None;
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(
            !row_text(&t, 2).contains("GetDisplayName ·"),
            "no symbol: {}",
            row_text(&t, 2)
        );
    }

    // -- §3.5 / §7.11: impact pane ---------------------------------------------------

    #[test]
    fn impact_three_headers_always_present() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        let header_row = row_text(&t, 30); // first interior row of the Impact block
        assert!(header_row.contains("SELECTED CHANGE"), "{header_row}");
        assert!(header_row.contains("CALLERS ·"), "{header_row}");
        assert!(header_row.contains("DOWNSTREAM ·"), "{header_row}");
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
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
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

    // -- bottom pane: AI Plan tab (docs/review/16) ----------------------------------------

    /// A snapshot whose semantic pane carries a validated, epoch-matched AI plan.
    fn ai_plan_snap(rows: usize) -> UiSnapshot {
        let mut snap = sample();
        snap.epoch = codescope_core::Epoch(3);
        snap.ai = AiStatus::Ready {
            epoch: codescope_core::Epoch(3),
        };
        snap.semantic = crate::snapshot::SemanticPane {
            title: "plan: auth refactor".to_string(),
            rows: (0..rows)
                .map(|i| crate::snapshot::SemRow {
                    depth: (i % 3) as u16,
                    label: format!("PlanStep{i}"),
                    relation: "calls",
                    changed: i % 2 == 0,
                    has_diagnostic: i == 1,
                })
                .collect(),
            note: String::new(),
            ai_generated: true,
        };
        snap
    }

    /// Drive the Loading → Ready edge so the app auto-switches to the AI plan.
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
    fn ai_plan_renders_rows_badge_and_title_after_loading_to_ready() {
        let plan = ai_plan_snap(3);
        let app = app_after_ai_landed(&plan);
        assert_eq!(app.bottom_view, crate::app::BottomView::AiPlan);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        // Tab strip on the block's top border.
        assert!(
            row_text(&t, 29).contains("Impact | AI Plan"),
            "tab strip: {}",
            row_text(&t, 29)
        );
        // Header row: semantic title + the MUTED AI badge.
        let header = row_text(&t, 30);
        assert!(
            header.contains("plan: auth refactor"),
            "plan title: {header}"
        );
        assert!(
            header.contains("plan: auth refactor AI"),
            "AI badge: {header}"
        );
        // Rows: depth-indented labels with relations; the diagnostic row shows ` !`.
        let (r0, r1, r2) = (row_text(&t, 31), row_text(&t, 32), row_text(&t, 33));
        assert!(r0.contains("PlanStep0 calls"), "row 0: {r0}");
        assert!(r1.contains("  PlanStep1 calls !"), "row 1: {r1}");
        assert!(r2.contains("    PlanStep2 calls"), "row 2: {r2}");
        // The deterministic columns are NOT drawn in the AI plan view.
        let body: String = (30..37).map(|y| row_text(&t, y)).collect();
        assert!(!body.contains("SELECTED CHANGE"), "impact replaced: {body}");
    }

    #[test]
    fn tab_strip_marks_the_active_tab() {
        let plan = ai_plan_snap(2);
        let mut app = App::new();
        app.update(plan.clone()); // no Loading edge: stays on Impact
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let row = row_text(&t, 29);
        let xi = row.find("Impact").unwrap() as u16;
        let xa = row.find("AI Plan").unwrap() as u16;
        assert_eq!(cell(&t, xi, 29).1, ACCENT, "Impact active");
        assert!(cell(&t, xi, 29).3.contains(Modifier::BOLD));
        assert_eq!(cell(&t, xa, 29).1, MUTED, "AI Plan inactive");
        app.apply(crate::action::Action::ToggleBottomView);
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let row = row_text(&t, 29);
        let xi = row.find("Impact").unwrap() as u16;
        let xa = row.find("AI Plan").unwrap() as u16;
        assert_eq!(cell(&t, xi, 29).1, MUTED, "Impact inactive");
        assert_eq!(cell(&t, xa, 29).1, ACCENT, "AI Plan active");
        assert!(cell(&t, xa, 29).3.contains(Modifier::BOLD));
    }

    #[test]
    fn ai_tab_shows_ellipsis_while_loading() {
        let mut snap = sample();
        snap.ai = AiStatus::Loading {
            since_epoch: codescope_core::Epoch(3),
        };
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert!(
            row_text(&t, 29).contains("AI Plan …"),
            "loading tab: {}",
            row_text(&t, 29)
        );
    }

    #[test]
    fn ai_plan_view_explains_empty_or_unavailable_states() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        // A validated-but-empty plan: nothing to scroll, say so.
        let mut app = App::new();
        let empty = ai_plan_snap(0);
        app.update(empty.clone());
        app.apply(crate::action::Action::ToggleBottomView);
        t.draw(|f| render(f, &app, &empty)).unwrap();
        assert!(
            row_text(&t, 30).contains("AI returned no renderable rows"),
            "empty plan: {}",
            row_text(&t, 30)
        );
        // AI off: unavailable. (update() flips the view back; toggle after it.)
        let mut snap = sample(); // ai: Disabled, semantic: default
        let mut app = App::new();
        app.update(snap.clone());
        app.apply(crate::action::Action::ToggleBottomView);
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(
            row_text(&t, 30).contains("AI plan unavailable"),
            "disabled: {}",
            row_text(&t, 30)
        );
        // A stale publish carries the dispatcher's note.
        snap.ai = AiStatus::Stale {
            epoch: codescope_core::Epoch(2),
        };
        snap.semantic.note = "AI view stale (repo changed); regenerating…".to_string();
        let mut app = App::new();
        app.update(snap.clone());
        app.apply(crate::action::Action::ToggleBottomView);
        t.draw(|f| render(f, &app, &snap)).unwrap();
        assert!(
            row_text(&t, 30).contains("AI view stale (repo changed)"),
            "stale note: {}",
            row_text(&t, 30)
        );
    }

    #[test]
    fn ai_plan_scrolls_with_a_truncation_marker() {
        let plan = ai_plan_snap(8);
        let app = app_after_ai_landed(&plan);
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        // Inner height 7: header + 5 rows + the marker (8 rows total).
        let (r5, r6) = (row_text(&t, 35), row_text(&t, 36));
        assert!(r5.contains("PlanStep4"), "fifth row: {r5}");
        assert!(r6.contains("… +3 more"), "truncation marker: {r6}");
        // Scrolling down moves the window; the end needs no marker.
        let mut app = app_after_ai_landed(&plan);
        app.apply(crate::action::Action::Focus(Pane::Impact));
        for _ in 0..3 {
            app.apply(crate::action::Action::Down);
        }
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let (r0, r4) = (row_text(&t, 31), row_text(&t, 35));
        assert!(r0.contains("PlanStep3"), "scrolled: {r0}");
        assert!(r4.contains("PlanStep7"), "last row: {r4}");
        let body: String = (31..37).map(|y| row_text(&t, y)).collect();
        assert!(!body.contains("more"), "no marker at the end: {body}");
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
        assert!(text.contains("Impact | AI Plan"), "tabs in zoom: {text}");
        assert!(text.contains("· ZOOM"), "zoom tag: {text}");
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
        // 79x40: focus-only fallback also goes through the tabbed dispatcher.
        let mut app = app_after_ai_landed(&plan);
        app.apply(crate::action::Action::Focus(Pane::Impact));
        let mut t = Terminal::new(TestBackend::new(79, 40)).unwrap();
        t.draw(|f| render(f, &app, &plan)).unwrap();
        let text = buffer_text(&t);
        assert!(text.contains("Impact | AI Plan"), "79x40 tabs: {text}");
        assert!(text.contains("PlanStep0"), "79x40 rows: {text}");
    }

    #[test]
    fn impact_view_still_renders_the_three_columns() {
        let mut snap = sample();
        snap.impact = impact_sample();
        snap.ai = AiStatus::Ready {
            epoch: codescope_core::Epoch(3),
        };
        snap.semantic = ai_plan_snap(3).semantic; // a plan exists but Impact is the view
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        let header_row = row_text(&t, 30);
        assert!(header_row.contains("SELECTED CHANGE"), "{header_row}");
        assert!(header_row.contains("CALLERS ·"), "{header_row}");
        assert!(header_row.contains("DOWNSTREAM ·"), "{header_row}");
        let tabs = row_text(&t, 29);
        assert!(tabs.contains("Impact | AI Plan"), "tabs: {tabs}");
        assert!(
            !row_text(&t, 31).contains("PlanStep0"),
            "plan hidden in Impact view"
        );
    }

    // -- §3.6 / §3.7: status + help bars ------------------------------------------------

    #[test]
    fn status_bar_shows_path_when_no_message() {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &app_with(&sample()), &sample()))
            .unwrap();
        let status = row_text(&t, 38);
        assert!(
            status.contains("internal/service/service.go"),
            "full path: {status}"
        );
        assert_eq!(cell(&t, 2, 38).1, MUTED, "path muted");
    }

    #[test]
    fn status_bar_colors_messages_by_severity() {
        let mut snap = sample();
        snap.status = crate::snapshot::StatusMessage {
            text: "AI timed out after 20s · A retry · m change model".to_string(),
            level: crate::snapshot::StatusLevel::Error,
        };
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert_eq!(cell(&t, 2, 38).1, ERROR, "AI failure is an error");
        assert!(
            row_text(&t, 38).contains("AI timed out after 20s"),
            "actionable text"
        );
        snap.status = crate::snapshot::StatusMessage {
            text: "base: main".to_string(),
            level: crate::snapshot::StatusLevel::Info,
        };
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert_eq!(cell(&t, 2, 38).1, MUTED, "info is muted");
    }

    #[test]
    fn status_bar_warning_level() {
        let mut snap = sample();
        snap.status = crate::snapshot::StatusMessage {
            text: "git-only (no supported language detected)".to_string(),
            level: crate::snapshot::StatusLevel::Warning,
        };
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert_eq!(cell(&t, 2, 38).1, WARN, "warning");
    }

    #[test]
    fn help_bar_compacts_with_width() {
        let snap = sample();
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        assert!(
            row_text(&t, 39).contains("[/] resize"),
            "full: {}",
            row_text(&t, 39)
        );
        // Focus-only at 79 wide: help at y=39, resize dropped at 64..95.
        let mut t = Terminal::new(TestBackend::new(79, 40)).unwrap();
        t.draw(|f| render(f, &App::new(), &snap)).unwrap();
        let help = row_text(&t, 39);
        assert!(help.contains("n/N hunk"), "{help}");
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
            row_text(&t, 1).contains("1 changed file"),
            "summary survives zoom"
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
        assert!(buffer_text(&t).contains("keyboard controls"));
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

    /// The files pane renders each semantic load state distinctly (lazy per-file
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
            text.contains("analysis failed — Tab to retry"),
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
