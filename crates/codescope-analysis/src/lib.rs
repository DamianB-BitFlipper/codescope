//! `codescope-analysis` — the semantic change-analysis layer.
//!
//! Turns git [`ChangeSet`](codescope_core::ChangeSet)s plus language-server symbol trees
//! into the domain results the TUI and AI layers consume (research 03):
//!
//! - [`mapper`]: pure hunk → symbol mapping with end-to-end
//!   [`MappingConfidence`](codescope_core::MappingConfidence).
//! - [`changes`]: per-symbol [`ChangedSymbol`](codescope_core::ChangedSymbol) aggregation
//!   (added / modified / deleted, worst-confidence-wins).
//! - [`graph`]: 1-hop impact graph over the language service, wrapped in
//!   [`Evidence`](codescope_core::Evidence) (honesty layer).
//! - [`digest`]: the compact 5-tier AI prompt payload (research 05 §4) with
//!   token-budget-aware truncation.
//! - [`source`]: [`SemanticSource`] — the narrow async query surface analysis consumes
//!   (`codescope-lsp`'s `LanguageService` binds to it with a thin delegation impl).
//! - [`engine`]: [`AnalysisEngine`] orchestration — change-set in, epoch-tagged
//!   [`AnalysisSnapshot`] out.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
#[cfg(test)]
mod testsupport;

pub mod changes;
pub mod digest;
pub mod engine;
pub mod graph;
pub mod mapper;
pub mod source;

pub use changes::{ChangedSymbolInfo, changed_symbols, changed_symbols_detailed, file_mappings};
pub use digest::{ChangeDigest, change_digest, estimate_tokens};
pub use engine::{AnalysisEngine, AnalysisSnapshot, FileAnalysis, FileSemanticResult};
pub use error::AnalysisError;
pub use graph::{annotate_diagnostics, build_impact_graph};
pub use mapper::{MappedHunk, map_changes, map_changes_detailed, map_changes_with_base};
pub use source::SemanticSource;
