//! The responsive layout tier system (docs/review/15 §1).
//!
//! A pure function of viewport size + zoom state → which pane arrangement to render.
//! There is exactly one master-detail arrangement at normal sizes; smaller terminals
//! (or an explicit zoom) get a focus-only fallback that keeps the chrome rows.
//! Character cells, not aspect ratio (fonts vary).

use ratatui::layout::Rect;

/// Which pane arrangement to render for the current viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Terminal too small to be usable: show only the size message.
    TooSmall,
    /// The focused pane gets the whole body between the chrome rows (explicit zoom, or
    /// the viewport is below the normal minimum).
    FocusOnly,
    /// The reference master-detail layout (docs/review/15 §1.1): top bar, summary bar,
    /// files+diff work row, full-width Impact pane, status bar, help bar.
    Normal,
}

/// Default width of the files pane in the normal layout (`App::files_width` starts here).
pub const DEFAULT_FILES_WIDTH: u16 = 42;
/// Narrowest the files pane can be resized to with `[`.
pub const MIN_FILES_WIDTH: u16 = 28;
/// Widest the files pane can be resized to with `]`.
pub const MAX_FILES_WIDTH: u16 = 56;
/// The diff pane never gets narrower than this; the files width yields first.
pub const MIN_DIFF_WIDTH: u16 = 48;
/// Height of the full-width Impact pane, including its border.
pub const IMPACT_HEIGHT: u16 = 9;

/// Choose the layout tier for a viewport of `area` cells.
///
/// Hard stop below 30x8. Zoom always wins (an explicit full-body inspection is honored at
/// every usable size). The normal layout needs at least 80x20: the six chrome/fixed rows
/// (1+1+9+1+1) plus the minimum seven-row work area exactly fill 20 lines, and the
/// files+diff split needs 80 columns to keep the diff at or above [`MIN_DIFF_WIDTH`].
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
#[must_use]
pub fn files_width(request: u16, work_width: u16) -> u16 {
    request
        .clamp(MIN_FILES_WIDTH, MAX_FILES_WIDTH)
        .min(work_width.saturating_sub(MIN_DIFF_WIDTH))
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
        assert_eq!(files_width(200, 140), MAX_FILES_WIDTH);
        // At 80 columns the diff's 48-cell minimum wins over the requested 42.
        assert_eq!(files_width(42, 80), 32);
        assert_eq!(files_width(56, 80), 32);
        // Never panics on a pathological width.
        assert_eq!(files_width(42, 10), 0);
    }
}
