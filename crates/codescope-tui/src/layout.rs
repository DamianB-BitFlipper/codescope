//! The responsive layout tier system (docs/review/13 §"Exact layout tiers").
//!
//! A pure function of viewport size + zoom state → which pane arrangement to render.
//! Width buys readable pane widths; height decides whether a vertical stack can afford three
//! bordered panes. Character cells, not aspect ratio (fonts vary).

use ratatui::layout::Rect;

/// Which pane arrangement to render for the current viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Terminal too small to be usable: show only the size message.
    TooSmall,
    /// Focused pane gets the whole `main` area (explicit zoom, or too narrow to split).
    FocusOnly,
    /// Medium width: files + one detail slot (diff normally; relations when Semantic focused).
    Medium,
    /// Tall and medium width: three-pane vertical stack, all at full width.
    TallStack,
    /// Wide: three columns (files / diff / relations).
    Spacious,
}

/// Choose the layout tier for a viewport of `area` cells.
///
/// Zoom always wins (an explicit full-main inspection is honored at every usable size).
/// Thresholds per the spec: too-small below 30×8; spacious at width ≥ 150; the tall stack at
/// width 48–149 when the main area has ≥ 34 rows; medium at width ≥ 80; else focus-only.
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
    // Outer chrome: two one-line bars when height allows, else the top bar alone.
    let main_h = if h >= 12 { h - 2 } else { h - 1 };
    if w >= 150 {
        return Tier::Spacious;
    }
    if (48..150).contains(&w) && main_h >= 34 {
        return Tier::TallStack;
    }
    if w >= 80 {
        return Tier::Medium;
    }
    Tier::FocusOnly
}

/// The split for the main content area in each tier, as Ratatui constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainSplit {
    /// FocusOnly: the focused pane gets everything.
    Single,
    /// TallStack: vertical files(10) / diff(rest) / relations(10).
    Vertical,
    /// Medium: horizontal files(32) / detail(rest).
    FilesDetail,
    /// Spacious: horizontal files(38) / diff(72 min) / relations(40).
    Columns,
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
    }

    #[test]
    fn spacious_at_150() {
        assert_eq!(tier(150, 36), Tier::Spacious);
        assert_eq!(tier(200, 40), Tier::Spacious);
    }

    #[test]
    fn tall_stack_needs_width_and_height() {
        assert_eq!(tier(100, 36), Tier::TallStack);   // 100 wide, 34 main rows
        assert_eq!(tier(100, 35), Tier::Medium);      // not tall enough for the stack
        assert_eq!(tier(48, 36), Tier::TallStack);    // narrow but tall enough
        assert_eq!(tier(47, 60), Tier::FocusOnly);    // too narrow for the stack
    }

    #[test]
    fn medium_at_80() {
        assert_eq!(tier(80, 30), Tier::Medium);
        assert_eq!(tier(79, 30), Tier::FocusOnly);
    }

    #[test]
    fn zoom_always_wins() {
        assert_eq!(choose_tier(Rect::new(0, 0, 200, 50), true), Tier::FocusOnly);
        assert_eq!(choose_tier(Rect::new(0, 0, 40, 20), true), Tier::FocusOnly);
    }

    #[test]
    fn short_height_drops_footer_then_stacks() {
        // h=10: footer dropped (main_h = 9), tall stack needs 34 → medium at 80+.
        assert_eq!(tier(100, 10), Tier::Medium);
        assert_eq!(tier(50, 10), Tier::FocusOnly);
    }
}
