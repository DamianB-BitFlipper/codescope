//! Environment probes and fixture-copy helpers shared by codescope test suites.

use crate::error::{Result, TestutilError};
use crate::go_fixture::{build_fixture, FixtureInfo};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use tempfile::TempDir;

/// Locate a runnable `gopls` on `PATH`.
///
/// Returns the resolved binary path when `gopls version` exits successfully, otherwise
/// `None`. Integration tests should **skip** (print `SKIP: …` and return) when this is
/// `None` — the env-skip pattern from research 08 §2, so CI with gopls installed runs the
/// tests by default.
#[must_use]
pub fn require_gopls() -> Option<PathBuf> {
    require_tool("gopls", &["version"])
}

/// Locate a runnable `go` toolchain on `PATH` (same contract as [`require_gopls`]).
#[must_use]
pub fn require_go() -> Option<PathBuf> {
    require_tool("go", &["version"])
}

/// `true` when live-AI tests are enabled via `CODESCOPE_LIVE=1`.
///
/// Live tests are additionally `#[ignore]`d; this gate is checked inside the test body so
/// `cargo test -- --ignored live_ai_smoke` still no-ops without the env var.
#[must_use]
pub fn live_ai_enabled() -> bool {
    std::env::var("CODESCOPE_LIVE").is_ok_and(|v| v == "1")
}

/// The process-wide canonical fixture, built once on first use into a temp dir that lives
/// for the rest of the process.
///
/// Tests must treat it as **read-only**; use [`copy_fixture_into`] to get a mutable copy.
pub fn canonical_fixture() -> Result<&'static FixtureInfo> {
    static CANONICAL: OnceLock<std::result::Result<(TempDir, FixtureInfo), String>> =
        OnceLock::new();
    let entry = CANONICAL.get_or_init(|| {
        let tmp = tempfile::Builder::new()
            .prefix("codescope-fixture-")
            .tempdir()
            .map_err(|e| format!("tempdir: {e}"))?;
        let info = build_fixture(tmp.path().join("go-fixture")).map_err(|e| e.to_string())?;
        Ok((tmp, info))
    });
    match entry {
        Ok((_tmp, info)) => Ok(info),
        Err(msg) => Err(TestutilError::Canonical(msg.clone())),
    }
}

/// Copy the canonical fixture (worktree **and** `.git`, preserving index state) into
/// `dest`, returning a [`FixtureInfo`] rooted there.
///
/// `dest` is created if missing and should be empty — pass a fresh
/// [`TempDir`] (it implements `AsRef<Path>`) or a subdirectory of one.
/// Deterministic rebuilds would produce the same bytes, but copying keeps per-test cost to
/// pure I/O and guarantees the canonical fixture is never mutated (research 08 §2).
pub fn copy_fixture_into(dest: impl AsRef<Path>) -> Result<FixtureInfo> {
    let canonical = canonical_fixture()?;
    let dest = dest.as_ref();
    copy_dir_recursive(&canonical.root, dest)?;
    tracing::debug!(dest = %dest.display(), "copied canonical fixture");
    Ok(FixtureInfo {
        root: dest.to_path_buf(),
        ..canonical.clone()
    })
}

/// Probe `PATH` for `tool` and verify it runs (`tool probe_args…` exits 0, output discarded).
fn require_tool(tool: &str, probe_args: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for candidate in candidate_names(tool) {
            let full = dir.join(candidate);
            if !full.is_file() {
                continue;
            }
            let runs = Command::new(&full)
                .args(probe_args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if runs {
                tracing::debug!(tool, path = %full.display(), "tool found");
                return Some(full);
            }
        }
    }
    tracing::debug!(tool, "tool not found on PATH");
    None
}

/// Platform-appropriate executable names for `tool`.
fn candidate_names(tool: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{tool}.exe"), tool.to_string()]
    } else {
        vec![tool.to_string()]
    }
}

/// Recursively copy `src` into `dst` (regular files and directories only; the fixture
/// contains no symlinks, and encountering one is an error rather than a silent skip).
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|source| TestutilError::Io {
        path: dst.to_path_buf(),
        source,
    })?;
    let entries = std::fs::read_dir(src).map_err(|source| TestutilError::Io {
        path: src.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| TestutilError::Io {
            path: src.to_path_buf(),
            source,
        })?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let kind = entry.file_type().map_err(|source| TestutilError::Io {
            path: from.clone(),
            source,
        })?;
        if kind.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if kind.is_file() {
            std::fs::copy(&from, &to).map_err(|source| TestutilError::Io {
                path: from.clone(),
                source,
            })?;
        } else {
            return Err(TestutilError::Io {
                path: from,
                source: std::io::Error::other("unsupported file type (symlink?) in fixture"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_ai_enabled_reads_env() {
        // Never enabled by default in CI/dev shells running this suite.
        match std::env::var("CODESCOPE_LIVE") {
            Ok(v) if v == "1" => assert!(live_ai_enabled()),
            _ => assert!(!live_ai_enabled()),
        }
    }

    #[test]
    fn require_tool_misses_nonexistent_binary() {
        assert!(require_tool("codescope-no-such-tool-xyz", &["--version"]).is_none());
    }

    #[test]
    fn candidate_names_cover_platform() {
        let names = candidate_names("go");
        assert!(names.contains(&"go".to_string()) || names.contains(&"go.exe".to_string()));
    }
}
