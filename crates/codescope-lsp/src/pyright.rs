//! Pyright descriptor and public service wrapper.

use std::ops::Deref;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;
use tokio::process::Command;

use crate::detect::python_project_root;
use crate::error::SemanticError;
use crate::options::LanguageServiceOptions;
use crate::standard::{StandardAdapter, StandardLspService};

/// Configuration hook that plugs Pyright into the shared LSP session.
pub struct PyrightAdapter;

impl StandardAdapter for PyrightAdapter {
    const SERVER_NAME: &'static str = "pyright";
    const LANGUAGE_ID: &'static str = "python";
    const FILE_EXTENSIONS: &'static [&'static str] = &["py", "pyi"];

    fn project_root(repo_root: &Utf8Path) -> Result<Utf8PathBuf, SemanticError> {
        Ok(python_project_root(repo_root))
    }

    fn command(
        _repo_root: &Utf8Path,
        project_root: &Utf8Path,
        _options: LanguageServiceOptions,
    ) -> Command {
        let program =
            std::env::var("CODESCOPE_PYRIGHT").unwrap_or_else(|_| "pyright-langserver".to_string());
        let mut command = Command::new(program);
        command
            .arg("--stdio")
            .current_dir(project_root.as_std_path());
        command
    }

    fn initialization_options(_options: LanguageServiceOptions) -> Value {
        Value::Null
    }
}

/// Pyright session over the shared semantic LSP implementation.
#[derive(Debug)]
pub struct PyrightService(StandardLspService);

impl PyrightService {
    /// Spawn `pyright-langserver --stdio` for `repo_root`.
    pub async fn start(repo_root: &Utf8Path) -> Result<Self, SemanticError> {
        Self::start_with_options(repo_root, LanguageServiceOptions::default()).await
    }

    /// Spawn Pyright with an explicit Codescope resource policy.
    pub async fn start_with_options(
        repo_root: &Utf8Path,
        options: LanguageServiceOptions,
    ) -> Result<Self, SemanticError> {
        StandardLspService::start::<PyrightAdapter>(repo_root, options)
            .await
            .map(Self)
    }

    /// Python project root passed to Pyright.
    #[must_use]
    pub fn python_root(&self) -> &Utf8Path {
        self.0.project_root()
    }

    /// Gracefully stop Pyright.
    pub async fn shutdown(self) {
        self.0.shutdown().await;
    }
}

impl Deref for PyrightService {
    type Target = StandardLspService;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_uses_stdio_transport() {
        let root = Utf8Path::new("/tmp/python-project");
        let command = PyrightAdapter::command(root, root, LanguageServiceOptions::default());
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(args, ["--stdio"]);
    }
}
