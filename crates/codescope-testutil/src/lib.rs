//! `codescope-testutil` — dev-facing test support for the codescope workspace.
//!
//! Three pillars (research 08):
//!
//! 1. [`go_fixture`] — a deterministic, regenerable Go repository with a cross-package
//!    call chain, an interface with two implementations, a two-commit feature branch, and
//!    a dirty working state (staged edit + staged rename + unstaged edit + untracked file).
//! 2. [`fake_lsp`] — a scriptable stdio JSON-RPC server for negative-path LSP client tests
//!    (canned/absent capabilities, canned `documentSymbol`, diagnostics pushes, `-32601`,
//!    malformed frames, response delays).
//! 3. [`fake_ai`] — [`fake_ai::ScriptedProvider`], a raw-TCP OpenAI-compatible
//!    chat-completions server driven by a queue of scripted steps (valid plans, malformed
//!    JSON, hallucinated entities, arbitrary epoch echo, 429 + `Retry-After`,
//!    hang-until-abort).
//!
//! Plus [`helpers`] for env probing ([`helpers::require_gopls`], [`helpers::require_go`],
//! [`helpers::live_ai_enabled`]) and per-test fixture copies
//! ([`helpers::copy_fixture_into`]).
//!
//! This crate is test-support machinery: it is meant to be used from `#[cfg(test)]` code
//! and `tests/` targets of other codescope crates, never at runtime.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
pub mod fake_ai;
pub mod fake_lsp;
pub mod go_fixture;
pub mod helpers;
pub mod scenarios;

pub use error::{Result, TestutilError};
pub use go_fixture::{FixtureInfo, build_fixture, reset_fixture};
pub use helpers::{copy_fixture_into, live_ai_enabled, require_go, require_gopls};
