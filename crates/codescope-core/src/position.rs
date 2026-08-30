//! Internal position model: zero-based line/column, column in UTF-8 code units.
//!
//! The LSP layer converts these to the per-session negotiated encoding (UTF-16 for gopls)
//! at the wire boundary only (research 01, decision 1). Ranges use **inclusive** start and
//! end bounds for containment: a symbol's extent includes its end position (typically the
//! closing brace), so containment/intersection here treat `end` as part of the range.

use crate::error::CoreError;
use std::cmp::Ordering;

/// A position in a text document: zero-based `line`, zero-based `col` in UTF-8 code units.
///
/// Orders lexicographically by `(line, col)`.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct Position {
    /// Zero-based line.
    pub line: u32,
    /// Zero-based column, counted in UTF-8 code units (bytes).
    pub col: u32,
}

impl Position {
    /// Create a position.
    #[must_use]
    pub fn new(line: u32, col: u32) -> Self {
        Position { line, col }
    }

    /// Convert an LSP position that is **already in UTF-8 columns**.
    ///
    /// The caller (the LSP client) must have converted the wire value from the session's
    /// negotiated encoding to UTF-8 code units first; this is a field rename, not an
    /// encoding conversion.
    #[must_use]
    pub fn from_lsp(pos: lsp_types::Position) -> Self {
        Position {
            line: pos.line,
            col: pos.character,
        }
    }

    /// View this position as an LSP position in UTF-8 columns (see [`Position::from_lsp`]).
    #[must_use]
    pub fn to_lsp(self) -> lsp_types::Position {
        lsp_types::Position::new(self.line, self.col)
    }
}

/// A range in a text document: zero-based UTF-8 start/end, end **inclusive** for the
/// containment helpers in this crate.
///
/// Serializes flat as `{"start_line":..,"start_col":..,"end_line":..,"end_col":..}`, which
/// matches the AI plan schema (research 05 §2).
///
/// Ordering is lexicographic by `(start_line, start_col, end_line, end_col)`, so sorting
/// ranges orders by start position first.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct LineRange {
    /// Zero-based start line.
    pub start_line: u32,
    /// Zero-based start column (UTF-8 code units).
    pub start_col: u32,
    /// Zero-based end line.
    pub end_line: u32,
    /// Zero-based end column (UTF-8 code units).
    pub end_col: u32,
}

impl LineRange {
    /// Create a range without validating that `end >= start`.
    ///
    /// Prefer [`LineRange::checked`] at trust boundaries; symbol ranges from a language
    /// server are expected to be well-formed.
    #[must_use]
    pub fn new(start_line: u32, start_col: u32, end_line: u32, end_col: u32) -> Self {
        LineRange {
            start_line,
            start_col,
            end_line,
            end_col,
        }
    }

    /// Create a range, rejecting an end that precedes the start.
    pub fn checked(
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
    ) -> Result<Self, CoreError> {
        let range = LineRange::new(start_line, start_col, end_line, end_col);
        if range.end() < range.start() {
            return Err(CoreError::InvalidRange {
                start_line,
                start_col,
                end_line,
                end_col,
            });
        }
        Ok(range)
    }

    /// A zero-width range at `pos`.
    #[must_use]
    pub fn point(pos: Position) -> Self {
        LineRange::new(pos.line, pos.col, pos.line, pos.col)
    }

    /// A line-granularity range from zero-based `start_line` to `end_line` (inclusive),
    /// with columns zeroed.
    ///
    /// Hunks carry no columns, so hunk↔symbol comparisons should use line spans; see
    /// [`LineRange::contains_lines`].
    #[must_use]
    pub fn from_line_span(start_line: u32, end_line: u32) -> Self {
        LineRange::new(start_line, 0, end_line, 0)
    }

    /// Start position.
    #[must_use]
    pub fn start(&self) -> Position {
        Position::new(self.start_line, self.start_col)
    }

    /// End position.
    #[must_use]
    pub fn end(&self) -> Position {
        Position::new(self.end_line, self.end_col)
    }

    /// `(start_line, end_line)` line span, columns ignored.
    #[must_use]
    pub fn line_span(&self) -> (u32, u32) {
        (self.start_line, self.end_line)
    }

    /// `true` if `end >= start`.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.end() >= self.start()
    }

    /// `true` if `pos` lies within this range (inclusive bounds).
    #[must_use]
    pub fn contains_pos(&self, pos: Position) -> bool {
        self.start() <= pos && pos <= self.end()
    }

    /// `true` if `other` is fully inside this range (inclusive bounds).
    #[must_use]
    pub fn contains_range(&self, other: &LineRange) -> bool {
        self.start() <= other.start() && other.end() <= self.end()
    }

    /// `true` if the two ranges share at least one position (inclusive bounds).
    #[must_use]
    pub fn intersects(&self, other: &LineRange) -> bool {
        self.start() <= other.end() && other.start() <= self.end()
    }

    /// Line-granularity containment: ignores columns entirely.
    ///
    /// This is the right comparison for mapping diff hunks (which have no columns) against
    /// symbol extents.
    #[must_use]
    pub fn contains_lines(&self, other: &LineRange) -> bool {
        self.start_line <= other.start_line && other.end_line <= self.end_line
    }

    /// Line-granularity intersection: ignores columns entirely.
    #[must_use]
    pub fn intersects_lines(&self, other: &LineRange) -> bool {
        self.start_line <= other.end_line && other.start_line <= self.end_line
    }

    /// Number of lines spanned (`end_line - start_line`); `0` for a single-line range.
    #[must_use]
    pub fn len_lines(&self) -> u32 {
        self.end_line.saturating_sub(self.start_line)
    }

    /// `true` if the range spans exactly one line.
    #[must_use]
    pub fn is_single_line(&self) -> bool {
        self.start_line == self.end_line
    }

    /// Convert an LSP range that is **already in UTF-8 columns** (see [`Position::from_lsp`]).
    #[must_use]
    pub fn from_lsp(range: lsp_types::Range) -> Self {
        LineRange {
            start_line: range.start.line,
            start_col: range.start.character,
            end_line: range.end.line,
            end_col: range.end.character,
        }
    }

    /// View this range as an LSP range in UTF-8 columns (see [`Position::from_lsp`]).
    #[must_use]
    pub fn to_lsp(self) -> lsp_types::Range {
        lsp_types::Range::new(self.start().to_lsp(), self.end().to_lsp())
    }
}

/// Total ordering helper for `Option<LineRange>`-free code: compares two ranges by start,
/// then end. Equivalent to the derived `Ord`; provided for readable `sort_by` calls.
#[must_use]
pub fn compare_ranges(a: &LineRange, b: &LineRange) -> Ordering {
    a.cmp(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_ordering_is_lexicographic() {
        assert!(Position::new(1, 99) < Position::new(2, 0));
        assert!(Position::new(1, 5) < Position::new(1, 6));
    }

    #[test]
    fn checked_rejects_inverted_range() {
        let err = LineRange::checked(10, 4, 9, 0).unwrap_err();
        assert!(matches!(err, CoreError::InvalidRange { .. }));
        assert!(LineRange::checked(10, 4, 10, 4).is_ok());
    }

    #[test]
    fn containment_is_inclusive() {
        let outer = LineRange::new(10, 0, 20, 1);
        let inner = LineRange::new(12, 2, 18, 0);
        assert!(outer.contains_range(&inner));
        assert!(!inner.contains_range(&outer));
        // Boundary-inclusive: a range contains itself.
        assert!(outer.contains_range(&outer));
        assert!(outer.contains_pos(Position::new(20, 1)));
        assert!(!outer.contains_pos(Position::new(20, 2)));
    }

    #[test]
    fn intersection() {
        let a = LineRange::new(10, 0, 20, 0);
        assert!(a.intersects(&LineRange::new(20, 0, 30, 0))); // touching at end
        assert!(!a.intersects(&LineRange::new(21, 0, 30, 0)));
    }

    #[test]
    fn line_granularity_ignores_columns() {
        let sym = LineRange::new(10, 4, 20, 1); // starts at col 4
        let hunk = LineRange::from_line_span(10, 20);
        assert!(!sym.contains_range(&hunk)); // col 0 < col 4
        assert!(sym.contains_lines(&hunk)); // line-level: contained
        assert!(sym.intersects_lines(&hunk));
    }

    #[test]
    fn lsp_round_trip() {
        let r = LineRange::new(1, 2, 3, 4);
        assert_eq!(LineRange::from_lsp(r.to_lsp()), r);
        let p = Position::new(5, 6);
        assert_eq!(Position::from_lsp(p.to_lsp()), p);
    }

    #[test]
    fn line_range_serializes_flat() {
        let v = serde_json::to_value(LineRange::new(1, 2, 3, 4)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"start_line":1,"start_col":2,"end_line":3,"end_col":4})
        );
    }
}
