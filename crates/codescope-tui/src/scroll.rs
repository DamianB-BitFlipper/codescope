//! Generic hover-scroll regions retained with the frame geometry.
//!
//! Rendering registers every independently scrollable rectangle with its displayed offset
//! and maximum. Mouse routing then resolves the region under the pointer and emits one
//! absolute setter. This keeps wheel behavior independent of keyboard focus and prevents
//! input handling from recomputing layout or guessing width-dependent scroll bounds.

use ratatui::layout::Rect;

/// Stable identity of one independently scrollable UI region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollRegionId {
    /// Changed files and their expanded symbols.
    Files,
    /// Focused source diff.
    Diff,
    /// Incoming callers in the deterministic relationship stack.
    Callers,
    /// Outgoing/downstream relationships in the deterministic relationship stack.
    Downstream,
    /// Generated or deterministic visual explanation on the right of Impact.
    GeneratedImpact,
}

/// One scrollable rectangle as it appeared in the last rendered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollRegion {
    pub(crate) id: ScrollRegionId,
    pub(crate) rect: Rect,
    pub(crate) offset: usize,
    pub(crate) max_offset: usize,
}

impl ScrollRegion {
    pub(crate) fn new(id: ScrollRegionId, rect: Rect, offset: usize, max_offset: usize) -> Self {
        Self {
            id,
            rect,
            offset: offset.min(max_offset),
            max_offset,
        }
    }

    /// Return the clamped absolute offset after a wheel delta, or `None` when the region
    /// cannot move in that direction. Positive deltas move toward later content.
    pub(crate) fn scrolled_offset(self, delta: i32) -> Option<usize> {
        let next = if delta < 0 {
            self.offset.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.offset
                .saturating_add(delta as usize)
                .min(self.max_offset)
        };
        (next != self.offset).then_some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_math_clamps_and_reports_only_real_movement() {
        let region = ScrollRegion::new(ScrollRegionId::Diff, Rect::new(1, 2, 3, 4), 5, 8);
        assert_eq!(region.scrolled_offset(3), Some(8));
        assert_eq!(region.scrolled_offset(-3), Some(2));
        assert_eq!(
            ScrollRegion {
                offset: 8,
                ..region
            }
            .scrolled_offset(3),
            None
        );
        assert_eq!(
            ScrollRegion {
                offset: 0,
                ..region
            }
            .scrolled_offset(-3),
            None
        );
    }
}
