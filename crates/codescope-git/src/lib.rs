//! `codescope-git` — read-only git CLI subprocess layer.
//!
//! Turns `git` porcelain-v2 / unified-diff output into the [`codescope_core`] git domain
//! types ([`RepoContext`](codescope_core::RepoContext), [`ChangeSet`](codescope_core::ChangeSet), ...).
//! Every command is hardened against user configuration and inherited environment that
//! would corrupt or redirect machine output (see [`runner`] internals), and passes
//! `--no-optional-locks` so nothing under `.git/` is ever written.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod diff;
mod error;
mod repo;
mod runner;
mod status;

pub use error::{GitError, Result};
pub use repo::{CommitSummary, GitRepo};
