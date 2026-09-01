//! Generic resizable-divider identities, preferences, and live drag handles.
//!
//! Layout code registers the dividers visible in the current frame. Mouse routing treats
//! every registered divider identically; axis and extent direction live on the identity,
//! so new splits do not require another drag-state/action branch.

use ratatui::layout::Rect;

/// Every user-resizable structural divider in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum DividerId {
    /// Changed files before the focused diff.
    FilesDiff = 0,
    /// Work area before the bottom review area (whose trailing height is stored).
    WorkReview = 1,
    /// Deterministic relationships before the generated explanation.
    RelationshipsGenerated = 2,
    /// Selected-change section before callers.
    SelectedCallers = 3,
    /// Callers section before downstream relationships.
    CallersDownstream = 4,
}

impl DividerId {
    /// All divider identities in stable persistence/render precedence order.
    pub const ALL: [Self; 5] = [
        Self::FilesDiff,
        Self::WorkReview,
        Self::RelationshipsGenerated,
        Self::SelectedCallers,
        Self::CallersDownstream,
    ];

    /// Stable key used in `[ui.dividers]` global configuration.
    #[must_use]
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::FilesDiff => "files_diff",
            Self::WorkReview => "work_review",
            Self::RelationshipsGenerated => "relationships_generated",
            Self::SelectedCallers => "selected_callers",
            Self::CallersDownstream => "callers_downstream",
        }
    }

    /// Default requested leading/trailing extent in terminal cells.
    #[must_use]
    pub const fn default_extent(self) -> u16 {
        match self {
            Self::FilesDiff => crate::layout::DEFAULT_FILES_WIDTH,
            Self::WorkReview => crate::layout::DEFAULT_IMPACT_HEIGHT,
            Self::RelationshipsGenerated => crate::layout::DEFAULT_IMPACT_LEFT_WIDTH,
            Self::SelectedCallers => crate::layout::DEFAULT_SELECTED_CHANGE_HEIGHT,
            Self::CallersDownstream => crate::layout::DEFAULT_CALLERS_HEIGHT,
        }
    }

    /// Safe preference floor; live layout may yield further in a constrained viewport.
    #[must_use]
    pub const fn minimum_extent(self) -> u16 {
        match self {
            Self::FilesDiff => crate::layout::MIN_FILES_WIDTH,
            Self::WorkReview => crate::layout::MIN_IMPACT_HEIGHT,
            Self::RelationshipsGenerated => crate::layout::MIN_IMPACT_LEFT_WIDTH,
            Self::SelectedCallers => crate::layout::MIN_SELECTED_CHANGE_HEIGHT,
            Self::CallersDownstream => crate::layout::MIN_RELATION_SECTION_HEIGHT,
        }
    }

    pub(crate) const fn axis(self) -> DividerAxis {
        match self {
            Self::FilesDiff | Self::RelationshipsGenerated => DividerAxis::Vertical,
            Self::WorkReview | Self::SelectedCallers | Self::CallersDownstream => {
                DividerAxis::Horizontal
            }
        }
    }

    const fn extent_side(self) -> ExtentSide {
        match self {
            Self::WorkReview => ExtentSide::After,
            _ => ExtentSide::Before,
        }
    }
}

/// Requested extents for every divider, indexed by [`DividerId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DividerSizes {
    values: [u16; DividerId::ALL.len()],
}

impl DividerSizes {
    /// Read one requested extent.
    #[must_use]
    pub const fn get(self, id: DividerId) -> u16 {
        self.values[id as usize]
    }

    /// Store one requested extent, clamped only to its stable safety floor.
    pub fn set(&mut self, id: DividerId, extent: u16) {
        self.values[id as usize] = extent.max(id.minimum_extent());
    }
}

impl Default for DividerSizes {
    fn default() -> Self {
        let mut values = [0; DividerId::ALL.len()];
        for id in DividerId::ALL {
            values[id as usize] = id.default_extent();
        }
        Self { values }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DividerAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtentSide {
    Before,
    After,
}

/// One divider actually visible in the last rendered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DividerHandle {
    pub(crate) id: DividerId,
    pub(crate) rect: Rect,
    pub(crate) effective_extent: u16,
}

impl DividerHandle {
    pub(crate) const fn new(id: DividerId, rect: Rect, effective_extent: u16) -> Self {
        Self {
            id,
            rect,
            effective_extent,
        }
    }

    /// Convert pointer movement into the absolute requested extent. Most dividers size
    /// the region before them; WorkReview sizes the region after it, so its delta flips.
    pub(crate) fn resized_extent_from(
        self,
        start_extent: u16,
        start_x: u16,
        start_y: u16,
        current_x: u16,
        current_y: u16,
    ) -> u16 {
        let delta = match self.id.axis() {
            DividerAxis::Vertical => i64::from(current_x) - i64::from(start_x),
            DividerAxis::Horizontal => i64::from(current_y) - i64::from(start_y),
        };
        let directed = match self.id.extent_side() {
            ExtentSide::Before => delta,
            ExtentSide::After => -delta,
        };
        (i64::from(start_extent) + directed).clamp(0, i64::from(u16::MAX)) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_divider_has_a_unique_stable_key_and_valid_default() {
        let mut keys = std::collections::HashSet::new();
        let sizes = DividerSizes::default();
        for id in DividerId::ALL {
            assert!(keys.insert(id.config_key()));
            assert!(sizes.get(id) >= id.minimum_extent());
        }
    }

    #[test]
    fn handle_math_comes_from_axis_and_extent_side() {
        let vertical = DividerHandle::new(DividerId::FilesDiff, Rect::new(0, 0, 2, 10), 40);
        assert_eq!(vertical.resized_extent_from(40, 10, 4, 15, 99), 45);

        let leading_horizontal =
            DividerHandle::new(DividerId::SelectedCallers, Rect::new(0, 0, 10, 2), 4);
        assert_eq!(leading_horizontal.resized_extent_from(4, 4, 10, 99, 12), 6);

        let trailing_horizontal =
            DividerHandle::new(DividerId::WorkReview, Rect::new(0, 0, 10, 2), 9);
        assert_eq!(trailing_horizontal.resized_extent_from(9, 4, 10, 99, 8), 11);
    }
}
