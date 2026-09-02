//! Shared layout geometry: the ONE computed frame plan, consumed by both rendering and
//! mouse hit-testing so a click can never target a rectangle the user did not see.
//!
//! Built once per frame in the draw closure and retained by the run loop. Do not cache it
//! in App or recompute it in input handling.

use ratatui::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::action::PlanNodeTarget;
use crate::app::{App, Pane};
use crate::divider::{DividerAxis, DividerHandle, DividerId};
use crate::layout::{
    choose_tier, files_width, impact_left_width, impact_section_heights, Tier, MIN_DIFF_WIDTH,
};
use crate::scroll::{ScrollRegion, ScrollRegionId};
use crate::snapshot::UiSnapshot;

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
    /// Bottom combined Impact pane outer rect.
    pub impact: Option<Rect>,
    /// Clickable left portion of the bottom bar when it contains a status message.
    pub status: Option<Rect>,
    /// Clickable fallback banner retaining the complete reason for the focused AI
    /// failure. This is separate from the transient bottom-bar status.
    pub(crate) ai_failure_status: Option<Rect>,
    /// Every structural divider actually visible and draggable in this frame.
    pub(crate) dividers: Vec<DividerHandle>,
    /// The visible files rows: (screen rect, physical row index). Physical indices index
    /// into the shared projection.
    pub file_row_rects: Vec<(Rect, usize)>,
    /// Physical index of the first visible file row (the scroll offset).
    pub files_first_visible: usize,
    /// Independently scrollable rectangles in the frame the user actually saw.
    pub(crate) scroll_regions: Vec<ScrollRegion>,
    /// Visible generated-plan node spans. Several rects may share one target when a box
    /// occupies multiple rows or a compact node has separate label/detail spans.
    pub(crate) plan_node_rects: Vec<(Rect, PlanNodeTarget)>,
    /// Exact laid-out diff text behind mouse selection and clipboard extraction.
    pub(crate) diff_copy: Option<crate::render::DiffCopyFrame>,
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
            // Match the renderer exactly: top context, focused pane, then the combined
            // commands/usage/path footer. Geometry is the mouse contract, so it cannot
            // retain rows that the renderer no longer draws.
            let rows = ratatui::layout::Layout::vertical([
                ratatui::layout::Constraint::Length(1),
                ratatui::layout::Constraint::Min(3),
                ratatui::layout::Constraint::Length(1),
            ])
            .split(area);
            let body = rows[1];
            if !snap.status.text.is_empty() {
                geo.status = Some(crate::render::bottom_bar_chunks(rows[2], app, snap)[0]);
            }
            match app.focused {
                Pane::Files => {
                    geo.files = Some(body);
                    geo.files_inner = Some(inner(body));
                    // The files viewport is usable in focus mode too.
                    let rows_proj = app.projected_file_rows();
                    let cap = inner(body).height as usize;
                    let first = app.files_first_visible(cap);
                    geo.files_first_visible = first;
                    let fi = inner(body);
                    let mut rects = Vec::new();
                    for (slot, phys) in (first..rows_proj.len()).take(cap).enumerate() {
                        rects.push((Rect::new(fi.x, fi.y + slot as u16, fi.width, 1), phys));
                    }
                    geo.file_row_rects = rects;
                    geo.register_scroll_region(
                        ScrollRegionId::Files,
                        body,
                        first,
                        rows_proj.len().saturating_sub(cap),
                    );
                }
                Pane::Diff => {
                    geo.diff = Some(body);
                    geo.diff_copy = Some(crate::render::diff_copy_frame(app, snap, body));
                    geo.register_scroll_region(
                        ScrollRegionId::Diff,
                        body,
                        usize::from(app.diff_scroll),
                        snap.diff.rows.len().saturating_sub(1),
                    );
                }
                Pane::Impact => {
                    geo.impact = Some(body);
                    geo.add_impact_regions(body, app, snap);
                }
            }
            return geo;
        }
        // Normal: the renderer's four-row stack. Rows: top, work, impact, bottom.
        let rows = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Min(7),
            ratatui::layout::Constraint::Length(crate::layout::impact_height(
                app.dividers.get(DividerId::WorkReview),
                area.height,
            )),
            ratatui::layout::Constraint::Length(1),
        ])
        .split(area);
        if !snap.status.text.is_empty() {
            geo.status = Some(crate::render::bottom_bar_chunks(rows[3], app, snap)[0]);
        }
        let work = rows[1];
        let impact = rows[2];
        let fw = files_width(app.dividers.get(DividerId::FilesDiff), work.width);
        let work_split = ratatui::layout::Layout::horizontal([
            ratatui::layout::Constraint::Length(fw),
            ratatui::layout::Constraint::Min(MIN_DIFF_WIDTH),
        ])
        .split(work);
        geo.files = Some(work_split[0]);
        geo.files_inner = Some(inner(work_split[0]));
        geo.diff = Some(work_split[1]);
        geo.diff_copy = Some(crate::render::diff_copy_frame(app, snap, work_split[1]));
        geo.impact = Some(impact);
        geo.add_impact_regions(impact, app, snap);
        geo.register_scroll_region(
            ScrollRegionId::Diff,
            work_split[1],
            usize::from(app.diff_scroll),
            snap.diff.rows.len().saturating_sub(1),
        );

        // Drag handles cover BOTH adjacent border cells (the two visible border columns/
        // rows of the shared boundary), so a press on either pane's border arms the drag.
        // The vertical handle stops before the work row's bottom border to avoid the
        // T-junction with the horizontal handle (review 24 M2).
        let vx = work_split[1].x.saturating_sub(1);
        geo.dividers.push(DividerHandle::new(
            DividerId::FilesDiff,
            Rect::new(vx, work.y, 2, work.height.saturating_sub(1)),
            work_split[0].width,
        ));
        let hy = impact.y.saturating_sub(1);
        geo.dividers.push(DividerHandle::new(
            DividerId::WorkReview,
            Rect::new(area.x, hy, area.width, 2),
            impact.height,
        ));

        // Files viewport: project the rows, compute the visible slice.
        if let Some(fi) = geo.files_inner {
            let rows_proj = app.projected_file_rows();
            let cap = fi.height as usize;
            let first = app.files_first_visible(cap);
            geo.files_first_visible = first;
            let mut rects = Vec::new();
            for (slot, phys) in (first..rows_proj.len()).take(cap).enumerate() {
                rects.push((Rect::new(fi.x, fi.y + slot as u16, fi.width, 1), phys));
            }
            geo.file_row_rects = rects;
            geo.register_scroll_region(
                ScrollRegionId::Files,
                work_split[0],
                first,
                rows_proj.len().saturating_sub(cap),
            );
        }

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

    /// Whether a point is over the visible status-message portion of the bottom bar.
    pub(crate) fn status_at(&self, x: u16, y: u16) -> bool {
        self.status.is_some_and(|rect| hit(rect, x, y))
    }

    /// Whether a point is over the visible AI-failure banner.
    pub(crate) fn ai_failure_status_at(&self, x: u16, y: u16) -> bool {
        self.ai_failure_status.is_some_and(|rect| hit(rect, x, y))
    }

    /// The visible handle for one stable divider identity.
    pub(crate) fn divider(&self, id: DividerId) -> Option<DividerHandle> {
        self.dividers.iter().copied().find(|handle| handle.id == id)
    }

    /// Highest-precedence divider under a point. Horizontal boundaries win at their
    /// intersections with vertical ones, matching the visible row-wide sectional.
    pub(crate) fn divider_at(&self, x: u16, y: u16) -> Option<DividerHandle> {
        self.dividers
            .iter()
            .copied()
            .filter(|handle| hit(handle.rect, x, y))
            .min_by_key(|handle| match handle.id.axis() {
                DividerAxis::Horizontal => 0,
                DividerAxis::Vertical => 1,
            })
    }

    /// Scrollable region under a point, resolved from retained frame geometry.
    pub(crate) fn scroll_region_at(&self, x: u16, y: u16) -> Option<ScrollRegion> {
        self.scroll_regions
            .iter()
            .copied()
            .find(|region| hit(region.rect, x, y))
    }

    /// Generated-plan node under a point, resolved from the exact rendered span layout.
    pub(crate) fn plan_node_at(&self, x: u16, y: u16) -> Option<PlanNodeTarget> {
        self.plan_node_rects
            .iter()
            .find_map(|(rect, target)| hit(*rect, x, y).then(|| target.clone()))
    }

    /// Resolve a screen cell to the exact physical diff line/column displayed there.
    pub(crate) fn diff_text_point_at(
        &self,
        x: u16,
        y: u16,
    ) -> Option<crate::action::DiffTextPoint> {
        let frame = self.diff_copy.as_ref()?;
        if !hit(frame.rect, x, y) {
            return None;
        }
        let row = frame
            .first_visible
            .saturating_add(usize::from(y.saturating_sub(frame.rect.y)));
        let line = frame.lines.get(row)?;
        let code_start = frame.code_starts.get(row).copied().flatten()?;
        let line_width = line.width();
        Some(crate::action::DiffTextPoint {
            row,
            column: usize::from(x.saturating_sub(frame.rect.x))
                .max(code_start.min(line_width))
                .min(line_width),
        })
    }

    /// Extract a linear display-cell selection from the same retained diff frame.
    pub(crate) fn selected_diff_text(&self, selection: crate::action::DiffTextSelection) -> String {
        let Some(frame) = &self.diff_copy else {
            return String::new();
        };
        let (start, end) = if (selection.start.row, selection.start.column)
            <= (selection.end.row, selection.end.column)
        {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
        let mut selected = Vec::new();
        for row in start.row..=end.row {
            let Some(line) = frame.lines.get(row) else {
                continue;
            };
            let Some(code_start) = frame.code_starts.get(row).copied().flatten() else {
                continue;
            };
            let line_width = line.width();
            let from = if row == start.row {
                start.column.max(code_start).min(line_width)
            } else {
                code_start.min(line_width)
            };
            let to = if row == end.row {
                end.column.saturating_add(1).min(line_width)
            } else {
                line_width
            };
            selected.push(slice_display_cells(line, from, to));
        }
        selected.join("\n")
    }

    fn register_scroll_region(
        &mut self,
        id: ScrollRegionId,
        rect: Rect,
        offset: usize,
        max_offset: usize,
    ) {
        if rect.width > 0 && rect.height > 0 {
            self.scroll_regions
                .push(ScrollRegion::new(id, rect, offset, max_offset));
        }
    }

    /// Register the generated split and the two sectionals within the deterministic
    /// relationship stack. The same helper serves normal and focus-only layouts.
    fn add_impact_regions(&mut self, impact: Rect, app: &App, snap: &UiSnapshot) {
        let content = inner(impact);
        if content.width < 2 || content.height == 0 {
            return;
        }
        let left_width = impact_left_width(
            app.dividers.get(DividerId::RelationshipsGenerated),
            content.width,
        );
        let divider_x = content.x.saturating_add(left_width);
        self.dividers.push(DividerHandle::new(
            DividerId::RelationshipsGenerated,
            Rect::new(
                divider_x.saturating_sub(1),
                content.y,
                2.min(content.width),
                content.height,
            ),
            left_width,
        ));

        let [selected, callers, downstream] = impact_section_heights(
            app.dividers.get(DividerId::SelectedCallers),
            app.dividers.get(DividerId::CallersDownstream),
            content.height,
        );
        let left = Rect::new(content.x, content.y, left_width, content.height);
        let rows = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Length(selected),
            ratatui::layout::Constraint::Length(callers),
            ratatui::layout::Constraint::Length(downstream),
        ])
        .split(left);
        let callers_capacity = impact_list_capacity(rows[1].height, true);
        let downstream_capacity = impact_list_capacity(rows[2].height, false);
        self.register_scroll_region(
            ScrollRegionId::Callers,
            rows[1],
            app.callers_scroll,
            scroll_max(snap.impact.callers.rows.len(), callers_capacity),
        );
        self.register_scroll_region(
            ScrollRegionId::Downstream,
            rows[2],
            app.downstream_scroll,
            scroll_max(snap.impact.downstream.rows.len(), downstream_capacity),
        );

        let generated = Rect::new(
            divider_x,
            content.y,
            content.width.saturating_sub(left_width),
            content.height,
        );
        // The generated Block owns a left border and one cell of left padding.
        let generated_inner = Rect::new(
            generated.x.saturating_add(2),
            generated.y,
            generated.width.saturating_sub(2),
            generated.height,
        );
        let generated_lines =
            crate::render::generated_impact_content(app, snap, generated_inner.width);
        let viewport = usize::from(generated_inner.height);
        let max_scroll = scroll_max(generated_lines.len(), viewport);
        let first = app.ai_plan_scroll.min(max_scroll);
        for (screen_row, line) in generated_lines
            .iter()
            .skip(first)
            .take(viewport)
            .enumerate()
        {
            let mut x = generated_inner.x;
            let right = generated_inner.x.saturating_add(generated_inner.width);
            if first + screen_row == 0 && snap.ai_failure_status().is_some() {
                let line_width = line
                    .spans
                    .iter()
                    .map(|span| span.text.width())
                    .sum::<usize>();
                let visible_width = u16::try_from(line_width)
                    .unwrap_or(u16::MAX)
                    .min(generated_inner.width);
                if visible_width > 0 {
                    self.ai_failure_status = Some(Rect::new(
                        generated_inner.x,
                        generated_inner.y.saturating_add(screen_row as u16),
                        visible_width,
                        1,
                    ));
                }
            }
            for span in &line.spans {
                let requested = u16::try_from(span.text.width()).unwrap_or(u16::MAX);
                let width = requested.min(right.saturating_sub(x));
                if width > 0 {
                    if let Some(target) = &span.target {
                        self.plan_node_rects.push((
                            Rect::new(
                                x,
                                generated_inner.y.saturating_add(screen_row as u16),
                                width,
                                1,
                            ),
                            target.clone(),
                        ));
                    }
                    x = x.saturating_add(width);
                }
                if x >= right {
                    break;
                }
            }
        }
        self.register_scroll_region(
            ScrollRegionId::GeneratedImpact,
            generated,
            app.ai_plan_scroll,
            max_scroll,
        );
        register_horizontal_sectional(
            &mut self.dividers,
            DividerId::SelectedCallers,
            rows[0],
            rows[1],
        );
        register_horizontal_sectional(
            &mut self.dividers,
            DividerId::CallersDownstream,
            rows[1],
            rows[2],
        );
    }
}

fn impact_list_capacity(section_height: u16, bottom_divider: bool) -> usize {
    usize::from(section_height)
        .saturating_sub(usize::from(bottom_divider))
        .saturating_sub(1) // header
}

fn scroll_max(content_len: usize, viewport_len: usize) -> usize {
    if viewport_len == 0 {
        0
    } else {
        content_len.saturating_sub(viewport_len)
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

fn slice_display_cells(text: &str, from: usize, to: usize) -> String {
    if from >= to {
        return String::new();
    }
    let mut out = String::new();
    let mut column = 0usize;
    for ch in text.chars() {
        let width = ch.width().unwrap_or(0);
        let end = column.saturating_add(width);
        if column < to && end > from {
            out.push(ch);
        }
        column = end;
        if column >= to {
            break;
        }
    }
    out
}

/// Add a two-cell hit target around one visible horizontal section boundary.
fn register_horizontal_sectional(
    handles: &mut Vec<DividerHandle>,
    id: DividerId,
    before: Rect,
    after: Rect,
) {
    if before.height == 0 || after.height == 0 || before.width == 0 {
        return;
    }
    handles.push(DividerHandle::new(
        id,
        Rect::new(before.x, after.y.saturating_sub(1), before.width, 2),
        before.height,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_geometry_matches_the_four_rendered_rows() {
        let app = App::new();
        let geometry = UiGeometry::build(Rect::new(0, 0, 140, 40), &app, &UiSnapshot::default());
        assert_eq!(geometry.files, Some(Rect::new(0, 1, 42, 22)));
        assert_eq!(geometry.diff, Some(Rect::new(42, 1, 98, 22)));
        assert_eq!(geometry.impact, Some(Rect::new(0, 23, 140, 16)));
        assert_eq!(geometry.pane_at(2, 0), None, "top bar is not a pane");
        assert_eq!(geometry.pane_at(2, 39), None, "bottom bar is not a pane");
    }

    #[test]
    fn focus_only_geometry_reserves_only_top_and_bottom_bars() {
        let app = App::new();
        let geometry = UiGeometry::build(Rect::new(0, 0, 79, 40), &app, &UiSnapshot::default());
        assert_eq!(geometry.files, Some(Rect::new(0, 1, 79, 38)));
        assert_eq!(geometry.pane_at(2, 0), None, "top bar is not clickable");
        assert_eq!(geometry.pane_at(2, 39), None, "bottom bar is not clickable");
    }

    #[test]
    fn status_geometry_covers_only_the_message_side_of_the_bottom_bar() {
        let mut snap = UiSnapshot::default();
        snap.status.text = "provider error".to_string();
        snap.files.push(crate::snapshot::FileRow {
            path: "src/main.rs".to_string(),
            status: "M",
            changed_symbol_count: 0,
            added_lines: 0,
            removed_lines: 0,
            symbols: Vec::new(),
            expanded: false,
            semantic: crate::snapshot::FileSemanticLoad::Unsupported,
        });
        let app = App::new();
        let geometry = UiGeometry::build(Rect::new(0, 0, 140, 40), &app, &snap);
        let status = geometry.status.expect("status hit target");
        assert!(geometry.status_at(status.x, status.y));
        assert!(
            !geometry.status_at(139, 39),
            "right-side usage/path is inert"
        );
    }
}
