//! Shared layout geometry: the ONE computed frame plan, consumed by both rendering and
//! mouse hit-testing so a click can never target a rectangle the user did not see.
//!
//! Built once per frame in the draw closure and retained by the run loop. Do not cache it
//! in App or recompute it in input handling.

use ratatui::layout::Rect;

use crate::app::{App, Pane};
use crate::layout::{choose_tier, files_width, Tier, MIN_DIFF_WIDTH};
use crate::snapshot::UiSnapshot;

/// The bottom-pane tab label rectangles (clickable).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BottomTabRects {
    /// The `Impact` label.
    pub impact: Option<Rect>,
    /// The `AI Plan` label.
    pub ai_plan: Option<Rect>,
}

/// The computed frame plan. `None` fields mean that region is not present/clickable in the
/// current tier (e.g. a hidden pane in FocusOnly, or everything in TooSmall).
#[derive(Debug, Clone, Default)]
pub struct UiGeometry {
    /// The full frame area.
    pub area: Rect,
    /// The active layout tier.
    pub tier: Tier,
    /// Files pane outer rect (with its border).
    pub files: Option<Rect>,
    /// Files pane inner content rect (rows live here).
    pub files_inner: Option<Rect>,
    /// Diff pane outer rect.
    pub diff: Option<Rect>,
    /// Bottom (Impact/AI) pane outer rect.
    pub impact: Option<Rect>,
    /// Bottom tab labels.
    pub tabs: BottomTabRects,
    /// The vertical files|diff drag handle (the shared border column).
    pub files_diff_handle: Option<Rect>,
    /// The horizontal work|impact drag handle (the shared border row).
    pub work_impact_handle: Option<Rect>,
    /// The visible files rows: (screen rect, physical row index). Physical indices index
    /// into the shared projection.
    pub file_row_rects: Vec<(Rect, usize)>,
    /// Physical index of the first visible file row (the scroll offset).
    pub files_first_visible: usize,
}

impl UiGeometry {
    /// Build the frame plan for `area` given the current app + snapshot.
    pub fn build(area: Rect, app: &App, snap: &UiSnapshot) -> Self {
        let tier = choose_tier(area, app.zoomed);
        let mut geo = UiGeometry {
            area,
            tier,
            ..Default::default()
        };
        if tier == Tier::TooSmall {
            return geo;
        }
        if tier == Tier::FocusOnly {
            // The focused pane occupies the BODY between the chrome rows (top bar,
            // summary, status, help) — not the whole terminal (review 24 M1). The help
            // row is dropped below height 12, matching the renderer.
            let body = if area.height >= 12 {
                ratatui::layout::Layout::vertical([
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Min(3),
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Length(1),
                ])
                .split(area)[2]
            } else {
                ratatui::layout::Layout::vertical([
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Min(3),
                    ratatui::layout::Constraint::Length(1),
                ])
                .split(area)[2]
            };
            match app.focused {
                Pane::Files => {
                    geo.files = Some(body);
                    geo.files_inner = Some(inner(body));
                    // The files viewport is usable in focus mode too.
                    let rows_proj = crate::file_rows::project(&snap.files);
                    let cap = inner(body).height as usize;
                    let first = crate::file_rows::first_visible(&snap.files, app.file_sel, cap);
                    geo.files_first_visible = first;
                    let fi = inner(body);
                    let mut rects = Vec::new();
                    for (slot, phys) in (first..rows_proj.len()).take(cap).enumerate() {
                        rects.push((Rect::new(fi.x, fi.y + slot as u16, fi.width, 1), phys));
                    }
                    geo.file_row_rects = rects;
                }
                Pane::Diff => {
                    geo.diff = Some(body);
                }
                Pane::Impact => {
                    geo.impact = Some(body);
                    geo.tabs = bottom_tab_rects(body, app);
                }
            }
            return geo;
        }
        // Normal: the six-row stack. Rows: top, summary, work, impact, status, help.
        let rows = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Min(7),
            ratatui::layout::Constraint::Length(crate::layout::impact_height(
                app.impact_height,
                area.height,
            )),
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Length(1),
        ])
        .split(area);
        let work = rows[2];
        let impact = rows[3];
        let fw = files_width(app.files_width, work.width);
        let work_split = ratatui::layout::Layout::horizontal([
            ratatui::layout::Constraint::Length(fw),
            ratatui::layout::Constraint::Min(MIN_DIFF_WIDTH),
        ])
        .split(work);
        geo.files = Some(work_split[0]);
        geo.files_inner = Some(inner(work_split[0]));
        geo.diff = Some(work_split[1]);
        geo.impact = Some(impact);

        // Drag handles cover BOTH adjacent border cells (the two visible border columns/
        // rows of the shared boundary), so a press on either pane's border arms the drag.
        // The vertical handle stops before the work row's bottom border to avoid the
        // T-junction with the horizontal handle (review 24 M2).
        let vx = work_split[1].x.saturating_sub(1);
        geo.files_diff_handle = Some(Rect::new(vx, work.y, 2, work.height.saturating_sub(1)));
        let hy = impact.y.saturating_sub(1);
        geo.work_impact_handle = Some(Rect::new(area.x, hy, area.width, 2));

        // Files viewport: project the rows, compute the visible slice.
        if let Some(fi) = geo.files_inner {
            let rows_proj = crate::file_rows::project(&snap.files);
            let cap = fi.height as usize;
            let first = crate::file_rows::first_visible(&snap.files, app.file_sel, cap);
            geo.files_first_visible = first;
            let mut rects = Vec::new();
            for (slot, phys) in (first..rows_proj.len()).take(cap).enumerate() {
                rects.push((Rect::new(fi.x, fi.y + slot as u16, fi.width, 1), phys));
            }
            geo.file_row_rects = rects;
        }

        // Bottom tab labels: they live in the impact pane's top border title.
        // The title is ` Impact | AI Plan ` starting after the border corner. Compute the
        // two label rects by display width.
        geo.tabs = bottom_tab_rects(impact, app);
        geo
    }

    /// The pane under a point, if any (outer rect hit).
    pub fn pane_at(&self, x: u16, y: u16) -> Option<Pane> {
        if self.files.is_some_and(|r| hit(r, x, y)) {
            return Some(Pane::Files);
        }
        if self.diff.is_some_and(|r| hit(r, x, y)) {
            return Some(Pane::Diff);
        }
        if self.impact.is_some_and(|r| hit(r, x, y)) {
            return Some(Pane::Impact);
        }
        None
    }
}

/// A pane block's inner rect: all four borders shrink by one cell (matches `pane_block`).
fn inner(r: Rect) -> Rect {
    Rect {
        x: r.x + 1,
        y: r.y + 1,
        width: r.width.saturating_sub(2),
        height: r.height.saturating_sub(2),
    }
}

/// Point-in-rect test.
fn hit(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// The ` Impact | AI Plan ` title label rects on the bottom pane's top border row.
fn bottom_tab_rects(impact: Rect, app: &App) -> BottomTabRects {
    use unicode_width::UnicodeWidthStr;
    // The title starts one cell after the left border corner, with a leading space.
    let mut x = impact.x + 2;
    let y = impact.y;
    let impact_label = "Impact";
    let ai_label = if matches!(app.snapshot.ai, codescope_core::AiStatus::Loading { .. }) {
        "AI Plan …"
    } else {
        "AI Plan"
    };
    let iw = UnicodeWidthStr::width(impact_label) as u16;
    let impact_rect = Rect::new(x, y, iw, 1);
    // " | " separator (3 cells) between the labels.
    x += iw + 3;
    let aw = UnicodeWidthStr::width(ai_label) as u16;
    let ai_rect = Rect::new(x, y, aw, 1);
    BottomTabRects {
        impact: Some(impact_rect),
        ai_plan: Some(ai_rect),
    }
}
