//! Error type for the (few) fallible constructors in this crate.

/// Errors produced by checked constructors in `codescope-core`.
///
/// Pure data validation only — this crate performs no I/O, so there are no I/O errors here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    /// A [`LineRange`](crate::LineRange) whose end precedes its start.
    #[error("invalid range: end ({end_line}:{end_col}) precedes start ({start_line}:{start_col})")]
    InvalidRange {
        /// Start line (zero-based).
        start_line: u32,
        /// Start column (zero-based, UTF-8 code units).
        start_col: u32,
        /// End line (zero-based).
        end_line: u32,
        /// End column (zero-based, UTF-8 code units).
        end_col: u32,
    },

    /// A [`FileId`](crate::FileId) must be a repo-relative path; an absolute path was given.
    #[error("file id must be repo-relative, got absolute path: {0}")]
    AbsolutePath(String),
}
