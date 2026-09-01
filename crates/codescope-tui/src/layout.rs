//! The responsive layout tier system (docs/review/15 §1).
//!
//! A pure function of viewport size + zoom state → which pane arrangement to render.
//! There is exactly one master-detail arrangement at normal sizes; smaller terminals
//! (or an explicit zoom) get a focus-only fallback that keeps the chrome rows.
//! Character cells, not aspect ratio (fonts vary).

use ratatui::layout::Rect;

/// Which pane arrangement to render for the current viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// Terminal too small to be usable: show only the size message.
    #[default]
    TooSmall,
    /// The focused pane gets the whole body between the chrome rows (explicit zoom, or
    /// the viewport is below the normal minimum).
    FocusOnly,
    /// The reference master-detail layout: top context bar, files+diff work row,
    /// full-width Impact pane, and one combined commands/usage/path footer.
    Normal,
}

/// Default width of the files pane in the normal layout.
pub const DEFAULT_FILES_WIDTH: u16 = 42;
/// Narrowest the files pane can be resized to by dragging its divider.
pub const MIN_FILES_WIDTH: u16 = 28;
/// Preferred maximum width of the files pane before live layout constraints apply.
pub const MAX_FILES_WIDTH: u16 = 56;
/// The diff pane never gets narrower than this; the files width yields first.
pub const MIN_DIFF_WIDTH: u16 = 48;
/// Height of the full-width Impact pane, including its border. The default gives the
/// generated half enough inner rows for the header block (title, intent, trust notes)
/// plus several visual lines; smaller terminals clamp back toward the minimum.
pub const IMPACT_HEIGHT: u16 = 16;
/// Default requested Impact-pane height.
pub const DEFAULT_IMPACT_HEIGHT: u16 = 16;
/// Minimum Impact-pane height.
pub const MIN_IMPACT_HEIGHT: u16 = 5;
/// Maximum Impact-pane height.
pub const MAX_IMPACT_HEIGHT: u16 = 18;
/// Minimum height of the work (files+diff) row the Impact pane may not consume.
pub const MIN_WORK_HEIGHT: u16 = 7;
/// Default width of the deterministic relationship stack inside Impact.
pub const DEFAULT_IMPACT_LEFT_WIDTH: u16 = 52;
/// Preferred minimum width of the deterministic relationship stack.
pub const MIN_IMPACT_LEFT_WIDTH: u16 = 24;
/// Preferred minimum width of the generated Impact breakdown.
pub const MIN_GENERATED_IMPACT_WIDTH: u16 = 36;
/// Preferred selected-change section height inside the deterministic relationship stack.
pub const DEFAULT_SELECTED_CHANGE_HEIGHT: u16 = 4;
/// Preferred callers section height inside the deterministic relationship stack.
pub const DEFAULT_CALLERS_HEIGHT: u16 = 5;
/// Minimum useful selected-change section height.
pub const MIN_SELECTED_CHANGE_HEIGHT: u16 = 3;
/// Minimum useful height of callers and downstream sections.
pub const MIN_RELATION_SECTION_HEIGHT: u16 = 2;

/// Choose the layout tier for a viewport of `area` cells.
///
/// Hard stop below 30x8. Zoom always wins (an explicit full-body inspection is honored at
/// every usable size). The normal layout needs at least 80x20: two chrome rows, the
/// minimum seven-row work area, and room for the Impact pane; the files+diff split needs
/// 80 columns to keep the diff at or above [`MIN_DIFF_WIDTH`].
#[must_use]
pub fn choose_tier(area: Rect, zoomed: bool) -> Tier {
    let w = area.width;
    let h = area.height;
    if w < 30 || h < 8 {
        return Tier::TooSmall;
    }
    if zoomed {
        return Tier::FocusOnly;
    }
    if w >= 80 && h >= 20 {
        return Tier::Normal;
    }
    Tier::FocusOnly
}

/// The files-pane width for a work row of `work_width` cells, given the App-owned
/// `request` (docs/review/15 §1.1): the request is clamped to
/// [`MIN_FILES_WIDTH`..=`MAX_FILES_WIDTH`] and then yields to [`MIN_DIFF_WIDTH`] without
/// changing the stored preference.
/// The Impact-pane height for a frame of `frame_height` rows, given the App-owned
/// `request`: clamped to `MIN_IMPACT_HEIGHT..=MAX_IMPACT_HEIGHT`, then yields to the
/// `MIN_WORK_HEIGHT` work area after reserving the top bar and combined bottom bar.
#[must_use]
pub fn impact_height(request: u16, frame_height: u16) -> u16 {
    let available = frame_height.saturating_sub(1 + MIN_WORK_HEIGHT + 1);
    // No arbitrary maximum: the Impact pane may grow until the work area hits its
    // minimum. A small floor keeps the pane grabbable.
    request
        .max(MIN_IMPACT_HEIGHT)
        .min(available.max(MIN_IMPACT_HEIGHT))
}

/// The files-pane width for a work row of `work_width` cells, given the App-owned
/// `request`: clamped to the fixed range, then yields to the diff's minimum.
#[must_use]
pub fn files_width(request: u16, work_width: u16) -> u16 {
    // No arbitrary maximum: the files pane may grow until the diff hits its own minimum.
    // A small floor keeps the pane grabbable (a zero-width pane's drag handle vanishes).
    request.max(MIN_FILES_WIDTH).min(
        work_width
            .saturating_sub(MIN_DIFF_WIDTH)
            .max(MIN_FILES_WIDTH),
    )
}

/// Width of the deterministic left half inside the Impact pane. At ordinary terminal
/// sizes both halves retain their useful minimum; constrained focus-only layouts split
/// the available cells evenly so generated content never disappears completely.
#[must_use]
pub fn impact_left_width(request: u16, content_width: u16) -> u16 {
    if content_width < MIN_IMPACT_LEFT_WIDTH + MIN_GENERATED_IMPACT_WIDTH {
        return content_width / 2;
    }
    request.max(MIN_IMPACT_LEFT_WIDTH).min(
        content_width
            .saturating_sub(MIN_GENERATED_IMPACT_WIDTH)
            .max(MIN_IMPACT_LEFT_WIDTH),
    )
}

/// Heights of selected-change, callers, and downstream sections. At useful heights the
/// first two honor their independent requests while reserving a minimum for everything
/// after them. Very short layouts degrade with the old deterministic 3/even split.
#[must_use]
pub fn impact_section_heights(
    selected_request: u16,
    callers_request: u16,
    total_height: u16,
) -> [u16; 3] {
    let required = MIN_SELECTED_CHANGE_HEIGHT + 2 * MIN_RELATION_SECTION_HEIGHT;
    if total_height < required {
        let selected = total_height.min(MIN_SELECTED_CHANGE_HEIGHT);
        let remaining = total_height.saturating_sub(selected);
        let callers = remaining / 2;
        return [selected, callers, remaining.saturating_sub(callers)];
    }

    let selected = selected_request
        .max(MIN_SELECTED_CHANGE_HEIGHT)
        .min(total_height.saturating_sub(2 * MIN_RELATION_SECTION_HEIGHT));
    let remaining = total_height.saturating_sub(selected);
    let callers = callers_request
        .max(MIN_RELATION_SECTION_HEIGHT)
        .min(remaining.saturating_sub(MIN_RELATION_SECTION_HEIGHT));
    [selected, callers, remaining.saturating_sub(callers)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier(w: u16, h: u16) -> Tier {
        choose_tier(Rect::new(0, 0, w, h), false)
    }

    #[test]
    fn too_small_boundaries() {
        assert_eq!(tier(29, 8), Tier::TooSmall);
        assert_eq!(tier(30, 7), Tier::TooSmall);
        assert_ne!(tier(30, 8), Tier::TooSmall);
        assert_eq!(tier(29, 40), Tier::TooSmall);
    }

    #[test]
    fn normal_needs_80x20() {
        assert_eq!(tier(80, 20), Tier::Normal);
        assert_eq!(tier(140, 40), Tier::Normal);
        assert_eq!(tier(200, 60), Tier::Normal);
        assert_eq!(tier(79, 40), Tier::FocusOnly);
        assert_eq!(tier(140, 19), Tier::FocusOnly);
    }

    #[test]
    fn zoom_always_wins() {
        assert_eq!(choose_tier(Rect::new(0, 0, 200, 50), true), Tier::FocusOnly);
        assert_eq!(choose_tier(Rect::new(0, 0, 40, 20), true), Tier::FocusOnly);
    }

    #[test]
    fn focus_only_covers_every_usable_small_size() {
        // Every size at or above the hard stop that is not normal is focus-only.
        assert_eq!(tier(30, 8), Tier::FocusOnly);
        assert_eq!(tier(79, 19), Tier::FocusOnly);
        assert_eq!(tier(60, 20), Tier::FocusOnly);
        assert_eq!(tier(80, 12), Tier::FocusOnly);
    }

    #[test]
    fn files_width_clamps_request_and_yields_to_diff() {
        assert_eq!(files_width(42, 140), 42);
        assert_eq!(files_width(10, 140), MIN_FILES_WIDTH);
        // No arbitrary maximum: a wide request is limited only by the diff's minimum.
        assert_eq!(files_width(200, 140), 140 - MIN_DIFF_WIDTH);
        // At 80 columns the diff's 48-cell minimum wins over the requested 42.
        assert_eq!(files_width(42, 80), 32);
        assert_eq!(files_width(56, 80), 32);
        // Pathological widths fall back to the floor, never panic or zero the pane.
        assert_eq!(files_width(42, 10), MIN_FILES_WIDTH);
    }

    #[test]
    fn impact_split_preserves_both_halves_and_degrades_evenly() {
        assert_eq!(impact_left_width(52, 138), 52);
        assert_eq!(impact_left_width(10, 100), MIN_IMPACT_LEFT_WIDTH);
        assert_eq!(
            impact_left_width(200, 100),
            100 - MIN_GENERATED_IMPACT_WIDTH
        );
        assert_eq!(impact_left_width(52, 40), 20);
    }

    #[test]
    fn impact_height_reserves_only_the_visible_chrome_and_work_floor() {
        assert_eq!(impact_height(12, 40), 12);
        assert_eq!(impact_height(12, 20), 11);
    }

    #[test]
    fn impact_sections_honor_both_requests_and_reserve_downstream() {
        assert_eq!(impact_section_heights(4, 5, 14), [4, 5, 5]);
        assert_eq!(impact_section_heights(8, 9, 14), [8, 4, 2]);
        assert_eq!(impact_section_heights(99, 99, 7), [3, 2, 2]);
        assert_eq!(impact_section_heights(4, 5, 3), [3, 0, 0]);
    }
}
