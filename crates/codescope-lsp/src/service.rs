//! `LanguageService`: the language-neutral semantic boundary (research 01).
//!
//! Enum dispatch over per-server adapters. Callers get `codescope-core` domain types
//! wrapped in [`codescope_core::Evidence`]; every relationship query is gated on the
//! [`codescope_core::FeatureSet`] resolved at initialize, so unsupported features fail
//! fast with [`SemanticError::Unsupported`] before anything goes on the wire.
//!
//! Adding a server (rust-analyzer, clangd, pyright, tsls, …) means: a new adapter module
//! owning spawn/initialize/enrichment specifics, and a new variant here. Nothing above
//! this enum changes.

use camino::{Utf8Path, Utf8PathBuf};
use codescope_core::{
    Diagnostic, Evidence, FeatureSet, FileId, Location, Position, SymbolRef, SymbolTree,
};

use crate::error::SemanticError;
use crate::gopls::GoplsService;

/// One running language-server session behind the common semantic surface.
#[derive(Debug)]
pub enum LanguageService {
    /// gopls (Go). The prototype's production adapter.
    Gopls(GoplsService),
}

impl LanguageService {
    /// Start the language service appropriate for `root`.
    ///
    /// Detection for the prototype: a `go.mod` at or above `root` selects gopls.
    pub async fn start(root: &Utf8Path) -> Result<Self, SemanticError> {
        Ok(LanguageService::Gopls(GoplsService::start(root).await?))
    }

    /// Capabilities resolved from the initialize handshake.
    #[must_use]
    pub fn features(&self) -> &FeatureSet {
        let LanguageService::Gopls(s) = self;
        s.features()
    }

    /// Absolute workspace root this session is anchored at.
    #[must_use]
    pub fn root(&self) -> &Utf8Path {
        let LanguageService::Gopls(s) = self;
        s.root()
    }

    /// `true` while the server process is believed alive.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        let LanguageService::Gopls(s) = self;
        s.is_alive()
    }

    /// Current push-diagnostics for `file` (empty when none), converted to the
    /// internal utf-8 position model.
    #[must_use]
    pub fn diagnostics(&self, file: &FileId) -> Vec<Diagnostic> {
        let LanguageService::Gopls(s) = self;
        s.diagnostics(file)
    }

    /// Hierarchical symbol tree of the current worktree content of `file`.
    pub async fn document_symbols(
        &self,
        file: &FileId,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        let LanguageService::Gopls(s) = self;
        s.document_symbols(file).await
    }

    /// Symbol tree of `content` opened as a temporary in-memory overlay for `file`
    /// (base-revision analysis; research 03, verified gopls fact 6). The worktree view
    /// is restored before returning.
    pub async fn base_document_symbols(
        &self,
        file: &FileId,
        content: &str,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        let LanguageService::Gopls(s) = self;
        s.base_document_symbols(file, content).await
    }

    /// Reference sites of the symbol at `pos` in `file`.
    pub async fn references(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<Location>>, SemanticError> {
        let LanguageService::Gopls(s) = self;
        s.references(file, pos).await
    }

    /// Callers of the symbol at `pos` (`callHierarchy/incomingCalls`).
    pub async fn incoming_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        let LanguageService::Gopls(s) = self;
        s.incoming_calls(file, pos).await
    }

    /// Callees of the symbol at `pos` (`callHierarchy/outgoingCalls`).
    pub async fn outgoing_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        let LanguageService::Gopls(s) = self;
        s.outgoing_calls(file, pos).await
    }

    /// Implementations of the interface-ish symbol at `pos`
    /// (`textDocument/implementation`).
    pub async fn implementations(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        let LanguageService::Gopls(s) = self;
        s.implementations(file, pos).await
    }

    /// Subtypes of the type symbol at `pos` (`typeHierarchy/subtypes`); for a Go
    /// interface these are its implementers.
    pub async fn type_subtypes(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        let LanguageService::Gopls(s) = self;
        s.type_subtypes(file, pos).await
    }

    /// Repo-relative [`FileId`] for an absolute path inside the root; `None` outside
    /// (dependency/module-cache locations are out of scope by design).
    #[must_use]
    pub fn file_id(&self, abs: &Utf8Path) -> Option<FileId> {
        let LanguageService::Gopls(s) = self;
        s.file_id(abs)
    }

    /// Absolute path of a repo-relative file id.
    #[must_use]
    pub fn abs_path(&self, file: &FileId) -> Utf8PathBuf {
        let LanguageService::Gopls(s) = self;
        s.abs_path(file)
    }

    /// `true` when this service owns `file` (gopls owns `*.go`). Non-owned files are
    /// skipped by the analysis engine instead of being mislabeled as Go.
    #[must_use]
    pub fn handles(&self, file: &FileId) -> bool {
        let LanguageService::Gopls(s) = self;
        s.handles(file)
    }

    /// Graceful teardown (shutdown → exit → kill after grace).
    pub async fn shutdown(self) {
        let LanguageService::Gopls(s) = self;
        s.shutdown().await;
    }
}
