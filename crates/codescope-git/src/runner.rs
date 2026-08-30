//! Hardened construction and execution of `git` subprocesses.
//!
//! Every invocation:
//! - removes inherited `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` / `GIT_EXTERNAL_DIFF`
//!   / `GIT_PAGER` (an inherited `GIT_DIR` would silently redirect every command to another
//!   repository; an external diff driver would replace the unified-diff format entirely);
//! - sets `GIT_OPTIONAL_LOCKS=0` **and** passes `--no-optional-locks` so `git status` never
//!   rewrites `.git/index` (the read-only guarantee), and `LC_ALL=C` for stable messages;
//! - forces `color.*=false` and unsets `diff.external` via `-c`, because user config such as
//!   `color.diff=always` or `diff.external=<tool>` corrupts machine output (verified).

use crate::error::{GitError, Result};
use camino::Utf8Path;
use std::process::{Output, Stdio};
use tokio::process::Command;

/// Config/env hardening shared by every git call.
const CONFIG_OVERRIDES: &[&str] = &[
    "-c",
    "color.ui=false",
    "-c",
    "color.diff=false",
    "-c",
    "color.status=false",
    "-c",
    "color.log=false",
    "-c",
    "diff.external=",
    // Patch-format diffs have no `-z`; without this, non-ASCII paths get C-quoted *and*
    // a user's `core.quotepath` setting could change path rendering between runs.
    // Harmless for `-z` outputs, which never quote.
    "-c",
    "core.quotepath=false",
    // `diff.suppressBlankEmpty=true` emits empty context lines without the leading space,
    // which breaks hunk-body parsing (review 03 finding 2).
    "-c",
    "diff.suppressBlankEmpty=false",
    // `core.fsmonitor=true` would spawn a daemon that writes under .git — the read-only
    // guarantee forbids that side effect (review 03 finding 4). Stat-cache behavior only.
    "-c",
    "core.fsmonitor=false",
];

/// A single hardened `git` invocation.
#[derive(Debug)]
pub(crate) struct GitCommand {
    /// User-visible arguments (excluding the hardening `-c` overrides), for error messages.
    args: Vec<String>,
    command: Command,
}

impl GitCommand {
    /// Build `git <args...>` running in `dir` (`None`: inherit the process cwd).
    pub(crate) fn new(dir: Option<&Utf8Path>, args: &[&str]) -> Self {
        let mut command = Command::new("git");
        command.arg("--no-optional-locks");
        command.args(CONFIG_OVERRIDES);
        command.args(args);
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        command
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_NAMESPACE")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_DIFF_OPTS")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_EXTERNAL_DIFF")
            .env_remove("GIT_PAGER")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        GitCommand {
            args: args.iter().map(|s| (*s).to_string()).collect(),
            command,
        }
    }

    /// Run to completion, capturing output. Does **not** check the exit status.
    pub(crate) async fn output(mut self) -> Result<GitOutput> {
        tracing::trace!(args = ?self.args, "running git");
        let output = self
            .command
            .output()
            .await
            .map_err(|source| GitError::Spawn {
                args: self.args.clone(),
                source,
            })?;
        Ok(GitOutput {
            args: self.args,
            output,
        })
    }

    /// Run to completion and require exit status 0.
    pub(crate) async fn run(self) -> Result<GitOutput> {
        let out = self.output().await?;
        out.require_success()?;
        Ok(out)
    }
}

/// Captured output of one git invocation, keeping the args for error reporting.
#[derive(Debug)]
pub(crate) struct GitOutput {
    args: Vec<String>,
    output: Output,
}

impl GitOutput {
    /// `true` when git exited with status 0.
    pub(crate) fn success(&self) -> bool {
        self.output.status.success()
    }

    /// Error unless git exited with status 0.
    pub(crate) fn require_success(&self) -> Result<()> {
        if self.success() {
            return Ok(());
        }
        Err(GitError::Command {
            args: self.args.clone(),
            status: self.output.status.code().unwrap_or(-1),
            stderr: self.stderr_trimmed(),
        })
    }

    /// Raw stdout bytes.
    pub(crate) fn stdout_bytes(&self) -> &[u8] {
        &self.output.stdout
    }

    /// Stdout as UTF-8 text.
    pub(crate) fn stdout_utf8(&self, context: &str) -> Result<&str> {
        std::str::from_utf8(&self.output.stdout).map_err(|_| GitError::NonUtf8 {
            context: context.to_string(),
        })
    }

    /// Stdout as UTF-8 with surrounding whitespace trimmed (single-value commands).
    pub(crate) fn stdout_trimmed(&self, context: &str) -> Result<String> {
        Ok(self.stdout_utf8(context)?.trim().to_string())
    }

    /// Lossy, trimmed stderr for error messages.
    pub(crate) fn stderr_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr)
            .trim()
            .to_string()
    }
}
