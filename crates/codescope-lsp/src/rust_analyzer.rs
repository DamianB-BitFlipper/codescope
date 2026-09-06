//! rust-analyzer descriptor and public service wrapper.

use std::ops::Deref;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::detect::rust_project_root;
use crate::error::{LspError, SemanticError};
use crate::options::LanguageServiceOptions;
use crate::standard::{StandardAdapter, StandardLspService};

/// Configuration hook that plugs rust-analyzer into the shared LSP session.
pub struct RustAnalyzerAdapter;

impl StandardAdapter for RustAnalyzerAdapter {
    const SERVER_NAME: &'static str = "rust-analyzer";
    const LANGUAGE_ID: &'static str = "rust";
    const FILE_EXTENSIONS: &'static [&'static str] = &["rs"];

    fn project_root(repo_root: &Utf8Path) -> Result<Utf8PathBuf, SemanticError> {
        rust_project_root(repo_root).ok_or_else(|| {
            SemanticError::Client(LspError::Protocol(format!(
                "no Cargo.toml found under {repo_root}"
            )))
        })
    }

    fn command(
        _repo_root: &Utf8Path,
        project_root: &Utf8Path,
        _options: LanguageServiceOptions,
    ) -> Command {
        let program = std::env::var("CODESCOPE_RUST_ANALYZER")
            .unwrap_or_else(|_| "rust-analyzer".to_string());
        let mut command = Command::new(program);
        command.current_dir(project_root.as_std_path());
        command
    }

    fn initialization_options(options: LanguageServiceOptions) -> Value {
        json!({
            "numThreads": options.max_threads,
            "cachePriming": { "numThreads": options.max_threads }
        })
    }
}

/// rust-analyzer session over the shared semantic LSP implementation.
#[derive(Debug)]
pub struct RustAnalyzerService(StandardLspService);

impl RustAnalyzerService {
    /// Spawn rust-analyzer for `repo_root`.
    pub async fn start(repo_root: &Utf8Path) -> Result<Self, SemanticError> {
        Self::start_with_options(repo_root, LanguageServiceOptions::default()).await
    }

    /// Spawn rust-analyzer with an explicit resource policy.
    pub async fn start_with_options(
        repo_root: &Utf8Path,
        options: LanguageServiceOptions,
    ) -> Result<Self, SemanticError> {
        StandardLspService::start::<RustAnalyzerAdapter>(repo_root, options)
            .await
            .map(Self)
    }

    /// Cargo package/workspace root passed to rust-analyzer.
    #[must_use]
    pub fn cargo_root(&self) -> &Utf8Path {
        self.0.project_root()
    }

    /// Gracefully stop rust-analyzer.
    pub async fn shutdown(self) {
        self.0.shutdown().await;
    }
}

impl Deref for RustAnalyzerService {
    type Target = StandardLspService;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_limits_workers_and_cache_priming() {
        let options =
            RustAnalyzerAdapter::initialization_options(LanguageServiceOptions { max_threads: 2 });
        assert_eq!(options["numThreads"], 2);
        assert_eq!(options["cachePriming"]["numThreads"], 2);
    }
}
