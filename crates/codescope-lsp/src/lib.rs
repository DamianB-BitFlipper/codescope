//! `codescope-lsp` — generic LSP client with gopls and rust-analyzer adapters.
//!
//! Layering (research 01):
//!
//! - [`framing`]: `Content-Length` base-protocol byte framing (malformed frames are
//!   skipped, never fatal).
//! - [`jsonrpc`]: message classification (response / notification / server→client request).
//! - [`client`]: `LspClient` — stdio JSON-RPC transport over `tokio::process` with
//!   request-id matching, out-of-order responses, a push-diagnostics cache, and a
//!   graceful `shutdown` → `exit` → kill-after-5s teardown.
//! - [`encoding`]: per-session position-encoding negotiation and utf-8 ⇄ utf-16
//!   conversion. **All** conversion happens at the wire boundary; everything above the
//!   client speaks `codescope-core` positions (utf-8 columns).
//! - [`capabilities`]: raw server capabilities → [`codescope_core::FeatureSet`], plus the
//!   all-null broken-session detection (research 01, quirk 5).
//! - [`service`]: `LanguageService` — enum-dispatch semantic surface returning
//!   `codescope-core` domain types wrapped in [`codescope_core::Evidence`].
//! - [`gopls`]: `GoplsService` — Go adapter (spawn, initialize, overlays, feature gating).
//! - [`rust_analyzer`]: `RustAnalyzerService` — Rust adapter over the same semantic boundary.
//!
//! Every relationship query is gated on the resolved [`codescope_core::FeatureSet`]
//! *before* anything is sent on the wire; unsupported features fail fast with
//! [`SemanticError::Unsupported`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod capabilities;
pub mod client;
mod content_cache;
pub mod detect;
pub mod encoding;
pub mod error;
pub mod framing;
pub mod gopls;
pub mod jsonrpc;
pub mod options;
pub mod rust_analyzer;
mod semantic_tokens;
pub mod service;
pub mod uri;

pub use client::{LspClient, ShutdownOutcome};
pub use detect::{detect_languages, Language};
pub use encoding::PositionEncoding;
pub use error::{LspError, SemanticError};
pub use gopls::GoplsService;
pub use options::LanguageServiceOptions;
pub use rust_analyzer::RustAnalyzerService;
pub use service::LanguageService;
