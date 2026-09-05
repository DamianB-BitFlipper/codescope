//! `codescope-ai` — the AI visualization engine.
//!
//! The AI only *chooses and parameterizes* visualizations; codescope owns facts,
//! validation, and rendering. This crate provides:
//!
//! - [`AiConfig`]: required env-first provider configuration; keyless local endpoints may be
//!   configured explicitly, and literal keys in config files are rejected
//!   ([`AiError::LiteralApiKeyInConfig`]).
//! - [`AiClient`]: OpenAI Responses, compatible Chat Completions, and native Anthropic Messages
//!   with controller-selected Auto or Required tool choice, an in-flight concurrency guard, a high
//!   rate ceiling, and a 3-strikes/60 s circuit breaker.
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
    AiClient, AiClientOptions, ChatMessage, RETRY_AFTER_CAP, RawPlanResponse, RawToolCall,
    TokenUsage,
};
pub use config::{
    ANTHROPIC_BASE_URL, AiConfig, AiFileConfig, DEFAULT_ANTHROPIC_MODEL, DEFAULT_MODEL,
    DEFAULT_OPENAI_MODEL, DEFAULT_TIMEOUT, OPENAI_BASE_URL, PRIME_BASE_URL, ProviderKind,
    ReasoningEffort,
};
pub use error::AiError;
pub use plan::parse_plan;
pub use scrub::{REDACTED, scrub_secrets};
pub use service::{
    AiActivityObserver, AiActivityUpdate, AiOutcome, AiService, AiToolActivityState,
    DiagramObserver, RetryPolicy, redact_repo_root,
};
pub use tools::{
    DIAGRAM_EDIT_TOOL_NAME, DIAGRAM_INSPECT_TOOL_NAME, LSP_INSPECT_TOOL_NAME, MAX_TOOL_CALLS,
    NoToolExecutor, ToolDef, ToolExecError, ToolExecutor, diagram_tools, is_diagram_tool,
    is_read_only_tool, read_only_tools, research_tools, semantic_tools,
};
pub use validator::{FactView, IMPACT_SUMMARY_MAX_BULLETS, Lookup, validate};
