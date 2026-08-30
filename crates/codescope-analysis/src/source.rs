//! The semantic query surface the analysis layer consumes.
//!
//! `codescope-lsp`'s `LanguageService` (enum-dispatch over the gopls adapter) is the
//! runtime implementation; [`SemanticSource`] narrows it to exactly what analysis needs so
//! that unit tests can script responses without any server process (research 08) and the
//! analysis algorithms stay decoupled from transport details.
//!
//! Binding the real service is a thin delegation impl (`impl SemanticSource for
//! LanguageService`) colocated with this trait once the service module lands; every method
//! below mirrors a documented `LanguageService` method 1:1 (`Evidence<T>` returns,
//! [`FeatureSet`] gating resolved at initialize).

use std::future::Future;

use codescope_core::{
    Diagnostic, Evidence, FeatureSet, FileId, Location, Position, SymbolRef, SymbolTree,
};
use codescope_lsp::SemanticError;

/// Async semantic queries used by the analysis layer, returning [`Evidence`]-wrapped
/// `codescope-core` domain types.
///
/// Implementations must gate on their [`FeatureSet`] and fail fast with
/// [`SemanticError::Unsupported`] rather than sending unsupported requests; callers also
/// pre-check [`SemanticSource::features`] so unsupported relations are skipped silently
/// (with a note) instead of erroring.
///
/// Futures are `Send` so orchestration can run under `tokio::spawn`.
pub trait SemanticSource {
    /// Capabilities resolved at initialize.
    fn features(&self) -> &FeatureSet;

    /// Current push-diagnostics cache entries for `file` (empty when none).
    fn diagnostics(&self, file: &FileId) -> Vec<Diagnostic>;

    /// `true` when this source owns `file` (language/extension routing). Default: everything.
    fn handles(&self, _file: &FileId) -> bool {
        true
    }

    /// Hierarchical symbol tree of the worktree content of `file`.
    fn document_symbols(
        &self,
        file: &FileId,
    ) -> impl Future<Output = Result<Evidence<SymbolTree>, SemanticError>> + Send;

    /// Symbol tree of `content` opened as an in-memory overlay for `file` at the base
    /// revision (research 03, verified fact 6: gopls honors overlay text differing from
    /// disk). The resulting tree's revision is [`codescope_core::Revision::Base`].
    fn base_document_symbols(
        &self,
        file: &FileId,
        content: &str,
    ) -> impl Future<Output = Result<Evidence<SymbolTree>, SemanticError>> + Send;

    /// Reference sites of the symbol at `pos` in `file`.
    fn references(
        &self,
        file: &FileId,
        pos: Position,
    ) -> impl Future<Output = Result<Evidence<Vec<Location>>, SemanticError>> + Send;

    /// Callers of the symbol at `pos` in `file` (`callHierarchy/incomingCalls`).
    fn incoming_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> impl Future<Output = Result<Evidence<Vec<SymbolRef>>, SemanticError>> + Send;

    /// Callees of the symbol at `pos` in `file` (`callHierarchy/outgoingCalls`).
    fn outgoing_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> impl Future<Output = Result<Evidence<Vec<SymbolRef>>, SemanticError>> + Send;

    /// Implementations of the interface-ish symbol at `pos` (`textDocument/implementation`).
    fn implementations(
        &self,
        file: &FileId,
        pos: Position,
    ) -> impl Future<Output = Result<Evidence<Vec<SymbolRef>>, SemanticError>> + Send;

    /// Subtypes of the type symbol at `pos` (`typeHierarchy/subtypes`); for a Go interface
    /// these are its implementers — used as a fallback when
    /// [`codescope_core::Feature::Implementation`] is unsupported.
    fn type_subtypes(
        &self,
        file: &FileId,
        pos: Position,
    ) -> impl Future<Output = Result<Evidence<Vec<SymbolRef>>, SemanticError>> + Send;
}


// ---------------------------------------------------------------------------
// Binding to the real language service.
// ---------------------------------------------------------------------------

use codescope_lsp::LanguageService;

/// Thin delegation from the runtime `LanguageService` to the analysis-facing trait.
/// Every method mirrors the service 1:1; `features()` and `diagnostics()` are sync.
impl SemanticSource for LanguageService {
    fn features(&self) -> &FeatureSet {
        LanguageService::features(self)
    }

    fn diagnostics(&self, file: &FileId) -> Vec<Diagnostic> {
        LanguageService::diagnostics(self, file)
    }

    fn handles(&self, file: &FileId) -> bool {
        LanguageService::handles(self, file)
    }

    async fn document_symbols(&self, file: &FileId) -> Result<Evidence<SymbolTree>, SemanticError> {
        LanguageService::document_symbols(self, file).await
    }

    async fn base_document_symbols(
        &self,
        file: &FileId,
        content: &str,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        LanguageService::base_document_symbols(self, file, content).await
    }

    async fn references(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<Location>>, SemanticError> {
        LanguageService::references(self, file, pos).await
    }

    async fn incoming_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        LanguageService::incoming_calls(self, file, pos).await
    }

    async fn outgoing_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        LanguageService::outgoing_calls(self, file, pos).await
    }

    async fn implementations(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        LanguageService::implementations(self, file, pos).await
    }

    async fn type_subtypes(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        LanguageService::type_subtypes(self, file, pos).await
    }
}
