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

use crate::detect::{detect_languages, Language};
use crate::error::SemanticError;
use crate::gopls::GoplsService;
use crate::rust_analyzer::RustAnalyzerService;

/// One running language-server session behind the common semantic surface.
#[derive(Debug)]
pub enum LanguageService {
    /// gopls (Go). The prototype's production adapter.
    Gopls(GoplsService),
    /// rust-analyzer (Rust). Proves the adapter-pluggability claim.
    RustAnalyzer(RustAnalyzerService),
}

impl LanguageService {
    /// Start the language service(s) appropriate for `root` (the git toplevel).
    ///
    /// Detection for the prototype: if any `go.mod`/`go.work` is present under `root`,
    /// gopls is started in multi-root mode (Go wins ties). If no Go is detected but a
    /// `Cargo.toml` is found under `root`, rust-analyzer is started at the Cargo
    /// package/workspace root. If no supported language is detected, a clear
    /// [`SemanticError::NoSupportedLanguage`] error is returned so the binary can
    /// distinguish "no language" from a real language-server failure.
    pub async fn start(root: &Utf8Path) -> Result<Self, SemanticError> {
        let languages = detect_languages(root);
        if languages.contains(&Language::Go) {
            return Ok(LanguageService::Gopls(GoplsService::start(root).await?));
        }
        if languages.contains(&Language::Rust) {
            return Ok(LanguageService::RustAnalyzer(
                RustAnalyzerService::start(root).await?,
            ));
        }
        Err(SemanticError::NoSupportedLanguage(languages))
    }

    /// Capabilities resolved from the initialize handshake.
    #[must_use]
    pub fn features(&self) -> &FeatureSet {
        match self {
            LanguageService::Gopls(s) => s.features(),
            LanguageService::RustAnalyzer(s) => s.features(),
        }
    }

    /// Display name of the active language for the TUI top bar.
    #[must_use]
    pub fn language_name(&self) -> &'static str {
        match self {
            LanguageService::Gopls(_) => Language::Go.as_str(),
            LanguageService::RustAnalyzer(_) => Language::Rust.as_str(),
        }
    }

    /// Absolute repository root this session is anchored at.
    #[must_use]
    pub fn root(&self) -> &Utf8Path {
        match self {
            LanguageService::Gopls(s) => s.repo_root(),
            LanguageService::RustAnalyzer(s) => s.repo_root(),
        }
    }

    /// `true` while the server process is believed alive.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        match self {
            LanguageService::Gopls(s) => s.is_alive(),
            LanguageService::RustAnalyzer(s) => s.is_alive(),
        }
    }

    /// Current push-diagnostics for `file` (empty when none), converted to the
    /// internal utf-8 position model.
    #[must_use]
    pub fn diagnostics(&self, file: &FileId) -> Vec<Diagnostic> {
        match self {
            LanguageService::Gopls(s) => s.diagnostics(file),
            LanguageService::RustAnalyzer(s) => s.diagnostics(file),
        }
    }

    /// Hierarchical symbol tree of the current worktree content of `file`.
    pub async fn document_symbols(
        &self,
        file: &FileId,
    ) -> Result<Evidence<SymbolTree>, SemanticError> {
        match self {
            LanguageService::Gopls(s) => s.document_symbols(file).await,
            LanguageService::RustAnalyzer(s) => s.document_symbols(file).await,
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
            LanguageService::RustAnalyzer(s) => s.base_document_symbols(file, content).await,
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
            LanguageService::RustAnalyzer(s) => s.references(file, pos).await,
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
            LanguageService::RustAnalyzer(s) => s.incoming_calls(file, pos).await,
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
            LanguageService::RustAnalyzer(s) => s.outgoing_calls(file, pos).await,
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
            LanguageService::RustAnalyzer(s) => s.implementations(file, pos).await,
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
            LanguageService::RustAnalyzer(s) => s.type_subtypes(file, pos).await,
        }
    }

    /// Hover text for the symbol at `pos` in `file`.
    ///
    /// Note: the gopls adapter currently does not expose hover, so the Gopls variant
    /// returns [`SemanticError::Unsupported`].
    pub async fn hover(
        &self,
        file: &FileId,
        pos: Position,
    ) -> Result<Option<String>, SemanticError> {
        match self {
            LanguageService::Gopls(_) => {
                Err(SemanticError::Unsupported(codescope_core::Feature::Hover))
            }
            LanguageService::RustAnalyzer(s) => s.hover(file, pos).await,
        }
    }

    /// Repo-relative [`FileId`] for an absolute path inside the repo toplevel; `None`
    /// outside (dependency/module-cache locations are out of scope by design).
    #[must_use]
    pub fn file_id(&self, abs: &Utf8Path) -> Option<FileId> {
        match self {
            LanguageService::Gopls(s) => s.file_id(abs),
            LanguageService::RustAnalyzer(s) => s.file_id(abs),
        }
    }

    /// Absolute path of a repo-relative file id.
    #[must_use]
    pub fn abs_path(&self, file: &FileId) -> Utf8PathBuf {
        match self {
            LanguageService::Gopls(s) => s.abs_path(file),
            LanguageService::RustAnalyzer(s) => s.abs_path(file),
        }
    }

    /// `true` when this service owns `file`.
    #[must_use]
    pub fn handles(&self, file: &FileId) -> bool {
        match self {
            LanguageService::Gopls(s) => s.handles(file),
            LanguageService::RustAnalyzer(s) => s.handles(file),
        }
    }

    /// Graceful teardown (shutdown → exit → kill after grace).
    pub async fn shutdown(self) {
        match self {
            LanguageService::Gopls(s) => s.shutdown().await,
            LanguageService::RustAnalyzer(s) => s.shutdown().await,
        }
    }
}
