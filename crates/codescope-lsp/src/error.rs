//! Error types for the LSP transport layer and the semantic service layer.

use std::io;
use std::time::Duration;

use camino::Utf8PathBuf;
use codescope_core::Feature;

/// Transport-level failures of [`crate::client::LspClient`].
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// The server process could not be spawned.
    #[error("failed to spawn language server `{program}`: {source}")]
    Spawn {
        /// Program that failed to spawn.
        program: String,
        /// Underlying IO error.
        source: io::Error,
    },

    /// Generic IO failure on the stdio pipes.
    #[error("stdio error: {0}")]
    Io(#[from] io::Error),

    /// A request did not complete within its deadline. The pending entry is
    /// removed; a late response is logged and dropped.
    #[error("request `{method}` timed out after {after:?}")]
    Timeout {
        /// JSON-RPC method that timed out.
        method: String,
        /// Deadline that elapsed.
        after: Duration,
    },

    /// The server returned a JSON-RPC error object.
    #[error("server error {code}: {message}")]
    Response {
        /// JSON-RPC error code.
        code: i64,
        /// Error message.
        message: String,
    },

    /// The server answered `-32601` (method not found). This should normally
    /// be prevented by feature gating; surfacing it means the server lied
    /// about its capabilities.
    #[error("method not found on server: {method}")]
    MethodNotFound {
        /// JSON-RPC method that was not found.
        method: String,
    },

    /// The server process exited (or closed its stdout) while the session was
    /// still expected to be alive. All pending requests fail with this error.
    #[error("language server exited unexpectedly; stderr tail: {stderr_tail}")]
    ServerExited {
        /// Last lines captured from the server's stderr.
        stderr_tail: String,
    },

    /// A message violated the protocol in a way we cannot recover locally
    /// (e.g. a response body that does not match the expected type).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The path could not be represented as a `file://` URI (or back).
    #[error("invalid path/URI: {0}")]
    InvalidUri(String),
}

/// Failures surfaced by the semantic service layer
/// ([`crate::service::LanguageService`]).
#[derive(Debug, thiserror::Error)]
pub enum SemanticError {
    /// The server did not advertise the capability backing this feature.
    /// Returned *before* anything is sent on the wire.
    #[error("feature {0:?} is not supported by this language server")]
    Unsupported(Feature),

    /// The initialize response carried capabilities that are null/absent for
    /// every provider — the known "server started but is broken" failure mode
    /// (research 01, quirk 5). The session must not be used.
    #[error("broken language server session: {0}")]
    BrokenSession(String),

    /// No workspace root could be determined.
    #[error("no Go module (go.mod) found at or above {0} — gopls only serves Go projects")]
    NoRoot(Utf8PathBuf),

    /// No supported language was detected in the repo.
    #[error("no supported language detected (found: {0:?})")]
    NoSupportedLanguage(Vec<crate::detect::Language>),

    /// A file involved in a query could not be read (needed for position
    /// encoding conversion or overlay/worktree sync).
    #[error("cannot read {path}: {source}")]
    FileRead {
        /// The file that could not be read.
        path: Utf8PathBuf,
        /// Underlying IO error.
        source: io::Error,
    },

    /// Transport failure.
    #[error(transparent)]
    Client(#[from] LspError),
}

impl SemanticError {
    /// Convenience: is this a timeout?
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, SemanticError::Client(LspError::Timeout { .. }))
    }
}
