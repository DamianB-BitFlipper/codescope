//! Error type for the git subprocess layer.

use camino::Utf8PathBuf;

/// Convenience alias used throughout `codescope-git`.
pub type Result<T, E = GitError> = std::result::Result<T, E>;

/// Errors produced while spawning `git`, or while parsing its output.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// Failed to spawn or wait on the `git` binary (not installed, not executable, ...).
    #[error("failed to run git {args:?}: {source}")]
    Spawn {
        /// The arguments the command was invoked with.
        args: Vec<String>,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// `git` exited with a non-zero status that the caller did not expect.
    #[error("git {args:?} exited with {status}: {stderr}")]
    Command {
        /// The arguments the command was invoked with.
        args: Vec<String>,
        /// Exit code (`-1` when terminated by a signal).
        status: i32,
        /// Captured stderr, trimmed.
        stderr: String,
    },

    /// The given path is not inside a git repository (or the repo has no worktree).
    #[error("not a git repository (or no worktree): {path}: {stderr}")]
    NotARepo {
        /// The path passed to [`GitRepo::discover`](crate::GitRepo::discover).
        path: Utf8PathBuf,
        /// Git's own error message.
        stderr: String,
    },

    /// Git produced bytes that are not valid UTF-8 where text was required.
    #[error("non-UTF-8 output from git ({context})")]
    NonUtf8 {
        /// What was being read (command / field).
        context: String,
    },

    /// A `status --porcelain=v2 -z` record did not match the documented format.
    #[error("malformed porcelain v2 status record: {detail}")]
    ParseStatus {
        /// Description of the malformed record.
        detail: String,
    },

    /// Unified diff output did not match the documented format (e.g. truncated hunk).
    #[error("malformed unified diff: {detail}")]
    ParseDiff {
        /// Description of the malformed section.
        detail: String,
    },

    /// No base ref could be inferred for the `Branch` change scope
    /// (unborn HEAD, no upstream/remote, no fork-point).
    #[error("no base ref could be inferred for the branch scope")]
    NoBase,
}

impl GitError {
    /// `true` when the error is [`GitError::NoBase`].
    #[must_use]
    pub fn is_no_base(&self) -> bool {
        matches!(self, GitError::NoBase)
    }
}
