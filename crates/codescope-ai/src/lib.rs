//! `codescope-ai` — the AI visualization engine.
//!
//! The AI only *chooses and parameterizes* visualizations; codescope owns facts,
//! validation, and rendering. This crate provides:
//!
//! - [`AiConfig`]: required env-first provider configuration; keyless local endpoints may be
//!   configured explicitly, and literal keys in config files are rejected
//!   ([`AiError::LiteralApiKeyInConfig`]).
//! - [`AiClient`]: OpenAI-compatible `POST {base}/chat/completions` with controller-selected
//!   Auto or Required tool choice, an in-flight concurrency guard, a high rate ceiling, and a
//!   3-strikes/60 s circuit breaker.
//! - [`parse_plan`]: tool-call arguments → [`codescope_core::VisualizationPlan`].
//! - [`validate`]: the deterministic fact-validation boundary (epoch, entities, semantic-edge
//!   evidence or Sequence transition adjacency, exact changed diff lines, and caps) over a [`FactView`].
//! - [`AiService`]: the request→research/edit tools→draft→validation loop returning an [`AiOutcome`];
//!   never blocks the UI (callers spawn it).
//! - [`tools`]: read-only research and shared incremental diagram tool definitions plus the
//!   [`ToolExecutor`] boundary the binary implements against the fact store.
//!
//! Everything degrades deterministically: any failure maps to an [`AiOutcome`] the TUI
//! can render as a status-line reason, never a blocking error.

#![recursion_limit = "256"]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod client;
mod config;
mod error;
mod plan;
pub mod scrub;
mod service;
pub mod tools;
mod validator;

pub use client::{
    AiClient, AiClientOptions, ChatMessage, RawPlanResponse, RawToolCall, TokenUsage,
    RETRY_AFTER_CAP,
};
pub use config::{
    AiConfig, AiFileConfig, ProviderKind, ReasoningEffort, ANTHROPIC_BASE_URL,
    DEFAULT_ANTHROPIC_MODEL, DEFAULT_MODEL, DEFAULT_OPENAI_MODEL, DEFAULT_TIMEOUT, OPENAI_BASE_URL,
    PRIME_BASE_URL,
};
pub use error::AiError;
pub use plan::parse_plan;
pub use scrub::{scrub_secrets, REDACTED};
pub use service::{
    redact_repo_root, AiActivityObserver, AiActivityUpdate, AiOutcome, AiService,
    AiToolActivityState, DiagramObserver, RetryPolicy,
};
pub use tools::{
    diagram_tools, is_diagram_tool, is_read_only_tool, read_only_tools, research_tools,
    semantic_tools, NoToolExecutor, ToolDef, ToolExecError, ToolExecutor, DIAGRAM_EDIT_TOOL_NAME,
    DIAGRAM_INSPECT_TOOL_NAME, LSP_INSPECT_TOOL_NAME, MAX_TOOL_CALLS,
};
pub use validator::{validate, FactView, Lookup, IMPACT_SUMMARY_MAX_BULLETS};
