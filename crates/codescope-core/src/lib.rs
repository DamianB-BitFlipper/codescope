//! `codescope-core` — shared domain types for codescope.
//!
//! This crate is the dependency root of the workspace: every other crate builds on these
//! types. It contains **no async, no I/O, and no external-service types** — pure data plus
//! small pure helpers.
//!
//! # Conventions
//!
//! - **Positions** are zero-based `(line, col)` with `col` counted in **UTF-8 code units**
//!   (bytes). The LSP layer converts to each server's negotiated encoding (UTF-16 for gopls)
//!   at the wire boundary; everything inside codescope uses [`Position`]/[`LineRange`].
//! - **Paths** are [`camino::Utf8PathBuf`] (UTF-8, platform-independent). Paths that identify
//!   a file inside the repository (symbol locations, diagnostics, plan entities) use the
//!   repo-relative [`FileId`] newtype; git-domain paths are plain `Utf8PathBuf` exactly as
//!   reported by `git` (repo-root-relative, since every command runs from the toplevel).
//! - **Hunk line numbers** are stored exactly as git emits them: 1-based, with a zero length
//!   on the empty side of a pure addition/deletion. Convert to zero-based lines before
//!   comparing against [`LineRange`]; see [`Hunk`] helpers.
//! - All types are `serde`-serializable. Enums serialize as `snake_case` strings so the
//!   JSON dialect matches the AI plan schema in `docs/research/05`.
//! - `lsp-types` appears only in `From`/conversion helpers so `codescope-lsp` can translate
//!   wire types; no other crate needs to depend on `lsp-types`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod diagram_draft;
mod epoch;
mod error;
mod file;
mod git;
mod impact;
mod mapping;
mod position;
mod relation;
mod semantic;
mod status;
mod viz;

pub use diagram_draft::*;
pub use epoch::*;
pub use error::*;
pub use file::*;
pub use git::*;
pub use impact::*;
pub use mapping::*;
pub use position::*;
pub use relation::*;
pub use semantic::*;
pub use status::*;
pub use viz::*;
