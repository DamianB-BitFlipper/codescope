//! `codescope-ai` — the optional AI visualization layer (research 05, 07).
//!
//! The AI only *chooses and parameterizes* visualizations; codescope owns facts,
//! validation, and rendering. This crate provides:
//!
//! - [`AiConfig`]: env-first opt-in configuration; disabled by default without a key;
//!   literal keys in config files are rejected ([`AiError::LiteralApiKeyInConfig`]).
//! - [`AiClient`]: OpenAI-compatible `POST {base}/chat/completions` with a required
//!   `submit_visualization_plan` tool call, local rate limiting (governor) and a
//!   3-strikes/60 s circuit breaker.
//! - [`parse_plan`]: tool-call arguments → [`codescope_core::VisualizationPlan`].
//! - [`validate`]: the deterministic fact-validation boundary (epoch gate, entity
//!   resolution, edge existence, hunks by reference, caps) over a [`FactView`].
//! - [`AiService`]: the request→tools→plan→validation loop returning an [`AiOutcome`];
//!   never blocks the UI (callers spawn it).
//! - [`tools`]: the read-only tool definitions and the [`ToolExecutor`] boundary the
//!   binary implements against the fact store.
//!
//! Everything degrades deterministically: any failure maps to an [`AiOutcome`] the TUI
//! can render as a status-line reason, never a blocking error.

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

pub use config::{
    AiConfig, AiFileConfig, ProviderKind, ANTHROPIC_BASE_URL, DEFAULT_ANTHROPIC_MODEL,
    DEFAULT_MODEL, DEFAULT_OPENAI_MODEL, DEFAULT_TIMEOUT, OPENAI_BASE_URL, PRIME_BASE_URL,
};
pub use client::{
    AiClient, AiClientOptions, ChatMessage, RawPlanResponse, RawToolCall, RETRY_AFTER_CAP,
};
pub use error::AiError;
pub use plan::{parse_plan, plan_tool, PlanParams};
pub use scrub::{scrub_secrets, REDACTED};
pub use service::{redact_repo_root, AiOutcome, AiService, RetryPolicy};
pub use validator::{validate, FactView, IMPACT_SUMMARY_MAX_BULLETS};
pub use tools::{
    is_read_only_tool, read_only_tools, NoToolExecutor, ToolDef, ToolExecError, ToolExecutor,
    MAX_TOOL_CALLS, PLAN_TOOL_NAME,
};
