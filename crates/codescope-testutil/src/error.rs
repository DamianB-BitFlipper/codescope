//! Crate error type.

use std::path::PathBuf;

/// Errors produced by codescope-testutil.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TestutilError {
    /// Filesystem operation failed.
    #[error("i/o error on {path}")]
    Io {
        /// Path the operation touched.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },

    /// An external tool (`git`, `go`, `gofmt`, …) could not be launched.
    #[error("failed to launch `{tool}`")]
    Spawn {
        /// Tool name.
        tool: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },

    /// An external tool ran but exited unsuccessfully.
    #[error("`{tool} {args}` exited with {status}: {stderr}")]
    ToolFailed {
        /// Tool name.
        tool: String,
        /// Arguments, space-joined (display only).
        args: String,
        /// Exit status description.
        status: String,
        /// Captured stderr (trimmed).
        stderr: String,
    },

    /// External tool produced output the fixture builder could not use.
    #[error("unexpected `{tool}` output: {detail}")]
    ToolOutput {
        /// Tool name.
        tool: String,
        /// What was wrong.
        detail: String,
    },

    /// JSON (de)serialization failed.
    #[error("json encode/decode failed")]
    Json(#[from] serde_json::Error),

    /// A JSON-RPC / HTTP protocol invariant was violated on the wire.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// A local test server could not bind or accept.
    #[error("network error: {0}")]
    Net(String),

    /// The process-wide canonical fixture could not be built.
    ///
    /// The failure is memoized as a string because the canonical fixture lives in a
    /// `OnceLock` shared by every caller.
    #[error("canonical fixture unavailable: {0}")]
    Canonical(String),
}

/// Crate-local result alias.
pub type Result<T> = std::result::Result<T, TestutilError>;
