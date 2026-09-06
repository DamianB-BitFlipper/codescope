//! `LanguageService`: the language-neutral semantic boundary (research 01).
//!
//! Dispatch over per-server adapters. Callers get `codescope-core` domain types
//! wrapped in [`codescope_core::Evidence`]; every relationship query is gated on the
//! [`codescope_core::FeatureSet`] resolved at initialize, so unsupported features fail
//! fast with [`SemanticError::Unsupported`] before anything goes on the wire.
//!
//! Conventional servers implement [`crate::standard::StandardAdapter`] and inherit the
//! shared overlay/query/cache implementation. Bespoke adapters can still own additional
//! enrichment, as gopls does. Nothing above this service changes.

use camino::{Utf8Path, Utf8PathBuf};
use codescope_core::{
    Diagnostic, Evidence, FeatureSet, FileId, Location, Position, SymbolRef, SymbolTree,
    SyntaxToken,
};

use crate::detect::{Language, detect_languages};
use crate::error::SemanticError;
use crate::gopls::GoplsService;
use crate::options::LanguageServiceOptions;
use crate::pyright::PyrightAdapter;
use crate::rust_analyzer::RustAnalyzerAdapter;
use crate::standard::StandardLspService;

/// One running language-server session behind the common semantic surface.
#[derive(Debug)]
pub enum LanguageService {
    /// gopls (Go). The prototype's production adapter.
    Gopls(GoplsService),
    /// A conventional stdio LSP powered by the shared adapter contract.
    Standard {
        /// Source language served by this session.
        language: Language,
        /// Shared capability-gated semantic implementation.
        service: StandardLspService,
    },
}

impl LanguageService {
    /// Start the language service(s) appropriate for `root` (the git toplevel).
    ///
    /// Detection precedence is Go, Rust, then Python. Go uses gopls in multi-root mode;
    /// Rust uses rust-analyzer at the Cargo root; Python uses Pyright at the nearest
    /// configured Python project root. If no supported language is detected, a clear
    /// [`SemanticError::NoSupportedLanguage`] error is returned so the binary can
    /// distinguish "no language" from a real language-server failure.
    pub async fn start(root: &Utf8Path) -> Result<Self, SemanticError> {
        Self::start_with_options(root, LanguageServiceOptions::default()).await
    }

    /// Start the detected language service with an explicit resource policy.
    pub async fn start_with_options(
        root: &Utf8Path,
        options: LanguageServiceOptions,
    ) -> Result<Self, SemanticError> {
        let languages = detect_languages(root);
        if languages.contains(&Language::Go) {
            return Ok(LanguageService::Gopls(
                GoplsService::start_with_options(root, options).await?,
            ));
        }
        if languages.contains(&Language::Rust) {
            return Ok(LanguageService::Standard {
                language: Language::Rust,
                service: StandardLspService::start::<RustAnalyzerAdapter>(root, options).await?,
            });
        }
        if languages.contains(&Language::Python) {
            return Ok(LanguageService::Standard {
                language: Language::Python,
                service: StandardLspService::start::<PyrightAdapter>(root, options).await?,
            });
        }
        Err(SemanticError::NoSupportedLanguage(languages))
    }

    /// Capabilities resolved from the initialize handshake.
    #[must_use]
    pub fn features(&self) -> &FeatureSet {
        match self {
            LanguageService::Gopls(s) => s.features(),
            LanguageService::Standard { service, .. } => service.features(),
        }
    }

    /// Display name of the active language for the TUI top bar.
    #[must_use]
    pub fn language_name(&self) -> &'static str {
        match self {
            LanguageService::Gopls(_) => Language::Go.as_str(),
            LanguageService::Standard { language, .. } => language.as_str(),
        }
    }

    /// Absolute repository root this session is anchored at.
    #[must_use]
    pub fn root(&self) -> &Utf8Path {
        match self {
            LanguageService::Gopls(s) => s.repo_root(),
            LanguageService::Standard { service, .. } => service.repo_root(),
        }
    }

    /// `true` while the server process is believed alive.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        match self {
            LanguageService::Gopls(s) => s.is_alive(),
            LanguageService::Standard { service, .. } => service.is_alive(),
        }
    }

    /// Current push-diagnostics for `file` (empty when none), converted to the
    /// internal utf-8 position model.
    #[must_use]
    pub fn diagnostics(&self, file: &FileId) -> Vec<Diagnostic> {
        match self {
            LanguageService::Gopls(s) => s.diagnostics(file),
            LanguageService::Standard { service, .. } => service.diagnostics(file),
        }
    }

    /// Hierarchical symbol tree of the current worktree content of `file`.
    pub async fn document_symbols(
        &self,
        file: &FileId,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.document_symbols(file).await,
            LanguageService::Standard { service, .. } => service.document_symbols(file).await,
        }
    }

    /// Symbol tree of `content` opened as a temporary in-memory overlay for `file`
    /// (base-revision analysis; research 03, verified fact 6). The resulting tree's
    /// revision is [`codescope_core::Revision::Base`].
    pub async fn base_document_symbols(
        &self,
        file: &FileId,
        content: &str,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.base_document_symbols(file, content).await,
            LanguageService::Standard { service, .. } => {
                service.base_document_symbols(file, content).await
            }
        }
    }

    /// Semantic syntax tokens for the current worktree content of `file`.
    pub async fn semantic_tokens(
        &self,
        file: &FileId,
    ) -> Result<Evidence<Vec<SyntaxToken>>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.semantic_tokens(file).await,
            LanguageService::Standard { service, .. } => service.semantic_tokens(file).await,
        }
    }

    /// Semantic syntax tokens for exact snapshot `content` opened as a temporary overlay.
    pub async fn semantic_tokens_for_content(
        &self,
        file: &FileId,
        content: &str,
    ) -> Result<Evidence<Vec<SyntaxToken>>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.semantic_tokens_for_content(file, content).await,
            LanguageService::Standard { service, .. } => {
                service.semantic_tokens_for_content(file, content).await
            }
        }
    }

    /// Reference sites of the symbol at `pos` in `file`.
    pub async fn references(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<Location>>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.references(file, pos).await,
            LanguageService::Standard { service, .. } => service.references(file, pos).await,
        }
    }

    /// Callers of the symbol at `pos` (`callHierarchy/incomingCalls`).
    pub async fn incoming_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.incoming_calls(file, pos).await,
            LanguageService::Standard { service, .. } => service.incoming_calls(file, pos).await,
        }
    }

    /// Callees of the symbol at `pos` (`callHierarchy/outgoingCalls`).
    pub async fn outgoing_calls(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.outgoing_calls(file, pos).await,
            LanguageService::Standard { service, .. } => service.outgoing_calls(file, pos).await,
        }
    }

    /// Implementations of the interface-ish symbol at `pos`
    /// (`textDocument/implementation`).
    pub async fn implementations(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.implementations(file, pos).await,
            LanguageService::Standard { service, .. } => service.implementations(file, pos).await,
        }
    }

    /// Supertypes of the type symbol at `pos` (`typeHierarchy/supertypes`).
    pub async fn type_supertypes(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.type_supertypes(file, pos).await,
            LanguageService::Standard { service, .. } => service.type_supertypes(file, pos).await,
        }
    }

    /// Subtypes of the type symbol at `pos` (`typeHierarchy/subtypes`).
    pub async fn type_subtypes(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Evidence<Vec<SymbolRef>>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.type_subtypes(file, pos).await,
            LanguageService::Standard { service, .. } => service.type_subtypes(file, pos).await,
        }
    }

    /// Hover text for the symbol at `pos` in `file`.
    ///
    pub async fn hover(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Option<String>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.hover(file, pos).await,
            LanguageService::Standard { service, .. } => service.hover(file, pos).await,
        }
    }

    /// Repo-relative [`FileId`] for an absolute path inside the repo toplevel; `None`
    /// outside (dependency/module-cache locations are out of scope by design).
    #[must_use]
    pub fn file_id(&self, abs: &Utf8Path) -> Option<FileId> {
        match self {
            LanguageService::Gopls(s) => s.file_id(abs),
            LanguageService::Standard { service, .. } => service.file_id(abs),
        }
    }

    /// Absolute path of a repo-relative file id.
    #[must_use]
    pub fn abs_path(&self, file: &FileId) -> Utf8PathBuf {
        match self {
            LanguageService::Gopls(s) => s.abs_path(file),
            LanguageService::Standard { service, .. } => service.abs_path(file),
        }
    }

    /// `true` when this service owns `file`.
    #[must_use]
    pub fn handles(&self, file: &FileId) -> bool {
        match self {
            LanguageService::Gopls(s) => s.handles(file),
            LanguageService::Standard { service, .. } => service.handles(file),
        }
    }

    /// Graceful teardown (shutdown → exit → kill after grace).
    pub async fn shutdown(self) {
        match self {
            LanguageService::Gopls(s) => s.shutdown().await,
            LanguageService::Standard { service, .. } => service.shutdown().await,
        }
    }
}
