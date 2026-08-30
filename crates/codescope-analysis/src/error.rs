//! Error type for the analysis layer.

use codescope_core::FileId;

/// Errors produced by the analysis layer.
///
/// The pure mapping functions are infallible; errors come from the orchestration layer
/// ([`crate::engine`]) talking to git and the language service.
#[derive(Debug, thiserror::Error)]
pub enum AnalysisError {
    /// A git query failed.
    #[error("git error: {0}")]
    Git(#[from] codescope_git::GitError),

    /// A semantic query failed in a way that is not survivable for this analysis pass
    /// (feature-gated queries that merely lack support degrade instead of erroring).
    #[error("language service error: {0}")]
    Semantic(#[from] codescope_lsp::SemanticError),

    /// A repo path could not be expressed as a repo-relative [`FileId`].
    #[error("invalid repo-relative path: {0}")]
    InvalidPath(#[from] codescope_core::CoreError),

    /// The file is not part of the analysed change-set.
    #[error("file not in change-set: {0}")]
    UnknownFile(FileId),
}
